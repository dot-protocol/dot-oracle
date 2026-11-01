//! The fold cache (docs/DESIGN.md, 21 Oct 2026): `GET /fold` answered from the last computation while nothing changed.
//!
//! Evidence: the Bridge folds every 0.65 s (22,000 folds in four hours, p95 15 s) and the ledger changes every few
//! seconds, so most folds are the last one again, and every write waited behind them. The cache is keyed by seat,
//! limit and minute (the fold's 24 h floor drifts by the minute) and fingerprinted by the store's newest sequence, so
//! any write misses. An acknowledgement moves a seat's cursors without moving the sequence, so each seat carries a
//! generation: an ack advances it after the store commits, a fill that began under an older generation is refused at
//! publication, and a lookup never returns an entry from an older generation (Emmy's review, 21 Oct: the ack race).
//! Same-seat folds take the seat's lane one at a time, so a burst of identical misses is one computation.
//!
//! Staleness, stated: a cached answer never lacks a new message (any write moves the sequence and misses) and
//! never shows an acknowledged one (the ack advances the generation). The one bounded staleness is the fold's 24 h
//! floor and the records' own expiry: a room first folded 24 h ago drops out of the answer at the floor, and a record
//! with `expires_ms` leaves the fold when the clock passes it (store::fold's LIVE filter); with no write and no ack in
//! between, the cached answer may still carry such rows for at most 60 s, because the minute is in the key and the
//! next minute recomputes. That is the whole staleness: 60 s at the floor and at a record's expiry, nothing else.
//! Budget: at most MAX_ENTRIES entries and `max_bytes` (MAX_BYTES by default) of serialized answers, a hard bound:
//! an answer larger than the budget is served and not cached; one that does not fit beside the rest clears the map
//! first; a replacement counts as total - old + new. A seat's earlier-minute entries and all its entries at an ack
//! are dropped.
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct FoldCache {
    max_bytes: usize,
    entries: Mutex<HashMap<String, Entry>>,
    bytes: std::sync::atomic::AtomicUsize,
    generations: Mutex<HashMap<String, u64>>,
    lanes: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    hits: std::sync::atomic::AtomicU64,
}

struct Entry {
    newest: i64,
    generation: u64,
    value: Value,
    size: usize,
}

/// At most this many entries in the map; past it the map is cleared whole (a count budget, stated: a fold answer is
/// a few KB, so 4,096 entries is at most tens of MB, and a clear costs one recomputation per live poller).
pub const MAX_ENTRIES: usize = 4096;
/// The fold's own limit clamp (store::fold): a key is made from the normalized limit, so limit=0, 50 and 500 are
/// three requests but at most two keys.
pub const LIMIT_RANGE: (i64, i64) = (1, 200);
/// The retained answers' serialized bytes, summed; past it the map is cleared whole.
pub const MAX_BYTES: usize = 64 << 20;

impl Default for FoldCache {
    fn default() -> Self {
        Self::with_budget(MAX_BYTES)
    }
}

impl FoldCache {
    /// A cache with its own byte budget (tests use a small one; the server uses MAX_BYTES).
    pub fn with_budget(max_bytes: usize) -> Self {
        FoldCache { max_bytes, entries: Mutex::new(HashMap::new()), bytes: Default::default(), generations: Mutex::new(HashMap::new()), lanes: Mutex::new(HashMap::new()), hits: Default::default() }
    }

    pub fn key(handle: &str, limit: i64, now_ms: i64) -> String {
        format!("{handle}:{}:{}", limit.clamp(LIMIT_RANGE.0, LIMIT_RANGE.1), now_ms / 60_000)
    }

    /// The minute an entry was keyed for; an entry of an earlier minute is expired at the seat's next publication
    /// (the fold's 24 h floor moves by the minute, so a minute-old answer is at most one minute stale and is never
    /// served once the minute turns, since the lookup key carries the current minute).
    fn minute_of(key: &str) -> Option<&str> {
        key.rsplit(':').next()
    }

    /// The seat's lane: same-seat folds run one at a time, so the second of a burst finds the first's answer.
    pub fn lane(&self, handle: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        lanes.entry(handle.to_string()).or_default().clone()
    }

    /// The seat's committed generation; a fill records it before computing and must still hold it to publish.
    pub fn generation(&self, handle: &str) -> u64 {
        self.generations.lock().unwrap_or_else(|e| e.into_inner()).get(handle).copied().unwrap_or(0)
    }

    /// An acknowledgement committed: advance the seat's generation and drop what it had (its entries are stale).
    pub fn invalidate(&self, handle: &str) {
        {
            let mut g = self.generations.lock().unwrap_or_else(|e| e.into_inner());
            *g.entry(handle.to_string()).or_insert(0) += 1;
        }
        let prefix = format!("{handle}:");
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|k, _| !k.starts_with(&prefix));
        self.bytes.store(entries.values().map(|e| e.size).sum(), std::sync::atomic::Ordering::Relaxed);
    }

    /// The cached answer for this key when the sequence and the seat's generation are both unchanged.
    pub fn lookup(&self, handle: &str, key: &str, newest: i64) -> Option<Value> {
        let generation = self.generation(handle);
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let e = entries.get(key)?;
        if e.newest != newest || e.generation != generation {
            return None;
        }
        self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(e.value.clone())
    }

    /// Publish a fill computed under `generation`; refused (false) when the seat was acknowledged meanwhile.
    pub fn publish(&self, handle: &str, key: String, newest: i64, generation: u64, value: Value) -> bool {
        if self.generation(handle) != generation {
            return false;
        }
        let size = value.to_string().len();
        if size > self.max_bytes {
            return false; // served to this request, never cached: the budget is a hard bound
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // expire the seat's entries of earlier minutes: the current minute is the only one a lookup can ask for
        let (prefix, minute) = (format!("{handle}:"), Self::minute_of(&key).unwrap_or("").to_string());
        entries.retain(|k, _| !k.starts_with(&prefix) || Self::minute_of(k) == Some(minute.as_str()));
        let existing = entries.get(&key).map(|e| e.size).unwrap_or(0);
        let total: usize = entries.values().map(|e| e.size).sum();
        if entries.len() >= MAX_ENTRIES || total - existing + size > self.max_bytes {
            entries.clear();
        }
        let total = entries.values().filter(|_| true).map(|e| e.size).sum::<usize>() - entries.get(&key).map(|e| e.size).unwrap_or(0) + size;
        entries.insert(key, Entry { newest, generation, value, size });
        self.bytes.store(total, std::sync::atomic::Ordering::Relaxed);
        true
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Serialized bytes retained right now (for /metrics/routes).
    pub fn bytes(&self) -> usize {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_second_fold_with_nothing_changed_is_the_first_again() {
        let c = FoldCache::default();
        let k = FoldCache::key("rocky", 50, 60_000);
        let g = c.generation("rocky");
        assert!(c.lookup("rocky", &k, 10).is_none());
        assert!(c.publish("rocky", k.clone(), 10, g, json!({"items": [1]})));
        assert_eq!(c.lookup("rocky", &k, 10), Some(json!({"items": [1]})));
        assert_eq!(c.hits(), 1);
        assert!(c.lookup("rocky", &k, 11).is_none(), "a write moved the sequence: miss");
        assert!(c.lookup("emmy", &FoldCache::key("emmy", 50, 60_000), 10).is_none(), "another seat has its own key");
        assert_ne!(FoldCache::key("rocky", 50, 60_000), FoldCache::key("rocky", 50, 120_000), "the minute is in the key");
        assert_eq!(FoldCache::key("rocky", 0, 0), FoldCache::key("rocky", 1, 0), "the limit is normalized as the fold clamps it");
        assert_eq!(FoldCache::key("rocky", 500, 0), FoldCache::key("rocky", 200, 0));
        // the minute turning: the old minute's entry is expired at the next publication and never served
        let k2 = FoldCache::key("rocky", 50, 120_000);
        assert!(c.publish("rocky", k2.clone(), 10, c.generation("rocky"), json!({"items": [2]})));
        assert!(c.lookup("rocky", &k, 10).is_none(), "the earlier minute is gone");
        assert_eq!(c.entries.lock().unwrap().len(), 1);
        assert_eq!(c.bytes(), json!({"items": [2]}).to_string().len(), "the byte budget tracks what is retained");
    }

    #[test]
    fn the_byte_budget_is_a_hard_bound() {
        let c = FoldCache::with_budget(100);
        let k = |h: &str| FoldCache::key(h, 50, 0);
        let v40 = json!({"pad": "x".repeat(30)}); // 40 bytes serialized
        assert_eq!(v40.to_string().len(), 40);
        assert!(c.publish("a", k("a"), 1, 0, v40.clone()));
        assert!(c.publish("b", k("b"), 1, 0, v40.clone()));
        assert_eq!(c.bytes(), 80);
        // a replacement counts total - old + new, so the same key again fits
        assert!(c.publish("a", k("a"), 2, 0, v40.clone()));
        assert_eq!((c.entries.lock().unwrap().len(), c.bytes()), (2, 80));
        // a third that does not fit beside the rest clears the map first, then fits alone
        assert!(c.publish("c", k("c"), 1, 0, v40.clone()));
        assert_eq!((c.entries.lock().unwrap().len(), c.bytes()), (1, 40));
        // an answer over the whole budget is never cached and the map is untouched
        let big = json!({"pad": "x".repeat(200)});
        assert!(!c.publish("d", k("d"), 1, 0, big));
        assert_eq!((c.entries.lock().unwrap().len(), c.bytes()), (1, 40));
        assert!(c.lookup("d", &k("d"), 1).is_none());
        assert!(c.bytes() <= 100);
        c.invalidate("c");
        assert_eq!(c.bytes(), 0);
    }

    #[test]
    fn an_ack_during_a_fill_refuses_the_stale_publication() {
        // the race Emmy named: GET begins (old cursors), POST /fold commits and invalidates, the old GET finishes
        let c = FoldCache::default();
        let k = FoldCache::key("rocky", 50, 60_000);
        let g = c.generation("rocky"); // the fill begins here
        c.invalidate("rocky"); // the ack commits and advances the generation
        assert!(!c.publish("rocky", k.clone(), 10, g, json!({"stale": true})), "the old fill may not publish");
        assert!(c.lookup("rocky", &k, 10).is_none());
        let g2 = c.generation("rocky");
        assert!(c.publish("rocky", k.clone(), 10, g2, json!({"fresh": true})));
        assert_eq!(c.lookup("rocky", &k, 10), Some(json!({"fresh": true})));
        // an ack after publication (ack_all, rewind: the same path) drops the entry and no lookup returns it
        c.invalidate("rocky");
        assert!(c.lookup("rocky", &k, 10).is_none());
        // a stale entry that somehow survived is still refused by its generation
        let c2 = FoldCache::default();
        let k2 = FoldCache::key("rocky", 50, 0);
        assert!(c2.publish("rocky", k2.clone(), 1, 0, json!(1)));
        *c2.generations.lock().unwrap().entry("rocky".into()).or_insert(0) += 1;
        assert!(c2.lookup("rocky", &k2, 1).is_none());
    }

    #[tokio::test]
    async fn same_seat_folds_take_one_lane() {
        let c = Arc::new(FoldCache::default());
        let lane = c.lane("rocky");
        let held = lane.clone().lock_owned().await;
        let c2 = c.clone();
        let waiter = tokio::spawn(async move {
            let lane = c2.lane("rocky");
            let _g = lane.lock().await; // waits for the first fill
            c2.lookup("rocky", &FoldCache::key("rocky", 50, 0), 5)
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "the second fold waits behind the first");
        assert!(c.publish("rocky", FoldCache::key("rocky", 50, 0), 5, c.generation("rocky"), json!("first")));
        drop(held);
        assert_eq!(waiter.await.unwrap(), Some(json!("first")), "the burst's second fold is the first's answer");
        assert!(!Arc::ptr_eq(&c.lane("rocky"), &c.lane("emmy")), "seats do not wait on each other");
    }
}
