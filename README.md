# progress-token

A Rust library for hierarchical progress tracking with support for cancellation, status updates, and nested operations.

[![Crates.io](https://img.shields.io/crates/v/progress-token.svg)](https://crates.io/crates/progress-token)
[![Documentation](https://docs.rs/progress-token/badge.svg)](https://docs.rs/progress-token)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## Features

- 🌳 **Hierarchical Progress**: Create trees of operations with weighted progress tracking
- 📢 **Status Updates**: Track and propagate status messages through the operation hierarchy
- ⏸️ **Cancellation**: Built-in support for cancelling operations and their children
- 📊 **Progress States**: Support for both determinate (0.0-1.0) and indeterminate progress
- 🔄 **Multiple Subscribers**: Allow multiple parts of your application to monitor progress
- 🧮 **Automatic Aggregation**: Progress automatically calculated from weighted child tasks

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
progress-token = "0.1.0"
```

## Quick Start

```rust
use progress_token::ProgressToken;

#[tokio::main]
async fn main() {
    // Create a root progress token
    let root = ProgressToken::new("Main task");
    
    // Create child tasks with weights
    let process = root.child(0.7, "Processing"); // 70% of total
    let cleanup = root.child(0.3, "Cleanup");    // 30% of total
    
    // Subscribe to progress updates
    let mut updates = root.subscribe();
    
    // Update progress as work is done
    process.progress(0.5);  // Root progress becomes 0.35 (0.5 * 0.7)
    process.status("Processing file 1/10");
    
    cleanup.progress(1.0);  // Root progress becomes 0.65 (0.35 + 1.0 * 0.3)
    cleanup.status("Cleanup complete");
}
```

## Usage Examples

### Basic Progress Tracking

```rust
let token = ProgressToken::new("Processing files");

// Update progress
token.progress(0.25);
token.status("Processing file 1/4");

// Mark as complete when done
token.complete();
```

### Status Updates

Status messages propagate through the hierarchy, with the most specific (deepest) status being reported:

```rust
let root = ProgressToken::new("Backup");
let compress = root.child(1.0, "Compressing files");

// Update status with more specific information
compress.status("Compressing images/photo1.jpg");

// Get full status hierarchy
let statuses = root.statuses();
assert_eq!(statuses, vec![
    "Backup",
    "Compressing files", 
    "Compressing images/photo1.jpg"
]);
```

### Progress Subscription

```rust
let token = ProgressToken::new("Long operation");
let mut updates = token.subscribe();

tokio::spawn(async move {
    while let Some(update) = updates.next().await {
        println!(
            "Progress: {:?}, Current status: {}", 
            update.progress,
            update.status()
        );
    }
});
```

### Cancellation

```rust
let token = ProgressToken::new("Cancellable operation");

// In one part of your code
if error_condition {
    token.cancel(); // Cancels this token and all children
}

// In another part
if token.is_cancelled() {
    return;
}
```

## Implementation Notes

- Progress values are automatically clamped to the range 0.0-1.0
- Child task weights should sum to 1.0 for accurate progress calculation
- Status updates and progress changes are broadcast to all subscribers
- Completed or cancelled tokens ignore further progress/status updates
- The broadcast channel has a reasonable buffer size to prevent lagging

## License

This project is licensed under the MIT License - see the [LICENSE.md](LICENSE.md) file for details. 
