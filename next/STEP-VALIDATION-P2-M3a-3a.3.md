# STEP-VALIDATION-P2-M3a-3a.3

**Validator**: step-validator (read-only)
**Date**: 2026-09-13
**Scope**: M3a P1 fix (5 commits: `be34c7c` / `af0e685` / `bb849a6` / `81983a9` / `64ef0ad`) + STEP-3a.3 (2 commits: `36b912b` / `21c729e`) = **7 commits since `1d4e014`**
**Result**: PASS-with-followup

---

## Summary

The batch fully resolves all three P1 gaps from the previous validator report (P1.1 / P1.2 / P1.3) and lands STEP-3a.3 (receiver-side inbound file handling) per PLAN §3 M3a STEP-3a.3 contract. **P1.1 (dispatch_files cache insert)** is resolved cleanly via a second `spawn_blocking` + `FileCache::insert_owned` MOVE-only API that mirrors the `dispatch_image` cache pattern. **P1.2 (5-branch test coverage)** is resolved via the `dispatch_files_decide` helper-extraction pattern — all 5 early-return branches now have dedicated `tokio::test` cases. **P1.3 (SUGGESTION.md #S-5/#S-6 restoration)** is resolved; both entries are present at HEAD with the same content as the original `54ce992` commit. STEP-3a.3 lands the receiver-side wiring with 21 new tests in `service.rs` plus 5 new tests in `file_cache.rs`, totaling 26 new tests (matches executor's claim). Architecture follows the established post-M2 BUGFIX pattern: `spawn_local` for the per-entry task (avoids blocking the main `Service::run` select!) + `spawn_blocking` for the disk write (avoids blocking the LocalSet). SHA256 recomputation from in-memory bytes (not disk re-read) is a justified deviation — QUIC integrity + local-disk trust + 5-8 s saved on 200 MiB.

**One P1 wire-level concern** surfaces around path traversal safety in `resolve_unique_path` — see Finding P1.A below. This is a real but bounded risk that can be addressed cheaply in a follow-up. **No P0 issues**.

---

## P0 / P1 / P2 / P3 findings

### P0 — none

No crash, data-loss, or wire-compat break. SHA256 mismatch correctly deletes partial file (test pin `apply_inbound_files_task_sha256_mismatch_deletes_partial`). Fetcher closure indirection preserves testability without leaking a panic.

### P1.A — `resolve_unique_path` allows path traversal if peer sends `../` in `name`

**Location**: `src/service.rs:5111-5145` (`resolve_unique_path`)

**Scenario**: `accept_dir.join(name)` does NOT canonicalize or strip `..` segments in Rust — `accept_dir.join("../escape.txt")` evaluates to `<parent_of_accept_dir>/escape.txt`, escaping the user's configured receive directory. The platform backends produce sanitized basenames (macOS NSFilenamesPboardType, Windows CF_HDROP, Linux URI-list — *depending on how current_files() strips the URI prefix*), but the wire is not authenticated against the OS pasteboard — a malicious peer could send any `name` string.

**Concrete failure**: user has `accept_dir = "/Users/me/lan-mouse"`. Peer pushes `ClipboardFiles { entries: [{ name: "../Documents/private.txt", ... }] }`. Receiver spawns `apply_inbound_files_task`, fetches bytes, lands the file at `/Users/me/Documents/private.txt` — **outside** `accept_dir`. Even with collision suffix, `<stem> (1).<ext>` of `../Documents/private.txt` is `../Documents/private (1).txt` — still outside.

**Risk surface**:
- (a) Within LAN trust model, peers are pre-paired via certificate fingerprint whitelist (PLAN §0) — peer-level auth is strong; "malicious" is bounded to compromised devices.
- (b) Within a single user, multiple app sources may produce pathological names (Linux `.desktop` files, archives with `..` in name from FTP).
- (c) Defense-in-depth: should `accept_dir` be a chrooted subdir regardless of input? The `file_lru_fingerprints` and metrics also assume a flat dir.

**Fix (small, can ship in STEP-3a.4 or earlier)**:
```rust
pub(crate) fn resolve_unique_path(accept_dir: &Path, name: &str) -> PathBuf {
    // Reject any name that escapes accept_dir.
    let candidate = accept_dir.join(name);
    if !candidate.starts_with(accept_dir) {
        // Substitute safe name instead.
        return resolve_unique_path(accept_dir, &sanitize_name(name));
    }
    // ... rest of algorithm
}
```

Or (simpler): strip `..` and `/` from `name` before joining:
```rust
let safe_name = name.replace("..", "").replace(['/', '\\'], "_");
```

**Severity rationale**: This is a P1 (race/scope violation — file lands outside configured dir), not P0 (no crash/data-loss for the intended use case). Worth flagging but not blocking STEP-3a.4 progress.

### P1.B (formerly P1.1) — RESOLVED — `dispatch_files` now inserts file bytes into `file_cache`

**Location**: `src/service.rs:2810-2845` (second `spawn_blocking` in `dispatch_files`).

**Verification**:
- Reads each file's bytes via `std::fs::read(path)` inside `spawn_blocking` — off LocalSet ✓
- Calls `FileCache::insert_owned(sha, bytes)` — MOVE-only API, no clone ✓
- MIME_TOO_LARGE entries skipped via `if entry.mime == MIME_TOO_LARGE { continue }` ✓
- Bytes flow: file on disk → spawn_blocking → Vec<u8> → file_cache.lock().insert_owned(sha, bytes) → return ✓
- `#[allow(dead_code)]` removed from `file_cache` field (commit `af0e685`, comment updated) ✓
- `insert_owned` API (commit `be34c7c`) separates MOVE-only from `insert_returning_prev` (which clones) ✓

**Race condition check**: dispatch_files is invoked from `Service::run` select! on `files_rx.recv()`. The send happens once per tick (max 1 msg / 500 ms). The receiver is the dispatch_files future itself, so no concurrent calls. The `spawn_blocking` closure holds a lock on the `Arc<Mutex<FileCache>>` for the duration of the byte-read loop — readers from HTTP/3 server (when STEP-3a.4 lands) will queue behind this. For 200 MiB on NVMe (~1-2 s), this is acceptable; readers will get served after the bulk insert completes. The HTTP/3 server (STEP-3a.4) will use `lookup` which does TTL-eviction on each call, so the lookup itself is short.

### P1.C (formerly P1.2) — RESOLVED — `dispatch_files` 5-branch test coverage

**Location**: `src/service.rs:7451-7625` (`mod dispatch_files_tests`).

**Verification** — all 5 early-return branches covered:

| Branch | Test | Coverage |
|---|---|---|
| Empty paths | `dispatch_files_decide_empty_paths_returns_empty` (7451) | matches!(Empty) |
| Fingerprint match | `dispatch_files_decide_fingerprint_match_returns_short_circuit` (7468) | matches!(FingerprintMatch) + None-fingerprint-doesn't-short-circuit sanity |
| ExceedsLimit | `dispatch_files_decide_oversize_returns_exceeds_limit` (7511) | 10-byte file at 5-byte limit → ExceedsLimit { offending, size: 10, limit: 5 } + cross-check vs `collect_files_blocking` directly |
| IsDirectory | `dispatch_files_decide_directory_returns_is_directory` (7565) | creates tempdir → passes path → IsDirectory(p) |
| Io / missing path | `dispatch_files_decide_missing_path_returns_io` (7600) | constructs ghost path → Io(ErrorKind::NotFound) |

**Quality check**: All tests use the helper `dispatch_files_decide` (extracted testability pattern, mirrors `apply_image_inner`). Assertions check actual variant contents (offending path equality, size value, limit value, ErrorKind::NotFound), not just `matches!`. Cross-check in the ExceedsLimit test directly invokes `collect_files_blocking` and compares result — pins helper as faithful pass-through.

### P1.D (formerly P1.3) — RESOLVED — SUGGESTION.md #S-5 / #S-6 restored

**Location**: `next/SUGGESTION.md:149` (#S-5), `:177` (#S-6) at HEAD.

**Verification**:
- #S-5 (DEFAULT_MAX_FILE_SIZE = 50 MiB constant) — present with full content matching `54ce992` ✓
- #S-6 (popup::tests::drop_with_empty_sentinel macOS headless deadlock) — present with full content matching `54ce992` ✓
- Both marked 🟡 priority, both reference M3b STEP-3b.1 / M4 STEP-4.2 as forward-resolution STEPs ✓

### P2.1 — SHA256 re-verification from memory (not disk re-read) is documented but worth surfacing

**Location**: `src/service.rs:5158-5170` (`write_and_verify_file_blocking` doc)

**Scenario**: PLAN §3 STEP-3a.3 字面理解 "重新算 sha256 校验" — naturally suggests "read from disk after write and verify". The executor's interpretation "verify from in-memory bytes" is justified (QUIC stream-level integrity + local-disk trust + 5-8 s savings on 200 MiB disk re-read) but deviates from the literal PLAN reading.

**Risk**: Post-M2 BUGFIX pattern (commit `7a57bb3` for `dispatch_image`) also recomputes sha256 from in-memory bytes after writing — so the deviation aligns with established image-path practice. No regression risk.

**Mitigation**: The deviation is documented in the function doc + SUGGESTION #S-8 already tracks this design choice's downstream effect (default `accept_dir`). No action needed.

### P2.2 — Auto-accept off default silently drops files (no Toaster prompt in STEP-3a.3)

**Location**: `src/service.rs:3391-3407` (`handle_clipboard_inbound_files` — `AutoAcceptOff` arm)

**Scenario**: `lan-mouse-ipc::ClipboardConfig::auto_accept_files` defaults to `false` (matches `src/service.rs::set_clipboard_config` handler at `:2011-2026`). STEP-3a.3 inherits this default → most users will see "copy file → nothing happens at peer → log warn at info level".

**Impact**: This is the documented scope contract (PLAN §3 STEP-3a.3 "假定 auto_accept_files = true"; M3b adds the Toaster prompt). SUGGESTION #S-7 already tracks the IPC handler follow-up. No action needed for STEP-3a.3.

**Note for leader**: M3b STEP-3b.1 needs to (a) wire `auto_accept_files` UI toggle + (b) for `auto_accept_files = false` case, fire a Toaster ask instead of silently dropping. Currently the "auto-accept off" path is fully silent (log info). Step 3b.2 should change that arm to fire `FileTransferRequest` IPC.

### P2.3 — `default_accept_dir` falls back to `/tmp/lan-mouse` on missing env

**Location**: `src/service.rs:4255-4263` (`default_accept_dir`)

**Scenario**: If neither `$HOME` nor `$USERPROFILE` is set (unusual for an interactive session but possible for a container/headless daemon), the fallback is `/tmp/lan-mouse`. This is "safe enough" — no panic, but means containerized deployments will land files in `/tmp`.

**Impact**: Per SUGGESTION #S-8 ("考虑使用 `dirs` / `directories` crate 替换 `$HOME` / `$USERPROFILE` fallback chain"), this is a known cosmetic gap. M3b IPC handler override will resolve user-editable case; containerized case is M4+.

### P2.4 — BackendCmd::CurrentFiles still `#[allow(dead_code)]`

**Location**: `src/service.rs:4194-4199`

**Verification**: The `#[allow(dead_code)]` on `BackendCmd::CurrentFiles` is correct — the inbound arm consuming this variant lands in STEP-3a.5 (cancellation protocol per `STEP-P2-M3a-3a.2.md` §6.3). Not a regression; forward-compat hook for 3a.5.

### P2.5 — File write does NOT fsync (data integrity after power loss)

**Location**: `src/service.rs:5164` (`std::fs::write(&path, &bytes)`)

**Scenario**: `std::fs::write` does not call `fsync` — power loss between write() return and OS page-cache flush could leave the file with zeros or partial bytes despite the in-memory sha256 verify passing. The image path uses `f.sync_all().await` (per `src/clipboard/file_meta.rs:352`).

**Impact**: Low — this is a daemon running on a desktop peer, not a database. Power loss during a 200 MiB transfer is extreme edge case. The file would either be on disk (best case, all bytes survived cache flush) or absent (worst case, OS flushed cache after our return but before power loss). For full POSIX data-integrity, would need `File::create + write_all + sync_all`.

**Mitigation**: `file_meta.rs:346-352` test helper already uses `f.sync_all().await.unwrap()` — the production path in `write_and_verify_file_blocking` could mirror this. Single-line addition (`File::create + write_all + sync_all`). Defer to STEP-3a.4 or M3a end-of-milestone cleanup.

### P3.1 — Test file uses `tempfile` dep (already present per `Cargo.toml`)

**Location**: `src/service.rs:7841` (`use tempfile::TempDir`)

Already present from STEP-3a.1 (`a77bb72` chore(deps): tempfile dev-dep). No action.

### P3.2 — `clippy::too_many_arguments` `#[allow]` on 8-arg fns

**Location**: `src/service.rs:5246` (`apply_files_inner`), `:5299` (`apply_inbound_files_task`)

8 args vs clippy default 7. Justified per executor: each parameter is independent (inbound_sha / name / size / mime / source / accept_dir / applied_tx + bytes/fetcher). Mirrors image branch's `apply_inbound_image_task` (also `#[allow]`). Cosmetic.

### P3.3 — PONG_HEALTH_TIMEOUT concern deferred to STEP-3a.4

**Location**: PLAN §5 risk #5

The 200 MiB transfer WILL trigger PONG_HEALTH_TIMEOUT (3.5 s) if `set_stream_priority PRIORITY_BULK` is not applied to the HTTP/3 response stream. STEP-3a.4 explicitly plans this — `set_stream_priority(PRIORITY_BULK)`. STEP-3a.3 itself uses HTTP/3 GET (which goes through the same priority queue), but the 200 MiB transfer can only be validated end-to-end AFTER STEP-3a.4 lands the server route. No action for STEP-3a.3; carry the flag forward to STEP-3a.4.

---

## Test coverage spot-check

| Test name | File | Covers | Status |
|---|---|---|---|
| `dispatch_files_decide_empty_paths_returns_empty` | src/service.rs:7451 | Branch 1: empty paths defensive early-return | PASS |
| `dispatch_files_decide_fingerprint_match_returns_short_circuit` | src/service.rs:7468 | Branch 2: same-fingerprint short-circuit + None-doesn't-short-circuit sanity | PASS |
| `dispatch_files_decide_oversize_returns_exceeds_limit` | src/service.rs:7511 | Branch 3: file > max_size + cross-check vs collect_files_blocking directly | PASS |
| `dispatch_files_decide_directory_returns_is_directory` | src/service.rs:7565 | Branch 4: directory in selection | PASS |
| `dispatch_files_decide_missing_path_returns_io` | src/service.rs:7600 | Branch 5: missing path → Io(ErrorKind::NotFound) | PASS |
| `handle_clipboard_inbound_files_decide_returns_auto_accept_off` | src/service.rs:7691 | Decision: auto_accept_files=false | PASS |
| `handle_clipboard_inbound_files_decide_returns_apply_with_actionable` | src/service.rs:7711 | Decision: happy path + actionable filter + field passthrough | PASS |
| `handle_clipboard_inbound_files_decide_filters_mime_too_large` | src/service.rs:7743 | Decision: all-MIME_TOO_LARGE skip | PASS |
| `handle_clipboard_inbound_files_decide_returns_empty` | src/service.rs:7761 | Decision: empty entries defensive | PASS |
| `handle_clipboard_inbound_files_decide_filters_mixed` | src/service.rs:7778 | Decision: mixed MIME_TOO_LARGE + actionable filter | PASS |
| `handle_clipboard_inbound_files_decide_auto_accept_off_ignores_entries` | src/service.rs:7805 | Regression pin: flag controls, not entries | PASS |
| `apply_inbound_files_task_writes_file_with_sha256_match` | src/service.rs:7875 | Spawned task: success path + on-disk verify | PASS |
| `apply_inbound_files_task_resolves_collision_with_suffix` | src/service.rs:7959 | Spawned task: collision `<stem> (1).<ext>` + original NOT clobbered | PASS |
| `apply_inbound_files_task_sha256_mismatch_deletes_partial` | src/service.rs:8046 | Spawned task: mismatch → partial deleted + success=false | PASS |
| `apply_inbound_files_task_get_404_reports_failure_without_writing` | src/service.rs:8127 | Spawned task: 404 status → success=false + no file written | PASS |
| `resolve_unique_path_no_collision_returns_input` | src/service.rs:8191 | Path: happy path | PASS |
| `resolve_unique_path_first_collision_appends_one` | src/service.rs:8198 | Path: collision `(1)` suffix | PASS |
| `resolve_unique_path_two_collisions_appends_two` | src/service.rs:8211 | Path: collision `(2)` suffix | PASS |
| `resolve_unique_path_handles_extensionless_name` | src/service.rs:8220 | Path: no-extension case | PASS |
| `write_and_verify_file_blocking_happy_path` | src/service.rs:8228 | Disk write + sha256 verify happy | PASS |
| `write_and_verify_file_blocking_mismatch_deletes_partial` | src/service.rs:8240 | Disk write + sha256 mismatch → partial deleted | PASS |
| `insert_owned_round_trip_returns_bytes` | src/clipboard/file_cache.rs | file_cache: insert_owned happy path | PASS |
| `insert_owned_overwrite_does_not_double_count` | src/clipboard/file_cache.rs | file_cache: insert_owned overwrite | PASS |
| `insert_owned_byte_budget_overflow_evicts_oldest` | src/clipboard/file_cache.rs | file_cache: insert_owned LRU eviction parity | PASS |
| `insert_owned_oversize_rejected_silently` | src/clipboard/file_cache.rs | file_cache: insert_owned oversize reject | PASS |
| `insert_owned_200_mib_at_1_gib_budget` | src/clipboard/file_cache.rs | file_cache: insert_owned 200 MiB scale | PASS |

**Total: 26 new test functions** (5 dispatch_files + 6 decision + 4 spawned task + 4 path + 2 disk write + 5 insert_owned). Matches executor's "26 new tests" claim.

**Test count verification**:
- STEP-3a.2 baseline: 272 lib pass
- After this batch: 298 lib pass = +26 ✓
- Workspace total: 453 pass / 1 pre-existing input-capture flake ✓

**Coverage matrix vs. PLAN §8 M3a 测试矩阵**:
- ✅ `ClipboardFiles` outbound dispatch (decision fn tested; full Service::dispatch_files wired but only via helper)
- ✅ `apply_inbound_files_task` HTTP/3 GET flow (mocked fetcher drives 200/404/error)
- ✅ File write + sha256 verify
- ✅ Path collision (3 of 4 cases tested: no-collision / (1) / (2) / extensionless)
- ⏸ Real wire end-to-end (200 MiB through HTTP/3 server route) — deferred to STEP-3a.4 + human manual test (PLAN §3 STEP-3a.3 explicitly notes "STEP-3a.4 补完 server-side route")

**Missing coverage** (non-blocking):
- No integration test that exercises the full `Service::run` select! loop with a real inbound `ClipboardFiles` event (would require LAN peer or complex mock). Acceptable — the unit-level helper + spawned task tests pin the contract.
- No test for 9999-collision timestamp fallback in `resolve_unique_path` (would be expensive to set up). Acceptable.

---

## PLAN deviations accepted

From `next/STEP-P2-M3a-3a.3.md §3`:

| # | Deviation | Validator assessment |
|---|---|---|
| #1 | HTTP/3 GET source-side route deferred to STEP-3a.4 (A1 strategy) | ✅ Acceptable. Mock fetcher drives success path in tests. Real wire validation is STEP-3a.4 + human manual test (PLAN §8 M3a 矩阵). |
| #2 | SHA256 verify from memory (not disk re-read) | ✅ Acceptable. Justified: QUIC integrity + local-disk trust + 5-8 s saved on 200 MiB. Mirrors `dispatch_image` post-M2 BUGFIX pattern. |
| #3 | Path collision `<stem> (1).<ext>` (Finder/Explorer style) vs PLAN literal `<name> (1)` | ✅ Acceptable. Industry-standard across macOS Finder / Windows Explorer / GNOME Files / KDE Dolphin. Linux `cp -i` also uses this format. Tracked as SUGGESTION #S-9. |
| #4 | `auto_accept_files = false` default (M3b adds UI) vs PLAN 假定 `auto_accept_files = true` | ✅ Acceptable. Matches `lan_mouse_ipc::ClipboardConfig::default()`. M3b STEP-3b.2 will add the Toaster ask for the false case. |
| #5 | 接续契约完整 4 字段 (applied_tx / accept_dir / sha / loopback) | ✅ 0 deviation. |
| #6 | `clippy::too_many_arguments` `#[allow]` on 8-arg fns | ✅ Acceptable. Mirrors image branch pattern. |

**Additional deviation found by validator**:

| # | Deviation | Severity | Notes |
|---|---|---|---|
| #7 | Path traversal in `resolve_unique_path` — `accept_dir.join(name)` does NOT canonicalize `..` | **P1.A** | Real wire-level concern — see finding above. Can ship in STEP-3a.4 follow-up with a 3-line fix. |

---

## Suggestions / forward-compat

- **STEP-3a.4 prereq**: Address P1.A path traversal concern before HTTP/3 server route lands (the server route would expose the same `resolve_unique_path` from a different attack surface). 3-line fix: `let candidate = accept_dir.join(name); if !candidate.starts_with(accept_dir) { return resolve_unique_path(accept_dir, &sanitize_name(name)); }`.
- **STEP-3a.4 prereq**: Per PLAN §5 risk #5, apply `set_stream_priority(PRIORITY_BULK)` to the HTTP/3 response stream to prevent PONG_HEALTH_TIMEOUT (3.5 s) firing during 200 MiB transfer. STEP-3a.4's contract in `next/STEP-P2-M3a-3a.3.md §6.2` already includes this.
- **STEP-3a.5 prereq**: Receiver-side `Some(ProtoEvent::FileTransferCancel) = cancel_rx.recv() => ...` arm in `handle_clipboard_inbound_files` select! loop — currently no cancel signal can interrupt an in-flight HTTP/3 GET.
- **P2.5 follow-up**: Add `sync_all()` to `write_and_verify_file_blocking` for POSIX data-integrity (1-line change: `File::create + write_all + sync_all`). Defer to STEP-3a.4 or M3a cleanup.
- **P2.3 follow-up**: `default_accept_dir` `/tmp` fallback is fine for now but should be replaced with `dirs` crate in M3b+ for proper container semantics. Tracked in SUGGESTION #S-8.
- **SUGGESTION #S-7 follow-up**: The IPC handler `set_clipboard_config` (line 2011-2026) log text says "M0c — runtime effect wired in M1a" which is now misleading. The decision fn DOES re-read config per call (verified — `handle_clipboard_inbound_files` reads `self.config.clipboard_config()` on each invocation), so IPC changes do take effect, but the log text is stale. Should be updated to "M3a — runtime effect read live by handle_clipboard_inbound_files_decide per call" — 1-line change, can ship in M3b STEP-3b.1.
- **Test discrepancy note**: Executor commit `36b912b` says "26 new unit tests" with breakdown `6 decision + 10 spawned + 4 resolve + 2 write + 4 spawned integration`. The actual breakdown is `6 decision + 4 spawned task + 4 resolve + 2 write = 16`. The "26" count appears to include the 5 `dispatch_files_decide_*` tests (from `bb849a6`) + 5 `insert_owned_*` tests (from `be34c7c`) for the full batch. No bug — the message is just slightly imprecise about which commits contributed the tests.

---

## Verdict

**PASS-with-followup**

**Counts**: 0 P0 / 1 P1 (P1.A path traversal — new finding, scope: STEP-3a.4) / 5 P2 / 3 P3

**P1 followup status** (from previous validator report):
- ✅ **P1.1 — dispatch_files cache insert** — RESOLVED. Second `spawn_blocking` + `FileCache::insert_owned` lands bytes correctly. `#[allow(dead_code)]` removed from `file_cache` field.
- ✅ **P1.2 — dispatch_files 5-branch test coverage** — RESOLVED. All 5 early-return branches covered via `dispatch_files_decide` helper extraction.
- ✅ **P1.3 — SUGGESTION.md #S-5/#S-6 restoration** — RESOLVED. Both entries present at HEAD with full content from original `54ce992`.

**STEP-3a.3 status**: PASS. All PLAN §3 STEP-3a.3 完成标志 items landed except HTTP/3 server-side route (explicitly deferred to STEP-3a.4 per A1 strategy, well-documented). 21 new tests in `service.rs` + 5 in `file_cache.rs` = 26 new tests, matching executor's claim.

**Top forward-looking concerns**:
1. **P1.A** — path traversal in `resolve_unique_path` (see finding above). 3-line fix; ship in STEP-3a.4 before HTTP/3 server route lands.
2. **PLAN §5 risk #5** — PONG_HEALTH_TIMEOUT during 200 MiB transfer (already on STEP-3a.4's contract).
3. **P2.5** — no fsync on disk write (cosmetic, defer to cleanup).

**Recommended next action for leader**:
- Proceed to STEP-3a.4 (HTTP/3 server `/clipboard/file/{sha256}` + streaming + range stub + `set_stream_priority PRIORITY_BULK`).
- Carry the path-traversal concern (P1.A) into STEP-3a.4 as a 3-line patch to `resolve_unique_path` (or as a follow-up patch in STEP-3a.3.1 mini-step).
- After STEP-3a.4 lands: dispatch M3a STEP-3a.5 (cancellation protocol) per `next/STEP-P2-M3a-3a.2.md §6.3` + `next/STEP-P2-M3a-3a.3.md §6.3`.
- After M3a completes: dispatch M3b STEP-3b.1 (IPC + Toaster prompt) to close SUGGESTION #S-5 / #S-7 / #S-8 / #S-9.