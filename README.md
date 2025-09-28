
# Resilient Parallel File Downloader

![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)
![License](https://img.shields.io/badge/License-MIT-blue.svg)

A high-performance, mutex-free parallel file downloader written in Rust, designed for unreliable network conditions.

## Features

- 🚀 **Mutex-free architecture** using atomic operations
- ⚡ **Parallel chunked downloads** with configurable thread count
- 🔄 **Automatic retries** with exponential backoff
- 📊 **Progress tracking** without blocking I/O
- 💾 **File pre-allocation** for efficient disk writes
- 🛡️ **Resilient to network failures** (timeouts, disconnects)

## Installation

1. Clone the repository:
```bash
git clone https://github.com/gilflorida2023/resilient-downloader.git
cd resilient-downloader
cargo build -r
cd target/release
./resiliant-downloader -t 8 https://ollama.com/download/ollama-linux-amd64.tgz
