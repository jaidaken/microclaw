use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

type ChatKey = (String, i64, String);

/// A message that arrived while an agent run was active for the same chat.
#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub sender_name: String,
    pub content: String,
    pub message_id: String,
    pub timestamp: String,
}

/// Internal per-chat slot tracking the turn lock and pending messages.
struct ChatSlot {
    /// Async mutex held for the duration of an agent run.
    turn_lock: Arc<Mutex<()>>,
    /// Messages queued while a run is active.
    pending_messages: Vec<PendingMessage>,
    /// Last time this slot was actively used.
    last_active: Instant,
}

impl ChatSlot {
    fn new() -> Self {
        Self {
            turn_lock: Arc::new(Mutex::new(())),
            pending_messages: Vec::new(),
            last_active: Instant::now(),
        }
    }
}

/// RAII guard that releases the per-chat turn lock when dropped.
pub struct TurnGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    key: ChatKey,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        debug!(
            channel = %self.key.0,
            chat_id = self.key.1,
            user_id = %self.key.2,
            "Chat turn released"
        );
    }
}

/// Per-chat turn serialization queue.
///
/// Ensures at most one agent run per (channel, chat_id). Messages arriving
/// during an active run are coalesced into `pending_messages` and can be
/// drained after the run completes.
pub struct ChatTurnQueue {
    slots: Mutex<HashMap<ChatKey, Arc<Mutex<ChatSlot>>>>,
    max_pending: usize,
    idle_ttl: Duration,
}

impl ChatTurnQueue {
    pub fn new(max_pending: usize) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            max_pending,
            idle_ttl: Duration::from_secs(600),
        }
    }

    /// Get or create the slot for a given chat, performing opportunistic
    /// cleanup of idle slots.
    async fn get_slot(&self, key: &ChatKey) -> Arc<Mutex<ChatSlot>> {
        let mut slots = self.slots.lock().await;
        // Opportunistic cleanup: remove idle slots (no pending messages, not locked)
        if slots.len() > 100 {
            let now = Instant::now();
            let ttl = self.idle_ttl;
            slots.retain(|_, slot_arc| {
                // Only remove if we can try-lock it (not actively held)
                if let Ok(slot) = slot_arc.try_lock() {
                    if slot.pending_messages.is_empty() && now.duration_since(slot.last_active) > ttl
                    {
                        return false; // remove
                    }
                }
                true
            });
        }
        slots
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ChatSlot::new())))
            .clone()
    }

    /// Acquire the turn for a chat. Blocks until the previous run completes.
    /// Returns a [`TurnGuard`] that releases the turn when dropped.
    pub async fn acquire(
        self: &Arc<Self>,
        channel: &str,
        chat_id: i64,
        user_id: &str,
    ) -> Option<TurnGuard> {
        let key: ChatKey = (channel.to_string(), chat_id, user_id.to_string());
        let slot_arc = self.get_slot(&key).await;

        let turn_lock = {
            let slot = slot_arc.lock().await;
            slot.turn_lock.clone()
        };

        let guard = match tokio::time::timeout(Duration::from_secs(60), turn_lock.lock_owned()).await
        {
            Ok(guard) => guard,
            Err(_) => {
                warn!(
                    channel = %key.0,
                    chat_id = key.1,
                    user_id = %key.2,
                    "ChatTurnQueue: timeout waiting for turn lock (60s); proceeding without lock"
                );
                return None;
            }
        };

        {
            let mut slot = slot_arc.lock().await;
            slot.last_active = Instant::now();
        }

        debug!(channel, chat_id, user_id, "Chat turn acquired");

        Some(TurnGuard {
            _guard: guard,
            key,
        })
    }

    /// Enqueue a message for a chat that currently has an active run.
    ///
    /// Returns `true` if the chat has an active run (message was queued).
    /// Returns `false` if no run is active (caller should start a new run).
    pub async fn enqueue_if_busy(
        &self,
        channel: &str,
        chat_id: i64,
        user_id: &str,
        msg: PendingMessage,
    ) -> bool {
        let key: ChatKey = (channel.to_string(), chat_id, user_id.to_string());
        let slot_arc = {
            let slots = self.slots.lock().await;
            match slots.get(&key) {
                Some(arc) => arc.clone(),
                None => return false,
            }
        };

        let mut slot = slot_arc.lock().await;
        if slot.turn_lock.try_lock().is_ok() {
            return false;
        }

        if slot.pending_messages.len() >= self.max_pending {
            slot.pending_messages.remove(0);
            warn!(
                channel,
                chat_id,
                user_id,
                max_pending = self.max_pending,
                "ChatTurnQueue: pending messages at capacity; dropped oldest"
            );
        }

        info!(
            channel,
            chat_id,
            user_id,
            sender = %msg.sender_name,
            pending_count = slot.pending_messages.len() + 1,
            "Message queued while chat turn is active"
        );
        slot.pending_messages.push(msg);
        true
    }

    /// Atomically try to start a new turn or enqueue the message.
    ///
    /// If no turn is active, acquires the lock and returns `Some(TurnGuard)`.
    /// If a turn is active, queues the message and returns `None`.
    ///
    /// This avoids the race window between a separate `enqueue_if_busy` check
    /// and a later `acquire()` call.
    pub async fn try_start_or_enqueue(
        self: &Arc<Self>,
        channel: &str,
        chat_id: i64,
        user_id: &str,
        msg: PendingMessage,
    ) -> Option<TurnGuard> {
        let key: ChatKey = (channel.to_string(), chat_id, user_id.to_string());
        let slot_arc = self.get_slot(&key).await;

        let turn_lock = {
            let slot = slot_arc.lock().await;
            slot.turn_lock.clone()
        };

        match turn_lock.try_lock_owned() {
            Ok(guard) => {
                let mut slot = slot_arc.lock().await;
                slot.last_active = Instant::now();
                debug!(channel, chat_id, user_id, "Chat turn acquired");
                Some(TurnGuard { _guard: guard, key })
            }
            Err(_) => {
                let mut slot = slot_arc.lock().await;
                if slot.pending_messages.len() >= self.max_pending {
                    slot.pending_messages.remove(0);
                    warn!(
                        channel,
                        chat_id,
                        user_id,
                        max_pending = self.max_pending,
                        "ChatTurnQueue: pending messages at capacity; dropped oldest"
                    );
                }
                info!(
                    channel,
                    chat_id,
                    user_id,
                    sender = %msg.sender_name,
                    pending_count = slot.pending_messages.len() + 1,
                    "Message queued while chat turn is active"
                );
                slot.pending_messages.push(msg);
                None
            }
        }
    }

    /// Drain all pending messages accumulated during the current turn.
    /// Returns them in arrival order.
    pub async fn drain_pending(
        &self,
        channel: &str,
        chat_id: i64,
        user_id: &str,
    ) -> Vec<PendingMessage> {
        let key: ChatKey = (channel.to_string(), chat_id, user_id.to_string());
        let slot_arc = {
            let slots = self.slots.lock().await;
            match slots.get(&key) {
                Some(arc) => arc.clone(),
                None => return Vec::new(),
            }
        };

        let mut slot = slot_arc.lock().await;
        std::mem::take(&mut slot.pending_messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_queue() -> Arc<ChatTurnQueue> {
        Arc::new(ChatTurnQueue::new(20))
    }

    fn make_msg(content: &str) -> PendingMessage {
        PendingMessage {
            sender_name: "user".to_string(),
            content: content.to_string(),
            message_id: format!("msg_{content}"),
            timestamp: "2026-04-01T00:00:00Z".to_string(),
        }
    }

    const U: &str = "u1";

    #[tokio::test]
    async fn test_acquire_release_basic() {
        let q = make_queue();
        let guard = q.acquire("telegram", 1, U).await;
        assert!(guard.is_some());
        drop(guard);
        let guard2 = q.acquire("telegram", 1, U).await;
        assert!(guard2.is_some());
    }

    #[tokio::test]
    async fn test_acquire_blocks_concurrent() {
        let q = make_queue();
        let counter = Arc::new(AtomicUsize::new(0));

        let guard = q.acquire("tg", 1, U).await.unwrap();
        let q2 = q.clone();
        let c2 = counter.clone();

        let handle = tokio::spawn(async move {
            let _g = q2.acquire("tg", 1, U).await;
            c2.fetch_add(1, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        drop(guard);
        handle.await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_enqueue_if_busy_returns_true_when_active() {
        let q = make_queue();
        let _guard = q.acquire("tg", 1, U).await.unwrap();

        let queued = q.enqueue_if_busy("tg", 1, U, make_msg("hello")).await;
        assert!(queued);

        let queued2 = q.enqueue_if_busy("tg", 1, U, make_msg("world")).await;
        assert!(queued2);
    }

    #[tokio::test]
    async fn test_enqueue_if_busy_returns_false_when_idle() {
        let q = make_queue();
        let queued = q.enqueue_if_busy("tg", 1, U, make_msg("hello")).await;
        assert!(!queued);
    }

    #[tokio::test]
    async fn test_drain_pending_returns_all() {
        let q = make_queue();
        let _guard = q.acquire("tg", 1, U).await.unwrap();

        q.enqueue_if_busy("tg", 1, U, make_msg("a")).await;
        q.enqueue_if_busy("tg", 1, U, make_msg("b")).await;
        q.enqueue_if_busy("tg", 1, U, make_msg("c")).await;

        let pending = q.drain_pending("tg", 1, U).await;
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].content, "a");
        assert_eq!(pending[1].content, "b");
        assert_eq!(pending[2].content, "c");
    }

    #[tokio::test]
    async fn test_drain_clears_queue() {
        let q = make_queue();
        let _guard = q.acquire("tg", 1, U).await.unwrap();

        q.enqueue_if_busy("tg", 1, U, make_msg("a")).await;
        let _ = q.drain_pending("tg", 1, U).await;

        let pending = q.drain_pending("tg", 1, U).await;
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn test_different_chats_independent() {
        let q = make_queue();
        let counter = Arc::new(AtomicUsize::new(0));

        let _guard_chat1 = q.acquire("tg", 1, U).await.unwrap();
        let q2 = q.clone();
        let c2 = counter.clone();

        let handle = tokio::spawn(async move {
            let _g = q2.acquire("tg", 2, U).await;
            c2.fetch_add(1, Ordering::SeqCst);
        });

        handle.await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_max_pending_drops_oldest() {
        let q = Arc::new(ChatTurnQueue::new(3));
        let _guard = q.acquire("tg", 1, U).await.unwrap();

        q.enqueue_if_busy("tg", 1, U, make_msg("a")).await;
        q.enqueue_if_busy("tg", 1, U, make_msg("b")).await;
        q.enqueue_if_busy("tg", 1, U, make_msg("c")).await;
        q.enqueue_if_busy("tg", 1, U, make_msg("d")).await;

        let pending = q.drain_pending("tg", 1, U).await;
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].content, "b");
        assert_eq!(pending[1].content, "c");
        assert_eq!(pending[2].content, "d");
    }

    #[tokio::test]
    async fn two_users_acquire_same_chat_id_independently() {
        let q = make_queue();
        let counter = Arc::new(AtomicUsize::new(0));

        let guard_u1 = q.acquire("web", 7, "u1").await.unwrap();
        let q2 = q.clone();
        let c2 = counter.clone();

        let handle = tokio::spawn(async move {
            let g = q2.acquire("web", 7, "u2").await;
            assert!(g.is_some(), "u2 must not block on u1's lock");
            c2.fetch_add(1, Ordering::SeqCst);
        });

        handle.await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(guard_u1);
    }

    #[tokio::test]
    async fn drain_pending_isolated_per_user() {
        let q = make_queue();
        let _g_u1 = q.acquire("web", 9, "u1").await.unwrap();
        let _g_u2 = q.acquire("web", 9, "u2").await.unwrap();

        q.enqueue_if_busy("web", 9, "u1", make_msg("u1-a")).await;
        q.enqueue_if_busy("web", 9, "u2", make_msg("u2-a")).await;

        let p1 = q.drain_pending("web", 9, "u1").await;
        let p2 = q.drain_pending("web", 9, "u2").await;
        assert_eq!(p1.len(), 1);
        assert_eq!(p1[0].content, "u1-a");
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].content, "u2-a");
    }
}
