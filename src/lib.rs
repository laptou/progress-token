use futures::{Stream, ready};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

/// Represents either a determinate progress value or indeterminate state
#[derive(Debug, Clone, Copy)]
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

#[derive(Debug, Clone, Copy)]
pub enum ProgressError {
    /// Too many progress updates have occurred since last polled, so some of
    /// them have been dropped
    Lagged,
}

/// Data for a progress update event
#[derive(Debug, Clone)]
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
    is_active: Arc<AtomicBool>,
    cancel_token: CancellationToken,
}

impl<S: Clone + Send + 'static> ProgressToken<S> {
    /// Create a new root ProgressToken
    pub fn new(status: S) -> Arc<Self> {
        let node = Arc::new(ProgressNode::new(status));

        Arc::new(Self {
            node,
            is_active: Arc::new(AtomicBool::new(true)),
            cancel_token: CancellationToken::new(),
        })
    }

    /// Create a child token
    pub fn child(parent: &Arc<Self>, weight: f64, status: S) -> Arc<Self> {
        let node = ProgressNode::child(&parent.node, weight, status);

        Arc::new(Self {
            node,
            is_active: Arc::new(AtomicBool::new(true)),
            cancel_token: parent.cancel_token.child_token(),
        })
    }

    /// Update the progress of this token
    pub fn progress(&self, progress: f64) {
        if !self.is_active.load(Ordering::Relaxed) || self.cancel_token.is_cancelled() {
            return;
        }

        // // Rate-limit updates to avoid too many notifications
        // let now = Instant::now();
        // let count = self.update_count.fetch_add(1, Ordering::Relaxed);

        // if now.duration_since(self.last_update) > Duration::from_millis(100) || count % 10 == 0 {
        let mut inner = self.node.inner.lock().unwrap();
        inner.progress = Progress::Determinate(progress.max(0.0).min(1.0));
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
        // }
    }

    /// Set the progress state to indeterminate
    pub fn indeterminate(&self) {
        if !self.is_active.load(Ordering::Relaxed) || self.cancel_token.is_cancelled() {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        inner.progress = Progress::Indeterminate;
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Update the status message
    pub fn status(&self, status: S) {
        if !self.is_active.load(Ordering::Relaxed) || self.cancel_token.is_cancelled() {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        inner.status = status;
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Mark the task as complete
    pub fn complete(&self) {
        if self.is_active.swap(false, Ordering::Relaxed) {
            let mut inner = self.node.inner.lock().unwrap();
            inner.is_completed = true;
            inner.progress = Progress::Determinate(1.0);
            drop(inner);

            ProgressNode::notify_subscribers(&self.node, false);
        }
    }

    /// Cancel this task and all its children
    pub fn cancel(&self) {
        if self.is_active.swap(false, Ordering::Relaxed) {
            self.cancel_token.cancel();

            ProgressNode::notify_subscribers(&self.node, true);
        }
    }

    /// Check if this token has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
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

    pub async fn updated(&self) -> Option<ProgressUpdate<S>> {
        let mut rx = {
            let inner = self.node.inner.lock().unwrap();
            inner.update_sender.subscribe()
        };

        tokio::select! {
            _ = self.cancel_token.cancelled() => {
                None
            }
            result = rx.recv() => {
                match result {
                    Ok(update) => Some(update),
                    Err(broadcast::error::RecvError::Closed) => unreachable!(),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
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
        if this.token.is_cancelled() {
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
        Arc<ProgressToken<String>>,
        Arc<ProgressToken<String>>,
        Arc<ProgressToken<String>>,
    ) {
        let root = ProgressToken::new("root".to_string());
        let child1 = ProgressToken::child(&root, 0.6, "child1".to_string());
        let child2 = ProgressToken::child(&root, 0.4, "child2".to_string());
        (root, child1, child2)
    }

    #[tokio::test]
    async fn test_basic_progress_updates() {
        let token = ProgressToken::new("test".to_string());
        token.progress(0.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
        );

        token.progress(1.0);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );

        // test progress clamping
        token.progress(1.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );

        token.progress(-0.5);
        assert!(matches!(token.state(), Progress::Determinate(p) if p.abs() < f64::EPSILON));
    }

    #[tokio::test]
    async fn test_hierarchical_progress() {
        let (root, child1, child2) = create_test_hierarchy().await;

        // update children progress
        child1.progress(0.5);
        child2.progress(0.5);

        // root progress should be weighted average: 0.5 * 0.6 + 0.5 * 0.4 = 0.5
        assert!(matches!(root.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON));

        child1.progress(1.0);
        // root progress should now be: 1.0 * 0.6 + 0.5 * 0.4 = 0.8
        assert!(matches!(root.state(), Progress::Determinate(p) if (p - 0.8).abs() < f64::EPSILON));
    }

    #[tokio::test]
    async fn test_indeterminate_state() {
        let (root, child1, child2) = create_test_hierarchy().await;

        // set one child to indeterminate
        child1.indeterminate();
        child2.progress(0.5);

        // root should be indeterminate
        assert!(matches!(root.state(), Progress::Indeterminate));

        // set child back to determinate
        child1.progress(0.5);
        assert!(matches!(root.state(), Progress::Determinate(_)));
    }

    #[tokio::test]
    async fn test_status_updates() {
        let token = ProgressToken::new("initial status".to_string());
        let statuses = token.statuses();
        assert_eq!(statuses, vec!["initial status".to_string()]);

        token.status("updated status".to_string());
        let statuses = token.statuses();
        assert_eq!(statuses, vec!["updated status".to_string()]);
    }

    #[tokio::test]
    async fn test_status_hierarchy() {
        let (root, child1, _) = create_test_hierarchy().await;

        let statuses = root.statuses();
        assert_eq!(statuses, vec!["root".to_string(), "child1".to_string()]);

        child1.status("updated child1".to_string());
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

        assert!(root.is_cancelled());
        assert!(child1.is_cancelled());
        assert!(child2.is_cancelled());

        // updates should not be processed after cancellation
        child1.progress(0.5);
        assert!(matches!(child1.state(), Progress::Determinate(p) if p.abs() < f64::EPSILON));
    }

    #[tokio::test]
    async fn test_completion() {
        let token = ProgressToken::new("test".to_string());
        token.complete();

        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );

        // updates after completion should not be processed
        token.progress(0.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );
    }

    #[tokio::test]
    async fn test_subscription() {
        let token = ProgressToken::new("test".to_string());
        let mut subscription = token.subscribe();

        // initial update
        let update = subscription.next().await.unwrap();
        assert_eq!(update.status(), &"test".to_string());
        assert!(matches!(update.progress, Progress::Determinate(p) if p.abs() < f64::EPSILON));

        // progress update
        token.progress(0.5);
        let update = subscription.next().await.unwrap();
        assert!(
            matches!(update.progress, Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
        );
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let token = ProgressToken::new("test".to_string());
        let mut sub1 = token.subscribe();
        let mut sub2 = token.subscribe();

        // Skip initial updates
        sub1.next().await.unwrap();
        sub2.next().await.unwrap();

        // both subscribers should receive updates
        token.progress(0.5);

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
        let token = Arc::new(ProgressToken::new("test".to_string()));
        let mut handles = vec![];

        // spawn multiple tasks updating the same token
        for i in 0..10 {
            let token = token.clone();
            handles.push(tokio::spawn(async move {
                sleep(Duration::from_millis(i * 10)).await;
                token.progress(i as f64 / 10.0);
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
        let token = ProgressToken::new("single".to_string());
        token.progress(0.5);
        assert!(
            matches!(token.state(), Progress::Determinate(p) if (p - 0.5).abs() < f64::EPSILON)
        );

        // deep hierarchy
        let mut current = ProgressToken::new("root".to_string());
        for i in 0..10 {
            current = ProgressToken::child(&current, 1.0, format!("child{}", i));
        }

        // update leaf node
        current.progress(1.0);
        // progress should propagate to root
        assert!(
            matches!(current.state(), Progress::Determinate(p) if (p - 1.0).abs() < f64::EPSILON)
        );
    }
}
