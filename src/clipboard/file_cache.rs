//! File-body byte cache (PLAN-2 / M3a STEP-3a.2).
//!
//! Stores the file bytes the local daemon has pushed via
//! `ClipboardFiles` so a remote peer can pull them with HTTP/3
//! `GET /clipboard/file/{sha256}` (wired in STEP-3a.4). Keyed by
//! SHA-256 (`[u8; 32]`).
//!
//! ## Differences from the existing `ClipboardCache` (M1b STEP-1b.2 + M2a STEP-2a.3)
//!
//! | Aspect              | `ClipboardCache` (text+image)   | `FileCache` (M3a)                  |
//! |---------------------|---------------------------------|--------------------------------------|
//! | Key | `[u8; 32]` (sha256) | `[u8; 32]` (sha256)                  |
//! | Value | `Vec<u8>` (text / image bytes) | `Vec<u8>` (file bytes)               |
//! | Capacity | 200 MiB byte budget | **1 GiB** byte budget                 |
//! | TTL | 5 min | 5 min (same)                          |
//! | Producer | text / image dispatch tick | file dispatch tick                   |
//! | Consumer | HTTP/3 server `/clipboard/{text,image}/{sha256}` | HTTP/3 server `/clipboard/file/{sha256}` (STEP-3a.4) |
//!
//! ## Why a separate cache type (not reuse `ClipboardCache`)
//!
//! `ClipboardCache` lives behind `Arc<Mutex<>>` clones shared
//! between the dispatcher (writer + active evictor) and the
//! per-peer HTTP/3 servers (readers on `/clipboard/text/` and
//! `/clipboard/image/`). File bodies ride a separate HTTP/3 route
//! (`/clipboard/file/`) — by M3a we want explicit byte-budget
//! isolation:
//!
//! - A 1 GiB file push **must not** evict cached text or images
//!   mid-session (would silently break text sync for the duration
//!   of the file transfer).
//! - The 200 MiB `ClipboardCache` budget is sized for screenshots
//!   + text; bumping it to 1 GiB would more than triple the
//!   baseline daemon RSS. 1 GiB is only justified by the file
//!   use-case.
//! - Two `Arc<Mutex<>>` instances are cheap — the heap overhead
//!   is a single `HashMap` + `VecDeque` each.
//!
//! The structural contract mirrors `ClipboardCache` exactly: byte
//! budget + TTL + active-eviction + lazy TTL eviction on read.
//! See that module's docstrings for the rationale.
//!
//! ## MIME_TOO_LARGE short-circuit (STEP-3a.1 contract)
//!
//! A `ClipboardFiles::entries` push where any entry has
//! `mime = "application/x-too-large"` is marked at the
//! source-side `collect_files` step (see
//! [`crate::clipboard::file_meta::FOUR_GIB`]). The dispatcher
//! MUST NOT call `file_cache.insert(...)` for such entries —
//! the receiver's inbound branch is expected to short-circuit
//! the HTTP/3 GET against the `MIME_TOO_LARGE` mime directly,
//! without ever asking for the bytes. This contract is enforced
//! at the dispatcher level (in [`crate::service::dispatch_files`]),
//! not here — `FileCache` is a pure byte store.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Total byte budget for cached file bodies.
///
/// **Why 1 GiB** (vs `ClipboardCache`'s 200 MiB): the file
/// use-case targets 200 MiB transfers as the M3a STEP-3a.4
/// performance milestone. A single 200 MiB push would consume
/// 100 % of the smaller 200 MiB text/image budget; 1 GiB
/// comfortably holds a 200 MiB file plus headroom for the
/// other clipboard bytes that may already be cached (text /
/// image). For comparison: a 1 GiB cache holds ~5× the M3a
/// performance target — enough for active transfers without
/// evicting recent text / image bytes (which are on a separate
/// cache anyway).
pub const FILE_CACHE_BYTE_BUDGET: usize = 1024 * 1024 * 1024;

/// Time-to-live for cached file payloads. Same 5-minute window
/// as [`crate::clipboard::cache::CLIPBOARD_CACHE_TTL`] — the
/// active-eviction contract (the dispatcher's "evict prev before
/// push" path) keeps the cache well under budget in practice;
/// the TTL is the fallback for "source silent > 5 min".
pub const FILE_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug)]
struct FileCacheEntry {
    bytes: Vec<u8>,
    /// Wall-clock-relative timestamp at `insert` time. Used by
    /// [`FileCache::lookup`] for the lazy TTL eviction. We use
    /// [`Instant`] (monotonic) rather than wall clock so a clock
    /// change (NTP correction, DST) cannot suddenly invalidate
    /// every cached entry.
    inserted_at: Instant,
}

/// Content-addressed file-body byte cache (sha256 → bytes).
///
/// Used by:
/// - the dispatcher (writer, active evictor) — every
///   `dispatch_files` call after STEP-3a.2's spawn_blocking
///   `collect_files_blocking` lands inserts here.
/// - the HTTP/3 server-side `/clipboard/file/{sha256}` handler
///   (reader) — wired in STEP-3a.4.
#[derive(Debug)]
pub struct FileCache {
    /// Maximum total bytes that may be stored at any moment.
    /// When `bytes_used > byte_budget` after a fresh insert the
    /// oldest entries are evicted until `bytes_used <= byte_budget`.
    byte_budget: usize,
    ttl: Duration,
    /// Backing store. `HashMap` for O(1) lookup by sha256; `lru`
    /// below provides LRU eviction order on overflow.
    entries: HashMap<[u8; 32], FileCacheEntry>,
    /// Insertion-order LRU list. The front is the **oldest** entry;
    /// the back is the **newest**. On byte-budget overflow we pop
    /// the front. `lookup` does **not** touch this list — TTL
    /// eviction only fires lazily on read.
    lru: VecDeque<[u8; 32]>,
    /// Sum of `entry.bytes.len()` for every entry currently in
    /// `entries`. Used by `insert` to enforce the byte budget
    /// without re-summing on every eviction pass.
    bytes_used: usize,
}

impl FileCache {
    /// Construct a cache with the default byte budget (1 GiB)
    /// and TTL (5 min).
    pub fn new() -> Self {
        Self::with_byte_budget_and_ttl(FILE_CACHE_BYTE_BUDGET, FILE_CACHE_TTL)
    }

    /// Construct a cache with custom byte budget / TTL. Used by
    /// tests that want a small byte budget (e.g. 10 bytes) to
    /// exercise eviction without allocating 1 GiB.
    pub fn with_byte_budget_and_ttl(byte_budget: usize, ttl: Duration) -> Self {
        Self {
            byte_budget,
            ttl,
            entries: HashMap::new(),
            lru: VecDeque::new(),
            bytes_used: 0,
        }
    }

    /// Insert a file body. Returns the previous value if an entry
    /// with the same key already existed.
    ///
    /// **Byte-budget enforcement**: if `bytes.len() > byte_budget`,
    /// the insert is rejected and `None` is returned (the bytes are
    /// dropped, no cache mutation). The defensive bound covers a
    /// single entry that could never fit; see the module-level doc
    /// for the rationale.
    ///
    /// On byte-budget overflow (after a successful insert) the
    /// **oldest** entries are evicted until the budget is met
    /// again. This is a best-effort fallback; the active eviction
    /// in the dispatcher should keep the cache size well under
    /// budget in practice.
    pub fn insert(&mut self, sha256: [u8; 32], bytes: Vec<u8>) -> Option<Vec<u8>> {
        let new_size = bytes.len();
        // Single-entry overflow: no way to fit, drop on the floor.
        // 1 GiB is well above any realistic file payload, so this
        // branch is defensive — a real `ClipboardFiles` from a
        // healthy peer never trips it.
        if new_size > self.byte_budget {
            log::warn!(
                "file cache: rejected entry of {new_size} bytes (budget {budget} bytes)",
                budget = self.byte_budget
            );
            return None;
        }
        let previous_entry = self.entries.insert(
            sha256,
            FileCacheEntry {
                bytes: bytes.clone(),
                inserted_at: Instant::now(),
            },
        );
        // Subtract the old size BEFORE adding the new size — if
        // this was an overwrite (same sha256), the net change is
        // the delta, not the sum.
        if let Some(prev) = &previous_entry {
            self.bytes_used -= prev.bytes.len();
        } else {
            // Only push onto the LRU deque if this is a fresh
            // insert. Re-inserting the same key would otherwise
            // create a phantom second entry in the LRU list,
            // which would let a stale entry outlive a
            // budget-evicting insert.
            self.lru.push_back(sha256);
        }
        self.bytes_used += new_size;
        // Budget eviction — best effort. Walk the LRU from the
        // front, dropping entries until we're back under budget.
        while self.bytes_used > self.byte_budget {
            if let Some(oldest) = self.lru.pop_front() {
                // The `oldest` might have been removed by an
                // explicit `remove()` between insert and this
                // point, so the `HashMap::remove` here is
                // `Option`-aware.
                if let Some(removed) = self.entries.remove(&oldest) {
                    self.bytes_used -= removed.bytes.len();
                }
            } else {
                // Defensive: HashMap and VecDeque should be in
                // sync, but if we ever drift we stop evicting
                // rather than spin. (Single-entry overflow check
                // above already guarantees we can never hit this
                // branch with a single fresh insert.)
                break;
            }
        }
        previous_entry.map(|e| e.bytes)
    }

    /// Look up a file body by sha256.
    ///
    /// Returns `Some(bytes)` on hit **and** within TTL. Returns
    /// `None` on miss, expired, or any other failure (no panic /
    /// error propagation — the HTTP/3 route maps `None` directly
    /// to 404 + `log warn`).
    ///
    /// Lazy TTL eviction: expired entries are removed from the
    /// map on lookup. This is the "5 min LRU 兜底" behaviour
    /// from the PLAN.
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

    /// Explicitly remove an entry. Returns `true` if the entry
    /// was present (useful for the dispatcher's "evict prev before
    /// push" path to log the eviction only when it actually
    /// happened).
    ///
    /// **Maintains the byte counter**: if the entry was present
    /// its `bytes.len()` is subtracted from `bytes_used`. The
    /// `lru` deque may still hold the removed sha256 — that's
    /// fine because `lookup` checks the HashMap first and never
    /// reads `lru`, and `insert`'s budget eviction path uses
    /// `entries.remove` which is a no-op for absent keys.
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
    /// diagnostics + tests; production code does not need to
    /// observe the cache size during normal operation (the byte
    /// budget is enforced transparently by `insert`).
    pub fn bytes(&self) -> usize {
        self.bytes_used
    }

    /// Configured byte budget. Exposed so callers can verify the
    /// production default (`1 GiB`) without inspecting constants —
    /// used by the cache tests below.
    pub fn byte_budget(&self) -> usize {
        self.byte_budget
    }

    /// Current entry count. Test-only helper — production code
    /// does not need to observe the cache size.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for FileCache {
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
        let mut cache = FileCache::new();
        let sha = [0xAA; 32];
        let bytes = b"hello world".to_vec();
        cache.insert(sha, bytes.clone());
        assert_eq!(cache.lookup(&sha), Some(bytes));
    }

    /// Lookup on an empty cache returns None.
    #[test]
    fn lookup_miss_on_empty_cache() {
        let mut cache = FileCache::new();
        let sha = [0xBB; 32];
        assert_eq!(cache.lookup(&sha), None);
    }

    /// Two distinct sha256 keys coexist (no cross-contamination).
    #[test]
    fn distinct_keys_dont_clobber_each_other() {
        let mut cache = FileCache::new();
        let sha_x = [0x01; 32];
        let sha_y = [0x02; 32];
        cache.insert(sha_x, b"X".to_vec());
        cache.insert(sha_y, b"Y".to_vec());
        assert_eq!(cache.lookup(&sha_x), Some(b"X".to_vec()));
        assert_eq!(cache.lookup(&sha_y), Some(b"Y".to_vec()));
    }

    /// Active eviction (the dispatcher's hot path): `remove(prev)`
    /// makes a subsequent `lookup(prev)` return None without
    /// touching other entries.
    #[test]
    fn remove_evicts_only_target_key() {
        let mut cache = FileCache::new();
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
    /// next `lookup`. Uses a 0-second TTL to avoid
    /// `tokio::time::sleep` in the test.
    #[test]
    fn expired_entries_are_evicted_on_lookup() {
        let mut cache = FileCache::with_byte_budget_and_ttl(1024, Duration::from_millis(0));
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

    /// Byte-budget overflow evicts the oldest entry (LRU from
    /// front). Budget = 10 bytes: the third 4-byte insert (total
    /// = 12 bytes > 10) pushes the oldest 4-byte entry out.
    #[test]
    fn byte_budget_overflow_evicts_oldest() {
        // 10-byte budget: 3 × 4-byte inserts. The first two fit
        // (8 bytes total); the third would push total to 12 > 10,
        // triggering eviction of the oldest 4-byte entry.
        let mut cache = FileCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
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

    /// Re-inserting the same key does not double-count against
    /// the byte budget (overwriting subtracts the old size before
    /// adding the new one).
    #[test]
    fn reinsert_same_key_does_not_double_count_bytes() {
        // 10-byte budget: re-insert the same key twice with sizes
        // 5 + 5 = 10 bytes; budget is not exceeded.
        let mut cache = FileCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
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

    /// Active-eviction contract: the dispatcher's
    /// `remove(prev) → insert(new)` pair must result in
    /// `lookup(prev) == None` and `lookup(new) == Some(new_bytes)`
    /// simultaneously — pins the "old X has 5 min TTL, new Y
    /// pushed, receiver pulls X within 5 min still gets old X"
    /// race fix (PLAN §1 评审 #3 2nd, mirrored here for files).
    #[test]
    fn active_eviction_concurrent_with_lookup_old_returns_miss() {
        let mut cache = FileCache::with_byte_budget_and_ttl(1024, Duration::from_secs(60));
        let sha_x = [0x33; 32];
        let sha_y = [0x44; 32];
        cache.insert(sha_x, b"X contents (large payload)".to_vec());

        // Active eviction path: source pushes new content.
        assert!(
            cache.remove(&sha_x),
            "X must have been present before eviction"
        );
        cache.insert(sha_y, b"Y contents (different large payload)".to_vec());

        // A receiver that started pulling X *before* the new
        // push would have raced with the eviction. From now on
        // (the new push already evicted X), X must be a miss.
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

    // ===== M3a STEP-3a.2 — byte-budget specific tests =====

    /// Default byte budget is **1 GiB** (PLAN §3 M3a STEP-3a.2
    /// "1 GiB LRU"). Verified by construction + `byte_budget()`
    /// getter so a future refactor that bumps the budget
    /// accidentally is caught.
    #[test]
    fn default_byte_budget_is_1_gib() {
        let cache = FileCache::new();
        assert_eq!(
            cache.byte_budget(),
            1024 * 1024 * 1024,
            "default byte budget must be 1 GiB"
        );
        assert_eq!(
            FILE_CACHE_BYTE_BUDGET,
            1024 * 1024 * 1024,
            "FILE_CACHE_BYTE_BUDGET constant must equal 1 GiB"
        );
    }

    /// `bytes()` API returns the total byte count after inserts /
    /// removes / lazy TTL eviction.
    #[test]
    fn bytes_returns_total_byte_count() {
        let mut cache = FileCache::with_byte_budget_and_ttl(1024, Duration::from_secs(60));
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
    /// does not mutate the cache, and the byte counter is
    /// unchanged.
    #[test]
    fn insert_larger_than_budget_is_rejected() {
        let mut cache = FileCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
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

    /// 1 GiB-cap sanity check at production scale: a 200 MiB
    /// insert fits comfortably under the default 1 GiB budget
    /// and is retrievable. Mirrors the dispatcher's real-world
    /// "push 200 MiB file" path (PLAN §3 M3a STEP-3a.4
    /// performance milestone). Uses a sparse buffer
    /// (`vec![0xAA; 200 MiB]`) — the test does not validate
    /// sha256 here, only the cache contract.
    #[test]
    fn default_1_gib_cache_holds_200_mib_insert() {
        let mut cache = FileCache::new();
        let buf_200_mib = vec![0xAAu8; 200 * 1024 * 1024];
        cache.insert([0x01; 32], buf_200_mib.clone());
        assert_eq!(cache.bytes(), 200 * 1024 * 1024);
        assert_eq!(cache.lookup(&[0x01; 32]), Some(buf_200_mib));
    }
}
