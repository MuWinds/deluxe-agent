//! The GUI-side render cache.
//!
//! Holds the renderer's output per transcript item, keyed by [`RenderKey`]. It
//! lives only in memory — the session stores the raw text and tool results, so
//! the display list can always be regenerated. A response is stored only if its
//! revision is not older than the newest request for that key, which is what
//! keeps a slow render from overwriting a newer one during streaming.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use uuid::Uuid;

use crate::renderer::protocol::{Node, RenderKey};

/// How many rendered items the cache holds before it evicts.
///
/// One entry per transcript item ever rendered, across every session opened
/// this process, would otherwise grow without bound. The cap is generous
/// relative to a session's step count, so switching between a few sessions
/// stays warm; exceeding it costs a re-render, never correctness, because a
/// served entry must still match its fingerprint.
const MAX_ENTRIES: usize = 4096;

/// Where one transcript item's render stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderStatus {
    /// No request has been made.
    #[default]
    Missing,
    /// A request is in flight.
    Pending,
    /// The display list is ready to draw.
    Ready,
    /// The renderer failed; draw the fallback and do not retry this input.
    Failed,
}

/// One cached render.
#[derive(Debug, Default)]
pub struct Entry {
    /// The newest revision the GUI has requested for this key.
    pub latest_revision: u64,
    /// A hash of the input, so identical content is not re-requested.
    pub fingerprint: u64,
    pub status: RenderStatus,
    /// The decoded display list, once ready.
    pub nodes: Option<Arc<Vec<Node>>>,
    /// When this entry was last written, for the eviction order. Higher is more
    /// recent; only the relative order matters.
    pub seq: u64,
    /// The cheap content signature last seen for this key, and the fingerprint
    /// it produced. Lets a frame reuse the fingerprint of an unchanged item
    /// instead of re-hashing its content. See [`RenderCache::fingerprint_for`].
    pub memo: Option<(u64, u64)>,
}

/// In-memory only; never written to the session file.
#[derive(Debug, Default)]
pub struct RenderCache {
    entries: HashMap<RenderKey, Entry>,
    /// Monotonic counter stamped onto an entry on every write.
    tick: u64,
}

impl RenderCache {
    /// The entry for a key, if any.
    pub fn get(&self, key: &RenderKey) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// The ready display list for a key, if it matches the current input.
    pub fn rendered(&self, key: &RenderKey, fingerprint: u64) -> Option<Arc<Vec<Node>>> {
        self.entries
            .get(key)
            .filter(|entry| entry.status == RenderStatus::Ready && entry.fingerprint == fingerprint)
            .and_then(|entry| entry.nodes.clone())
    }

    /// Whether a request for this exact input is already in flight or was tried.
    ///
    /// Used to avoid re-requesting identical content while it is pending, and to
    /// avoid retrying an input the renderer already rejected.
    pub fn was_requested(&self, key: &RenderKey, fingerprint: u64) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| entry.fingerprint == fingerprint)
    }

    /// Records that a request at `revision` is in flight for `key`.
    pub fn begin(&mut self, key: &RenderKey, revision: u64, fingerprint: u64) {
        let seq = self.next_seq();
        let entry = self.entries.entry(key.clone()).or_default();
        entry.latest_revision = revision;
        entry.fingerprint = fingerprint;
        entry.status = RenderStatus::Pending;
        entry.seq = seq;
        self.prune();
    }

    /// Stores a display list if it is not stale.
    pub fn store(&mut self, key: &RenderKey, revision: u64, nodes: Arc<Vec<Node>>) {
        let seq = self.next_seq();
        let entry = self.entries.entry(key.clone()).or_default();
        if revision >= entry.latest_revision {
            entry.latest_revision = revision;
            entry.nodes = Some(nodes);
            entry.status = RenderStatus::Ready;
            entry.seq = seq;
        }
        self.prune();
    }

    /// Marks a render as failed if it is not stale.
    pub fn fail(&mut self, key: &RenderKey, revision: u64) {
        let seq = self.next_seq();
        let entry = self.entries.entry(key.clone()).or_default();
        if revision >= entry.latest_revision {
            entry.latest_revision = revision;
            entry.status = RenderStatus::Failed;
            entry.nodes = None;
            entry.seq = seq;
        }
        self.prune();
    }

    /// Drops entries whose session is no longer open.
    pub fn retain_sessions(&mut self, live: &HashSet<Uuid>) {
        self.entries.retain(|key, _| match key {
            RenderKey::Message { session, .. } | RenderKey::Tool { session, .. } => {
                live.contains(session)
            }
        });
    }

    /// Drops every entry.
    ///
    /// Used when the renderer becomes available again after being unavailable:
    /// the entries marked [`RenderStatus::Failed`] would otherwise never be
    /// retried, so the transcript would stay on the fallback forever.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drops a session's message renders.
    ///
    /// A compaction rewrites the transcript and shifts step indices, so a
    /// message entry keyed by index no longer describes the step now at that
    /// index — and its memo would hand out the old fingerprint. Tool entries
    /// are keyed by call id and survive a shift untouched.
    pub fn clear_session_messages(&mut self, session: Uuid) {
        self.entries.retain(
            |key, _| !matches!(key, RenderKey::Message { session: s, .. } if *s == session),
        );
    }

    /// The content fingerprint for `key`, recomputed by `compute` only when
    /// `signature` differs from the last one seen for this key.
    ///
    /// `signature` is a cheap proxy for the item's render inputs — a length, or
    /// a small tuple of lengths — that changes whenever the content does. This
    /// is what stops `collect_rendered` from re-hashing every message and
    /// serialising every tool result on every frame: an unchanged item reuses
    /// the fingerprint it produced last time. It never touches `nodes` or
    /// `status`, so it cannot serve a stale display list.
    pub fn fingerprint_for(
        &mut self,
        key: &RenderKey,
        signature: u64,
        compute: impl FnOnce() -> u64,
    ) -> u64 {
        let entry = self.entries.entry(key.clone()).or_default();
        match entry.memo {
            Some((seen, fingerprint)) if seen == signature => fingerprint,
            _ => {
                let fingerprint = compute();
                entry.memo = Some((signature, fingerprint));
                fingerprint
            }
        }
    }

    /// The next recency stamp.
    fn next_seq(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }

    /// Evicts the least-recently-written entries once the cache is over
    /// [`MAX_ENTRIES`].
    ///
    /// It evicts down to half the cap rather than to the cap, so an insert at
    /// the limit does not sort on every call — pruning is rare instead. An
    /// evicted entry that is still on screen is simply re-requested.
    pub(super) fn prune(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }

        let target = MAX_ENTRIES / 2;
        let mut order: Vec<(u64, RenderKey)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.seq, key.clone()))
            .collect();
        order.sort_unstable_by_key(|(seq, _)| *seq);

        let drop_count = self.entries.len() - target;
        for (_, key) in order.into_iter().take(drop_count) {
            self.entries.remove(&key);
        }
    }
}

/// Hashes anything to a stable fingerprint.
///
/// Used only to skip re-requesting identical content; never a cache key on its
/// own, so a collision merely wastes one render. The caller folds in every input
/// the render depends on — the text, the tool call, and the layout width — so a
/// change in any of them produces a new fingerprint.
pub fn fingerprint(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::protocol::{ColorRole, Node, Run};

    fn key() -> RenderKey {
        RenderKey::Message {
            session: Uuid::nil(),
            step: 0,
        }
    }

    fn nodes() -> Arc<Vec<Node>> {
        Arc::new(vec![Node::Text {
            runs: vec![Run::text("hi", 14.0, ColorRole::Text)],
            wrap: true,
            selectable: true,
        }])
    }

    #[test]
    fn a_stale_response_does_not_overwrite_a_newer_one() {
        let mut cache = RenderCache::default();
        cache.begin(&key(), 2, 100);
        // A response for revision 1 arrives late and is dropped.
        cache.store(&key(), 1, Arc::new(vec![]));
        assert_eq!(
            cache.get(&key()).expect("entry").status,
            RenderStatus::Pending
        );
        cache.store(&key(), 2, nodes());
        assert_eq!(
            cache.get(&key()).expect("entry").status,
            RenderStatus::Ready
        );
        assert!(cache.rendered(&key(), 100).is_some());
    }

    #[test]
    fn a_changed_fingerprint_is_not_served_from_cache() {
        let mut cache = RenderCache::default();
        cache.begin(&key(), 1, 1);
        cache.store(&key(), 1, nodes());
        assert!(cache.rendered(&key(), 1).is_some());
        assert!(cache.rendered(&key(), 2).is_none(), "stale fingerprint");
    }

    #[test]
    fn sessions_that_closed_are_evicted() {
        let mut cache = RenderCache::default();
        let open = Uuid::new_v4();
        let closed = Uuid::new_v4();
        cache.begin(
            &RenderKey::Message {
                session: open,
                step: 0,
            },
            1,
            1,
        );
        cache.begin(
            &RenderKey::Message {
                session: closed,
                step: 0,
            },
            1,
            1,
        );
        let live: HashSet<Uuid> = [open].into_iter().collect();
        cache.retain_sessions(&live);
        assert!(cache
            .get(&RenderKey::Message {
                session: open,
                step: 0
            })
            .is_some());
        assert!(cache
            .get(&RenderKey::Message {
                session: closed,
                step: 0
            })
            .is_none());
    }

    #[test]
    fn the_cache_stays_bounded_as_items_accumulate() {
        let mut cache = RenderCache::default();
        for step in 0..(MAX_ENTRIES + 10) {
            let key = RenderKey::Message {
                session: Uuid::nil(),
                step,
            };
            cache.begin(&key, 1, step as u64);
            cache.store(&key, 1, nodes());
        }

        assert!(
            cache.entries.len() <= MAX_ENTRIES,
            "the cache grew past its cap: {} entries",
            cache.entries.len()
        );
    }

    #[test]
    fn pruning_keeps_the_most_recently_written_entry() {
        let mut cache = RenderCache::default();
        for step in 0..MAX_ENTRIES {
            cache.begin(
                &RenderKey::Message {
                    session: Uuid::nil(),
                    step,
                },
                1,
                step as u64,
            );
        }

        // Rewrite the oldest key, then overflow the cache: recency must protect
        // it even though its step index is the smallest.
        let keep = RenderKey::Message {
            session: Uuid::nil(),
            step: 0,
        };
        cache.begin(&keep, 2, u64::MAX);

        for step in MAX_ENTRIES..(MAX_ENTRIES + 10) {
            cache.begin(
                &RenderKey::Message {
                    session: Uuid::nil(),
                    step,
                },
                1,
                step as u64,
            );
        }

        assert!(
            cache.get(&keep).is_some(),
            "the most recently written entry was evicted"
        );
    }

    #[test]
    fn a_fingerprint_is_reused_until_its_signature_changes() {
        let mut cache = RenderCache::default();
        let mut computes = 0;

        let first = cache.fingerprint_for(&key(), 1, || {
            computes += 1;
            42
        });
        let same = cache.fingerprint_for(&key(), 1, || {
            computes += 1;
            99
        });
        let changed = cache.fingerprint_for(&key(), 2, || {
            computes += 1;
            7
        });

        assert_eq!(first, 42);
        assert_eq!(same, 42, "an unchanged signature reuses the fingerprint");
        assert_eq!(changed, 7, "a changed signature recomputes");
        assert_eq!(computes, 2, "the expensive compute ran only on change");
    }

    #[test]
    fn a_compaction_drops_only_the_sessions_message_renders() {
        let mut cache = RenderCache::default();
        let session = Uuid::new_v4();
        cache.store(&RenderKey::Message { session, step: 0 }, 1, nodes());
        cache.store(
            &RenderKey::Tool {
                session,
                call_id: "call".into(),
            },
            1,
            nodes(),
        );

        cache.clear_session_messages(session);

        assert!(
            cache
                .get(&RenderKey::Message { session, step: 0 })
                .is_none(),
            "index-keyed message renders are dropped"
        );
        assert!(
            cache
                .get(&RenderKey::Tool {
                    session,
                    call_id: "call".into()
                })
                .is_some(),
            "call-id-keyed tool renders survive the shift"
        );
    }
}
