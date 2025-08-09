// main.rs - A resilient and resumable file downloader written in Rust.
// This version is optimized for robustness with a persistent retry loop for each thread, now with jitter.

use clap::Parser;
use reqwest::blocking::Client;
use std::fs::{self, OpenOptions};
use std::io::{self, Write, Read, Seek};
use std::path::{Path, PathBuf};
use std::thread::{self, sleep};
use std::time::Duration;
use std::sync::{Arc, Mutex};
use serde::{Serialize, Deserialize};
use num_cpus;
use rand::Rng; // We need a crate for randomness.

// Use the `clap` crate to easily parse command-line arguments.
#[derive(Parser, Debug)]
#[command(version, about = "A resilient and resumable file downloader that handles network interruptions.", long_about = None)]
struct Args {
    /// The URL of the file to download.
    #[arg(index = 1)]
    url: String,

    /// Number of parallel threads to use for downloading. Use 'auto' to detect CPU cores.
    #[arg(short, long, default_value = "auto", value_parser = parse_threads)]
    threads: usize,
}

// Custom parser to handle `auto` or a number for the threads argument.
fn parse_threads(s: &str) -> Result<usize, String> {
    if s.to_lowercase() == "auto" {
        Ok(num_cpus::get())
    } else {
        s.parse::<usize>().map_err(|e| e.to_string())
    }
}

// Struct to store the state of a single download chunk.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct ChunkState {
    start: u64,
    end: u64,
    completed: bool,
    downloaded_bytes: u64,
}

// Struct to store the overall download state, saved to a control file.
#[derive(Serialize, Deserialize, Debug)]
struct DownloadState {
    url: String,
    total_size: u64,
    chunks: Vec<ChunkState>,
}

// Function to save the download state to a JSON file.
fn save_state(path: &PathBuf, state: &DownloadState) -> io::Result<()> {
    let json = serde_json::to_string_pretty(state)?;
    fs::write(path, json)?;
    Ok(())
}

// Function to load the download state from a JSON file.
fn load_state(path: &PathBuf) -> io::Result<DownloadState> {
    let json = fs::read_to_string(path)?;
    let state: DownloadState = serde_json::from_str(&json)?;
    Ok(state)
}

// Function to pre-allocate the file and fill it with zeros.
fn preallocate_file(path: &Path, size: u64) -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)?;
    file.set_len(size)?;
    Ok(())
}

// Function to download a single chunk.
fn download_chunk(
    url: &str,
    file_path: &PathBuf,
    chunk_index: usize,
    state: Arc<Mutex<DownloadState>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Client is created once per thread for connection pooling.
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
        
    let initial_delay = Duration::from_secs(1);
    let max_delay = Duration::from_secs(60);
    let mut current_delay = initial_delay;

    // This loop ensures the thread keeps trying until its chunk is fully downloaded.
    loop {
        // CRUCIAL: Get the current progress from the shared state at the start of EACH retry loop.
        let (start, end, downloaded) = {
            let state_guard = state.lock().unwrap();
            let chunk_state = &state_guard.chunks[chunk_index];
            (chunk_state.start, chunk_state.end, chunk_state.downloaded_bytes)
        };
        
        // If the chunk is already complete, break the loop.
        if start + downloaded > end {
            break;
        }

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RANGE,
            format!("bytes={}-{}", start + downloaded, end).parse().unwrap(),
        );

        match client.get(url).headers(headers).send() {
            Ok(mut response) => {
                if !response.status().is_success() {
                    eprintln!("Thread {} failed: {}", chunk_index, response.status());
                    break;
                }

                current_delay = initial_delay;

                let mut output_file = OpenOptions::new()
                    .write(true)
                    .open(file_path)?;

                output_file.seek(io::SeekFrom::Start(start + downloaded))?;
                
                let mut buffer = [0; 8192];
                
                loop {
                    match response.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(bytes_read) => {
                            output_file.write_all(&buffer[..bytes_read])?;

                            // Update the shared state immediately after writing to the file
                            let mut state_guard = state.lock().unwrap();
                            state_guard.chunks[chunk_index].downloaded_bytes += bytes_read as u64;
                        }
                        Err(e) => {
                            eprintln!("Thread {} error reading stream: {}", chunk_index, e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                // Generate a random jitter and add it to the current backoff delay.
                let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..1000));
                let jittered_delay = current_delay + jitter;
                
                eprintln!("Thread {} connection error: {}. Retrying in {:?}...", chunk_index, e, jittered_delay);
                sleep(jittered_delay);
                current_delay = std::cmp::min(current_delay * 2, max_delay);
            }
        }
    }

    let mut state_guard = state.lock().unwrap();
    state_guard.chunks[chunk_index].completed = true;

    let control_path = file_path.with_extension("control");
    save_state(&control_path, &state_guard).unwrap_or_else(|e| eprintln!("Error saving state file: {}", e));

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let url = &args.url;
    let threads = args.threads;

    let file_name = url.split('/').last().unwrap_or("downloaded_file");
    let final_file_path = PathBuf::from(file_name);
    let downloading_file_path = final_file_path.with_extension("downloading");
    let control_path = final_file_path.with_extension("control");

    let mut state: DownloadState;

    if control_path.exists() {
        println!("Resuming download from a previous session...");
        state = load_state(&control_path)?;
    } else {
        println!("Starting new download with {} threads.", threads);

        let initial_delay = Duration::from_secs(1);
        let max_delay = Duration::from_secs(60);
        let mut current_delay = initial_delay;

        let total_size: u64;

        // Client is created once for the HEAD request.
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        loop {
            match client.head(url).send() {
                Ok(head_response) => {
                    if !head_response.status().is_success() {
                        eprintln!("HEAD request failed: {}", head_response.status());
                        sleep(current_delay);
                        current_delay = std::cmp::min(current_delay * 2, max_delay);
                        continue;
                    }

                    total_size = head_response
                        .headers()
                        .get(reqwest::header::CONTENT_LENGTH)
                        .and_then(|ct_len| ct_len.to_str().ok())
                        .and_then(|ct_len| ct_len.parse::<u64>().ok())
                        .ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from("Failed to get Content-Length. Cannot use multi-threaded mode."))?;
                    
                    break; // Exit the retry loop on success
                }
                Err(e) => {
                    eprintln!("HEAD request connection error: {}. Retrying in {:?}...", e, current_delay);
                    sleep(current_delay);
                    current_delay = std::cmp::min(current_delay * 2, max_delay);
                }
            }
        }
        
        println!("Total file size: {} bytes", total_size);

        preallocate_file(&downloading_file_path, total_size)?;

        let chunk_size = total_size / threads as u64;
        let chunks: Vec<ChunkState> = (0..threads)
            .map(|i| {
                let start = i as u64 * chunk_size;
                let end = if i == threads - 1 {
                    total_size - 1
                } else {
                    start + chunk_size - 1
                };
                ChunkState { start, end, completed: false, downloaded_bytes: 0 }
            })
            .collect();
        
        state = DownloadState { url: url.to_string(), total_size, chunks };
        save_state(&control_path, &state)?;
    }
    
    let shared_state = Arc::new(Mutex::new(state));

    let mut download_handles = Vec::with_capacity(threads);

    for i in 0..threads {
        if !shared_state.lock().unwrap().chunks[i].completed {
            let url_clone = url.to_string();
            let state_clone = shared_state.clone();
            let downloading_file_path_clone = downloading_file_path.clone();

            let handle = thread::spawn(move || {
                download_chunk(&url_clone, &downloading_file_path_clone, i, state_clone)
            });
            download_handles.push(handle);
        }
    }

    // Now, create the dedicated display thread.
    let display_state_clone = shared_state.clone();
    let display_handle = thread::spawn(move || {
        let mut last_total_downloaded: u64 = 0;
        loop {
            let state_guard = display_state_clone.lock().unwrap();
            let total_downloaded: u64 = state_guard.chunks.iter().map(|c| c.downloaded_bytes).sum();
            let total_size = state_guard.total_size;

            let all_completed = state_guard.chunks.iter().all(|c| c.completed);
            drop(state_guard);

            if total_downloaded != last_total_downloaded {
                let total_progress = (total_downloaded as f64 / total_size as f64) * 100.0;
                
                // Use a carriage return for a clean, single-line update.
                print!("\rTotal Progress: {:.2}% ({} / {} bytes)", total_progress, total_downloaded, total_size);
                io::stdout().flush().unwrap();
                
                last_total_downloaded = total_downloaded;
            }

            if all_completed {
                break;
            }
            
            sleep(Duration::from_millis(500));
        }
    });

    for handle in download_handles {
        handle.join().unwrap()?;
    }

    display_handle.join().unwrap();
    
    // Print a final newline to make sure the shell prompt starts on a fresh line.
    println!("\nDownload complete!");

    fs::rename(&downloading_file_path, &final_file_path)?;
    fs::remove_file(&control_path)?;

    Ok(())
}

