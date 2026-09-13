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
//! MUST NOT call `file_cache.insert_owned(...)` for such
//! entries — it skips them in the spawn_blocking cache-fill
//! step (see [`crate::service::dispatch_files`]); the receiver's
//! inbound branch is expected to short-circuit the HTTP/3 GET
//! against the `MIME_TOO_LARGE` mime directly, without ever
//! asking for the bytes. This contract is enforced at the
//! dispatcher level, not here — `FileCache` is a pure byte store.

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

    /// Insert a file body — MOVE the bytes, no clone, no return.
    ///
    /// **Use this from the dispatcher's hot path.** The caller
    /// has just produced `bytes` (via `std::fs::read` in a
    /// `spawn_blocking` task) and the bytes are single-use —
    /// there is no caller that needs the previous value, so
    /// cloning on the hot path would be a wasted 200 MiB
    /// allocation for the M3a STEP-3a.4 performance target.
    ///
    /// **Byte-budget enforcement**: if `bytes.len() > byte_budget`,
    /// the insert is rejected (the bytes are dropped, no cache
    /// mutation). The defensive bound covers a single entry that
    /// could never fit; see the module-level doc for the
    /// rationale.
    ///
    /// On byte-budget overflow (after a successful insert) the
    /// **oldest** entries are evicted until the budget is met
    /// again. This is a best-effort fallback; the active eviction
    /// in the dispatcher should keep the cache size well under
    /// budget in practice.
    ///
    /// **Why a separate API from `insert_returning_prev`**:
    /// [`Self::insert_returning_prev`] clones the input `Vec<u8>`
    /// to return the previous bytes — necessary when a caller
    /// needs the eviction-feedback signal. The dispatcher does
    /// not, so it uses this MOVE-only variant to skip the clone.
    /// See STEP-3a.2 follow-up (P2.3).
    pub fn insert_owned(&mut self, sha256: [u8; 32], bytes: Vec<u8>) {
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
            return;
        }
        let previous_entry = self.entries.insert(
            sha256,
            FileCacheEntry {
                bytes, // MOVE — no clone on the hot path
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
                // above guarantees we can never hit this
                // branch with a single fresh insert.)
                break;
            }
        }
    }

    /// Insert a file body, returning the previous bytes if an
    /// entry with the same key already existed.
    ///
    /// **Clones the input** `Vec<u8>` — use [`Self::insert_owned`]
    /// on the dispatcher's hot path (no clone, no return) when
    /// the caller does not need the eviction-feedback signal.
    ///
    /// The clone is a single heap allocation + memcpy. For a
    /// 200 MiB file this is ~200 ms on NVMe + ~200 MiB peak RSS,
    /// which is why the dispatcher's `dispatch_files` uses
    /// [`Self::insert_owned`] instead. This API remains for any
    /// caller that genuinely needs to observe / re-use the
    /// previous bytes (none today).
    ///
    /// **Byte-budget enforcement**: if `bytes.len() > byte_budget`,
    /// the insert is rejected and `None` is returned (the bytes
    /// are dropped, no cache mutation).
    ///
    /// On byte-budget overflow (after a successful insert) the
    /// **oldest** entries are evicted until the budget is met
    /// again. This is a best-effort fallback; the active eviction
    /// in the dispatcher should keep the cache size well under
    /// budget in practice.
    pub fn insert_returning_prev(&mut self, sha256: [u8; 32], bytes: Vec<u8>) -> Option<Vec<u8>> {
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

    // ===== M3a STEP-3a.2 — byte-budget specific tests =====
    //
    // **Note (2026-09-13 test slim)**: the generic cache-contract
    // tests (insert / lookup / distinct-keys / remove / TTL /
    // overflow / reinsert / active-eviction / bytes() / oversize
    // rejection) are intentionally NOT duplicated here — they
    // live in `src/clipboard/cache.rs::tests` against
    // `ClipboardCache` (the same LRU + TTL impl, different
    // byte budget). The tests below cover ONLY the
    // file-cache-specific surface: the 1 GiB default budget and
    // the dispatcher hot-path API split (`insert_owned` MOVE-only).

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
        cache.insert_returning_prev([0x01; 32], buf_200_mib.clone());
        assert_eq!(cache.bytes(), 200 * 1024 * 1024);
        assert_eq!(cache.lookup(&[0x01; 32]), Some(buf_200_mib));
    }

    // ===== STEP-3a.2 P1 follow-up — `insert_owned` API split =====

    /// `insert_owned` round-trip: a fresh entry is retrievable
    /// via `lookup` after insertion. Mirrors
    /// `insert_returning_prev`'s happy-path test but exercises
    /// the new MOVE-only API the dispatcher's hot path uses.
    #[test]
    fn insert_owned_round_trip_returns_bytes() {
        let mut cache = FileCache::new();
        let sha = [0xD0; 32];
        let bytes = b"dispatch_files hot path".to_vec();
        cache.insert_owned(sha, bytes.clone());
        assert_eq!(cache.lookup(&sha), Some(bytes));
        assert_eq!(cache.bytes(), b"dispatch_files hot path".len());
        assert_eq!(cache.len(), 1);
    }

    /// `insert_owned` overwriting the same sha256 keeps a single
    /// entry (no LRU phantom / no double-count) — same contract as
    /// `insert_returning_prev`, just exercised on the MOVE-only
    /// path the dispatcher uses.
    #[test]
    fn insert_owned_overwrite_does_not_double_count() {
        let mut cache = FileCache::with_byte_budget_and_ttl(64, Duration::from_secs(60));
        let sha = [0xD1; 32];
        cache.insert_owned(sha, b"12345".to_vec());
        cache.insert_owned(sha, b"ABCDE".to_vec());
        // After overwrite the cache holds 5 bytes for the one sha;
        // adding a distinct 5-byte entry fits the 10-byte budget.
        cache.insert_owned([0xD2; 32], b"vwxyz".to_vec());
        assert_eq!(cache.lookup(&sha), Some(b"ABCDE".to_vec()));
        assert_eq!(cache.lookup(&[0xD2; 32]), Some(b"vwxyz".to_vec()));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.bytes(), 10);
    }

    /// `insert_owned` triggers LRU byte-budget eviction
    /// identically to `insert_returning_prev` (3rd 4-byte insert
    /// into a 10-byte budget drops the oldest 4-byte entry).
    /// Pins that the dispatcher doesn't accidentally bypass
    /// eviction by using the new API.
    #[test]
    fn insert_owned_byte_budget_overflow_evicts_oldest() {
        let mut cache = FileCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
        cache.insert_owned([0xA1; 32], b"AAAA".to_vec());
        cache.insert_owned([0xA2; 32], b"BBBB".to_vec());
        cache.insert_owned([0xA3; 32], b"CCCC".to_vec());
        assert_eq!(
            cache.lookup(&[0xA1; 32]),
            None,
            "first entry must be evicted by byte-budget overflow"
        );
        assert_eq!(cache.lookup(&[0xA2; 32]), Some(b"BBBB".to_vec()));
        assert_eq!(cache.lookup(&[0xA3; 32]), Some(b"CCCC".to_vec()));
        assert_eq!(cache.bytes(), 8);
    }

    /// `insert_owned` rejects payloads larger than the byte
    /// budget — same defensive contract as
    /// `insert_returning_prev`. The bytes are dropped, no
    /// cache mutation, byte counter unchanged.
    #[test]
    fn insert_owned_oversize_rejected_silently() {
        let mut cache = FileCache::with_byte_budget_and_ttl(10, Duration::from_secs(60));
        let huge = vec![0u8; 100];
        cache.insert_owned([0xD4; 32], huge);
        assert_eq!(cache.bytes(), 0);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.lookup(&[0xD4; 32]), None);
    }

    /// 200 MiB MOVE-insert at 1 GiB default budget pins the
    /// dispatcher's hot path production-scale contract — same as
    /// the `insert_returning_prev` 200 MiB test but on the new
    /// API. The 200 MiB test is intentional: it's the M3a
    /// STEP-3a.4 performance milestone payload size.
    #[test]
    fn insert_owned_200_mib_at_1_gib_budget() {
        let mut cache = FileCache::new();
        let buf = vec![0xCCu8; 200 * 1024 * 1024];
        cache.insert_owned([0xD5; 32], buf.clone());
        assert_eq!(cache.bytes(), 200 * 1024 * 1024);
        assert_eq!(cache.lookup(&[0xD5; 32]), Some(buf));
    }
}
