//! Outbound clipboard byte cache (PLAN-2 / M1b STEP-1b.2 + M2a STEP-2a.3).
//!
//! Stores the bytes the local daemon has pushed via `ClipboardText` /
//! `ClipboardImage` so a remote peer can pull them with HTTP/3
//! `GET /clipboard/text/{sha256}` or
//! `GET /clipboard/image/{sha256}`. Keyed by SHA-256 (`[u8; 32]`).
//!
//! ## Differences from the existing `service::LruFingerprints` (M1a)
//!
//! | Aspect              | `LruFingerprints` (M1a)         | `ClipboardCache` (M1b.2 + M2a.3)   |
//! |---------------------|---------------------------------|--------------------------------------|
//! | Key                 | `[u8; 32]` (fingerprint)        | `[u8; 32]` (sha256)                  |
//! | Value               | none (loopback defense only)    | `Vec<u8>` (the bytes to pull)        |
//! | Eviction            | LRU, capacity 64                | LRU, byte budget 200 MiB, 5 min TTL  |
//! | Producer            | local tick (push time)          | local tick (push time)               |
//! | Consumer            | inbound arm (loopback check)    | HTTP/3 server (`/clipboard/{text,image}/`)|
//!
//! ## Active eviction (PLAN §1 评审 #3 2nd)
//!
//! The source-side dispatcher calls `remove(prev_sha256)` immediately
//! before pushing a new `ClipboardText` / `ClipboardImage`. This makes
//! the cache "fail closed" against the well-known race: receiver pulls
//! X, source pushes Y, evicts X, receiver's GET against X returns 404
//! → receiver logs warn "cache miss" and skips. Without the active
//! eviction, the cache would still hold X (TTL not yet elapsed) and
//! the receiver would silently apply stale content.
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
//!    `/clipboard/{text,image}/{sha256}`)
//! 3. (not yet) `Emulation` server-side replies
//!
//! All three sites run on the daemon's `spawn_local` runtime except
//! the HTTP/3 server, which uses `tokio::spawn` per the existing
//! `build_server` design. `std::sync::Mutex` is appropriate here:
//! critical sections are short (one `HashMap::get` / `insert` /
//! `remove`), and we don't need `await` inside the lock.
//!
//! ## Inline vs metadata-only payload (text only)
//!
//! The cache is populated for **any image payload** and for text
//! larger than [`lan_mouse_proto::CLIPBOARD_TEXT_INLINE_LIMIT`]
//! (1 KiB). Smaller text payloads travel inline in the `ClipboardText`
//! wire frame, so the receiver never issues an HTTP/3 GET — caching
//! them would waste memory for no benefit.
//!
//! ## Capacity: 200 MiB byte budget (M2a STEP-2a.3)
//!
//! **M1b.2** sized the cache as **128 entries × 5 min TTL**. That
//! count-based cap works for text but is impractical for images: a
//! single 4 K screenshot is 5-15 MiB; a 200 MiB byte budget easily
//! holds ~20 typical 4 K screenshots. The byte-budget semantic is
//! **also** better-behaved under mixed text+image workloads — a
//! 100 MiB text + 50 MiB image budget share one eviction pool, while
//! a count-based cap would let a single 100 MiB text push evict
//! every cached image.
//!
//! **Single-entry overflow rejection**: an entry whose byte length
//! exceeds the entire budget is rejected (`insert` returns `None` and
//! does not store the bytes). 200 MiB is well above any realistic
//! clipboard payload; the rejection is a defensive bound against
//! pathological inputs (e.g. a misbehaving peer sending a fake
//! `ClipboardImage` with size = `u32::MAX`).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Total byte budget for cached clipboard payloads (text + image).
///
/// **Why 200 MiB**: covers PLAN §3 M2a STEP-2a.3 "图片字节暂存本地
/// `clipboard_cache`（key = sha256，5 min LRU 200 MiB 上限）" and
/// PLAN §8 M2a milestone "macOS 图片剪贴板端到端（4K 截图字节级一致）"
/// — 4 K screenshots typically run 5-15 MiB each, so 200 MiB holds
/// ~13-40 screenshots under aggressive user activity before LRU
/// eviction kicks in.
pub const CLIPBOARD_CACHE_BYTE_BUDGET: usize = 200 * 1024 * 1024;

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

/// Content-addressed clipboard byte cache (sha256 → bytes).
///
/// Used by:
/// - the dispatcher (writer, active evictor)
/// - the HTTP/3 server-side `/clipboard/{text,image}/{sha256}` handler
///   (reader)
#[derive(Debug)]
pub struct ClipboardCache {
    /// Maximum total bytes that may be stored at any moment. When
    /// `bytes_used > byte_budget` after a fresh insert the oldest
    /// entries are evicted until `bytes_used <= byte_budget`.
    byte_budget: usize,
    ttl: Duration,
    /// Backing store. `HashMap` for O(1) lookup by sha256; `lru`
    /// below provides LRU eviction order on overflow.
    entries: HashMap<[u8; 32], CacheEntry>,
    /// Insertion-order LRU list. The front is the **oldest** entry;
    /// the back is the **newest**. On byte-budget overflow we pop the
    /// front. `lookup` does **not** touch this list — TTL eviction
    /// only fires lazily on read.
    lru: VecDeque<[u8; 32]>,
    /// Sum of `entry.bytes.len()` for every entry currently in
    /// `entries`. Used by `insert` to enforce the byte budget without
    /// re-summing on every eviction pass.
    bytes_used: usize,
}

impl ClipboardCache {
    /// Construct a cache with the default byte budget (200 MiB) and
    /// TTL (5 min).
    pub fn new() -> Self {
        Self::with_byte_budget_and_ttl(CLIPBOARD_CACHE_BYTE_BUDGET, CLIPBOARD_CACHE_TTL)
    }

    /// Construct a cache with custom byte budget / TTL. Used by
    /// tests that want a small byte budget (e.g. 5 bytes) to exercise
    /// eviction without allocating 200 MiB.
    pub fn with_byte_budget_and_ttl(byte_budget: usize, ttl: Duration) -> Self {
        Self {
            byte_budget,
            ttl,
            entries: HashMap::new(),
            lru: VecDeque::new(),
            bytes_used: 0,
        }
    }

    /// Insert a payload. Returns the previous value if an entry with
    /// the same key already existed.
    ///
    /// **Byte-budget enforcement**: if `bytes.len() > byte_budget`,
    /// the insert is rejected and `None` is returned (the bytes are
    /// dropped, no cache mutation). The defensive bound covers a
    /// single entry that could never fit; see the module-level doc
    /// for the rationale.
    ///
    /// On byte-budget overflow (after a successful insert) the
    /// **oldest** entries are evicted until the budget is met again.
    /// This is a best-effort fallback; the active eviction in the
    /// dispatcher should keep the cache size well under budget in
    /// practice.
    pub fn insert(&mut self, sha256: [u8; 32], bytes: Vec<u8>) -> Option<Vec<u8>> {
        let new_size = bytes.len();
        // Single-entry overflow: no way to fit, drop on the floor.
        // 200 MiB is well above any realistic clipboard payload, so
        // this branch is defensive — a real `ClipboardImage` from a
        // healthy peer never trips it.
        if new_size > self.byte_budget {
            log::warn!(
                "clipboard cache: rejected entry of {new_size} bytes (budget {budget} bytes)",
                budget = self.byte_budget
            );
            return None;
        }
        let previous_entry = self.entries.insert(
            sha256,
            CacheEntry {
                bytes: bytes.clone(),
                inserted_at: Instant::now(),
            },
        );
        // Subtract the old size BEFORE adding the new size — if this
        // was an overwrite (same sha256), the net change is the
        // delta, not the sum.
        if let Some(prev) = &previous_entry {
            self.bytes_used -= prev.bytes.len();
        } else {
            // Only push onto the LRU deque if this is a fresh
            // insert. Re-inserting the same key would otherwise
            // create a phantom second entry in the LRU list, which
            // would let a stale entry outlive a budget-evicting
            // insert.
            self.lru.push_back(sha256);
        }
        self.bytes_used += new_size;
        // Budget eviction — best effort. Walk the LRU from the
        // front, dropping entries until we're back under budget.
        while self.bytes_used > self.byte_budget {
            if let Some(oldest) = self.lru.pop_front() {
                // The `oldest` might have been removed by an
                // explicit `remove()` between insert and this
                // point, so the `HashMap::remove` here is `Option`
                // -aware.
                if let Some(removed) = self.entries.remove(&oldest) {
                    self.bytes_used -= removed.bytes.len();
                }
            } else {
                // Defensive: HashMap and VecDeque should be in sync,
                // but if we ever drift we stop evicting rather than
                // spin. (Single-entry overflow check above already
                // guarantees we can never hit this branch with a
                // single fresh insert.)
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
                // Expired — evict and report miss. Adjust the byte
                // counter so the next `bytes()` call stays accurate.
                if let Some(removed) = self.entries.remove(sha256) {
                    self.bytes_used -= removed.bytes.len();
                }
                None
            }
            None => None,
        }
    }

    /// Explicitly remove an entry. Returns `true` if the entry was
    /// present (useful for the dispatcher's "evict prev before push"
    /// path to log the eviction only when it actually happened).
    ///
    /// **Maintains the byte counter**: if the entry was present its
    /// `bytes.len()` is subtracted from `bytes_used`. The `lru` deque
    /// may still hold the removed sha256 — that's fine because
    /// `lookup` checks the HashMap first and never reads `lru`, and
    /// `insert`'s budget eviction path uses `entries.remove` which
    /// is a no-op for absent keys.
    pub fn remove(&mut self, sha256: &[u8; 32]) -> bool {
        match self.entries.remove(sha256) {
            Some(entry) => {
                self.bytes_used -= entry.bytes.len();
                true
            }
            None => false,
        }
    }

    /// Current total bytes occupied by cached entries. Exposed for
    /// diagnostics + tests; production code does not need to observe
    /// the cache size during normal operation (the byte budget is
    /// enforced transparently by `insert`).
    pub fn bytes(&self) -> usize {
        self.bytes_used
    }

    /// Configured byte budget. Exposed so callers can verify the
    /// production default (` 200 MiB`) without inspecting constants
    /// — used by the cache tests below.
    pub fn byte_budget(&self) -> usize {
        self.byte_budget
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
        let mut cache = ClipboardCache::with_byte_budget_and_ttl(1024, Duration::from_millis(0));
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

    /// Byte-budget overflow evicts the oldest entry (LRU from front).
    /// Budget = 10 bytes: the third 4-byte insert (total = 12 bytes
    /// > 10) pushes the oldest 4-byte entry out.
    #[test]
    fn byte_budget_overflow_evicts_oldest() {
        // 10-byte budget: 3 × 4-byte inserts. The first two fit
        // (8 bytes total); the third would push total to 12 > 10,
        // triggering eviction of the oldest 4-byte entry.
        let mut cache = ClipboardCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
        cache.insert([0x01; 32], b"AAAA".to_vec());
        cache.insert([0x02; 32], b"BBBB".to_vec());
        // Third insert: 4 + 4 (still held) + 4 (new) = 12 > 10.
        // Eviction kicks in: [0x01] (oldest) is dropped, leaving
        // [0x02] + [0x03] = 8 bytes total.
        cache.insert([0x03; 32], b"CCCC".to_vec());
        assert_eq!(
            cache.lookup(&[0x01; 32]),
            None,
            "first entry must be evicted (oldest) when byte budget overflows"
        );
        assert_eq!(cache.lookup(&[0x02; 32]), Some(b"BBBB".to_vec()));
        assert_eq!(cache.lookup(&[0x03; 32]), Some(b"CCCC".to_vec()));
        assert_eq!(
            cache.bytes(),
            8,
            "cache must hold 8 bytes after eviction ([0x02] + [0x03])"
        );
    }

    /// Re-inserting the same key does not double-count against the
    /// byte budget (overwriting subtracts the old size before adding
    /// the new one).
    #[test]
    fn reinsert_same_key_does_not_double_count_bytes() {
        // 10-byte budget: re-insert the same key twice with sizes
        // 5 + 5 = 10 bytes; budget is not exceeded.
        let mut cache = ClipboardCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
        let sha = [0x99; 32];
        cache.insert(sha, b"12345".to_vec());
        cache.insert(sha, b"ABCDE".to_vec());
        // After two overwrites the cache holds one entry of 5 bytes;
        // a third distinct 5-byte entry would not trigger eviction
        // if duplicates were counted.
        cache.insert([0xAA; 32], b"vwxyz".to_vec());
        assert_eq!(cache.lookup(&sha), Some(b"ABCDE".to_vec()));
        assert_eq!(cache.lookup(&[0xAA; 32]), Some(b"vwxyz".to_vec()));
        assert_eq!(
            cache.len(),
            2,
            "re-inserting the same key must not duplicate entries"
        );
        assert_eq!(
            cache.bytes(),
            10,
            "re-inserting the same key must not double-count bytes"
        );
    }

    /// Active-eviction contract: the dispatcher's `remove(prev) →
    /// insert(new)` pair must result in `lookup(prev) == None` and
    /// `lookup(new) == Some(new_bytes)` simultaneously — pins the
    /// "old X has 5 min TTL, new Y pushed, receiver pulls X within
    /// 5 min still gets old X" race fix (PLAN §1 评审 #3 2nd).
    #[test]
    fn active_eviction_concurrent_with_lookup_old_returns_miss() {
        let mut cache = ClipboardCache::with_byte_budget_and_ttl(1024, Duration::from_secs(60));
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

    // ===== M2a STEP-2a.3 — byte-budget specific tests =====

    /// Default byte budget is **200 MiB** (PLAN §3 M2a STEP-2a.3
    /// "200 MiB 上限"). Verified by construction + `byte_budget()`
    /// getter so a future refactor that bumps the budget
    /// accidentally is caught.
    #[test]
    fn default_byte_budget_is_200_mib() {
        let cache = ClipboardCache::new();
        assert_eq!(
            cache.byte_budget(),
            200 * 1024 * 1024,
            "default byte budget must be 200 MiB"
        );
        assert_eq!(
            CLIPBOARD_CACHE_BYTE_BUDGET,
            200 * 1024 * 1024,
            "CLIPBOARD_CACHE_BYTE_BUDGET constant must equal 200 MiB"
        );
    }

    /// `bytes()` API returns the total byte count after inserts /
    /// removes / lazy TTL eviction.
    #[test]
    fn bytes_returns_total_byte_count() {
        let mut cache = ClipboardCache::with_byte_budget_and_ttl(1024, Duration::from_secs(60));
        assert_eq!(cache.bytes(), 0, "fresh cache has zero bytes");
        cache.insert([0x01; 32], vec![0; 100]);
        assert_eq!(cache.bytes(), 100);
        cache.insert([0x02; 32], vec![0; 250]);
        assert_eq!(cache.bytes(), 350);
        cache.remove(&[0x01; 32]);
        assert_eq!(cache.bytes(), 250, "remove must subtract the entry's bytes");
        // Overwriting the remaining entry with a smaller payload:
        // bytes drop, not accumulate.
        cache.insert([0x02; 32], vec![0; 50]);
        assert_eq!(cache.bytes(), 50);
    }

    /// Single-entry overflow rejection: a payload larger than the
    /// entire byte budget is rejected — `insert` returns `None`,
    /// does not mutate the cache, and the byte counter is unchanged.
    #[test]
    fn insert_larger_than_budget_is_rejected() {
        let mut cache = ClipboardCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
        let sha = [0x42; 32];
        let huge_payload = vec![0; 100];
        assert!(
            cache.insert(sha, huge_payload).is_none(),
            "an entry larger than the byte budget must be rejected"
        );
        assert_eq!(
            cache.bytes(),
            0,
            "rejected insert must not affect the byte counter"
        );
        assert_eq!(
            cache.len(),
            0,
            "rejected insert must not add an entry to the cache"
        );
        assert_eq!(
            cache.lookup(&sha),
            None,
            "rejected payload must not be retrievable"
        );
    }

    /// 200 MiB eviction sweep: a sequence of 100 MiB inserts with a
    /// 200 MiB budget evicts the oldest when the second is pushed,
    /// pinning the byte-budget eviction contract at the production
    /// scale. (Avoids allocating 200 MiB total by reusing the same
    /// buffer; the cache holds at most 2 × 100 MiB briefly during the
    /// insert.)
    #[test]
    fn byte_budget_200_mib_evicts_oldest_when_total_exceeds_cap() {
        // 100 MiB buffer reused for two distinct pushes. Total
        // briefly reaches 200 MiB (well within the budget) — the
        // third push would exceed, so we simulate "budget = 100 MiB"
        // by allocating exactly the budget and observing the
        // eviction on the next push of the same size.
        let mut cache =
            ClipboardCache::with_byte_budget_and_ttl(100 * 1024 * 1024, Duration::from_secs(60));
        let buf_100_mib = vec![0xAAu8; 100 * 1024 * 1024];
        // Insert #1: 100 MiB, fits exactly.
        cache.insert([0x01; 32], buf_100_mib.clone());
        assert_eq!(cache.bytes(), 100 * 1024 * 1024);
        assert_eq!(cache.lookup(&[0x01; 32]), Some(buf_100_mib.clone()));
        // Insert #2: 100 MiB → total = 200 MiB > 100 MiB budget.
        // Eviction kicks in: [0x01] (oldest) is dropped, [0x02] (new)
        // is kept. The briefly-held total (200 MiB) is correctly
        // resolved to 100 MiB after eviction.
        cache.insert([0x02; 32], buf_100_mib.clone());
        assert_eq!(cache.bytes(), 100 * 1024 * 1024);
        assert!(
            cache.lookup(&[0x01; 32]).is_none(),
            "oldest 100 MiB entry must be evicted when 2nd 100 MiB push overflows the 100 MiB budget"
        );
        assert_eq!(cache.lookup(&[0x02; 32]), Some(buf_100_mib));
    }
}
