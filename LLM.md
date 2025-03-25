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
- RAII-style completion guards

---

## method signatures

### ProgressToken<S>

core type that represents a progress tracking token. the type parameter `S` represents the type of status messages (usually `String`).

```rust
impl<S: Clone + Send + 'static> ProgressToken<S> {
    // creation
    pub fn new(status: impl Into<S>) -> Self
    pub fn child(&self, weight: f64, status: impl Into<S>) -> Self

    // progress updates
    pub fn update_progress(&self, progress: f64)  // value is clamped to 0.0-1.0
    pub fn update_indeterminate(&self)
    pub fn update_status(&self, status: impl Into<S>)
    pub fn update(&self, progress: Progress, status: impl Into<S>)
    
    // completion and cancellation
    pub fn complete(&self)  // sets progress to 1.0, prevents further updates
    pub fn cancel(&self)    // cancels this and all child tokens
    pub fn complete_guard(&self) -> CompleteGuard<'_, S>  // RAII completion
    pub fn cancelled(&self) -> WaitForCancellationFuture
    pub fn check(&self) -> Result<(), ProgressError>  // returns Err(Cancelled) if cancelled
    pub fn is_cancelled(&self) -> bool
    
    // state inspection
    pub fn state(&self) -> Progress
    pub fn statuses(&self) -> Vec<S>
    
    // subscription
    pub fn subscribe(&self) -> ProgressStream<'_, S>
    pub async fn updated(&self) -> Result<ProgressUpdate<S>, ProgressError>
}
```

### CompleteGuard<'a, S>

RAII guard that completes a token when dropped.

```rust
impl<'a, S: Clone + Send + 'static> CompleteGuard<'a, S> {
    pub fn forget(self)  // prevents completion when dropped
}
```

### Progress

represents either a determinate progress value or indeterminate state.

```rust
#[derive(Debug, Clone, Copy)]
pub enum Progress {
    Determinate(f64),  // value between 0.0 and 1.0
    Indeterminate,
}

impl Progress {
    pub fn as_f64(&self) -> Option<f64>
}
```

### ProgressUpdate<S>

data for a progress update event.

```rust
#[derive(Debug, Clone)]
pub struct ProgressUpdate<S> {
    pub progress: Progress,
    pub statuses: Vec<S>,
    pub is_cancelled: bool,
}

impl<S> ProgressUpdate<S> {
    pub fn status(&self) -> &S  // returns most specific (last) status
}
```

### ProgressError

error type for progress update operations.

```rust
#[derive(Debug, Clone, Copy)]
pub enum ProgressError {
    Lagged,     // updates were missed due to slow consumption
    Cancelled,  // token was cancelled
}
```

---

## completion and cancellation behavior

both completion and cancellation are permanent states that prevent any further updates to a token:

```rust
let token = ProgressToken::new("task");

// after completion:
token.complete();
token.update_progress(0.5);     // ignored
token.update_status("update");  // ignored
token.update_indeterminate();   // ignored

// after cancellation:
token.cancel();
token.update_progress(0.5);     // ignored
token.update_status("update");  // ignored
token.update_indeterminate();   // ignored
```

key points:
- completion sets progress to 1.0 permanently
- cancellation propagates to all child tokens
- neither state can be reversed
- both states prevent all further updates
- subscribers are notified of both states

### using completion guards

completion guards provide RAII-style completion:

```rust
let token = ProgressToken::new("task");

{
    let _guard = token.complete_guard();
    token.update_progress(0.5);
    // guard drop -> token.complete()
}

// prevent completion:
{
    let guard = token.complete_guard();
    token.update_progress(0.5);
    guard.forget();  // completion won't happen
}
```

---

## getting started

### creating a root token

```rust
use progress_token::ProgressToken;

#[tokio::main]
async fn main() {
    let token = ProgressToken::new("initial task");
    
    token.update_progress(0.25);
    token.update_status("processing");

    let mut updates = token.subscribe();
    tokio::spawn(async move {
        while let Some(update) = updates.next().await {
            println!("progress: {:?}, status: {:?}", update.progress, update.status());
        }
    });
    
    token.complete();  // prevents further updates
}
```

### creating child tokens

```rust
let root = ProgressToken::new("main task");
let process = root.child(0.7, "processing");  // 70% weight
let cleanup = root.child(0.3, "cleanup");     // 30% weight

process.update_progress(0.5);  // root progress = 0.35 (0.7 * 0.5)
cleanup.update_progress(1.0);  // root progress = 0.65 (0.35 + 0.3 * 1.0)

process.complete();  // sets process progress to 1.0
cleanup.complete();  // sets cleanup progress to 1.0
```

### handling updates

```rust
let token = ProgressToken::new("task");
let mut updates = token.subscribe();

// stream interface
while let Some(Ok(update)) = updates.next().await {
    println!("progress: {:?}", update.progress);
    println!("status: {}", update.status());
    println!("cancelled: {}", update.is_cancelled);
}

// handle errors
while let Some(result) = updates.next().await {
    match result {
        Ok(update) => {
            println!("progress: {:?}", update.progress);
            println!("status: {}", update.status());
            println!("cancelled: {}", update.is_cancelled);
        }
        Err(ProgressError::Lagged) => {
            println!("some updates were missed");
        }
        Err(ProgressError::Cancelled) => {
            println!("token was cancelled");
            break;
        }
    }
}

// single update
match token.updated().await {
    Ok(update) => println!("progress: {:?}", update.progress),
    Err(ProgressError::Cancelled) => println!("cancelled"),
    Err(ProgressError::Lagged) => println!("updates missed"),
}
```

---

## best practices

- always check cancellation before expensive operations
- use completion guards for RAII-style completion
- ensure child weights sum to approximately 1.0
- handle both completion and cancellation states
- consider rate limiting frequent updates
- drop completion guards in reverse order of creation
- use `forget()` sparingly and only when necessary

---

## error handling

common error cases:

```rust
// handle missed updates
match token.updated().await {
    Err(ProgressError::Lagged) => {
        // some updates were missed, but can continue
    }
    Err(ProgressError::Cancelled) => {
        // token was cancelled, stop processing
        return;
    }
    Ok(update) => {
        // process update
    }
}

// check cancellation before expensive work
if token.cancel_token.is_cancelled() {
    return;
}

// or use the ? operator for convenient cancellation handling
async fn process_with_cancellation(token: &ProgressToken<String>) -> Result<(), ProgressError> {
    token.check()?;  // early return if cancelled
    
    // do work...
    token.update_progress(0.5);
    
    token.check()?;  // check again before more work
    
    // continue processing...
    token.complete();
    Ok(())
}
```

---

## summary

this guide provided a comprehensive reference for the progress-token crate, including all method signatures and detailed behavior descriptions. use the examples and signatures above to implement robust progress tracking in your async rust applications.

remember that completion and cancellation are permanent states that prevent further updates, and consider using completion guards for automatic cleanup.

happy coding! 
