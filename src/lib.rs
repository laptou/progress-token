//! A hierarchical progress tracking library that allows monitoring and reporting progress
//! of long-running tasks with support for cancellation, status updates, and nested operations.
//!
//! # Overview
//!
//! This library provides a [`ProgressToken`] type that combines progress tracking, status updates,
//! and cancellation into a single unified interface. Unlike a basic [`CancellationToken`],
//! `ProgressToken` supports:
//!
//! - Hierarchical progress tracking with weighted child operations
//! - Status message updates that propagate through the hierarchy
//! - Both determinate (0.0-1.0) and indeterminate progress states
//! - Multiple subscribers for progress/status updates
//! - Automatic progress aggregation from child tasks
//!
//! # Examples
//!
//! Basic usage with progress updates:
//!
//! ```rust
//! use progress_token::ProgressToken;
//!
//! #[tokio::main]
//! async fn main() {
//!     // Create a root token
//!     let token = ProgressToken::new("Processing files");
//!     
//!     // Update progress as work is done
//!     token.progress(0.25);
//!     token.status("Processing file 1/4");
//!     
//!     // Subscribe to updates
//!     let mut updates = token.subscribe();
//!     while let Some(update) = updates.next().await {
//!         println!("Progress: {:?}, Status: {}",
//!             update.progress,
//!             update.status());
//!     }
//!
//!     // Mark as complete when done - no further updates will be processed
//!     token.complete();
//! }
//! ```
//!
//! Using hierarchical progress with weighted child tasks:
//!
//! ```rust
//! use progress_token::ProgressToken;
//!
//! #[tokio::main]
//! async fn main() {
//!     let root = ProgressToken::new("Main task");
//!     
//!     // Create child tasks with weights
//!     let process = root.child(0.7, "Processing"); // 70% of total
//!     let cleanup = root.child(0.3, "Cleanup");    // 30% of total
//!     
//!     // Child progress is automatically weighted and propagated
//!     process.progress(0.5);  // Root progress becomes 0.35 (0.5 * 0.7)
//!     cleanup.progress(1.0);  // Root progress becomes 0.65 (0.35 + 1.0 * 0.3)
//!     
//!     // Complete each task when done
//!     process.complete();  // Sets progress to 1.0 and prevents further updates
//!     cleanup.complete();  // Sets progress to 1.0 and prevents further updates
//! }
//! ```
//!
//! # Status Updates
//!
//! The status feature allows tracking the current operation being performed. Status messages
//! propagate up through the hierarchy, with the most specific (deepest) status being reported:
//!
//! ```rust
//! use progress_token::ProgressToken;
//!
//! #[tokio::main]
//! async fn main() {
//!     let root = ProgressToken::new("Backup");
//!     let compress = root.child(1.0, "Compressing files");
//!     
//!     // Update status with more specific information
//!     compress.status("Compressing images/photo1.jpg");
//!     
//!     // Get full status hierarchy
//!     let statuses = root.statuses();
//!     assert_eq!(statuses, vec![
//!         "Backup",
//!         "Compressing files",
//!         "Compressing images/photo1.jpg"
//!     ]);
//!
//!     // Once cancelled or completed, status updates are ignored
//!     compress.complete();
//!     compress.status("This update will be ignored");
//! }
//! ```
//!
//! # Differences from CancellationToken
//!
//! While this library includes cancellation support, [`ProgressToken`] provides several
//! advantages over a basic [`CancellationToken`]:
//!
//! - **Rich Status Information**: Track not just whether an operation is cancelled, but its
//!   current progress and status.
//! - **Hierarchical Structure**: Create trees of operations where progress and cancellation
//!   flow naturally through parent-child relationships.
//! - **Progress Aggregation**: Automatically calculate overall progress from weighted child
//!   tasks instead of managing it manually.
//! - **Multiple Subscribers**: Allow multiple parts of your application to monitor progress
//!   through the subscription system.
//! - **Determinate/Indeterminate States**: Handle both measurable progress and operations
//!   where progress cannot be determined.
//!
//! # Implementation Notes
//!
//! - Progress values are automatically clamped to the range 0.0-1.0
//! - Child task weights should sum to 1.0 for accurate progress calculation
//! - Status updates and progress changes are broadcast to all subscribers
//! - Once a token is completed or cancelled, all further updates are ignored
//! - The broadcast channel has a reasonable buffer size to prevent lagging
//! - Completion and cancellation are permanent states - they cannot be undone
//! - When a token is cancelled, all its children are also cancelled
//! - When a token is completed, its progress is set to 1.0 and no further updates are allowed

use futures::{Stream, ready};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

/// A guard that automatically marks a [`ProgressToken`] as complete when dropped
#[must_use = "if unused, the progress token will be completed immediately"]
pub struct CompleteGuard<'a, S: Clone + Send + 'static> {
    token: &'a ProgressToken<S>,
}

impl<'a, S: Clone + Send + 'static> CompleteGuard<'a, S> {
    /// Forgets the guard without completing the progress token
    pub fn forget(self) {
        std::mem::forget(self);
    }
}

impl<'a, S: Clone + Send + 'static> Drop for CompleteGuard<'a, S> {
    fn drop(&mut self) {
        self.token.complete();
    }
}

/// Represents either a determinate progress value or indeterminate state
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Progress {
    Determinate(f64),
    Indeterminate,
}

impl Progress {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Progress::Determinate(v) => Some(*v),
            Progress::Indeterminate => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Error)]
pub enum ProgressError {
    /// Too many progress updates have occurred since last polled, so some of
    /// them have been dropped
    #[error("progress updates lagged")]
    Lagged,
    /// This progress token has been cancelled, no more updates are coming
    #[error("the operation has been cancelled")]
    Cancelled,
}

/// Data for a progress update event
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProgressUpdate<S> {
    pub progress: Progress,
    pub statuses: Vec<S>,
    pub is_cancelled: bool,
}

impl<S> ProgressUpdate<S> {
    pub fn status(&self) -> &S {
        self.statuses.last().unwrap()
    }
}

/// Inner data of a progress node
struct ProgressNodeInner<S> {
    // Tree structure
    parent: Option<Arc<ProgressNode<S>>>,
    children: Vec<(Arc<ProgressNode<S>>, f64)>, // Node and its weight

    // Progress state
    progress: Progress,
    status: S,
    is_completed: bool,

    // Subscriber management
    update_sender: broadcast::Sender<ProgressUpdate<S>>,
}

/// A node in the progress tree
struct ProgressNode<S> {
    inner: Mutex<ProgressNodeInner<S>>,
    // change_notify: Notify,
}

impl<S: Clone + Send> ProgressNode<S> {
    fn new(status: S) -> Self {
        // create broadcast channel with reasonable buffer size
        let (tx, _) = broadcast::channel(16);

        Self {
            inner: Mutex::new(ProgressNodeInner {
                parent: None,
                children: Vec::new(),
                progress: Progress::Determinate(0.0),
                status,
                is_completed: false,
                update_sender: tx,
            }),
            // change_notify: Notify::new(),
        }
    }

    fn child(parent: &Arc<Self>, weight: f64, status: S) -> Arc<Self> {
        let mut parent_inner = parent.inner.lock().unwrap();

        // create broadcast channel with reasonable buffer size
        let (tx, _) = broadcast::channel(16);

        let child = Self {
            inner: Mutex::new(ProgressNodeInner {
                parent: Some(parent.clone()),
                children: Vec::new(),
                progress: Progress::Determinate(0.0),
                status,
                is_completed: false,
                update_sender: tx,
            }),
            // change_notify: Notify::new(),
        };

        let child = Arc::new(child);

        parent_inner.children.push((child.clone(), weight));

        child
    }

    fn calculate_progress(node: &Arc<Self>) -> Progress {
        let inner = node.inner.lock().unwrap();

        // If this node itself is indeterminate, propagate that
        if matches!(inner.progress, Progress::Indeterminate) {
            return Progress::Indeterminate;
        }

        if inner.children.is_empty() {
            return inner.progress;
        }

        // Check if any active child is indeterminate
        let has_indeterminate = inner
            .children
            .iter()
            .filter(|(child, _)| {
                let child_inner = child.inner.lock().unwrap();
                !child_inner.is_completed
            })
            .any(|(child, _)| matches!(Self::calculate_progress(child), Progress::Indeterminate));

        if has_indeterminate {
            return Progress::Indeterminate;
        }

        // Calculate weighted average of determinate children
        let total: f64 = inner
            .children
            .iter()
            .map(|(child, weight)| {
                match Self::calculate_progress(child) {
                    Progress::Determinate(p) => p * weight,
                    Progress::Indeterminate => 0.0, // Shouldn't happen due to check above
                }
            })
            .sum();

        Progress::Determinate(total)
    }

    fn get_status_hierarchy(node: &Arc<Self>) -> Vec<S> {
        let inner = node.inner.lock().unwrap();
        let mut result = vec![inner.status.clone()];

        // Find active child
        if !inner.children.is_empty() {
            let active_child = inner
                .children
                .iter()
                .filter(|(child, _)| {
                    let child_inner = child.inner.lock().unwrap();
                    !child_inner.is_completed
                })
                .next();

            if let Some((child, _)) = active_child {
                let child_statuses = Self::get_status_hierarchy(child);
                result.extend(child_statuses);
            }
        }

        result
    }

    fn notify_subscribers(node: &Arc<Self>, is_cancelled: bool) {
        // Create update while holding the lock
        let update = ProgressUpdate {
            progress: Self::calculate_progress(node),
            statuses: Self::get_status_hierarchy(node),
            is_cancelled,
        };

        // Send updates without holding the lock
        {
            let inner = node.inner.lock().unwrap();
            // broadcast to all subscribers, ignore send errors (no subscribers/full)
            let _ = inner.update_sender.send(update);
        };

        // Notify waiters
        // node.change_notify.notify_waiters();

        // Propagate to parent
        let parent = {
            let inner = node.inner.lock().unwrap();
            inner.parent.clone()
        };

        if let Some(parent) = parent {
            Self::notify_subscribers(&parent, false);
        }
    }
}

/// A token that tracks the progress of a task and can be organized hierarchically
#[derive(Clone)]
pub struct ProgressToken<S> {
    node: Arc<ProgressNode<S>>,
    cancel_token: CancellationToken,
}

impl<S: std::fmt::Debug> std::fmt::Debug for ProgressToken<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressToken")
            .field("is_cancelled", &self.cancel_token.is_cancelled())
            .finish()
    }
}

impl<S: Clone + Send + 'static> ProgressToken<S> {
    /// Create a new root ProgressToken
    pub fn new(status: impl Into<S>) -> Self {
        let node = Arc::new(ProgressNode::new(status.into()));

        Self {
            node,
            cancel_token: CancellationToken::new(),
        }
    }

    /// Create a child token
    pub fn child(&self, weight: f64, status: impl Into<S>) -> Self {
        let node = ProgressNode::child(&self.node, weight, status.into());

        Self {
            node,
            cancel_token: self.cancel_token.child_token(),
        }
    }

    /// Update the progress of this token
    pub fn update_progress(&self, progress: f64) {
        if self.is_cancelled() {
            return;
        }

        let is_completed = {
            let inner = self.node.inner.lock().unwrap();
            inner.is_completed
        };

        if is_completed {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        inner.progress = Progress::Determinate(progress.max(0.0).min(1.0));
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Set the progress state to indeterminate
    pub fn update_indeterminate(&self) {
        if self.is_cancelled() {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        if inner.is_completed {
            return;
        }

        inner.progress = Progress::Indeterminate;
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Update the status message
    pub fn update_status(&self, status: impl Into<S>) {
        if self.is_cancelled() {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        if inner.is_completed {
            return;
        }

        inner.status = status.into();
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Update the progress and status message
    pub fn update(&self, progress: Progress, status: impl Into<S>) {
        if self.is_cancelled() {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        if inner.is_completed {
            return;
        }

        inner.status = status.into();
        inner.progress = progress;
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Mark the task as complete
    pub fn complete(&self) {
        if self.is_cancelled() {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        if !inner.is_completed {
            inner.is_completed = true;
            inner.progress = Progress::Determinate(1.0);
            drop(inner);

            ProgressNode::notify_subscribers(&self.node, false);
        }
    }

    /// Returns ProgressError::Cancelled if the token is cancelled, otherwise Ok.
    pub fn check(&self) -> Result<(), ProgressError> {
        if self.is_cancelled() {
            Err(ProgressError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// Cancel this task and all its children
    pub fn cancel(&self) {
        if !self.cancel_token.is_cancelled() {
            self.cancel_token.cancel();
            ProgressNode::notify_subscribers(&self.node, true);
        }
    }

    /// Get the current progress state asynchronously
    pub fn state(&self) -> Progress {
        ProgressNode::calculate_progress(&self.node)
    }

    /// Get all status messages in this hierarchy asynchronously
    pub fn statuses(&self) -> Vec<S> {
        ProgressNode::get_status_hierarchy(&self.node)
    }

    pub fn cancelled(&self) -> WaitForCancellationFuture {
        self.cancel_token.cancelled()
    }

    pub async fn updated(&self) -> Result<ProgressUpdate<S>, ProgressError> {
        let mut rx = {
            let inner = self.node.inner.lock().unwrap();
            inner.update_sender.subscribe()
        };

        tokio::select! {
            _ = self.cancel_token.cancelled() => {
                Err(ProgressError::Cancelled)
            }
            result = rx.recv() => {
                match result {
                    Ok(update) => Ok(update),
                    Err(broadcast::error::RecvError::Closed) => Err(ProgressError::Cancelled),
                    Err(broadcast::error::RecvError::Lagged(_)) => Err(ProgressError::Lagged),
                }
            }
        }
    }

    /// Subscribe to progress updates from this token
    pub fn subscribe(&self) -> ProgressStream<'_, S> {
        let rx = {
            let inner = self.node.inner.lock().unwrap();
            inner.update_sender.subscribe()
        };

        ProgressStream {
            token: self,
            rx: BroadcastStream::new(rx),
        }
    }

    /// Creates a guard that will automatically mark this token as complete when dropped
    pub fn complete_guard(&self) -> CompleteGuard<'_, S> {
        CompleteGuard { token: self }
    }
}

pin_project! {
    /// A Future that is resolved once the corresponding [`ProgressToken`]
    /// is updated. Resolves to `None` if the progress token is cancelled.
    #[must_use = "futures do nothing unless polled"]
    pub struct WaitForUpdateFuture<'a, S> {
        token: &'a ProgressToken<S>,
        #[pin]
        future: tokio::sync::futures::Notified<'a>,
    }
}

impl<'a, S: Clone + Send + 'static> Future for WaitForUpdateFuture<'a, S> {
    type Output = Option<ProgressUpdate<S>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        if this.token.cancel_token.is_cancelled() {
            return Poll::Ready(None);
        }

        ready!(this.future.as_mut().poll(cx));

        Poll::Ready(Some(ProgressUpdate {
            progress: this.token.state(),
            statuses: this.token.statuses(),
            is_cancelled: false,
        }))
    }
}

pin_project! {
    /// A Stream that yields progress updates from a token
    #[must_use = "streams do nothing unless polled"]
    pub struct ProgressStream<'a, S> {
        token: &'a ProgressToken<S>,
        #[pin]
        rx: BroadcastStream<ProgressUpdate<S>>,
    }
}

impl<'a, S: Clone + Send + 'static> Stream for ProgressStream<'a, S> {
    type Item = ProgressUpdate<S>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project()
            .rx
            .poll_next(cx)
            .map(|opt| opt.map(|res| res.unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio::time::sleep;

    // helper function to create a test hierarchy
    async fn create_test_hierarchy() -> (
        ProgressToken<String>,
        ProgressToken<String>,
        ProgressToken<String>,
    ) {
        let root = ProgressToken::new("root".to_string());
        let child1 = root.child(0.6, "child1".to_string());
        let child2 = root.child(0.4, "child2".to_string());
        (root, child1, child2)
    }

    #[tokio::test]
    async fn test_basic_progress_updates() {
        let token: ProgressToken<String> = ProgressToken::new("test".to_string());
        token.update_progress(0.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
        );

        token.update_progress(1.0);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );

        // test progress clamping
        token.update_progress(1.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );

        token.update_progress(-0.5);
        assert!(matches!(token.state(), Progress::Determinate(p) if p.abs() < f64::EPSILON));
    }

    #[tokio::test]
    async fn test_hierarchical_progress() {
        let (root, child1, child2) = create_test_hierarchy().await;

        // update children progress
        child1.update_progress(0.5);
        child2.update_progress(0.5);

        // root progress should be weighted average: 0.5 * 0.6 + 0.5 * 0.4 = 0.5
        assert!(matches!(root.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON));

        child1.update_progress(1.0);
        // root progress should now be: 1.0 * 0.6 + 0.5 * 0.4 = 0.8
        assert!(matches!(root.state(), Progress::Determinate(p) if (p - 0.8).abs() < f64::EPSILON));
    }

    #[tokio::test]
    async fn test_indeterminate_state() {
        let (root, child1, child2) = create_test_hierarchy().await;

        // set one child to indeterminate
        child1.update_indeterminate();
        child2.update_progress(0.5);

        // root should be indeterminate
        assert!(matches!(root.state(), Progress::Indeterminate));

        // set child back to determinate
        child1.update_progress(0.5);
        assert!(matches!(root.state(), Progress::Determinate(_)));
    }

    #[tokio::test]
    async fn test_status_updates() {
        let token: ProgressToken<String> = ProgressToken::new("initial status".to_string());
        let statuses = token.statuses();
        assert_eq!(statuses, vec!["initial status".to_string()]);

        token.update_status("updated status".to_string());
        let statuses = token.statuses();
        assert_eq!(statuses, vec!["updated status".to_string()]);
    }

    #[tokio::test]
    async fn test_status_hierarchy() {
        let (root, child1, _) = create_test_hierarchy().await;

        let statuses = root.statuses();
        assert_eq!(statuses, vec!["root".to_string(), "child1".to_string()]);

        child1.update_status("updated child1".to_string());
        let statuses = root.statuses();
        assert_eq!(
            statuses,
            vec!["root".to_string(), "updated child1".to_string()]
        );
    }

    #[tokio::test]
    async fn test_cancellation() {
        let (root, child1, child2) = create_test_hierarchy().await;

        // cancel root
        root.cancel();

        assert!(root.cancel_token.is_cancelled());
        assert!(child1.cancel_token.is_cancelled());
        assert!(child2.cancel_token.is_cancelled());

        // updates should not be processed after cancellation
        child1.update_progress(0.5);
        assert!(matches!(child1.state(), Progress::Determinate(p) if p.abs() < f64::EPSILON));
    }

    #[tokio::test]
    async fn test_complete_guard() {
        let token: ProgressToken<String> = ProgressToken::new("test".to_string());

        {
            let _guard = token.complete_guard();
            token.update_progress(0.5);
            assert!(
                matches!(token.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
            );
        } // guard is dropped here, token should be completed

        // token should be completed and at progress 1.0
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );

        // updates after completion should be ignored
        token.update_progress(0.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );

        // test forget
        let token: ProgressToken<String> = ProgressToken::new("test2".to_string());
        {
            let guard = token.complete_guard();
            token.update_progress(0.5);
            guard.forget(); // prevent completion
        }

        // token should still be at 0.5 since guard was forgotten
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
        );
    }

    #[tokio::test]
    async fn test_subscription() {
        let token: ProgressToken<String> = ProgressToken::new("test".to_string());
        let mut subscription = token.subscribe();

        // initial update
        let update = subscription.next().await.unwrap();
        assert_eq!(update.status(), &"test".to_string());
        assert!(matches!(update.progress, Progress::Determinate(p) if p.abs() < f64::EPSILON));

        // progress update
        token.update_progress(0.5);
        let update = subscription.next().await.unwrap();
        assert!(
            matches!(update.progress, Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
        );
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let token: ProgressToken<String> = ProgressToken::new("test".to_string());
        let mut sub1 = token.subscribe();
        let mut sub2 = token.subscribe();

        // Skip initial updates
        sub1.next().await.unwrap();
        sub2.next().await.unwrap();

        // both subscribers should receive updates
        token.update_progress(0.5);

        let update1 = sub1.next().await.unwrap();
        let update2 = sub2.next().await.unwrap();

        assert!(
            matches!(update1.progress, Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON),
            "{update1:?}"
        );
        assert!(
            matches!(update2.progress, Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON),
            "{update2:?}"
        );
    }

    #[tokio::test]
    async fn test_concurrent_updates() {
        let token: ProgressToken<String> = ProgressToken::new("test".to_string());
        let mut handles = vec![];

        // spawn multiple tasks updating the same token
        for i in 0..10 {
            let token = token.clone();
            handles.push(tokio::spawn(async move {
                sleep(Duration::from_millis(i * 10)).await;
                token.update_progress(i as f64 / 10.0);
            }));
        }

        // wait for all tasks to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // final progress should be from the last update (0.9)
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 0.9).abs() < f64::EPSILON)
        );
    }

    #[tokio::test]
    async fn test_edge_cases() {
        // single node tree
        let token: ProgressToken<String> = ProgressToken::new("single".to_string());
        token.update_progress(0.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
        );

        // deep hierarchy
        let mut current: ProgressToken<String> = ProgressToken::new("root".to_string());
        for i in 0..10 {
            current = current.child(1.0, format!("child{}", i));
        }

        // update leaf node
        current.update_progress(1.0);
        // progress should propagate to root
        assert!(
            matches!(current.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );
    }
}
