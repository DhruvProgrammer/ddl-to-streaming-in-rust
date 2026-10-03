//! Seek generations and cancellation.
//!
//! A playback session is identified by an opaque, client-supplied session id.
//! Every request carries a monotonically increasing generation. When a new
//! generation arrives, the previous one's upstream work is cancelled
//! immediately — this is what keeps `10s → 30s → 90s → 15s` from leaving four
//! half-dead origin connections behind.
//!
//! Rules:
//!   * `generation > current` — the new request supersedes the old: the old is
//!     cancelled, the new one proceeds.
//!   * `generation == current` — a legitimate parallel range request. Allowed.
//!   * `generation < current` — a stale, already-abandoned request that
//!     somehow arrived late. Refused, so it can never overwrite the live one.
//!
//! Cancellation is reference counted: a session is only torn down once every
//! handle for the current generation has been dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::errors::{ErrorCode, PlayerError};

/// Maximum session id length. Bounds map growth from hostile input.
pub const MAX_SESSION_ID_LEN: usize = 64;

/// Upper bound on live sessions, so the map cannot grow without limit.
pub const MAX_SESSIONS: usize = 4096;

#[derive(Debug)]
struct GenerationSlot {
    generation: u64,
    parent: CancellationToken,
    live: AtomicUsize,
}

impl GenerationSlot {
    fn is_current(&self) -> bool {
        !self.parent.is_cancelled()
    }
}

/// A live stream's claim on the session. Dropping it releases everything.
#[derive(Debug)]
pub struct StreamHandle {
    session: String,
    generation: u64,
    slot: Arc<GenerationSlot>,
    /// Cancelled when this specific request ends, or when superseded.
    token: CancellationToken,
    registry: Arc<StreamRegistry>,
    /// Concurrency permit, held for exactly as long as upstream work runs.
    permit: Option<OwnedSemaphorePermit>,
    was_superseded: bool,
}

impl StreamHandle {
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// True once a newer generation has taken over this session.
    pub fn is_superseded(&self) -> bool {
        self.was_superseded || self.slot.parent.is_cancelled()
    }

    pub const fn permit(&self) -> Option<&OwnedSemaphorePermit> {
        self.permit.as_ref()
    }

    /// Detach the permit so it can outlive the handle (the pump task owns it).
    pub fn take_permit(&mut self) -> Option<OwnedSemaphorePermit> {
        self.permit.take()
    }

    /// End this request without cancelling siblings of the same generation.
    pub fn finish(&self) {
        self.token.cancel();
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.token.cancel();
        if self.slot.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Last handle for this generation: tear the session down.
            self.slot.parent.cancel();
            if let Ok(mut slots) = self.registry.slots.lock() {
                if let Some(current) = slots.get(&self.session) {
                    if Arc::ptr_eq(current, &self.slot) {
                        slots.remove(&self.session);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct StreamRegistry {
    slots: Mutex<HashMap<String, Arc<GenerationSlot>>>,
    concurrency: Arc<Semaphore>,
    max_concurrent: usize,
    superseded: AtomicU64,
    rejected: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct RegistryStats {
    pub sessions: usize,
    pub superseded: u64,
    pub rejected: u64,
    pub in_flight: usize,
}

impl StreamRegistry {
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(HashMap::new()),
            concurrency: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            superseded: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        })
    }

    pub fn stats(&self) -> RegistryStats {
        RegistryStats {
            sessions: self.slots.lock().map(|m| m.len()).unwrap_or(0),
            superseded: self.superseded.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            in_flight: self.max_concurrent - self.concurrency.available_permits(),
        }
    }

    /// Claim a generation. Returns an error for stale generations and when the
    /// concurrency ceiling is reached.
    pub fn register(
        self: &Arc<Self>,
        session: Option<&str>,
        generation: Option<u64>,
    ) -> Result<StreamHandle, PlayerError> {
        let permit = self.concurrency.clone().try_acquire_owned().map_err(|_| {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            PlayerError::new(ErrorCode::TooManyRequests)
                .with_reason("concurrency ceiling reached")
                .with_user_action("Close other streams or retry in a moment.")
        })?;

        let Some(session) = session.map(str::to_owned) else {
            return Ok(StreamHandle {
                session: String::new(),
                generation: generation.unwrap_or(0),
                slot: Arc::new(GenerationSlot {
                    generation: generation.unwrap_or(0),
                    parent: CancellationToken::new(),
                    live: AtomicUsize::new(1),
                }),
                token: CancellationToken::new(),
                registry: self.clone(),
                permit: Some(permit),
                was_superseded: false,
            });
        };

        if session.is_empty() || session.len() > MAX_SESSION_ID_LEN {
            return Err(
                PlayerError::new(ErrorCode::InvalidUrl).with_reason("invalid stream session id")
            );
        }

        let mut slots = self.slots.lock().map_err(|_| {
            PlayerError::new(ErrorCode::UnknownError).with_reason("session registry poisoned")
        })?;

        let existing = slots.get(&session).cloned();
        match (existing, generation.unwrap_or(0)) {
            // Newer or equal generation: proceed. Strictly newer cancels the old.
            (Some(slot), gen) if gen < slot.generation => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                drop(slots);
                Err(PlayerError::new(ErrorCode::SupersededRequest)
                    .with_reason("request generation is older than the live one")
                    .with_user_action("Discarded: a newer seek superseded this request."))
            }
            (Some(slot), gen) => {
                let superseded = gen > slot.generation;
                if superseded {
                    // Kill the previous generation's upstream work right now.
                    slot.parent.cancel();
                    self.superseded.fetch_add(1, Ordering::Relaxed);
                    let new_slot = Arc::new(GenerationSlot {
                        generation: gen,
                        parent: CancellationToken::new(),
                        live: AtomicUsize::new(1),
                    });
                    let token = new_slot.parent.child_token();
                    slots.insert(session.clone(), new_slot.clone());
                    drop(slots);
                    return Ok(StreamHandle {
                        session,
                        generation: gen,
                        slot: new_slot,
                        token,
                        registry: self.clone(),
                        permit: Some(permit),
                        was_superseded: false,
                    });
                }
                // Same generation: a parallel request. Share the slot.
                slot.live.fetch_add(1, Ordering::Relaxed);
                let token = slot.parent.child_token();
                drop(slots);
                Ok(StreamHandle {
                    session,
                    generation: gen,
                    slot,
                    token,
                    registry: self.clone(),
                    permit: Some(permit),
                    was_superseded: false,
                })
            }
            (None, gen) => {
                if slots.len() >= MAX_SESSIONS {
                    self.rejected.fetch_add(1, Ordering::Relaxed);
                    drop(slots);
                    return Err(PlayerError::new(ErrorCode::TooManyRequests)
                        .with_reason("session registry is full")
                        .with_user_action("Close other players and retry."));
                }
                let slot = Arc::new(GenerationSlot {
                    generation: gen,
                    parent: CancellationToken::new(),
                    live: AtomicUsize::new(1),
                });
                let token = slot.parent.child_token();
                slots.insert(session.clone(), slot.clone());
                drop(slots);
                Ok(StreamHandle {
                    session,
                    generation: gen,
                    slot,
                    token,
                    registry: self.clone(),
                    permit: Some(permit),
                    was_superseded: false,
                })
            }
        }
    }

    pub fn is_current(&self, session: &str, generation: u64) -> bool {
        self.slots
            .lock()
            .ok()
            .and_then(|m| {
                m.get(session)
                    .map(|s| s.generation == generation && s.is_current())
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn handle_cancels_its_token_on_drop() {
        let reg = StreamRegistry::new(8);
        let token = {
            let h = reg.register(Some("s1"), Some(1)).unwrap();
            h.token().clone()
        };
        assert!(token.is_cancelled());
    }

    #[test]
    fn newer_generation_cancels_the_previous() {
        let reg = StreamRegistry::new(8);
        let old = reg.register(Some("s1"), Some(1)).unwrap();
        let old_token = old.token().clone();
        let new = reg.register(Some("s1"), Some(2)).unwrap();
        assert!(old_token.is_cancelled(), "old generation must be cancelled");
        assert!(old.is_superseded());
        assert!(!new.token().is_cancelled());
        assert_eq!(new.generation(), 2);
        assert_eq!(reg.stats().superseded, 1);
    }

    #[test]
    fn stale_generation_is_refused() {
        let reg = StreamRegistry::new(8);
        let _keep = reg.register(Some("s1"), Some(5)).unwrap();
        let err = reg.register(Some("s1"), Some(4)).unwrap_err();
        assert_eq!(err.code, ErrorCode::SupersededRequest);
        assert!(!err.retryable);
        assert_eq!(reg.stats().rejected, 1);
    }

    #[test]
    fn equal_generation_is_allowed_for_parallel_ranges() {
        let reg = StreamRegistry::new(8);
        let a = reg.register(Some("s1"), Some(3)).unwrap();
        let b = reg.register(Some("s1"), Some(3)).unwrap();
        assert!(!a.token().is_cancelled());
        assert!(!b.token().is_cancelled());
        // Dropping one must not kill the other.
        drop(a);
        assert!(!b.token().is_cancelled());
        assert_eq!(reg.stats().sessions, 1);
    }

    #[test]
    fn rapid_seeking_supersedes_every_older_generation() {
        // The browser aborts the old request but it has not finished yet, so
        // the new generation arrives first. Generations are monotonic.
        let reg = StreamRegistry::new(64);
        let mut previous: Vec<CancellationToken> = Vec::new();
        let mut current: Option<StreamHandle> = None;

        for gen in 1u64..=6 {
            let next = reg.register(Some("rapid"), Some(gen)).unwrap();
            if let Some(prev) = current.take() {
                previous.push(prev.token().clone());
                drop(prev);
            }
            current = Some(next);
        }

        assert_eq!(reg.stats().sessions, 1, "one live playback session");
        assert_eq!(reg.stats().in_flight, 1, "one live origin connection");
        assert_eq!(current.as_ref().unwrap().generation(), 6);
        assert_eq!(reg.stats().superseded, 5);
        for t in &previous {
            assert!(t.is_cancelled(), "an obsolete generation stayed alive");
        }
    }

    #[test]
    fn seeking_to_earlier_positions_still_advances_the_generation() {
        // 10s -> 30s -> 90s -> 15s -> 120s -> 45s. The player counts seeks, not
        // timestamps, so every one of these supersedes the one before it.
        let offsets = [10u64, 30, 90, 15, 120, 45];
        let reg = StreamRegistry::new(64);
        let mut tokens = Vec::new();
        let mut current: Option<StreamHandle> = None;
        for (gen, _) in offsets.iter().enumerate() {
            let next = reg.register(Some("seek"), Some(gen as u64 + 1)).unwrap();
            if let Some(prev) = current.take() {
                tokens.push(prev.token().clone());
                drop(prev);
            }
            current = Some(next);
        }
        assert_eq!(reg.stats().in_flight, 1);
        assert_eq!(tokens.len(), offsets.len() - 1);
        assert!(tokens.iter().all(|t| t.is_cancelled()));
        assert_eq!(reg.stats().superseded as usize, offsets.len() - 1);
    }

    #[test]
    fn a_stale_generation_delivered_late_is_refused() {
        let reg = StreamRegistry::new(64);
        let mut current = reg.register(Some("rapid"), Some(1)).unwrap();
        for gen in 2u64..=6 {
            current = reg.register(Some("rapid"), Some(gen)).unwrap();
        }
        // Generation 1 arriving after 6 must not be able to interfere with it.
        let stale = reg.register(Some("rapid"), Some(1));
        assert_eq!(stale.unwrap_err().code, ErrorCode::SupersededRequest);
        assert_eq!(reg.stats().sessions, 1);
        assert_eq!(reg.stats().in_flight, 1);
        assert_eq!(current.generation(), 6);
    }

    #[test]
    fn session_is_removed_when_last_handle_drops() {
        let reg = StreamRegistry::new(8);
        let a = reg.register(Some("s1"), Some(1)).unwrap();
        let b = reg.register(Some("s1"), Some(1)).unwrap();
        drop(a);
        assert_eq!(reg.stats().sessions, 1);
        drop(b);
        assert_eq!(reg.stats().sessions, 0);
        assert_eq!(reg.stats().in_flight, 0);
    }

    #[test]
    fn concurrency_ceiling_rejects_instead_of_queueing() {
        let reg = StreamRegistry::new(2);
        let _a = reg.register(None, None).unwrap();
        let _b = reg.register(None, None).unwrap();
        let err = reg.register(None, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::TooManyRequests);
        assert!(err.retryable);
        assert_eq!(reg.stats().in_flight, 2);
    }

    #[test]
    fn permit_is_released_on_drop() {
        let reg = StreamRegistry::new(1);
        let h = reg.register(None, None).unwrap();
        assert_eq!(reg.stats().in_flight, 1);
        drop(h);
        assert_eq!(reg.stats().in_flight, 0);
        // Slot must be reusable afterwards.
        assert!(reg.register(None, None).is_ok());
    }

    #[test]
    fn session_ids_are_validated() {
        let reg = StreamRegistry::new(8);
        assert!(reg.register(Some(""), Some(1)).is_err());
        assert!(reg
            .register(Some(&"x".repeat(MAX_SESSION_ID_LEN + 1)), Some(1))
            .is_err());
        assert!(reg
            .register(Some(&"x".repeat(MAX_SESSION_ID_LEN)), Some(1))
            .is_ok());
    }

    #[test]
    fn dropping_a_handle_cancels_its_token() {
        let reg = StreamRegistry::new(8);
        let token = {
            let handle = reg.register(Some("s1"), Some(1)).unwrap();
            handle.token().clone()
        };
        assert!(token.is_cancelled());
        assert_eq!(reg.stats().sessions, 0);
    }

    #[test]
    fn is_current_tracks_the_live_generation() {
        let reg = StreamRegistry::new(8);
        let _h = reg.register(Some("s1"), Some(7)).unwrap();
        assert!(reg.is_current("s1", 7));
        assert!(!reg.is_current("s1", 8));
        assert!(!reg.is_current("s2", 7));
    }

    #[test]
    fn session_count_is_bounded() {
        let reg = StreamRegistry::new(MAX_SESSIONS + 16);
        let mut keep = Vec::new();
        for i in 0..MAX_SESSIONS {
            keep.push(reg.register(Some(&format!("s{i}")), Some(1)).unwrap());
        }
        assert_eq!(reg.stats().sessions, MAX_SESSIONS);
        assert!(reg.register(Some("overflow"), Some(1)).is_err());
    }

    #[tokio::test]
    async fn a_newer_generation_cancels_the_previous_within_one_pass() {
        let reg = StreamRegistry::new(8);
        let old = reg.register(Some("s1"), Some(1)).unwrap();
        let child = old.token().clone();
        let next = reg.register(Some("s1"), Some(2)).unwrap();
        tokio::time::timeout(Duration::from_millis(50), child.cancelled())
            .await
            .expect("superseded generation must be cancelled promptly");
        assert!(!next.token().is_cancelled());
    }
}
