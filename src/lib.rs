use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use futures::{Stream, stream};

/// Represents either a determinate progress value or indeterminate state
#[derive(Debug, Clone, Copy)]
pub enum ProgressState {
    Determinate(f64),
    Indeterminate,
}

impl ProgressState {
    fn as_f64(&self) -> Option<f64> {
        match self {
            ProgressState::Determinate(v) => Some(*v),
            ProgressState::Indeterminate => None,
        }
    }
}

/// Data for a progress update event
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub token_id: Uuid,
    pub progress: ProgressState,
    pub status: String,
    pub path: Vec<String>,
    pub is_cancelled: bool,
}

/// Inner data of a progress node
struct ProgressNodeInner {
    // Tree structure
    parent: Option<Arc<ProgressNode>>,
    parent_idx: usize,
    children: Vec<(Arc<ProgressNode>, f64)>, // Node and its weight

    // Progress state
    progress: ProgressState,
    status: String,
    is_completed: bool,

    // Handle tracking
    handle_count: usize,

    // Subscriber management
    subscribers: HashMap<Uuid, mpsc::Sender<ProgressUpdate>>,
}

/// A node in the progress tree
struct ProgressNode {
    id: Uuid,
    inner: Mutex<ProgressNodeInner>,
    change_notify: Notify,
}

impl ProgressNode {
    fn new(status: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            inner: Mutex::new(ProgressNodeInner {
                parent: None,
                parent_idx: 0,
                children: Vec::new(),
                progress: ProgressState::Determinate(0.0),
                status,
                is_completed: false,
                handle_count: 1,
                subscribers: HashMap::new(),
            }),
            change_notify: Notify::new(),
        }
    }

    fn child(parent: &Arc<Self>, weight: f64, status: String) -> Arc<Self> {
        let mut parent_inner = parent.inner.lock().unwrap();

        let child = Self {
            id: Uuid::new_v4(),
            inner: Mutex::new(ProgressNodeInner {
                parent: Some(parent.clone()),
                parent_idx: parent_inner.children.len(),
                children: Vec::new(),
                progress: ProgressState::Determinate(0.0),
                status,
                is_completed: false,
                handle_count: 1,
                subscribers: HashMap::new(),
            }),
            change_notify: Notify::new(),
        };

        let child = Arc::new(child);

        parent_inner
            .children
            .push((child.clone(), weight));

        child
    }

    // fn clone(&self) -> Self {
    //     let mut inner = self.inner.lock().unwrap();
    //     inner.handle_count += 1;

    //     Self {
    //         id: self.id,
    //         inner: self.inner.clone(),
    //         change_notify: self.change_notify.clone(),
    //     }
    // }

    fn calculate_progress(node: &Arc<Self>) -> ProgressState {
        let inner = node.inner.lock().unwrap();

        // If this node itself is indeterminate, propagate that
        if matches!(inner.progress, ProgressState::Indeterminate) {
            return ProgressState::Indeterminate;
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
            .any(|(child, _)| {
                matches!(
                    Self::calculate_progress(child),
                    ProgressState::Indeterminate
                )
            });

        if has_indeterminate {
            return ProgressState::Indeterminate;
        }

        // Calculate weighted average of determinate children
        let total: f64 = inner
            .children
            .iter()
            .map(|(child, weight)| {
                match Self::calculate_progress(child) {
                    ProgressState::Determinate(p) => p * weight,
                    ProgressState::Indeterminate => 0.0, // Shouldn't happen due to check above
                }
            })
            .sum();

        ProgressState::Determinate(total)
    }

    fn get_status_hierarchy(node: &Arc<Self>) -> Vec<String> {
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
        let inner = node.inner.lock().unwrap();

        // Create update
        let update = ProgressUpdate {
            token_id: node.id,
            progress: Self::calculate_progress(node),
            status: inner.status.clone(),
            path: Self::get_status_hierarchy(node),
            is_cancelled,
        };

        // Send to subscribers
        for (_, sender) in &inner.subscribers {
            let _ = sender.try_send(update.clone());
        }

        // Notify waiters
        node.change_notify.notify_waiters();

        // Propagate to parent
        if let Some(parent) = &inner.parent {
            Self::notify_subscribers(parent, false);
        }
    }

    fn subscribe(&self) -> (Uuid, mpsc::Receiver<ProgressUpdate>) {
        let (tx, rx) = mpsc::channel(16);
        let id = Uuid::new_v4();

        {
            let mut inner = self.inner.lock().unwrap();
            inner.subscribers.insert(id, tx.clone());
        }

        // Send initial state
        let update = ProgressUpdate {
            token_id: self.id,
            progress: self.inner.lock().unwrap().progress,
            status: self.inner.lock().unwrap().status.clone(),
            path: vec![self.inner.lock().unwrap().status.clone()],
            is_cancelled: false,
        };

        let _ = tx.try_send(update);

        (id, rx)
    }

    fn unsubscribe(&self, id: Uuid) {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribers.remove(&id);
    }
}

/// A token that tracks the progress of a task and can be organized hierarchically
#[derive(Clone)]
pub struct ProgressToken {
    node: Arc<ProgressNode>,
    update_count: Arc<AtomicU64>,
    last_update: Instant,
    is_active: Arc<AtomicBool>,
    cancel_token: CancellationToken,
}

impl ProgressToken {
    /// Create a new root ProgressToken
    pub fn new(status: impl Into<String>) -> Arc<Self> {
        let status_str = status.into();
        let node = Arc::new(ProgressNode::new(status_str));

        Arc::new(Self {
            node,
            update_count: Arc::new(AtomicU64::new(0)),
            last_update: Instant::now(),
            is_active: Arc::new(AtomicBool::new(true)),
            cancel_token: CancellationToken::new(),
        })
    }

    /// Create a child token
    pub fn child(parent: &Arc<Self>, weight: f64, status: impl Into<String>) -> Arc<Self> {
        let status_str = status.into();
        let node = ProgressNode::child(&parent.node, weight, status_str);

        Arc::new(Self {
            node,
            update_count: Arc::new(AtomicU64::new(0)),
            last_update: Instant::now(),
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
        inner.progress = ProgressState::Determinate(progress.max(0.0).min(1.0));
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
        inner.progress = ProgressState::Indeterminate;
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Update the status message
    pub fn status(&self, status: impl Into<String>) {
        if !self.is_active.load(Ordering::Relaxed) || self.cancel_token.is_cancelled() {
            return;
        }

        let mut inner = self.node.inner.lock().unwrap();
        inner.status = status.into();
        drop(inner);

        ProgressNode::notify_subscribers(&self.node, false);
    }

    /// Mark the task as complete
    pub fn complete(&self) {
        if self.is_active.swap(false, Ordering::Relaxed) {
            let mut inner = self.node.inner.lock().unwrap();
            inner.is_completed = true;
            inner.progress = ProgressState::Determinate(1.0);
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

    /// Get a handle to the cancellation token
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Check if this token has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// Get the current progress value asynchronously
    pub async fn value(&self) -> ProgressState {
        ProgressNode::calculate_progress(&self.node)
    }

    /// Get all status messages in this hierarchy asynchronously
    pub async fn statuses(&self) -> Vec<String> {
        ProgressNode::get_status_hierarchy(&self.node)
    }

    /// Subscribe to progress updates from this token
    /// Returns a subscription that will automatically unsubscribe when dropped
    pub fn subscribe(&self) -> ProgressSubscription {
        let (id, rx) = self.node.subscribe();
        ProgressSubscription {
            token: self.node.clone(),
            id,
            rx,
        }
    }
}

/// A subscription to progress updates that automatically unsubscribes when dropped
pub struct ProgressSubscription {
    token: Arc<ProgressNode>,
    id: Uuid,
    rx: mpsc::Receiver<ProgressUpdate>,
}

impl Drop for ProgressSubscription {
    fn drop(&mut self) {
        self.token.unsubscribe(self.id);
    }
}

impl ProgressSubscription {
    /// Get the next update from the subscription
    pub async fn next(&mut self) -> Option<ProgressUpdate> {
        self.rx.recv().await
    }

    /// Convert the subscription into a stream of updates
    pub fn into_stream(self) -> impl Stream<Item = ProgressUpdate> {
        stream::unfold(self, |mut sub| async move {
            sub.next().await.map(|update| (update, sub))
        })
    }

    /// Try to get the next update without waiting
    pub fn try_next(&mut self) -> Option<ProgressUpdate> {
        self.rx.try_recv().ok()
    }
}

// /// Subscribe to progress updates from a token
// pub async fn subscribe(
//     token: &ProgressToken,
// ) -> (Uuid, tokio::sync::mpsc::Receiver<ProgressUpdate>) {
//     token.node.subscribe().await
// }

// /// Unsubscribe from progress updates
// pub fn unsubscribe(token: &ProgressToken, id: Uuid) {
//     token.node.unsubscribe(id);
// }
