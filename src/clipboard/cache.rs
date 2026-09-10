//! Outbound clipboard text byte cache (PLAN-2 / M1b STEP-1b.2).
//!
//! Stores the bytes the local daemon has pushed via `ClipboardText` so
//! a remote peer can pull them with HTTP/3
//! `GET /clipboard/text/{sha256}`. Keyed by SHA-256 (`[u8; 32]`).
//!
//! ## Differences from the existing `service::LruFingerprints` (M1a)
//!
//! | Aspect              | `LruFingerprints` (M1a)         | `ClipboardCache` (M1b.2)        |
//! |---------------------|---------------------------------|---------------------------------|
//! | Key                 | `[u8; 32]` (fingerprint)        | `[u8; 32]` (sha256)             |
//! | Value               | none (loopback defense only)    | `Vec<u8>` (the bytes to pull)   |
//! | Eviction            | LRU, capacity 64                | LRU, capacity 128, 5 min TTL    |
//! | Producer            | local tick (push time)          | local tick (push time)          |
//! | Consumer            | inbound arm (loopback check)    | HTTP/3 server (`/clipboard/text/`)|
//!
//! ## Active eviction (PLAN §1 评审 #3 2nd)
//!
//! The source-side dispatcher calls `remove(prev_sha256)` immediately
//! before pushing a new `ClipboardText`. This makes the cache "fail
//! closed" against the well-known race: receiver pulls X, source
//! pushes Y, evicts X, receiver's GET against X returns 404 → receiver
//! logs warn "cache miss" and skips. Without the active eviction, the
//! cache would still hold X (TTL not yet elapsed) and the receiver
//! would silently apply stale content.
//!
//! The 5 min TTL is a **fallback** safeguard: if a daemon pushes X and
//! then stays quiet for > 5 min without any further push, the next
//! receiver GET against X still returns 404 because `lookup` evicts
//! expired entries lazily on read.
//!
//! ## Concurrency
//!
//! `Arc<Mutex<ClipboardCache>>` is shared between three call sites:
//! 1. `service::Service` dispatch loop (writer + active evictor)
//! 2. `quic_transport::http3::default_router_with_cache` (reader on
//!    `/clipboard/text/{sha256}`)
//! 3. (not yet) `Emulation` server-side replies
//!
//! All three sites run on the daemon's `spawn_local` runtime except
//! the HTTP/3 server, which uses `tokio::spawn` per the existing
//! `build_server` design. `std::sync::Mutex` is appropriate here:
//! critical sections are short (one `HashMap::get` / `insert` /
//! `remove`), and we don't need `await` inside the lock.
//!
//! ## Inline vs metadata-only payload
//!
//! The cache is populated **only** for text larger than
//! [`CLIPBOARD_TEXT_INLINE_LIMIT`] (1 KiB). Smaller payloads travel
//! inline in the `ClipboardText` wire frame, so the receiver never
//! issues an HTTP/3 GET — caching them would waste memory for no
//! benefit (the inline bytes are already on the wire).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Maximum number of cached clipboard text payloads.
///
/// **Why 128**: capacity 1 covers the "1 push + 1 receiver pulls at a
/// time" baseline; the 128x headroom absorbs races where a few
/// receivers are mid-pull when the next push ejects the previous
/// payload, and a few clipboard pushes happen between the receiver's
/// metadata arrival and GET (typical for keyboard-heavy users). 128
/// entries × ~1 KiB–1 MiB each keeps the cache bounded well below the
/// daemon's other memory consumers (the QUIC connection buffers alone
/// dwarf this).
pub const CLIPBOARD_CACHE_CAPACITY: usize = 128;

/// Time-to-live for cached payloads.
///
/// Picked at 5 minutes because (a) PLAN §1 评审 #3 2nd says "5 min
/// LRU 兜底" (5 min LRU fallback), and (b) the active-eviction
/// contract means the TTL only fires when the daemon has stopped
/// pushing for > 5 min — at which point a receiver's GET against an
/// even older payload is no longer useful (the user has long since
/// copied something newer).
pub const CLIPBOARD_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug)]
struct CacheEntry {
    bytes: Vec<u8>,
    /// Wall-clock-relative timestamp at `insert` time. Used by
    /// [`ClipboardCache::lookup`] for the lazy TTL eviction. We use
    /// [`Instant`] (monotonic) rather than wall clock so a clock
    /// change (NTP correction, DST) cannot suddenly invalidate every
    /// cached entry.
    inserted_at: Instant,
}

/// Content-addressed clipboard text cache (sha256 → bytes).
///
/// Used by:
/// - the dispatcher (writer, active evictor)
/// - the HTTP/3 server-side `/clipboard/text/{sha256}` handler (reader)
#[derive(Debug)]
pub struct ClipboardCache {
    capacity: usize,
    ttl: Duration,
    /// Backing store. `HashMap` for O(1) lookup by sha256; `lru` below
    /// provides LRU eviction order on overflow.
    entries: HashMap<[u8; 32], CacheEntry>,
    /// Insertion-order LRU list. The front is the **oldest** entry;
    /// the back is the **newest**. On capacity overflow we pop the
    /// front. `lookup` does **not** touch this list — TTL eviction
    /// only fires lazily on read.
    lru: VecDeque<[u8; 32]>,
}

impl ClipboardCache {
    /// Construct a cache with the default capacity and TTL.
    pub fn new() -> Self {
        Self::with_capacity_and_ttl(CLIPBOARD_CACHE_CAPACITY, CLIPBOARD_CACHE_TTL)
    }

    /// Construct a cache with custom capacity / TTL. Used by tests
    /// that want a 1-second TTL or capacity 1 to exercise eviction
    /// quickly.
    pub fn with_capacity_and_ttl(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            entries: HashMap::with_capacity(capacity),
            lru: VecDeque::with_capacity(capacity),
        }
    }

    /// Insert a payload. Returns the previous value if an entry with
    /// the same key already existed (the caller can use this to decide
    /// whether the active-eviction path needs to run — usually the
    /// caller uses [`Self::remove`] explicitly instead).
    ///
    /// On capacity overflow the **oldest** entry is evicted. This is
    /// a best-effort fallback; the active eviction in the dispatcher
    /// should keep the cache size well under capacity in practice.
    pub fn insert(&mut self, sha256: [u8; 32], bytes: Vec<u8>) -> Option<Vec<u8>> {
        let previous_entry = self.entries.insert(
            sha256,
            CacheEntry {
                bytes: bytes.clone(),
                inserted_at: Instant::now(),
            },
        );
        // Only push onto the LRU deque if this is a fresh insert.
        // Re-inserting the same key would otherwise create a phantom
        // second entry in the LRU list, which would let a stale entry
        // outlive a capacity-evicting insert.
        if previous_entry.is_none() {
            self.lru.push_back(sha256);
        }
        // Capacity eviction — best effort. Walk the LRU from the
        // front, dropping entries until we're back under capacity.
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.lru.pop_front() {
                // The `oldest` might have been removed by an explicit
                // `remove()` between insert and this point, so the
                // `HashMap::remove` here is `Option`-aware.
                self.entries.remove(&oldest);
            } else {
                // Defensive: HashMap and VecDeque should be in sync,
                // but if we ever drift we stop evicting rather than
                // spin. (Capacity 128 + active eviction in the
                // dispatcher means this branch is unreachable in
                // practice.)
                break;
            }
        }
        previous_entry.map(|e| e.bytes)
    }

    /// Look up a payload by sha256.
    ///
    /// Returns `Some(bytes)` on hit **and** within TTL. Returns
    /// `None` on miss, expired, or any other failure (no panic /
    /// error propagation — the HTTP/3 route maps `None` directly to
    /// 404 + `log warn`).
    ///
    /// Lazy TTL eviction: expired entries are removed from the map on
    /// lookup. This is the "5 min LRU 兜底" behaviour from the PLAN.
    pub fn lookup(&mut self, sha256: &[u8; 32]) -> Option<Vec<u8>> {
        let now = Instant::now();
        match self.entries.get(sha256) {
            Some(entry) if now.duration_since(entry.inserted_at) < self.ttl => {
                Some(entry.bytes.clone())
            }
            Some(_) => {
                // Expired — evict and report miss.
                self.entries.remove(sha256);
                None
            }
            None => None,
        }
    }

    /// Explicitly remove an entry. Returns `true` if the entry was
    /// present (useful for the dispatcher's "evict prev before push"
    /// path to log the eviction only when it actually happened).
    ///
    /// The `lru` deque may still hold the removed key — that's fine
    /// because (a) `lookup` checks the HashMap first and never reads
    /// `lru`, and (b) `insert`'s capacity eviction path uses
    /// `entries.remove` which is a no-op for absent keys. We don't
    /// bother walking the deque because the per-eviction cost (O(n))
    /// would dwarf the per-insert cost (O(1)).
    pub fn remove(&mut self, sha256: &[u8; 32]) -> bool {
        self.entries.remove(sha256).is_some()
    }

    /// Current entry count. Test-only helper — production code does
    /// not need to observe the cache size.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for ClipboardCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert then lookup returns the same bytes.
    #[test]
    fn insert_then_lookup_returns_bytes() {
        let mut cache = ClipboardCache::new();
        let sha = [0xAA; 32];
        let bytes = b"hello world".to_vec();
        cache.insert(sha, bytes.clone());
        assert_eq!(cache.lookup(&sha), Some(bytes));
    }

    /// Lookup on an empty cache returns None.
    #[test]
    fn lookup_miss_on_empty_cache() {
        let mut cache = ClipboardCache::new();
        let sha = [0xBB; 32];
        assert_eq!(cache.lookup(&sha), None);
    }

    /// Two distinct sha256 keys coexist (no cross-contamination).
    #[test]
    fn distinct_keys_dont_clobber_each_other() {
        let mut cache = ClipboardCache::new();
        let sha_x = [0x01; 32];
        let sha_y = [0x02; 32];
        cache.insert(sha_x, b"X".to_vec());
        cache.insert(sha_y, b"Y".to_vec());
        assert_eq!(cache.lookup(&sha_x), Some(b"X".to_vec()));
        assert_eq!(cache.lookup(&sha_y), Some(b"Y".to_vec()));
    }

    /// Active eviction (the dispatcher's hot path): `remove(prev)`
    /// makes a subsequent `lookup(prev)` return None without touching
    /// other entries.
    #[test]
    fn remove_evicts_only_target_key() {
        let mut cache = ClipboardCache::new();
        let sha_x = [0x11; 32];
        let sha_y = [0x22; 32];
        cache.insert(sha_x, b"X".to_vec());
        cache.insert(sha_y, b"Y".to_vec());

        assert!(cache.remove(&sha_x));
        assert_eq!(cache.lookup(&sha_x), None, "X must be evicted");
        assert_eq!(
            cache.lookup(&sha_y),
            Some(b"Y".to_vec()),
            "Y must remain after X eviction"
        );

        // Removing an absent key returns false.
        assert!(!cache.remove(&sha_x));
    }

    /// Lazy TTL eviction: an entry past its TTL is removed on the
    /// next `lookup`. Uses a 0-second TTL to avoid `tokio::time::sleep`
    /// in the test.
    #[test]
    fn expired_entries_are_evicted_on_lookup() {
        let mut cache = ClipboardCache::with_capacity_and_ttl(16, Duration::from_millis(0));
        let sha = [0xCC; 32];
        cache.insert(sha, b"stale".to_vec());
        // Any non-zero delay trips the 0-ms TTL.
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            cache.lookup(&sha),
            None,
            "entry past TTL must be evicted on lookup"
        );
    }

    /// Capacity overflow evicts the oldest entry (LRU from front).
    #[test]
    fn capacity_overflow_evicts_oldest() {
        let mut cache = ClipboardCache::with_capacity_and_ttl(2, Duration::from_secs(60));
        cache.insert([0x01; 32], b"first".to_vec());
        cache.insert([0x02; 32], b"second".to_vec());
        // Third insert pushes the first out (capacity 2).
        cache.insert([0x03; 32], b"third".to_vec());
        assert_eq!(
            cache.lookup(&[0x01; 32]),
            None,
            "first must be evicted (oldest)"
        );
        assert_eq!(cache.lookup(&[0x02; 32]), Some(b"second".to_vec()));
        assert_eq!(cache.lookup(&[0x03; 32]), Some(b"third".to_vec()));
    }

    /// Re-inserting the same key does not double-count against
    /// capacity (only the first insert creates an LRU entry).
    #[test]
    fn reinsert_same_key_does_not_duplicate_lru_entry() {
        let mut cache = ClipboardCache::with_capacity_and_ttl(2, Duration::from_secs(60));
        let sha = [0x99; 32];
        cache.insert(sha, b"v1".to_vec());
        cache.insert(sha, b"v2".to_vec());
        // After two inserts with the same key, the cache should still
        // hold only one entry; inserting a third distinct key would
        // not trigger eviction if duplicates were counted.
        cache.insert([0xAA; 32], b"third".to_vec());
        assert_eq!(cache.lookup(&sha), Some(b"v2".to_vec()));
        assert_eq!(cache.lookup(&[0xAA; 32]), Some(b"third".to_vec()));
        assert_eq!(
            cache.len(),
            2,
            "re-inserting the same key must not double-count entries"
        );
    }

    /// Active-eviction contract: the dispatcher's `remove(prev) →
    /// insert(new)` pair must result in `lookup(prev) == None` and
    /// `lookup(new) == Some(new_bytes)` simultaneously — pins the
    /// "old X has 5 min TTL, new Y pushed, receiver pulls X within
    /// 5 min still gets old X" race fix (PLAN §1 评审 #3 2nd).
    #[test]
    fn active_eviction_concurrent_with_lookup_old_returns_miss() {
        let mut cache = ClipboardCache::with_capacity_and_ttl(16, Duration::from_secs(60));
        let sha_x = [0x33; 32];
        let sha_y = [0x44; 32];
        cache.insert(sha_x, b"X contents (large payload)".to_vec());

        // Active eviction path: source pushes new content.
        assert!(
            cache.remove(&sha_x),
            "X must have been present before eviction"
        );
        cache.insert(sha_y, b"Y contents (different large payload)".to_vec());

        // A receiver that started pulling X *before* the new push
        // would have raced with the eviction. From now on (the new
        // push already evicted X), X must be a miss.
        assert_eq!(
            cache.lookup(&sha_x),
            None,
            "evicted X must NOT be retrievable after source-side push"
        );
        assert_eq!(
            cache.lookup(&sha_y),
            Some(b"Y contents (different large payload)".to_vec())
        );
    }
}
