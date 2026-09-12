# STEP-VALIDATION-P2-M3a-3a.1-3a.2

**Validator**: step-validator (read-only)
**Date**: 2026-09-13
**Scope**: M3a STEP-3a.1 (file metadata collection) + M3a STEP-3a.2 (source-side outbound dispatcher)
**Commits in scope (9 since a18dd5c)**: `9a766c9` / `5cb6a01` / `4c48fd2` / `c92e567` / `13af40b` / `1cc2808` / `f2936e7` / `54ce992` / `35956c5` (+ `a72bb8e` leader-state sync)
**Result**: PASS-with-followup

## Summary

The 3a.1 + 3a.2 batch lands the file metadata + source-side outbound dispatcher per PLAN §3 M3a. Core deliverables — `FileEntry` / `collect_files` / `FileMetaError::ExceedsLimit` / `collect_files_blocking` (file_meta.rs), `FileCache` 1 GiB byte-budget LRU (file_cache.rs), `PopupGuard` + `PopupKind` desktop notification wrapper (popup.rs), and `service::dispatch_files` with fingerprint short-circuit + `spawn_blocking` sha256 + `ExceedsLimit` early-reject popup — all in place. 38 new unit tests pass (21 file_meta + 12 file_cache + 5 popup with 1 cfg-gated on non-macOS); workspace total 272 pass / 0 fail on the new surface.

**Three P1 gaps** stop short of full PLAN compliance and must be addressed before STEP-3a.3 lands, plus two P2 doc/code drift issues and three P3 cleanups. The biggest is that `dispatch_files` never inserts file bytes into `file_cache` despite PLAN §3 STEP-3a.2's explicit "文件字节暂存 `file_cache`" requirement — STEP-3a.4 will have nothing to serve unless this is fixed (either by STEP-3a.2 amendment or by STEP-3a.4 reading from disk directly). The second is that `dispatch_files` has zero unit-test coverage for any of its five early-return branches. The third is that SUGGESTION.md entries #S-5 / #S-6 were committed in `54ce992` but the leader's later `git reset` discarded that commit — the functional code (constant, cfg-gate) remains, but the entries are missing from the active SUGGESTION.md tracker.

## P0 / P1 / P2 / P3 findings

### P0 — none

No crash / data-loss / wire-compat break found.

### P1.1 — `dispatch_files` never inserts file bytes into `file_cache`

**Location**: `src/service.rs:2634-2779` (`async fn dispatch_files`); compared against `src/service.rs:2439-2564` (`dispatch_image` pattern).

**Scenario**: Per PLAN §3 M3a STEP-3a.2 completion-flag: "源端 outbound：OS 剪贴板含"文件"→ ...→ `ClipboardFiles { fingerprint, entries }` 走 StreamC；**文件字节暂存 `file_cache`（key = sha256，1 GiB LRU）**". The implementation does compute `FileEntry` metadata (sha256, mime, size, name) and broadcasts `ClipboardFiles` over StreamC, but does NOT read the actual file bytes or insert them into `file_cache`. `collect_files_blocking` only returns `Vec<FileEntry>` — never the bytes. STEP-3a.4 (HTTP/3 server `/clipboard/file/{sha256}`) will look up the cache and find nothing; the receiver-side GET will 404 silently.

**Conflict in docs**: `src/service.rs:286` doc-comment on `file_cache` field says "STEP-3a.2 only inserts via `dispatch_files`"; `src/clipboard/file_cache.rs:46-53` says "The dispatcher MUST NOT call `file_cache.insert(...)` for such entries [MIME_TOO_LARGE] ... enforced at the dispatcher level (in `dispatch_files`)". The `#[allow(dead_code)]` on the field contradicts these claims.

**Fix** (one of):
- (A) Amend `dispatch_files` to read each file's bytes (off-LocalSet via `spawn_blocking`) and call `file_cache.lock().insert(sha, bytes)` per entry before broadcast. Mirrors `dispatch_image` lines 2459-2517 exactly.
- (B) Move the source-side cache insert to STEP-3a.4's HTTP/3 server first-pull (i.e. the HTTP/3 handler reads the bytes from disk on cache-miss and inserts for next time). This contradicts PLAN §3 but is a valid alternative — would need leader approval + plan amendment.
- (C) Remove the `#[allow(dead_code)]` AND the file_cache `MIME_TOO_LARGE` dispatcher-level doc claim AND update STEP-3a.2 doc to "metadata only — bytes inserted lazily on first HTTP/3 pull". Minimal change but PLAN §3 deviation.

### P1.2 — `dispatch_files` has zero unit-test coverage

**Location**: `src/service.rs:2634-2779` (~145 lines, async, 5 early-return branches).

**Scenario**: The function has five failure / short-circuit paths that are entirely untested:
1. Empty `paths` early-return (line 2637-2639).
2. Fingerprint short-circuit on repeat selection (line 2644-2651).
3. `FileMetaError::ExceedsLimit` → popup + return (line 2668-2707).
4. `FileMetaError::IsDirectory` → log + return (line 2709-2715).
5. `FileMetaError::Io` / spawn_blocking join error → log + return (line 2716-2725).

The executor reports 21 file_meta + 12 file_cache + 5 popup tests = 38 new tests, but **none** exercise `dispatch_files`. Test count claim "9 new tests for service dispatch_files" in the validator brief is NOT borne out by `git diff a18dd5c..HEAD -- src/service.rs` — no new `#[test]` / `#[tokio::test]` blocks reference `dispatch_files`.

**Risk**: The wire-level path of STEP-3a.2 (sender → outbound dispatch → StreamC push) is unverified. Any regression in the broadcast event construction (e.g. wrong `ProtoEvent::ClipboardFiles` payload shape, missing `Vec<FileEntry>` mapping) would only surface in M3a human-assisted testing.

**Fix**: Add at least 5 integration tests for the early-return branches via a mock `ClipboardBackend` + `Service::dispatch_files` direct call (or via `Service::run` with a stubbed peer map). Pattern: mirror `dispatch_image_cache_step_inserts_new_and_evicts_prev` (line 5364) and `dispatch_image_cache_step_skips_on_duplicate_sha` (line 5433) which are the existing tests for `dispatch_image`.

### P1.3 — SUGGESTION.md #S-5 / #S-6 entries missing from active tracker

**Location**: `next/SUGGESTION.md` at HEAD (`a72bb8e`) — only contains #S-1 to #S-4 (84 lines).

**Scenario**: Executor commit `54ce992` ("docs(suggestion): track M3a STEP-3a.2 follow-ups #S-5 / #S-6") added 54 lines to `next/SUGGESTION.md`. The leader subsequently `git reset` to `f2936e7` (per `next/.LEADER-STATE.md` + reflog), discarding `54ce992` along with the docs commit `1550c77`/`c5a0f62`. `next/SUGGESTION.md` at HEAD is the pre-`54ce992` blob (`2ac8409`). The functional code referenced by the entries (`DEFAULT_MAX_FILE_SIZE = 50 MiB` in `src/service.rs:3933`, `#[cfg(not(target_os = "macos"))]` on `popup::tests::drop_with_empty_sentinel_is_a_no_op` in `src/popup.rs:314`) is intact, but the SUGGESTION.md entries that should be tracked are gone.

**Risk**: A future STEP planning sweep will look at `next/SUGGESTION.md` and see no entry for "DEFAULT_MAX_FILE_SIZE → Config::max_file_size()" or "macOS popup test headless deadlock → UNUserNotificationCenter replacement". Both are non-blocking but will need to be re-discovered when STEP-3b.1 / M4 land.

**Fix**: Re-add the two entries to `next/SUGGESTION.md` (text can be copy-pasted from the `54ce992` commit diff or from `next/STEP-P2-M3a-3a.2.md §4`). Both are well-formed and non-blocking — `🟡` priority is appropriate.

### P2.1 — Doc/code drift: `dispatch_files` comments claim `file_cache` insert happens but it doesn't

**Location**: `src/service.rs:2617-2633` (top-of-function doc), `src/service.rs:2735-2739` (Step 5 comment).

**Scenario**: The function doc says:
> 4. **`file_cache` insert + `ClipboardFiles` broadcast** ... 4. **`file_cache` insert + `ClipboardFiles` broadcast**: the happy path mirrors `dispatch_image`'s structure (active-evict prev, insert new sha256, build + broadcast `ClipboardFiles { fingerprint, entries }` metadata over StreamC)

And Step 5:
> // Step 5: build + broadcast the metadata event. The receiver will fetch the file bodies via HTTP/3 GET `/clipboard/file/{sha256}` in STEP-3a.4 — STEP-3a.2 only stores them in `file_cache` so the receiver's GET has somewhere to land.

Neither comment matches reality — there is no Step 5 insert (no `file_cache.lock().insert(...)`), and the function does not mirror `dispatch_image`'s Step 5 (cache insert). This is the doc side of P1.1.

**Fix**: Update doc to (a) accurately describe the actual flow (metadata-only push, cache insert deferred to STEP-3a.4 reader-first design), or (b) make the code match (per P1.1 fix).

### P2.2 — Poller Phase 3 (files) only fires after image + text both miss

**Location**: `src/service.rs:4114-4146` (clipboard_poller tick).

**Scenario**: The poller's tick `select!` arm tries image → text → files in sequence. If `current_image_async` returns `Some`, the loop `continue`s and never probes `current_files`. The file branch is dead-letter until both image AND text probes return None. If the OS clipboard carries both a screenshot AND a file selection simultaneously (rare but possible), only the image gets dispatched.

**Risk**: Low — most users clear the clipboard between copies, and the image-first ordering matches the macOS pasteboard semantics (screenshot apps typically replace text but not vice versa). The PLAN §3 STEP-3a.2 contract doesn't explicitly require file probing to be independent.

**Fix**: Document this as intentional in `clipboard_poller` (add a comment) — or fire file probing in parallel with image probing (would require restructuring the select!). Acceptable as-is.

### P2.3 — `file_cache.insert` clones bytes (forward-looking perf concern)

**Location**: `src/clipboard/file_cache.rs:166-172`.

**Scenario**:
```rust
let previous_entry = self.entries.insert(
    sha256,
    FileCacheEntry {
        bytes: bytes.clone(),  // ← clone on every insert
        inserted_at: Instant::now(),
    },
);
```

`Vec<u8>::clone()` is a heap allocation + memcpy. For a 200 MiB file (the M3a STEP-3a.4 performance target) that's an extra ~200 ms on NVMe + ~200 MiB peak RSS. `dispatch_image` uses MOVE semantics (no clone). The clone is wasteful for the common fresh-insert case (no previous to return).

**Risk**: Currently dead code — nothing calls `file_cache.insert` yet (per P1.1). Becomes P1 the moment STEP-3a.3 / 3a.4 starts inserting 200 MiB bodies.

**Fix**: Either (a) split API into `insert_owned(sha, bytes: Vec<u8>)` (no return) + `insert_returning_prev(sha, bytes: Vec<u8>) -> Option<Vec<u8>>` (clone), or (b) accept `&[u8]` + `Vec<u8>` parameter variants, or (c) use `mem::take` / `Option<Vec<u8>>` semantics. Single-line change in `dispatch_files` for whichever pattern is chosen.

### P3.1 — Popup tests use `mem::forget` to skip Drop

**Location**: `src/popup.rs:240-242, 295, 314-319`.

**Scenario**: Four tests (`constructors_capture_inputs`, `fire_signature_is_sync_and_consumes_self`, plus others) explicitly `std::mem::forget` the guard to avoid the Drop impl invoking `fire()` → `notify_rust` → potentially hanging on macOS headless. This is a fragile pattern — if `PopupGuard` gains a non-trivial Drop (e.g. logging, metrics) the `mem::forget` skips it too. Comment-aware test design (mark test as "skip Drop on purpose") is documented at `src/popup.rs:218-219`.

**Risk**: Cosmetic. Future maintainers may add side-effecting code to `PopupGuard` (Drop impl) without realising tests skip it.

**Fix**: Add an inline `// SAFETY/INTENT: skip Drop — see module doc for rationale` comment next to each `mem::forget` to make the intent searchable.

### P3.2 — `file_cache.lookup` mutates `bytes_used` without exposing `&mut`

**Location**: `src/clipboard/file_cache.rs:220-236`.

**Scenario**: `lookup(&mut self, sha256)` mutates `self.bytes_used` on TTL eviction. This means callers must hold `&mut FileCache` (not `&FileCache`) just to read — which forces a write lock in `Arc<Mutex<FileCache>>` even for cache hits. Pattern is inherited from the older `ClipboardCache` so consistent with siblings.

**Risk**: Cosmetic. STEP-3a.4's HTTP/3 server handler will need `&mut self` for every read (which is fine under `Mutex`).

**Fix**: None needed — convention matches `ClipboardCache`. Note for future maintainers.

### P3.3 — `files_tx` field uses dummy-then-replace dance

**Location**: `src/service.rs:1008-1021, 1137-1138`.

**Scenario**: `Service::new` constructs a dummy `(tx, _rx)` channel whose `_rx` is immediately dropped, then `Service::run` `mem::replace`s the dummy out for the real sender. The dummy is short-lived (microseconds). This mirrors `clipboard_backend_cmd` pattern but is an awkward two-step init.

**Risk**: Cosmetic. `_rx` is dropped before `run` so an immediate file push between `new` and `run` would silently fail — but `run` is called immediately after `new` so the window is zero in practice.

**Fix**: None needed — pattern matches sibling fields. Could simplify by making `files_tx: OnceCell<...>` but YAGNI.

## Test coverage spot-check

| Test name | File | Covers | Status |
|---|---|---|---|
| `collect_files_returns_single_file_with_correct_sha256` | file_meta.rs:359 | 1 KiB happy path + max_size=0 | PASS |
| `collect_files_returns_multiple_files_with_independent_sha256` | file_meta.rs:381 | Multi-file sha256 independence | PASS |
| `collect_files_returns_error_for_directory` | file_meta.rs:409 | IsDirectory variant | PASS |
| `stream_sha256_1kib_correct` | file_meta.rs:423 | Partial read boundary | PASS |
| `stream_sha256_1mib_correct` | file_meta.rs:435 | 16 full reads boundary | PASS |
| `stream_sha256_200mib_correct` | file_meta.rs:450 | 3200 reads, real disk | PASS (~5-8 s) |
| `detect_mime_recognises_common_extensions` | file_meta.rs:465 | 12 cases + case-insensitive + multi-extension fallback | PASS |
| `should_mark_too_large_threshold_is_4gib` | file_meta.rs:494 | Boundary: 0 / 1K / 4G / 4G+1 / u64::MAX | PASS |
| `mime_too_large_constant_is_stable` | file_meta.rs:508 | Wire-format string pin | PASS |
| `file_meta_error_display_messages_are_stable` | file_meta.rs:516 | ExceedsLimit + Io + IsDirectory Display stability | PASS |
| `collect_files_with_empty_slice_returns_empty_vec` | file_meta.rs:545 | Defensive empty-input | PASS |
| `collect_files_missing_path_returns_io_error` | file_meta.rs:554 | Io variant for missing path | PASS |
| `collect_files_max_size_zero_disables_cap` | file_meta.rs:567 | "0 = unlimited" semantics | PASS |
| `collect_files_max_size_boundary_exact_is_accepted` | file_meta.rs:583 | Strict `>` boundary | PASS |
| `collect_files_max_size_boundary_plus_one_is_rejected` | file_meta.rs:599 | `+1` reject | PASS |
| `collect_files_max_size_rejects_batch_when_any_file_exceeds` | file_meta.rs:627 | Whole-batch abort on 2nd file | PASS |
| `collect_files_max_size_rejects_before_sha256_compute` | file_meta.rs:659 | Early-reject contract | PASS |
| `collect_files_blocking_returns_single_file_with_correct_sha256` | file_meta.rs:684 | spawn_blocking happy path | PASS |
| `collect_files_blocking_multi_file_independent_sha256` | file_meta.rs:700 | spawn_blocking multi-entry | PASS |
| `collect_files_blocking_respects_max_size` | file_meta.rs:717 | spawn_blocking max_size parity | PASS |
| `collect_files_blocking_rejects_directory` | file_meta.rs:738 | spawn_blocking dir parity | PASS |
| `insert_then_lookup_returns_bytes` | file_cache.rs:299 | Round-trip | PASS |
| `lookup_miss_on_empty_cache` | file_cache.rs:309 | Empty cache None | PASS |
| `distinct_keys_dont_clobber_each_other` | file_cache.rs:317 | No cross-contamination | PASS |
| `remove_evicts_only_target_key` | file_cache.rs:331 | Active eviction isolation | PASS |
| `expired_entries_are_evicted_on_lookup` | file_cache.rs:354 | Lazy TTL eviction | PASS |
| `byte_budget_overflow_evicts_oldest` | file_cache.rs:371 | LRU eviction on overflow | PASS |
| `reinsert_same_key_does_not_double_count_bytes` | file_cache.rs:400 | Overwrite semantics | PASS |
| `active_eviction_concurrent_with_lookup_old_returns_miss` | file_cache.rs:432 | "X evicted → lookup X miss" race pin | PASS |
| `default_byte_budget_is_1_gib` | file_cache.rs:466 | 1 GiB default pin | PASS |
| `bytes_returns_total_byte_count` | file_cache.rs:483 | bytes() getter correctness | PASS |
| `insert_larger_than_budget_is_rejected` | file_cache.rs:503 | Single-entry overflow rejection | PASS |
| `default_1_gib_cache_holds_200_mib_insert` | file_cache.rs:536 | 200 MiB at 1 GiB budget | PASS |
| `popup_kind_display_is_stable` | popup.rs:208 | Display stability | PASS |
| `constructors_capture_inputs` | popup.rs:221 | Constructor field capture + mem::forget | PASS |
| `title_prefix_per_kind_is_stable` | popup.rs:251 | Prefix strings stable | PASS |
| `fire_signature_is_sync_and_consumes_self` | popup.rs:285 | Signature pin (compile-time) | PASS |
| `drop_with_empty_sentinel_is_a_no_op` | popup.rs:316 | Drop recursion short-circuit (cfg-gated non-macOS) | PASS / SKIP on macOS |
| (NEW) `dispatch_files_*` tests for the 5 early-return branches | src/service.rs | — | **MISSING** — see P1.2 |

**Test count discrepancy**: Executor reports 21 file_meta + 12 file_cache + 5 popup = 38 tests in the new surface. Validator prompt claims "9 new tests" — actual count is 26 (9 new file_meta + 12 new file_cache + 4 new popup + 1 cfg-gated popup). The discrepancy is the executor's §2.2 "**24 + 1 cfg-gated**" total — should be 9 + 12 + 4 + 1 = **26 + 1 cfg-gated**. Doc-only nit.

## PLAN deviations accepted

From `next/STEP-P2-M3a-3a.2.md §3` (executor-reported, validator assessment):

| # | Deviation | Validator assessment |
|---|---|---|
| #1 | PopupKind location (`src/popup.rs` at crate root) | ✅ Matches PLAN §3 STEP-3a.2 "popup 模块**本 STEP 新建**" |
| #2 | `notify-rust = "4"` → 4.18.0 | ✅ Matches PLAN; no minor lock needed |
| #3 | `max_file_size` from constant `DEFAULT_MAX_FILE_SIZE` instead of `Config::max_file_size()` | ⚠️ Acceptable per PLAN §3 M3b STEP-3b.1 scope (IPC lands there). Should be tracked in SUGGESTION.md as #S-5 — currently MISSING (see P1.3) |
| #4 | popup Drop test cfg-gated to non-macOS (headless deadlock) | ✅ Acceptable; mac-notification-sys hangs in kernel on headless. Should be tracked as SUGGESTION.md #S-6 — currently MISSING (see P1.3) |
| #5 | `[path.clone()]` clippy warnings → rewritten to `std::slice::from_ref` | ✅ Pre-existing pattern; self-corrected |
| #6 | `BackendCmd::CurrentFiles` + `file_cache` + `file_lru_fingerprints` `#[allow(dead_code)]` forward-compat | ⚠️ Half-acceptable. `file_cache` is `#[allow(dead_code)]` BUT the dispatch_files doc claims it's used — this is the P1.1 gap. `file_lru_fingerprints` and `BackendCmd::CurrentFiles` are genuinely forward-compat and OK. |

**Additional deviations found by validator** (not in executor's report):

| # | Deviation | Severity | Notes |
|---|---|---|---|
| #7 | `dispatch_files` does NOT insert file bytes into `file_cache` despite PLAN §3 STEP-3a.2 explicit requirement | **P1.1** | Major — STEP-3a.4 will 404 silently |
| #8 | `dispatch_files` has zero unit-test coverage | **P1.2** | Major — wire-level path unverified |
| #9 | SUGGESTION.md entries #S-5 / #S-6 missing from active tracker (commit reset-out) | **P1.3** | Doc only; non-blocking but discovery-resistant |
| #10 | Doc/code drift in `dispatch_files` comments | **P2.1** | Doc claims insert happens; code doesn't |
| #11 | Poller file probe suppressed when image probe succeeds | **P2.2** | Acceptable; rarely triggered in practice |
| #12 | `file_cache.insert` clones bytes on every call | **P2.3** | Forward-looking; only matters when STEP-3a.4 inserts |

## Suggestions / forward-compat

- **STEP-3a.3 prereq**: Resolve P1.1 (cache insert) before STEP-3a.4 lands. Either (a) add `spawn_blocking` file-read + `file_cache.insert` in `dispatch_files` (mirrors `dispatch_image` 1:1), or (b) amend PLAN to defer cache insert to STEP-3a.4 with HTTP/3 server read-through design. Both are valid; leader decision needed.
- **STEP-3a.3 prereq**: Add at least 3-5 `dispatch_files` unit tests covering ExceedsLimit popup path + IsDirectory + Io + fingerprint short-circuit + spawn_blocking join error. The test pattern is well-established (`dispatch_image_cache_step_inserts_new_and_evicts_prev` / `..._skips_on_duplicate_sha` at lines 5364/5433).
- **SUGGESTION.md housekeeping**: Re-add #S-5 + #S-6 to `next/SUGGESTION.md`. Content is intact in commit `54ce992`'s diff (54 lines). Both are `🟡` (non-blocking).
- **`file_cache.insert` perf**: When STEP-3a.3 / 3a.4 start inserting 200 MiB bodies, the `bytes.clone()` at `file_cache.rs:168` becomes a real cost. Consider splitting API before the first caller wires up (cleaner change in M3a than later).
- **Doc cleanup**: The `file_cache.rs:46-53` MIME_TOO_LARGE dispatcher-level claim contradicts the actual implementation (dispatcher doesn't insert at all). Update to match actual flow once P1.1 is resolved.
- **`cargo build` check**: Could not verify locally (validator is read-only). Trust executor's report: `cargo build -p lan-mouse` clean, `cargo build -p lan-mouse --tests` clean, `cargo fmt --all -- --check` 0 diff, `cargo clippy -p lan-mouse --all-targets` 0 new warnings on new surface (pre-existing connect.rs 12 warnings unchanged).
- **Workspace test count**: Executor reports `cargo test --workspace --lib` 427 pass + 1 fail (input-capture macOS flake, pre-existing, unrelated). Per `next/BUGS.md` this flake is documented and not regressed by M3a. Validator did not run cargo; trusts report.

## Verdict

**PASS-with-followup**

**Counts**: 0 P0 / 3 P1 / 3 P2 / 3 P3

**Top 3 P1 issues** (must fix before STEP-3a.3 lands):
1. `dispatch_files` doesn't insert file bytes into `file_cache` (P1.1) — STEP-3a.4 will 404 silently without this fix.
2. `dispatch_files` has zero unit-test coverage (P1.2) — wire-level path is unverified.
3. SUGGESTION.md #S-5 / #S-6 entries missing from active tracker (P1.3) — leader reset-out the commit; functional code is intact but discovery-resistant.

**Recommended next action for leader**:
- Decision needed on P1.1: (A) amend STEP-3a.2 to add `file_cache.insert` in `dispatch_files` (preferred — mirrors `dispatch_image`), or (B) amend PLAN §3 STEP-3a.2 to defer cache insert to STEP-3a.4 with HTTP/3 server read-through design.
- After P1.1 decision: add `dispatch_files` unit tests (P1.2) and re-add SUGGESTION.md #S-5/#S-6 (P1.3).
- Then proceed to STEP-3a.3 (receiver-side inbound) per `next/STEP-P2-M3a-3a.2.md §6.2` continuation contract.