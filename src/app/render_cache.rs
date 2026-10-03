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
}

/// In-memory only; never written to the session file.
#[derive(Debug, Default)]
pub struct RenderCache {
    entries: HashMap<RenderKey, Entry>,
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
        let entry = self.entries.entry(key.clone()).or_default();
        entry.latest_revision = revision;
        entry.fingerprint = fingerprint;
        entry.status = RenderStatus::Pending;
    }

    /// Stores a display list if it is not stale.
    pub fn store(&mut self, key: &RenderKey, revision: u64, nodes: Arc<Vec<Node>>) {
        let entry = self.entries.entry(key.clone()).or_default();
        if revision >= entry.latest_revision {
            entry.latest_revision = revision;
            entry.nodes = Some(nodes);
            entry.status = RenderStatus::Ready;
        }
    }

    /// Marks a render as failed if it is not stale.
    pub fn fail(&mut self, key: &RenderKey, revision: u64) {
        let entry = self.entries.entry(key.clone()).or_default();
        if revision >= entry.latest_revision {
            entry.latest_revision = revision;
            entry.status = RenderStatus::Failed;
            entry.nodes = None;
        }
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
}
