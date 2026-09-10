# Cross-platform clipboard text end-to-end test (M1b STEP-1b.4)

> **Goal**: validate the full M1b clipboard text pipeline — inline small text,
> Meta + HTTP/3 large text (> 1 KiB), loopback LRU skip, and active eviction
> on rapid source switching — across all three cross-platform peer combinations:
>
> - **macOS ↔ Windows** (UTF-8 ↔ UTF-16 conversion + process boundary)
> - **macOS ↔ Linux** (NSPasteboard via pbcopy/pbpaste ↔ xclip / wl-paste)
> - **Windows ↔ Linux** (CF_UNICODETEXT ↔ xclip / wl-paste)
>
> All peers run `lan-mouse` daemons with `enable_clipboard_to = true` and
> pre-validated QUIC mTLS pairing (M0a–M0c peer entries in `config.toml`).
>
> **Owner**: human operator (this template is the checklist; clipboard events
> touch the OS pasteboard and cannot be exercised from a headless CI matrix).
>
> **Related STEPs**: 1b.1 (inline/meta split), 1b.2 (HTTP/3 pull + active
> cache eviction), 1b.3 (loopback LRU 128/60s + metrics).

---

## 0. Prerequisites (run on every peer)

### 0.1 Daemon build + launch

```bash
# Build (one terminal per machine)
git checkout main
cargo build --release -p lan-mouse
```

### 0.2 Daemon start (with verbose clipboard logging)

```bash
# ─── macOS ───
RUST_LOG="lan_mouse=debug,lan_mouse::service::clipboard=trace" \
  ./target/release/lan-mouse

# ─── Windows (PowerShell) ───
$env:RUST_LOG = "lan_mouse=debug,lan_mouse::service::clipboard=trace"
.\target\release\lan-mouse.exe

# ─── Linux (X11) ───
RUST_LOG="lan_mouse=debug,lan_mouse::service::clipboard=trace" \
  ./target/release/lan-mouse

# ─── Linux (Wayland) ───
# same command; lan-mouse auto-detects wl-clipboard vs xclip
```

### 0.3 Pairing + clipboard config

Verify each machine's `config.toml` contains a peer entry:

```toml
[[clients]]
host = "peer.local"
port = 4252
trust_address = true
enable_clipboard_to = true   # ← must be true for cross-device sync

[clipboard]
# daemon-global; defaults are fine for this matrix
```

**Daemon log check** — within 5 s of launch, both sides should print:

```text
[INFO  lan_mouse] connected to peer: <peer>.local:4252
[TRACE lan_mouse::service::clipboard] hit rate: skip=0 allow=0 (0/0)   # no traffic yet
```

### 0.4 Per-platform clipboard command reference

| Action | macOS | Windows (PowerShell) | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| **Write** | `echo "..." \| pbcopy` | `Set-Clipboard -Value "..."` | `echo "..." \| xclip -selection clipboard -i` | `echo "..." \| wl-copy` |
| **Read** | `pbpaste` | `Get-Clipboard` | `xclip -selection clipboard -o` | `wl-paste` |
| **Read file → clipboard** | `pbcopy < /tmp/x.txt` | `Get-Content /tmp/x.txt -Raw \| Set-Clipboard` | `xclip -selection clipboard -i < /tmp/x.txt` | `wl-copy < /tmp/x.txt` |
| **Write clipboard → file** | `pbpaste > /tmp/x.txt` | `Get-Clipboard \| Set-Content -Path /tmp/x.txt -NoNewline` | `xclip -selection clipboard -o > /tmp/x.txt` | `wl-paste > /tmp/x.txt` |

> **Daemon log on every OS clipboard change** (≈ 500 ms tick):
> `[DEBUG lan_mouse::service::clipboard] change detected sha256=<hex> size=<N>`

---

## 1. Test scenarios (apply to each pair group in §2)

Each pair group (§2) runs the four scenarios below in order. Mark each
checkbox with ✅ / ❌ + one-line evidence (log excerpt / sha256sum output)
before moving on. If any scenario fails, copy the daemon log snippet into
`next/SUGGESTION.md` and pause the matrix.

### Scenario S1 — Small text (≤ 1 KiB; inline path)

**Covers**: 1b.1 inline branch + 1a.4 dispatcher + StreamC round-trip.

```bash
# ─── A side (source) ───
echo "small text — line 1
line 2 with CJK: 中文 emoji 🦀 é
line 3 ends here" | <WRITE_CMD>

# Wait 1 s for daemon tick (500 ms poll) + StreamC push
sleep 1

# ─── B side (sink) ───
<READ_CMD>     # verify the exact 3 lines, byte-for-byte
```

**Expected log (A side)**:
```text
[DEBUG lan_mouse::service::clipboard] change detected sha256=<...> size=<~120>
[DEBUG lan_mouse::service::clipboard] sending ClipboardText (inline) to peer
```

**Expected log (B side)**:
```text
[DEBUG lan_mouse::service::clipboard] inbound ClipboardText (inline) sha256=<...>
[DEBUG lan_mouse::service::clipboard] apply_inbound_clipboard_text (size=<~120>)
[TRACE lan_mouse::service::clipboard] hit rate: skip=0 allow=1 (0%)
```

**Pass criteria** — output on B is byte-identical to input on A. CJK, emoji,
and accented characters are preserved (UTF-8 → CF_UNICODETEXT round-trip
through Windows requires the dispatcher's UTF-16 conversion — see 1a.3
test matrix).

### Scenario S2 — Large text (1 MiB; Meta + HTTP/3 pull path)

**Covers**: 1b.2 inbound Meta pull + `Http3Client::get_text` + HTTP/3
`/clipboard/text/{sha256}` route + `clipboard_cache` eviction.

#### 2.1 Generate 1 MiB random text

```bash
# ─── A side (any peer with shell access) ───
head -c 1048576 /dev/urandom | base64 > /tmp/big.txt
wc -c /tmp/big.txt                                  # → 1398120 (base64 inflates ~33%)
sha256sum /tmp/big.txt                              # → <sha_a>  ← SAVE THIS
```

> **Note**: `base64` inflates 1 MiB random to ~1.4 MiB which is well past the
> 1 KiB threshold; this exercises the Meta + HTTP/3 pull path (1b.1 / 1b.2)
> rather than the inline branch.

#### 2.2 Push to clipboard

```bash
# ─── A side ───
<WRITE_FILE_TO_CLIPBOARD> /tmp/big.txt
# Wait 1 s for dispatcher tick + StreamC Meta push + HTTP/3 pull initiation
sleep 1
```

**Expected log (A side)**:
```text
[DEBUG lan_mouse::service::clipboard] change detected sha256=<sha_a> size=1398120
[DEBUG lan_mouse::service::clipboard] sending ClipboardText (meta) to peer sha256=<sha_a>
[DEBUG lan_mouse::service::clipboard] clipboard cache: evicted prev outbound sha=<sha_prev>
[TRACE lan_mouse::service::clipboard] clipboard cache: inserted sha256=<sha_a> size=1398120
```

#### 2.3 Verify B side round-trip

```bash
# ─── B side ───
<READ_CLIPBOARD_TO_FILE> /tmp/back.txt
sha256sum /tmp/back.txt                             # → <sha_b>
diff /tmp/big.txt /tmp/back.txt                     # → exit 0 (no output)
[ "$sha_a" = "$sha_b" ] && echo "BYTE-IDENTICAL ✓" || echo "MISMATCH ✗"
```

**Expected log (B side)**:
```text
[DEBUG lan_mouse::service::clipboard] inbound ClipboardText (meta) sha256=<sha_a>
[DEBUG lan_mouse::service::clipboard] HTTP/3 GET /clipboard/text/<sha_a> → 200
[DEBUG lan_mouse::service::clipboard] apply_inbound_clipboard_text (size=1398120)
[TRACE lan_mouse::service::clipboard] hit rate: skip=0 allow=1 (0%)
```

**Pass criteria** — `diff` exits 0; `sha256sum` matches A-side; the apply
log line appears within 2 s of the source-side `change detected` line.

### Scenario S3 — Same content repeated copy (loopback LRU skip)

**Covers**: 1b.3 LRU 128 / 60 s TTL + `mark_local_write`前置 + metrics
counters.

```bash
# ─── A side (source) ───
echo "loopback-test-$(date +%s)" | <WRITE_CMD>
sleep 0.5
echo "loopback-test-$(date +%s)" | <WRITE_CMD>     # 2nd push within 60s TTL → A.apply skip
sleep 0.5
echo "loopback-test-$(date +%s)" | <WRITE_CMD>     # 3rd push within 60s TTL → A.apply skip
sleep 0.5
echo "loopback-test-$(date +%s)" | <WRITE_CMD>     # 4th push within 60s TTL → A.apply skip
sleep 0.5
echo "loopback-test-$(date +%s)" | <WRITE_CMD>     # 5th push within 60s TTL → A.apply skip

# ─── B side (sink) — verify each copy lands ───
sleep 1
<READ_CMD>     # → 5th (most recent) content
```

**Expected log (A side, within 60 s)**:
```text
# t0: change detected on B's push → inbound apply → mark_local_write
[DEBUG lan_mouse::service::clipboard] inbound ClipboardText (inline) sha256=<...>
[DEBUG lan_mouse::service::clipboard] apply_inbound_clipboard_text
[DEBUG lan_mouse::service::clipboard] mark_local_write sha256=<...>

# t+0.5: A copies same content the daemon just received → outbound skip
[DEBUG lan_mouse::service::clipboard] change detected sha256=<...>
[DEBUG lan_mouse::service::clipboard] loopback LRU hit — skip push
```

**Expected log (B side)**:
```text
# A pushes 5 times → first push lands, subsequent 4 push identical content
# B's tick is quiescent (no LRU mark on B) → B broadcasts once → A.skip x4
[TRACE lan_mouse::service::clipboard] hit rate: skip=4 allow=1 (80%)
```

**Pass criteria** — A's daemon log shows exactly **1** `apply_inbound_clipboard_text`
line + **4** `loopback LRU hit — skip push` lines; B's final `pbpaste` (or
equivalent) returns the 5th push content; B's local clipboard does not
"jitter" (no extra `change detected` between the 5 A pushes); the hit-rate
trace shows 4 skips + 1 allow after the cycle.

> **Distinguishing 1b.3 skip vs 1b.2 404**: the loopback skip happens
> **before** the HTTP/3 GET — A's outbound dispatcher sees the fingerprint
> in its own LRU and skips the StreamC push entirely. A 404 in §2.3 of S2
> would happen **after** the StreamC Meta push reaches B and B's HTTP/3 GET
> misses. Different log sites, different semantics.

### Scenario S4 — Different source rapid switching (active eviction)

**Covers**: 1b.2 source-side `cache.remove(prev_fingerprint)` on each push +
the race where B's HTTP/3 GET arrives after A has already evicted the
previous sha.

```bash
# ─── A side — write X1, then 100 ms later write X2, etc. ───
echo "X1" | <WRITE_CMD>
sleep 0.1
echo "X2" | <WRITE_CMD>     # cache.remove(X1) + cache: X2
sleep 0.1
echo "X3" | <WRITE_CMD>     # cache.remove(X2) + cache: X3
sleep 0.1
echo "X4" | <WRITE_CMD>     # cache.remove(X3) + cache: X4
sleep 0.1
echo "X5" | <WRITE_CMD>     # cache.remove(X4) + cache: X5

# ─── B side — verify final clipboard contains X5 ───
sleep 1
<READ_CMD>     # → "X5"
```

**Expected log (A side)**:
```text
[DEBUG lan_mouse::service::clipboard] change detected sha256=<X1_sha> size=2
[DEBUG lan_mouse::service::clipboard] sending ClipboardText (meta) sha256=<X1_sha>
[DEBUG lan_mouse::service::clipboard] clipboard cache: evicted prev outbound sha=<prev_sha>
[DEBUG lan_mouse::service::clipboard] clipboard cache: inserted sha256=<X1_sha> size=2
# ... repeat for X2..X5 within 1 s ...
[DEBUG lan_mouse::service::clipboard] change detected sha256=<X5_sha> size=2
[DEBUG lan_mouse::service::clipboard] sending ClipboardText (meta) sha256=<X5_sha>
```

**Expected log (B side)**:
```text
# At least one of X1..X4 may surface as "404 cache miss" — that's expected
# (race: A evicted before B's HTTP/3 GET landed). See §3 of S4 for the
# acceptance contract.
[DEBUG lan_mouse::service::clipboard] inbound ClipboardText (meta) sha256=<X1_sha>
[WARN  lan_mouse::service::clipboard] HTTP/3 GET /clipboard/text/<X1_sha> → 404 — skipping
# ... etc ...
[DEBUG lan_mouse::service::clipboard] inbound ClipboardText (meta) sha256=<X5_sha>
[DEBUG lan_mouse::service::clipboard] HTTP/3 GET /clipboard/text/<X5_sha> → 200
[DEBUG lan_mouse::service::clipboard] apply_inbound_clipboard_text (size=2)
```

**Pass criteria** —
1. **Final state**: B's `<READ_CMD>` returns `X5` (not X1..X4).
2. **No errors** on B side: `WARN` lines with `404 cache miss` are
   **acceptable** (per 1b.2 §4: "404 静默忽略"); any `ERROR` /
   `daemon disconnect` / `panic` lines are **not acceptable**.
3. **No stall**: B's `apply_inbound_clipboard_text` log for X5 appears
   within 2 s of A's last `change detected` line.
4. **A does not push stale X1..X4 again**: A's outbound dispatcher must
   not re-broadcast an old sha (the tick deduplicates by `clipboard_last_text`
   cache; once the local clipboard holds X5, ticks for X1..X4 are quiescent).

---

## 2. Cross-platform peer-pair groups

Each group runs §1 S1 + S2 + S3 + S4 (4 scenarios × 2 directions = 8 trials).

### 2.1 macOS ↔ Windows

**Setup** — A = macOS, B = Windows (PowerShell). Both machines on the same
LAN; mDNS resolving `mac-a.local` / `win-b`; firewall 4252/UDP open.

| Direction | S1 | S2 (1 MiB) | S3 (loopback skip) | S4 (rapid switch) |
|---|---|---|---|---|
| **A(macOS) → B(Win)** | ☐ | ☐ | ☐ | ☐ |
| **B(Win) → A(macOS)** | ☐ | ☐ | ☐ | ☐ |

#### macOS-side command map
- Write: `echo "..." | pbcopy` (S1/S3) / `pbcopy < /tmp/big.txt` (S2) / `echo "X$N" | pbcopy` (S4)
- Read: `pbpaste` / `pbpaste > /tmp/back.txt`

#### Windows-side command map
- Write: `Set-Clipboard -Value "..."` / `Get-Content /tmp/big.txt -Raw | Set-Clipboard`
- Read: `Get-Clipboard` / `Get-Clipboard | Set-Content -Path /tmp/back.txt -NoNewline`

#### Known platform quirks
- **Windows `Set-Clipboard` is slow** (~100 ms first call, ~10 ms subsequent);
  allow ≥ 1 s after `Set-Clipboard` before reading on the macOS side.
- **Windows ↔ UTF-8**: lan-mouse dispatcher converts UTF-8 → UTF-16LE for
  `CF_UNICODETEXT` and back; verify byte counts match on sha256sum (the
  conversion is byte-faithful for valid UTF-8).
- **PowerShell pipeline**: `Get-Clipboard | Set-Content` adds a trailing
  newline; use `-NoNewline` (as above) for byte-exact comparisons.

---

### 2.2 macOS ↔ Linux

**Setup** — A = macOS, B = Linux (X11 or Wayland). Both on same LAN; mDNS
resolving `mac-a.local` / `linux-b.local`; firewall 4252/UDP open.

| Direction | S1 | S2 (1 MiB) | S3 (loopback skip) | S4 (rapid switch) |
|---|---|---|---|---|
| **A(macOS) → B(Linux)** | ☐ | ☐ | ☐ | ☐ |
| **B(Linux) → A(macOS)** | ☐ | ☐ | ☐ | ☐ |

#### macOS-side command map
- Write: `echo "..." | pbcopy` / `pbcopy < /tmp/big.txt`
- Read: `pbpaste` / `pbpaste > /tmp/back.txt`

#### Linux X11 command map
- Write: `echo "..." | xclip -selection clipboard -i` /
  `xclip -selection clipboard -i < /tmp/big.txt`
- Read: `xclip -selection clipboard -o` /
  `xclip -selection clipboard -o > /tmp/back.txt`

#### Linux Wayland command map
- Write: `echo "..." | wl-copy` / `wl-copy < /tmp/big.txt`
- Read: `wl-paste` / `wl-paste > /tmp/back.txt`

#### Known platform quirks
- **`xclip -selection clipboard` vs `xclip -selection primary`**: the
  clipboard is `clipboard` (Ctrl-V / Ctrl-C); the `primary` selection is
  mouse-driven (middle-click paste). Using `-selection primary` here would
  silently fail the test.
- **X11 vs Wayland mismatch**: if A runs X11 and B runs Wayland, lan-mouse
  on B will fail to read X11's clipboard (no IPC bridge). Both sides must
  use the same display protocol.
- **Tool detection on Linux**: on first clipboard read, daemon logs
  `tool detected: xclip` or `tool detected: wl-copy`. If neither is on
  `$PATH`, daemon logs `clipboard backend unavailable: install xclip or
  wl-clipboard` and the **mouse/keyboard sharing continues to work**;
  clipboard sync on that side is silently disabled.

---

### 2.3 Windows ↔ Linux

**Setup** — A = Windows (PowerShell), B = Linux (X11 or Wayland). Same LAN;
firewall 4252/UDP open; Windows hostname resolvable from Linux
(`win-a.local` or via `win-a` NetBIOS).

| Direction | S1 | S2 (1 MiB) | S3 (loopback skip) | S4 (rapid switch) |
|---|---|---|---|---|
| **A(Win) → B(Linux)** | ☐ | ☐ | ☐ | ☐ |
| **B(Linux) → A(Win)** | ☐ | ☐ | ☐ | ☐ |

#### Windows-side command map
- Write: `Set-Clipboard -Value "..."` /
  `Get-Content /tmp/big.txt -Raw | Set-Clipboard`
- Read: `Get-Clipboard` / `Get-Clipboard | Set-Content -Path /tmp/back.txt -NoNewline`

#### Linux command map
- Same as §2.2.

#### Known platform quirks
- **PowerShell `Get-Content -Raw`**: required to read the file as a single
  string (without `-Raw`, the array of lines confuses `Set-Clipboard`).
- **Linux → Windows newline conversion**: the dispatcher treats `\n`
  as the canonical separator; PowerShell `Set-Content -NoNewline` is
  byte-exact when the source is single-line. For multi-line big text,
  the byte-level round-trip holds because lan-mouse streams raw UTF-8
  bytes over HTTP/3 (no normalization).

---

## 3. Result capture template

For each peer group + direction + scenario, record one row:

```text
Group:    macOS ↔ Windows
Direction: A(macOS) → B(Win)
Scenario:  S2 (1 MiB)
Date:      2026-09-XX HH:MM
Operator:  <your name>

Source side (macOS) — sha256sum of /tmp/big.txt:
  <sha256-a>  /tmp/big.txt

Sink side (Windows) — sha256sum of /tmp/back.txt:
  <sha256-b>  /tmp/back.txt

sha256-a == sha256-b?  [YES/NO]
diff exit code:        [0 / 1 / 127]
Time to first byte:    [<N>] ms (from daemon log timestamp diff)
Apply log appeared:    [YES/NO]

Evidence (paste 5-10 log lines):
  [DEBUG lan_mouse::service::clipboard] change detected sha256=...
  [DEBUG lan_mouse::service::clipboard] sending ClipboardText (meta) to peer ...
  [DEBUG lan_mouse::service::clipboard] HTTP/3 GET /clipboard/text/<sha> → 200
  ...

Result: ✅ PASS / ❌ FAIL
Notes:   (any caveats, e.g. "first attempt failed with 404 cache miss; second attempt clean")
```

After running all 24 cells (3 groups × 2 directions × 4 scenarios), the
matrix is complete. Aggregate the sha256 results, count of `apply_inbound`
events, and any `WARN` / `ERROR` log lines into the M1b validator input.

---

## 4. Troubleshooting

### Symptom: B's clipboard stays empty after S2

Check A's log for `sending ClipboardText (meta)` — if missing, the dispatcher
didn't pick up the change. Likely causes:
- **macOS Accessibility / Input Monitoring permission** not yet granted to
  the daemon (`pbcopy` will return success but the clipboard listener may
  miss the change). Open System Settings → Privacy & Security → Input
  Monitoring and add the daemon binary.
- **`enable_clipboard_to = false`** in the peer entry of `config.toml` —
  fix and restart daemon.

Check B's log for `HTTP/3 GET /clipboard/text/<sha>`:
- **`→ 404 cache miss`**: 1b.2 active eviction race — A already pushed a
  newer sha. Run the scenario again with bigger `sleep` between pushes.
- **No GET at all**: the inbound Meta event was not delivered — check the
  StreamC connection is open (`netstat -an | grep 4252` or
  `ss -u 'sport = :4252'`).

### Symptom: B's clipboard lands as garbage bytes after S2 (S1 works)

The HTTP/3 GET succeeded but `apply_inbound_clipboard_text` wrote wrong
bytes. This indicates a sha mismatch between A's cache key and B's
HTTP/3 lookup — file this as a bug in `next/SUGGESTION.md` with the
sha values from both sides.

### Symptom: hit-rate log shows `skip=N allow=N+1` instead of `skip=4 allow=1` for S3

Some of the 5 pushes weren't deduped at the dispatcher tick layer. This is
expected when the pushes are spaced **less than 500 ms apart** (the daemon
tick) but **more than 60 s apart** for the LRU TTL — i.e., when the LRU
was already cleared between pushes. Adjust S3 to space pushes
**≤ 500 ms apart** (the spec) and re-run.

### Symptom: rapid-switch S4 final clipboard contains X2 or X3 (not X5)

Active eviction is incomplete — A is broadcasting stale Meta events. Check
`clipboard cache: evicted prev outbound sha=...` log lines on A. If missing,
file as a bug; the `evict_prev_outbound_clipboard_cache` helper is not
firing before the new push.

---

## 5. M1b milestone gate (final checklist)

After all 24 cells of the matrix pass, run the following to close out
M1b:

```bash
# ─── 5.1 Static checks (must pass on host platform) ───
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check

# ─── 5.2 Cross-platform compile (best-effort; may need cross-toolchain) ───
cargo check --target x86_64-unknown-linux-gnu -p lan-mouse-proto
cargo check --target x86_64-unknown-linux-gnu -p lan-mouse-ipc
cargo check --target x86_64-pc-windows-gnu    -p lan-mouse-proto
cargo check --target x86_64-pc-windows-gnu    -p lan-mouse-ipc

# ─── 5.3 M1b completion log ───
# Attach the §3 result template (24 rows) + the static check output to
# next/STEP-P2-M1b-1b.4.md as the human-validation appendix.
```

**M1b milestone closure requires all of:**

| Item | Owner | Pass criteria |
|---|---|---|
| Static checks (5.1) | executor | 0 errors / 0 clippy warnings / 0 fmt diff |
| Cross-platform check (5.2) | executor or CI | 4/4 pass (or document missing toolchain) |
| S1 small text 6/6 cells | human | byte-identical + apply log appears |
| S2 1 MiB text 6/6 cells | human | sha256 matches + diff = 0 |
| S3 loopback skip 6/6 cells | human | LRU skip count = 4 per cycle |
| S4 rapid switch 6/6 cells | human | final clipboard = last push (X5) |
| SUGGESTION.md cleanup | executor | all 24 cells clean → no new entries |

When every box above is checked, M1b is ready for validator review.

---

## 6. Out of scope (do NOT execute as part of this matrix)

- **Image clipboard** (PNG / JPG / BMP / DIB) — M2a / M2b scope; this
  template is **text-only**.
- **File copy** (200 MiB round-trip) — M3a / M3b scope.
- **GUI Toaster / accept-reject flow** — M4 scope.
- **Wayland portal restrictions on clipboard reads** (`wlr-data-control`
  protocol version) — out of scope for M1b; use `wl-paste`/`wl-copy`
  tools as documented.
- **Cross-machine clock skew** — both peers should be NTP-synced for the
  hit-rate timestamp log lines to align with reality.
