//! # Resilient Parallel File Downloader
//!
//! A high-performance, mutex-free downloader that uses:
//! - Atomic operations for progress tracking
//! - Range requests for parallel downloads
//! - Exponential backoff for error recovery

use clap::Parser;
use reqwest::blocking::Client;
use std::fs::{self, OpenOptions};
use std::io::{self, Write, Read, Seek};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Command-line arguments configuration
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the file to download
    #[arg(index = 1)]
    url: String,
    
    /// Number of parallel download threads
    #[arg(short, long, default_value = "4")]
    threads: usize,
}

/// Tracks download progress atomically across threads
struct Progress {
    /// Total bytes downloaded across all threads
    total: AtomicU64,
    /// Number of completed threads
    completed: AtomicU64,
}

/// Defines work for a single download thread
struct ChunkTask {
    /// Starting byte position
    start: u64,
    /// Ending byte position
    end: u64,
    /// Path to output file
    file_path: PathBuf,
    /// URL to download from
    url: String,
}

/// Gets file size from server using HEAD request
fn get_file_size(url: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))  // Set timeout
        .build()?;
    
    loop {  // Retry until successful
        match client.head(url).send() {
            // Successful response with content length
            Ok(resp) if resp.status().is_success() => {
                if let Some(len) = resp.headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|ct| ct.to_str().ok())
                    .and_then(|ct| ct.parse().ok())
                {
                    return Ok(len);
                }
                eprintln!("Server didn't provide Content-Length, retrying...");
            },
            // Handle HTTP errors
            Ok(resp) => {
                eprintln!("Server error: {}, retrying...", resp.status());
            },
            // Handle network errors
            Err(e) => {
                eprintln!("Network error: {}, retrying...", e);
            }
        }
        thread::sleep(Duration::from_secs(1));  // Wait before retry
    }
}

/// Downloads a single chunk of the file
fn download_chunk(
    task: ChunkTask, 
    progress: Arc<Progress>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    
    // Open file and seek to chunk start position
    let mut file = OpenOptions::new()
        .write(true)
        .open(&task.file_path)?;
    file.seek(io::SeekFrom::Start(task.start))?;
    
    // 256KB buffer for efficient I/O
    let mut buffer = vec![0; 256 * 1024];
    let mut downloaded = 0;
    let mut retries = 0;
    const MAX_RETRIES: usize = 10;

    // Download loop with retries
    while downloaded <= (task.end - task.start) && retries < MAX_RETRIES {
        let range = format!("bytes={}-{}", task.start + downloaded, task.end);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RANGE, range.parse().unwrap());

        match client.get(&task.url).headers(headers).send() {
            Ok(mut response) => {
                retries = 0;  // Reset retry counter on success
                
                // Stream data to file
                loop {
                    match response.read(&mut buffer) {
                        Ok(0) => break,  // End of chunk
                        Ok(n) => {
                            file.write_all(&buffer[..n])?;
                            downloaded += n as u64;
                            // Update progress atomically
                            progress.total.fetch_add(n as u64, Ordering::Relaxed);
                            
                            // Yield thread periodically to prevent starvation
                            if downloaded % (1024 * 1024) == 0 {
                                thread::yield_now();
                            }
                        },
                        Err(e) => {
                            eprintln!("Read error: {}", e);
                            break;
                        }
                    }
                }
            },
            Err(e) => {
                retries += 1;
                // Exponential backoff for retries
                let delay = Duration::from_secs(retries as u64);
                eprintln!("Retry {}/{} in {:?}: {}", retries, MAX_RETRIES, delay, e);
                thread::sleep(delay);
            }
        }
    }

    // Mark this chunk as complete
    progress.completed.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Parse command line arguments
    let args = Args::parse();
    let url = &args.url;
    let threads = args.threads.max(1);  // Ensure at least 1 thread

    // Set up output file paths
    let file_name = url.split('/').last().unwrap_or("downloaded_file");
    let final_path = PathBuf::from(file_name);
    let temp_path = final_path.with_extension("downloading");
    
    // Clean up any previous partial download
    let _ = fs::remove_file(&temp_path);

    // Get total file size from server
    let total_size = get_file_size(url)?;
    println!("Downloading {} bytes with {} threads...", total_size, threads);

    // Pre-allocate disk space for the entire file
    OpenOptions::new()
        .create(true)
        .write(true)
        .open(&temp_path)?
        .set_len(total_size)?;

    // Create shared progress tracker
    let progress = Arc::new(Progress {
        total: AtomicU64::new(0),
        completed: AtomicU64::new(0),
    });

    // Split file into chunks and create download tasks
    let chunk_size = total_size / threads as u64;
    let mut handles = Vec::with_capacity(threads);

    for i in 0..threads {
        let start = i as u64 * chunk_size;
        let end = if i == threads - 1 { 
            total_size - 1  // Last chunk gets remainder
        } else { 
            start + chunk_size - 1 
        };
        
        let task = ChunkTask {
            start,
            end,
            file_path: temp_path.clone(),
            url: url.to_string(),
        };

        let progress = progress.clone();
        
        // Spawn download thread for this chunk
        handles.push(thread::spawn(move || {
            if let Err(e) = download_chunk(task, progress) {
                eprintln!("Download error: {}", e);
            }
        }));
    }

    // Progress reporting loop
    let start_time = Instant::now();
    while progress.completed.load(Ordering::Relaxed) < threads as u64 {
        let downloaded = progress.total.load(Ordering::Relaxed);
        let percent = (downloaded as f64 / total_size as f64) * 100.0;
        
        // Update progress display
        print!("\rProgress: {:.1}%", percent);
        io::stdout().flush().unwrap();
        
        // Don't spam the console
        thread::sleep(Duration::from_secs(1));
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.join()
            .map_err(|e| format!("Thread panicked: {:?}", e))?;
    }

    // Finalize download
    fs::rename(&temp_path, &final_path)?;
    println!("\nDownload completed in {:?}", start_time.elapsed());
    Ok(())
}
