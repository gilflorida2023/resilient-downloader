// main.rs - A resilient file downloader written in Rust.

use clap::Parser;
use reqwest::blocking::Client;
use std::fs::OpenOptions;
use std::io::{self, Write, Read, Seek};
use std::path::PathBuf;
use std::thread::{self, sleep};
use std::time::Duration;
use std::sync::{Arc, Mutex};

// Use num_cpus to automatically detect the number of logical CPU cores.
use num_cpus;

// Use the `clap` crate to easily parse command-line arguments.
#[derive(Parser, Debug)]
#[command(version, about = "A resilient file downloader that handles network interruptions.", long_about = None)]
struct Args {
    /// The URL of the file to download.
    #[arg(index = 1)]
    url: String,

    /// Number of parallel threads to use for downloading. Use 'auto' to detect CPU cores.
    #[arg(short, long, default_value_t = 1, value_parser = parse_threads)]
    threads: usize,
    
    /// Enable verbose output, showing progress for each thread.
    #[arg(short, long, action)]
    verbose: bool,
}

// Custom parser to handle `auto` or a number for the threads argument.
fn parse_threads(s: &str) -> Result<usize, String> {
    if s.to_lowercase() == "auto" {
        Ok(num_cpus::get())
    } else {
        s.parse::<usize>().map_err(|e| e.to_string())
    }
}

// Function to perform a single-threaded download (fallback behavior).
fn download_single_threaded(url: &str, path: &PathBuf) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::new();
    let mut downloaded_size: u64 = path.exists().then(|| path.metadata().unwrap().len()).unwrap_or(0);
    let mut total_size: Option<u64> = None;
    let initial_delay = Duration::from_secs(1);
    let max_delay = Duration::from_secs(60);
    let mut current_delay = initial_delay;
    let file_name = path.file_name().unwrap().to_str().unwrap();

    loop {
        if let Some(size) = total_size {
            if downloaded_size >= size {
                println!("\nDownload complete!");
                break;
            }
        }

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RANGE,
            format!("bytes={}-", downloaded_size).parse().unwrap(),
        );

        match client.get(url).headers(headers).send() {
            Ok(mut response) => {
                if !response.status().is_success() {
                    eprintln!("Error: Server returned an error status: {}", response.status());
                    break;
                }

                if total_size.is_none() {
                    let new_content_length = response
                        .headers()
                        .get(reqwest::header::CONTENT_LENGTH)
                        .and_then(|ct_len| ct_len.to_str().ok())
                        .and_then(|ct_len| ct_len.parse::<u64>().ok())
                        .unwrap_or(0);
                    total_size = Some(new_content_length + downloaded_size);

                    if let Some(size) = total_size {
                        println!("Downloading '{}', Total size: {} bytes", file_name, size);
                    } else {
                        println!("Downloading '{}', Total size: Unknown", file_name);
                    }
                }

                current_delay = initial_delay;

                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;

                let mut buffer = [0; 8192];
                loop {
                    match response.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(bytes_read) => {
                            file.write_all(&buffer[..bytes_read])?;
                            downloaded_size += bytes_read as u64;

                            if let Some(size) = total_size {
                                let progress = (downloaded_size as f64 / size as f64) * 100.0;
                                print!("\rProgress: {:.2}% ({} / {} bytes)", progress, downloaded_size, size);
                            } else {
                                print!("\rDownloaded: {} bytes", downloaded_size);
                            }
                            io::stdout().flush()?;
                        }
                        Err(e) => {
                            eprintln!("\nError reading stream: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("\nConnection error: {}. Retrying in {:?}...", e, current_delay);
                sleep(current_delay);
                current_delay = std::cmp::min(current_delay * 2, max_delay);
            }
        }
    }
    Ok(())
}

fn download_chunk(
    url: &str,
    path: &PathBuf,
    start: u64,
    end: u64,
    thread_id: usize,
    status: Arc<Mutex<Vec<u64>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::new();
    let initial_delay = Duration::from_secs(1);
    let max_delay = Duration::from_secs(60);
    let mut current_delay = initial_delay;
    let mut downloaded = 0;

    if path.exists() {
        if let Ok(metadata) = path.metadata() {
            if metadata.len() > start {
                downloaded = metadata.len() - start;
                let mut status_guard = status.lock().unwrap();
                status_guard[thread_id] = downloaded;
            }
        }
    }

    loop {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RANGE,
            format!("bytes={}-{}", start + downloaded, end).parse().unwrap(),
        );

        match client.get(url).headers(headers).send() {
            Ok(mut response) => {
                if !response.status().is_success() {
                    eprintln!("Thread {} failed: {}", thread_id, response.status());
                    break;
                }

                current_delay = initial_delay;

                let mut file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(path)?;
                file.seek(io::SeekFrom::Start(start + downloaded))?;

                let mut buffer = [0; 8192];
                loop {
                    match response.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(bytes_read) => {
                            file.write_all(&buffer[..bytes_read])?;
                            downloaded += bytes_read as u64;
                            let mut status_guard = status.lock().unwrap();
                            status_guard[thread_id] = downloaded;
                        }
                        Err(e) => {
                            eprintln!("Thread {} error reading stream: {}", thread_id, e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Thread {} connection error: {}. Retrying in {:?}...", thread_id, e, current_delay);
                sleep(current_delay);
                current_delay = std::cmp::min(current_delay * 2, max_delay);
            }
        }
        if start + downloaded > end {
            break;
        }
    }

    Ok(())
}


fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let url = &args.url;
    let threads = args.threads;
    let verbose = args.verbose;

    let file_name = url.split('/').last().unwrap_or("downloaded_file");
    let path = PathBuf::from(file_name);
    
    // Fallback to single-threaded download if threads = 1.
    if threads == 1 {
        // Since the user is requesting verbose output, let's just make
        // the single-threaded mode print a bit more.
        if verbose {
            println!("Starting single-threaded download (verbose mode)...");
        }
        return download_single_threaded(url, &path);
    }

    println!("Attempting to download with {} threads.", threads);

    let client = Client::new();
    let head_response = client.head(url).send()?;
    let total_size = head_response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|ct_len| ct_len.to_str().ok())
        .and_then(|ct_len| ct_len.parse::<u64>().ok())
        .ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from("Failed to get Content-Length from HEAD request. Cannot use multi-threaded mode."))?;

    println!("Total file size: {} bytes", total_size);
    if verbose {
        println!("\n[Verbose Output]");
    }

    let chunk_size = total_size / threads as u64;
    let mut chunks: Vec<(u64, u64)> = Vec::with_capacity(threads);
    for i in 0..threads {
        let start = i as u64 * chunk_size;
        let end = if i == threads - 1 {
            total_size - 1
        } else {
            start + chunk_size - 1
        };
        chunks.push((start, end));
    }
    
    // Create an Arc<Mutex<Vec<u64>>> to track the progress of each thread
    let downloaded_status: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(vec![0; threads]));

    let mut handles = Vec::with_capacity(threads);

    for (i, &(start, end)) in chunks.iter().enumerate() {
        let url_clone = url.to_string();
        let path_clone = path.clone();
        let status_clone = downloaded_status.clone();
        let handle = thread::spawn(move || {
            download_chunk(&url_clone, &path_clone, start, end, i, status_clone)
        });
        handles.push(handle);
    }
    
    loop {
        let total_downloaded: u64 = downloaded_status.lock().unwrap().iter().sum();
        
        if verbose {
            // Clear the previous lines and print the new ones
            for _ in 0..threads {
                print!("\x1B[1A\x1B[2K"); // Move cursor up and clear line
            }
            
            let status_guard = downloaded_status.lock().unwrap();
            for (i, &downloaded_bytes) in status_guard.iter().enumerate() {
                let chunk_start = chunks[i].0;
                let chunk_end = chunks[i].1;
                let chunk_total_size = chunk_end - chunk_start + 1;
                let progress = (downloaded_bytes as f64 / chunk_total_size as f64) * 100.0;
                println!("Thread {}: {:.2}% ({} / {} bytes)", i, progress, downloaded_bytes, chunk_total_size);
            }
        }

        let total_progress = (total_downloaded as f64 / total_size as f64) * 100.0;
        print!("\rTotal Progress: {:.2}% ({} / {} bytes)", total_progress, total_downloaded, total_size);
        io::stdout().flush()?;
        
        if total_downloaded >= total_size {
            break;
        }
        
        sleep(Duration::from_millis(500));
    }

    for handle in handles {
        handle.join().unwrap()?;
    }

    println!("\nDownload complete!");
    Ok(())
}

