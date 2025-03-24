# progress-token usage guide

this guide provides a comprehensive introduction and reference for using the progress-token rust crate. this crate is designed for hierarchical progress tracking in asynchronous rust applications. it supports both determinate (0.0 to 1.0) and indeterminate progress states, alongside advanced cancellation, status updates, and subscription streams.

---

## features

- hierarchical progress structure using weighted tasks
- propagation of status messages in a tree structure
- support for cancellation and completion that stops further updates
- automatic aggregation of progress from nested child tasks
- multiple subscribers via a broadcast channel (buffer size of 16)
- integration with tokio for asynchronous operations

---

## installation

to use this crate, add the following to your `Cargo.toml`:

```toml
[dependencies]
progress-token = "0.1.0"
```

---

## getting started

### creating a root token

the primary entry point is the `ProgressToken` type. create a new root token as follows:

```rust
use progress_token::ProgressToken;

#[tokio::main]
async fn main() {
    // create a root token with an initial status
    let token = ProgressToken::new("initial task");
    
    // update token progress and status
    token.progress(0.25);
    token.status("started processing");

    // subscribe to updates
    let mut updates = token.subscribe();
    tokio::spawn(async move {
        while let Some(update) = updates.next().await {
            println!("progress: {:?}, status: {:?}", update.progress, update.status());
        }
    });
    
    // further progress updates
    token.progress(0.5);
    token.status("mid task processing");
    
    // mark completion
    token.complete();
}
```

---

### creating child tokens

root tokens can spawn child tokens to reflect subtasks. the child tokens contribute to the overall progress via weighted averages.

```rust
use progress_token::ProgressToken;

#[tokio::main]
async fn main() {
    // create a root token
    let root = ProgressToken::new("main task");
    
    // create child tokens with specified weights
    let process = root.child(0.7, "processing");
    let cleanup = root.child(0.3, "cleanup");
    
    // update progress in child tokens
    process.progress(0.5); // contributes 0.35 (0.7 * 0.5)
    cleanup.progress(1.0); // contributes 0.30, overall progress becomes 0.65

    // update status at different levels
    process.status("processing file 1 of 10");
    cleanup.status("cleanup in progress");

    // aggregate statuses from root downwards
    let statuses = root.statuses();
    println!("status hierarchy: {:?}", statuses);
}
```

---

### progress clamping and indeterminate state

- progress values are automatically clamped between 0.0 and 1.0.
- to set an indeterminate state (when exact progress is unknown), use the `indeterminate` method:

```rust
use progress_token::ProgressToken;

fn set_indeterminate(token: &ProgressToken<String>) {
    token.indeterminate();
}
```

---

### cancellation and completion

**cancellation:**

- cancel a token to prevent any further progress or status updates.
- cancellation propagates to all child tokens.

```rust
use progress_token::ProgressToken;

fn cancel_task(token: &ProgressToken<String>) {
    token.cancel();
    // subsequent updates will be ignored
}
```

**completion:**

- mark a token as complete when its task is finished. this sets progress to 1.0.

```rust
use progress_token::ProgressToken;

fn complete_task(token: &ProgressToken<String>) {
    token.complete();
}
```

---

### subscribing to progress updates

use the `subscribe` method to receive a stream of `ProgressUpdate` events. each update contains:

- **progress:** a `Progress` enum (either `Determinate(f64)` or `Indeterminate`)
- **statuses:** a vector representing the hierarchy of status messages, from the root to the deepest active child
- **is_cancelled:** a boolean indicating if the token has been cancelled

```rust
use progress_token::ProgressToken;
use futures::StreamExt;

#[tokio::main]
async fn main() {
    let token = ProgressToken::new("long op");
    let mut sub = token.subscribe();

    // handle updates in an asynchronous task
    tokio::spawn(async move {
        while let Some(update) = sub.next().await {
            println!("progress: {:?}, status hierarchy: {:?}", update.progress, update.statuses);
        }
    });

    token.progress(0.3);
    token.status("30% done");
}
```

---

### asynchronous update handling

alternatively, use the `updated` method to await a single update. note that if the token is cancelled, it returns an error:

```rust
use progress_token::{ProgressToken, ProgressError};

#[tokio::main]
async fn main() {
    let token = ProgressToken::new("async update");
    
    match token.updated().await {
        Ok(update) => {
            println!("received update: {:?}, status: {:?}", update.progress, update.status());
        },
        Err(ProgressError::Cancelled) => {
            println!("token was cancelled");
        },
        Err(ProgressError::Lagged) => {
            println!("updates lagged");
        }
    }
}
```

---

## deep dive: internal mechanics

### progress aggregation

- the root token calculates overall progress as a weighted average of its child tokens' progress.
- if any child is indeterminate, the overall progress becomes indeterminate.
- each child token is created with a weight; ensure weights sum to roughly 1.0 for accurate results.

### status hierarchy

- every token holds a status message that represents its current operation.
- calling `statuses` on a token returns a vector of status messages beginning at the root token and ending at the deepest active child.

### subscription mechanism

- updates are sent via a tokio broadcast channel with a limited buffer (16 slots).
- if updates are produced too rapidly, lagging may occur, resulting in `ProgressError::Lagged`.

### cancellation mechanism

- cancellation is implemented using tokio's `CancellationToken`.
- when a token is cancelled, it prevents further progress and status updates, and this state propagates to all child tokens.

---

## best practices

- always check `is_cancelled` before engaging in resource-intensive work.
- share tokens safely with `Arc<ProgressToken<S>>` when working in multithreaded contexts.
- ensure child tokens have weights that add up to approximately 1.0 for proper progress calculation.
- handle potential `Lagged` errors when subscribing to updates.
- update progress and status frequently but consider rate limiting if necessary.

---

## advanced examples

### nested hierarchical progress

this example demonstrates a multi-level progress tree with nested tokens:

```rust
use progress_token::ProgressToken;
use futures::StreamExt;

#[tokio::main]
async fn main() {
    // create a root token for a backup operation
    let root = ProgressToken::new("backup task");
    
    // create child tokens with weights
    let compress = root.child(0.8, "compress files");
    let upload = root.child(0.2, "upload files");
    
    // update child token progress
    compress.progress(0.75); // weighted to 0.6 overall
    upload.progress(1.0);      // weighted to 0.2 overall
    
    // update status messages
    compress.status("compressing images");
    upload.status("upload in progress");

    // subscribe and display updates
    let mut sub = root.subscribe();
    tokio::spawn(async move {
        while let Some(update) = sub.next().await {
            println!("overall progress: {:?}, status hierarchy: {:?}", update.progress, update.statuses);
        }
    });
    
    // mark tokens complete
    compress.complete();
    upload.complete();
    root.complete();
}
```

### handling updates concurrently

this example shows multiple tasks updating the same token concurrently:

```rust
use progress_token::ProgressToken;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() {
    let token = ProgressToken::new("concurrent op");
    
    // spawn several tasks to update progress concurrently
    for i in 0..10 {
        let t = token.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(i * 10)).await;
            t.progress(i as f64 / 10.0);
        });
    }
    
    // wait, then check final progress
    sleep(Duration::from_secs(1)).await;
    println!("final progress: {:?}", token.state());
}
```

---

## summary

this guide provided an exhaustive overview of the progress-token crate. by leveraging its hierarchical progress tracking, cancellation features, and subscription mechanism, you can seamlessly incorporate detailed progress reporting into your asynchronous rust applications. use the examples and best practices above to integrate these features in a robust and error-free manner.

happy coding! 
