// main.rs - Complete mutex-free parallel downloader
use clap::Parser;
use reqwest::blocking::Client;
use std::fs::{self, OpenOptions};
use std::io::{self, Write, Read, Seek};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(version, about = "Mutex-free parallel downloader", long_about = None)]
struct Args {
    #[arg(index = 1)]
    url: String,
    #[arg(short, long, default_value = "4", value_parser = parse_threads)]
    threads: usize,
}

fn parse_threads(s: &str) -> Result<usize, String> {
    s.parse::<usize>().map_err(|e| e.to_string())
}

// Atomic progress tracking
struct Progress {
    total: AtomicU64,
    completed: AtomicU64,
}

// Per-thread download state
struct ChunkTask {
    start: u64,
    end: u64,
    file_path: PathBuf,
    url: String,
}

fn get_file_size(url: &str) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    
    loop {
        match client.head(url).send() {
            Ok(resp) if resp.status().is_success() => {
                if let Some(len) = resp.headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|ct| ct.to_str().ok())
                    .and_then(|ct| ct.parse().ok())
                {
                    return Ok(len);
                }
                eprintln!("HEAD request succeeded but no Content-Length, retrying...");
            },
            Ok(resp) => {
                eprintln!("HEAD request failed with status: {}, retrying...", resp.status());
            },
            Err(e) => {
                eprintln!("HEAD request error: {}, retrying...", e);
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn download_chunk(task: ChunkTask, progress: Arc<Progress>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    
    let mut file = OpenOptions::new().write(true).open(&task.file_path)?;
    file.seek(io::SeekFrom::Start(task.start))?;
    
    let mut buffer = vec![0; 256 * 1024]; // 256KB buffer
    let mut downloaded = 0;
    let mut retries = 0;
    const MAX_RETRIES: usize = 10;

    while downloaded <= (task.end - task.start) && retries < MAX_RETRIES {
        let range = format!("bytes={}-{}", task.start + downloaded, task.end);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RANGE, range.parse().unwrap());

        match client.get(&task.url).headers(headers).send() {
            Ok(mut response) => {
                retries = 0;
                loop {
                    match response.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            file.write_all(&buffer[..n])?;
                            downloaded += n as u64;
                            progress.total.fetch_add(n as u64, Ordering::Relaxed);
                            
                            // Cooperative yield every 1MB
                            if downloaded % (1024 * 1024) == 0 {
                                thread::yield_now();
                            }
                        },
                        Err(e) => {
                            //eprintln!("Read error: {}", e);
                            break;
                        }
                    }
                }
            },
            Err(e) => {
                retries += 1;
                eprintln!("Retry {}/{}: {}", retries, MAX_RETRIES, e);
                thread::sleep(Duration::from_secs(retries as u64));
            }
        }
    }

    progress.completed.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let url = &args.url;
    let threads = args.threads.max(1);

    // File setup
    let file_name = url.split('/').last().unwrap_or("downloaded_file");
    let final_path = PathBuf::from(file_name);
    let temp_path = final_path.with_extension("downloading");
    let _ = fs::remove_file(&temp_path);

    // Get file size
    let total_size = get_file_size(url)?;

    // Preallocate file
    OpenOptions::new()
        .create(true)
        .write(true)
        .open(&temp_path)?
        .set_len(total_size)?;

    println!("Downloading {} bytes with {} threads...", total_size, threads);

    // Create progress tracker
    let progress = Arc::new(Progress {
        total: AtomicU64::new(0),
        completed: AtomicU64::new(0),
    });

    // Create and launch tasks
    let chunk_size = total_size / threads as u64;
    let mut handles = Vec::new();

    for i in 0..threads {
        let start = i as u64 * chunk_size;
        let end = if i == threads - 1 { total_size - 1 } else { start + chunk_size - 1 };
        
        let task = ChunkTask {
            start,
            end,
            file_path: temp_path.clone(),
            url: url.to_string(),
        };

        let progress = progress.clone();
        
        handles.push(thread::spawn(move || {
            if let Err(e) = download_chunk(task, progress) {
                eprintln!("Thread failed: {}", e);
            }
        }));
    }

    // Progress monitoring
    let start_time = Instant::now();
    while progress.completed.load(Ordering::Relaxed) < threads as u64 {
        let downloaded = progress.total.load(Ordering::Relaxed);
        let percent = (downloaded as f64 / total_size as f64) * 100.0;
        print!("\rProgress: {:.1}%", percent);
        io::stdout().flush().unwrap();
        thread::sleep(Duration::from_secs(1));
    }

    // Cleanup
    for handle in handles {
        handle.join().map_err(|e| format!("Thread join error: {:?}", e))?;
    }

    fs::rename(&temp_path, &final_path)?;
    println!("\nDownload complete in {:?}!", start_time.elapsed());
    Ok(())
}
