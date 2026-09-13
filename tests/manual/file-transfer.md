# Cross-platform file-transfer end-to-end test (M5 STEP-5.2)

> **Goal**: validate the full M5 file-transfer pipeline — 200 MiB performance
> (有线 100 Mbps LAN < 30 s + Wi-Fi < 60 s, **双向**), **cancel 双向**, **拔网
> 双向**, **keepalive↔idle race 专项** (传输完成后 30 s 内连接仍 active + 60 s
> 静默期不 disconnect), **Pong 实测间隔 ≤ 600 ms** — across the same cross-platform
> peer combinations as M2b / M3a / M4 (macOS ↔ Windows, macOS ↔ Linux, Windows
> ↔ Linux).
>
> All peers run `lan-mouse` daemons with `enable_clipboard_to = true` and
> pre-validated QUIC mTLS pairing from M0a–M0c + M3a + M4 STEP-4.2 (set_files)
> + 4.3 (re-inject) + M5 STEP-5.1 (FileTransferFailed).
>
> **Owner**: human operator (this template is the checklist; file transfer
> events touch the OS pasteboard (re-inject) and the network link (拔网 /
> keepalive), and cannot be exercised from a headless CI matrix).
>
> **Related STEPs**: 3a.1 (file metadata + sha256), 3a.2 (file trait surface
> + max_size), 3a.3 (dispatch_files + handle_clipboard_inbound_files), 3a.4
> (HTTP/3 /clipboard/file route), 3a.5 (cancel + supersede), 4.2 (set_files
> trait + macOS/Windows/Linux impls), 4.3 (re-inject + skip conditions + pre-stamp),
> 5.1 (FileTransferFailed + .partial handling).
>
> **Unit test coverage (executor-落地; no human action)**: see
> `next/STEP-P2-M5-5.2.md` §1.3 for the 4 new structural tests in
> `src/connect.rs::tests` covering:
> - `ping_interval_within_pong_interval_budget` — PING_INTERVAL (500 ms) ≤ 600 ms
> - `pong_health_timeout_outpaces_quic_idle_timeout_default` — 3.5 s < 5 s
> - `keepalive_interval_does_not_exceed_idle_timeout` — 5 s ≤ 5 s + 500 ms < 5 s
> - `pong_health_silence_detection_thresholds_correctly` — silence detection at
>   threshold boundary + 2 s of regular Pong arrivals → no close

---

## 0. Prerequisites (run on every peer)

### 0.1 Daemon build + launch

```bash
git checkout main
cargo build --release -p lan-mouse
```

### 0.2 Daemon start (with verbose clipboard + transfer logging)

```bash
# ─── macOS ───
RUST_LOG="lan_mouse=debug,lan_mouse::service=trace,lan_mouse::quic_transport=trace,lan_mouse::clipboard=trace" \
  ./target/release/lan-mouse

# ─── Windows (PowerShell) ───
$env:RUST_LOG = "lan_mouse=debug,lan_mouse::service=trace,lan_mouse::quic_transport=trace,lan_mouse::clipboard=trace"
.\target\release\lan-mouse.exe

# ─── Linux (X11) ───
RUST_LOG="lan_mouse=debug,lan_mouse::service=trace,lan_mouse::quic_transport=trace,lan_mouse::clipboard=trace" \
  ./target/release/lan-mouse

# ─── Linux (Wayland) ───
# same command; lan-mouse auto-detects wl-paste / wl-copy vs xclip
```

### 0.3 Pairing + clipboard + file-transfer config

Verify each machine's `config.toml` carries the M4 fields + `accept_dir`:

```toml
[[clients]]
host = "peer.local"
port = 4252
trust_address = true
enable_clipboard_to = true   # clipboard must be on for file re-inject to work

[clipboard]
enabled = true
accept_dir = "/Users/me/Downloads/lan-mouse"   # macOS / Linux
# accept_dir = "C:\\Users\\me\\Downloads\\lan-mouse"   # Windows
ignore_text = false
ignore_images = false
ignore_files = false
max_file_size = 52428800   # 50 MiB; bump to 200 MiB+ for the 200 MiB scenarios
keep_partial = false
inject_to_clipboard = true   # M4 default; controls whether received files land in OS clipboard
```

**Daemon log check** — within 5 s of launch, both sides should print:

```text
[INFO  lan_mouse] connected to peer: <peer>.local:4252
[INFO  lan_mouse::clipboard] backend ready: <macos: NSPasteboard | windows: OpenClipboard | linux: xclip / wl-paste>
[INFO  lan_mouse] QUIC max_idle_timeout = 5s, keep_alive_interval = 5s
[INFO  lan_mouse::connect] pong watchdog active: 3.5s threshold
```

### 0.4 200 MiB fixture (one-time, on any peer with shell access)

```bash
# ─── macOS / Linux (deterministic via dd + urandom) ───
dd if=/dev/urandom of=/tmp/payload-200mib.bin bs=1M count=200 status=none
sha256sum /tmp/payload-200mib.bin | tee /tmp/payload-200mib.sha256
# → expect 200 MiB exact; sha256 = "<sha>  /tmp/payload-200mib.bin"

# ─── Windows (PowerShell, single-pass fill) ───
$bytes = New-Object byte[] (200 * 1024 * 1024)
(New-Object Random).NextBytes($bytes)
[IO.File]::WriteAllBytes("C:\tmp\payload-200mib.bin", $bytes)
Get-FileHash -Algorithm SHA256 C:\tmp\payload-200mib.bin | Tee-Object -FilePath C:\tmp\payload-200mib.sha256
```

> **Determinism**: `/dev/urandom` produces non-deterministic bytes (the
> sha256 will differ per run); for **byte-level identity** checks, both
> sides just compare to the source sha (not to a reference sha). For
> **LAN throughput** benchmarks, the non-determinism doesn't matter —
> we're timing the transfer, not the content.

### 0.5 Per-platform file-copy command reference

| Action | macOS | Windows (PowerShell) | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| **Generate 200 MiB** | `dd if=/dev/urandom of=/tmp/payload-200mib.bin bs=1M count=200 status=none` | `[IO.File]::WriteAllBytes("C:\tmp\payload-200mib.bin", (New-Object byte[] (200MB)))` | same as macOS | same as macOS |
| **Copy file to clipboard** | open in Finder, `Cmd-C` (Finder copies as file promise — native file-clipboard) | open in Explorer, `Ctrl-C`; OR `Set-Clipboard -Path C:\tmp\payload-200mib.bin` (PowerShell 7+) | `xclip -selection clipboard -t text/uri-list -i /tmp/payload-200mib.bin` | `wl-copy < /tmp/payload-200mib.bin` (auto-detect mime) |
| **Compute sha256** | `shasum -a 256 /tmp/payload-200mib.bin` | `Get-FileHash -Algorithm SHA256 C:\tmp\payload-200mib.bin` | `sha256sum /tmp/payload-200mib.bin` | `sha256sum /tmp/payload-200mib.bin` |
| **Verify landed file** | `ls -la "$HOME/Downloads/lan-mouse/"` | `Get-ChildItem C:\Users\me\Downloads\lan-mouse\` | `ls -la ~/Downloads/lan-mouse/` | `ls -la ~/Downloads/lan-mouse/` |
| **Watch QUIC link** | `lsof -i UDP:4252 -P` (macOS) or `netstat -anu | grep 4252` | `netstat -ano | findstr :4252` | `ss -u 'sport = :4252'` | `ss -u 'sport = :4252'` |

> **Daemon log on every clipboard file change** (≈ 500 ms tick):
> `[DEBUG lan_mouse::service::clipboard] file change detected sha256=<hex> size=209715200 mime=...`
> `[DEBUG lan_mouse::service] dispatching files: <N> entries, total <bytes>`

### 0.6 Wi-Fi measurement setup (评审 #5 3rd 双档)

For the **Wi-Fi < 60 s** leg:
- Both peers on the same Wi-Fi SSID (5 GHz preferred over 2.4 GHz)
- Wi-Fi signal ≥ -65 dBm on both sides (use `iwconfig` / `netsh wlan show interfaces`)
- No active downloads / uploads on either peer (congestion distorts the measurement)
- The 200 MiB transfer on a typical 5 GHz Wi-Fi (≈ 100-150 Mbps effective) typically lands in 30-50 s — well within the 60 s budget

### 0.7 keepalive↔idle race fixture

After completing the 200 MiB transfer in any scenario, leave both peers idle
(no clipboard / file activity) for **30 s** and verify the connection survives,
then extend to **60 s** of total idle.

---

## 1. Test scenarios (apply to each peer group in §2)

Each peer group (§2) runs the **five scenarios** below in order. Mark each
checkbox with ✅ / ❌ + one-line evidence (timing / sha256sum output / log excerpt)
before moving on. If any scenario fails, copy the daemon log snippet into
`next/SUGGESTION.md` and pause the matrix.

### Scenario S1 — 200 MiB wired LAN performance (100 Mbps target < 30 s, **双向**)

**Covers**: 3a.1-3a.4 wire protocol + M0c Ping/Pong keepalive + M5 STEP-5.1
拔网 .partial handling (no .partial on success path).

#### 1.1 Generate 200 MiB fixture (one-time, on A side)

```bash
# ─── A side (any peer) ───
dd if=/dev/urandom of=/tmp/payload-200mib.bin bs=1M count=200 status=none
sha256sum /tmp/payload-200mib.bin | tee /tmp/payload-200mib.sha256
# → record: <sha256-A>  /tmp/payload-200mib.bin
```

#### 1.2 Push A → B over wired 100 Mbps LAN (timing required)

```bash
# ─── A side ───
# Pre-create receive dir if absent
ssh peer-B mkdir -p "$HOME/Downloads/lan-mouse"  # or manual mkdir

# Copy file to clipboard
open /tmp/payload-200mib.bin   # macOS Finder; select-all + Cmd-C
# OR: cp to a watched Finder folder + drag-drop (alternative)
# Windows: explorer /tmp/payload-200mib.bin + Ctrl-C
# Linux: xclip -selection clipboard -t text/uri-list -i /tmp/payload-200mib.bin
# Wayland: wl-copy < /tmp/payload-200mib.bin

# ─── START TIMING ───
date +%s.%N | tee /tmp/transfer-a2b-start.txt
sleep 1   # let the daemon 500ms tick + StreamC push fire
# ─── END TIMING (B side, after file landed) ───
```

#### 1.3 Verify B side landed file + timing

```bash
# ─── B side ───
# Wait for the file to appear + sha256-verify (auto via daemon apply)
ls -la "$HOME/Downloads/lan-mouse/payload-200mib.bin"
# → expect size = 209715200 (200 MiB exact)
sha256sum "$HOME/Downloads/lan-mouse/payload-200mib.bin"
# → expect sha256 == <sha256-A>

# Compute transfer duration
date +%s.%N | tee /tmp/transfer-a2b-end.txt
python3 -c "
import sys
with open('/tmp/transfer-a2b-start.txt') as f: start = float(f.read().strip())
with open('/tmp/transfer-a2b-end.txt') as f: end = float(f.read().strip())
print(f'A→B duration: {end - start:.2f}s (budget < 30s)')
assert end - start < 30.0, f'FAIL: A→B took {end - start:.2f}s, exceeds 30s budget'
"
```

**Expected log (A side)**:

```text
# t0: outbound dispatch
[DEBUG lan_mouse::service] dispatching files: 1 entries, total 209715200 bytes, sha256=<sha>
[DEBUG lan_mouse::service] dispatch_files: inserted file_cache sha=<sha> size=209715200

# t0+0.5s: peer Meta push + HTTP/3 GET
[DEBUG lan_mouse::service::clipboard] sending ClipboardFiles (meta) to peer sha256=<sha> entries=1
[DEBUG lan_mouse::service::clipboard] HTTP/3 GET /clipboard/file/<sha> → 200 (size=209715200)

# t0+~16s: file landed on B (via HTTP/3 streaming)
[DEBUG lan_mouse::service] inbound ClipboardFiles (meta) sha=<sha> entries=1
[INFO  lan_mouse::service] file apply succeeded: <path> size=209715200

# t0+~16s: re-inject (if inject_to_clipboard=true)
[DEBUG lan_mouse::service] clipboard re-inject: dispatching set_files(1 path(s))
```

**Expected log (B side)**:

```text
# During transfer
[DEBUG lan_mouse::service::clipboard] apply_inbound_files_task starting sha=<sha>
[TRACE lan_mouse::service] file write progress: 50% (100 MiB)
[TRACE lan_mouse::service] file write progress: 100% (200 MiB)
[INFO  lan_mouse::service] file apply succeeded: <path> size=209715200

# Post-transfer: heartbeat keeps connection alive
[TRACE lan_mouse::connect] ping_heartbeat: sent Ping to <addr>
[TRACE lan_mouse::connect] stream A forwarder: <addr> → handle <N> (Pong)
```

**Pass criteria**:
1. B side `payload-200mib.bin` size = exactly 209715200 bytes
2. B side sha256 == A side sha256 (byte-level identity)
3. A→B duration **< 30 s** (100 Mbps LAN target; theoretical 16 s + QUIC overhead)
4. No `.partial` files left behind (default `keep_partial=false`)
5. Re-inject visible in A's log (if `inject_to_clipboard=true`)

#### 1.4 Reverse direction: B → A (timing required)

Repeat the same scenario in reverse: B initiates copy, A receives.

```bash
# ─── B side ───
date +%s.%N | tee /tmp/transfer-b2a-start.txt

# ─── A side ───
# Wait for file to appear on A
sha256sum "$HOME/Downloads/lan-mouse/payload-200mib.bin"
date +%s.%N | tee /tmp/transfer-b2a-end.txt

# Verify B→A duration < 30s
python3 -c "
import sys
start = float(open('/tmp/transfer-b2a-start.txt').read().strip())
end = float(open('/tmp/transfer-b2a-end.txt').read().strip())
print(f'B→A duration: {end - start:.2f}s (budget < 30s)')
assert end - start < 30.0, f'FAIL: B→A took {end - start:.2f}s, exceeds 30s budget'
"
```

**Pass criteria**: B→A duration < 30 s, sha256 matches.

---

### Scenario S2 — 200 MiB Wi-Fi performance (typical 30-50% of wired bandwidth, **双向**)

**Covers**: same as S1, but on Wi-Fi (typical 5 GHz: 100-150 Mbps effective).
Budget: **< 60 s** per direction.

#### 2.1 Setup

- Disconnect wired LAN cables (or note actual link type via `networksetup -listallhardwareports` on macOS / `Get-NetAdapter` on Windows / `iwconfig` on Linux)
- Both peers on the same 5 GHz Wi-Fi SSID
- Verify no other heavy traffic (e.g., pause any cloud sync / Steam downloads)

#### 2.2 Push A → B over Wi-Fi (timing required)

Same procedure as S1 §1.2-§1.3, but budget is **< 60 s** instead of 30 s.

**Pass criteria**:
1. B side landed file size + sha256 == A side
2. A→B duration **< 60 s** (Wi-Fi budget)
3. Re-inject visible (if `inject_to_clipboard=true`)

#### 2.3 Reverse direction B → A

Same as S1 §1.4 with **< 60 s** budget.

---

### Scenario S3 — Cancel mid-transfer (**双向**)

**Covers**: 3a.5 cancel + supersede + M5 STEP-5.1 `.partial` cleanup.

#### 3.1 Push A → B, cancel within 1 s

```bash
# ─── A side ───
date +%s.%N | tee /tmp/cancel-a2b-start.txt
# Copy file to clipboard (same as S1 §1.2)
open /tmp/payload-200mib.bin
sleep 0.3   # wait for daemon tick (500ms) to dispatch Meta
# IMMEDIATELY overwrite the clipboard with another file → triggers cancel
echo "cancel-sentinel" | pbcopy   # macOS
# OR: copy a small file
# OR: same `cp` + drag-drop a smaller file to overwrite

# ─── B side ───
# Wait 2 s
sleep 2
ls -la "$HOME/Downloads/lan-mouse/"
# → expect either: no `payload-200mib.bin` (cancelled before fetch),
#   OR: `payload-200mib.bin.partial` was deleted (cancelled mid-fetch,
#   default keep_partial=false)
# → expect: NO `payload-200mib.bin` (full file) since cancel hit before
#   write completed
date +%s.%N | tee /tmp/cancel-a2b-end.txt
```

**Expected log (B side)**:

```text
# Cancel detected mid-fetch (or mid-write)
[DEBUG lan_mouse::service] apply_inbound_files_task cancel received for sha=<sha>
[INFO  lan_mouse::service] file apply cancelled: <path> (.partial removed)
# OR:
[INFO  lan_mouse::service] file apply cancelled: no .partial (cancel hit before fetch)
```

**Pass criteria**:
1. B side has NO `payload-200mib.bin` (full file)
2. B side has NO `payload-200mib.bin.partial` (cancelled + default cleanup)
3. Cancel latency < 1 s (from A overwrites clipboard to B cleanup)
4. No `ERROR` / panic lines on either side

#### 3.2 Reverse direction: B → A cancel

Same as §3.1 in reverse.

---

### Scenario S4 — Disconnect mid-transfer (**双向**)

**Covers**: M5 STEP-5.1 `FrontendEvent::FileTransferFailed { sha256, reason, ts_ms }`
+ `.partial` cleanup + GUI toast notification.

#### 4.1 Push A → B, **disconnect B's network** during transfer

```bash
# ─── A side ───
date +%s.%N | tee /tmp/disconnect-a2b-start.txt
# Copy file to clipboard (same as S1 §1.2)
open /tmp/payload-200mib.bin
# Wait 1 s (transfer should be in progress, ~5% complete at 100 Mbps)
sleep 1

# ─── B side: SIMULATE NETWORK DISCONNECT ───
# macOS:  networksetup -setairportpower en0 off   (Wi-Fi)
#         sudo ifconfig <iface> down              (wired)
# Linux:  sudo ip link set <iface> down
#         nmcli device disconnect <iface>
# Windows: Disable-NetAdapter -Name "Ethernet" -Confirm:$false
#          (or just unplug the cable)

# Wait 5 s
sleep 5

# ─── A side: check FileTransferFailed ───
# (the IPC event is observable in the A side log when it tries to push)
grep -E "FileTransferFailed|connection lost|stream error" /var/log/lan-mouse-A.log | tail -5
# → expect: stream error + cleanup + WAKE_CLOSE_CODE force-close

# ─── B side: check .partial cleanup ───
ls -la "$HOME/Downloads/lan-mouse/" | grep -E "payload-200mib.bin"
# → expect: NO .partial file (default keep_partial=false)
date +%s.%N | tee /tmp/disconnect-a2b-end.txt

# ─── B side: RE-ENABLE NETWORK ───
# (reverse the above commands)
```

**Expected log (A side)**:

```text
# t+~1s: stream error during HTTP/3 GET
[WARN  lan_mouse::service::clipboard] HTTP/3 GET /clipboard/file/<sha> → stream error: ConnectionReset
[INFO  lan_mouse::service] apply_inbound_files_task error: io::ErrorKind::ConnectionReset
[INFO  lan_mouse::service] notify_frontend: FrontendEvent::FileTransferFailed { sha256: <sha>, reason: "connection lost", ts_ms: <epoch_ms> }
[INFO  lan_mouse::service] .partial cleanup: <path>.partial removed
```

**Expected log (B side, after re-enabling network)**:

```text
# Reconnect on next quic supervisor tick (1-2 s)
[INFO  lan_mouse::connect] reconnecting to peer <addr> after disconnect
[INFO  lan_mouse] connected to peer: <peer>.local:4252
```

**Pass criteria**:
1. A side log shows `FileTransferFailed` event with `reason: "connection lost"` (or `"timeout"` depending on QUIC layer)
2. A side latency from disconnect to `FileTransferFailed` event **< 5 s**
3. B side has NO `.partial` file (default cleanup)
4. B side re-establishes QUIC link within 2 s of network re-enable
5. If GUI is connected, Toaster shows "File transfer failed: connection lost" notification (M5 STEP-5.1 user-facing toast — out of unit-test scope)

#### 4.2 Reverse direction: B → A disconnect

Same as §4.1 in reverse (B initiates, A receives, A's network drops).

---

### Scenario S5 — keepalive↔idle race (**structural**)

**Covers**: M0c Ping/Pong keepalive + M5 STEP-5.2 keepalive↔idle race专项.

#### 5.1 Setup

After completing S1 (200 MiB wired transfer), leave both peers idle.
Do **NOT** touch clipboard / files / mouse / keyboard for the duration.

#### 5.2 Observe connection survival over 30 s of silence

```bash
# ─── A side (start observing) ───
date +%s.%N | tee /tmp/idle-a2b-start.txt
# Watch QUIC link activity
lsof -i UDP:4252 -P 2>/dev/null | head -10
# OR: netstat -anu | grep 4252 (Linux/macOS)
# OR: ss -u 'sport = :4252' (Linux)

# Sleep 30 s
sleep 30

# ─── A side (after 30 s) ───
date +%s.%N | tee /tmp/idle-a2b-end.txt
# Re-check QUIC link
lsof -i UDP:4252 -P 2>/dev/null | head -10
# → expect: still connected (UDP socket still listed with peer address)
sha256sum /tmp/payload-200mib.bin   # any non-clipboard activity
# (this is just to verify the daemon is responsive; the connection must
#  have stayed alive throughout the 30 s silence)

# Verify Pong intervals ≤ 600 ms in A's log
grep "Pong\|pong" /var/log/lan-mouse-A.log | tail -20
# → expect: Pong lines every ~500 ms throughout the 30 s window
```

**Expected log (A side, during 30 s silence)**:

```text
# Ping fires every 500 ms
[TRACE lan_mouse::connect] ping_heartbeat: sent Ping to <addr> (t=0.5s)
[TRACE lan_mouse::connect] ping_heartbeat: sent Ping to <addr> (t=1.0s)
...
[TRACE lan_mouse::connect] ping_heartbeat: sent Ping to <addr> (t=29.5s)

# Pong arrives within ~50-100 ms of each Ping
[TRACE lan_mouse::connect] stream A forwarder: <addr> → handle <N> (Pong)
[TRACE lan_mouse::connect] stream A forwarder: <addr> → handle <N> (Pong)
... (one Pong per Ping, ~500ms cadence)
```

**Pass criteria**:
1. Connection still alive after 30 s of silence (UDP socket still listed)
2. Pong interval **≤ 600 ms** (verified via log timestamps: each Pong line
   should be < 600 ms after the previous Pong line)
3. NO `pong_health_watchdog` warnings (would fire if Pong gap > 3.5 s)
4. NO `max_idle_timeout` warnings (would fire if QUIC layer closes)

#### 5.3 Extend to 60 s of silence (additional guarantee)

```bash
# Continue sleeping (already at 30 s)
sleep 30
# Total: 60 s of silence
date +%s.%N | tee /tmp/idle-a2b-60s.txt

# Re-check QUIC link
lsof -i UDP:4252 -P 2>/dev/null | head -10
# → expect: still connected
```

**Pass criteria**: Connection still alive after **60 s** of total silence.

---

## 2. Cross-platform peer-pair groups

Each group runs §1 S1 + S2 + S3 + S4 + S5 in each direction. The total
matrix is **5 scenarios × 2 directions × 3 groups = 30 cells** for full
coverage (S5 is structural and may be run only once on any pair group).

### 2.1 macOS ↔ Windows

**Setup** — A = macOS, B = Windows (PowerShell). Both on the same wired
LAN (or Wi-Fi for S2). mDNS resolving `mac-a.local` / `win-b`; firewall
4252/UDP open.

| Direction | S1 (LAN) | S2 (Wi-Fi) | S3 (cancel) | S4 (拔网) | S5 (race) |
|---|---|---|---|---|---|
| **A(macOS) → B(Win)** | ☐ | ☐ | ☐ | ☐ | ☐ |
| **B(Win) → A(macOS)** | ☐ | ☐ | ☐ | ☐ | ☐ |

#### macOS-side command map
- Copy file to clipboard: open in Finder, `Cmd-C` (file promise)
- Compute sha256: `shasum -a 256 /tmp/payload-200mib.bin`
- Watch link: `lsof -i UDP:4252 -P`
- Disconnect: `sudo ifconfig en0 down` (wired) / `networksetup -setairportpower en0 off` (Wi-Fi)

#### Windows-side command map
- Copy file to clipboard: PowerShell 7+ `Set-Clipboard -Path C:\tmp\payload-200mib.bin`
  (legacy path: open in Explorer + `Ctrl-C`)
- Compute sha256: `Get-FileHash -Algorithm SHA256 C:\tmp\payload-200mib.bin`
- Watch link: `netstat -ano | findstr :4252`
- Disconnect: `Disable-NetAdapter -Name "Ethernet" -Confirm:$false`

#### Known platform quirks
- **macOS Finder file-clipboard is a "promise"**: the actual file bytes are
  fetched from disk lazily by the destination app. lan-mouse's
  `backend.current_files()` reads the file directly via
  `NSPasteboard.general().propertyList(forType: .fileURL)` — works
  consistently.
- **PowerShell `Set-Clipboard -Path`** is PowerShell 7+ only; legacy path
  requires an interactive file-manager copy.

---

### 2.2 macOS ↔ Linux

**Setup** — A = macOS, B = Linux (X11 or Wayland). Same LAN / Wi-Fi.

| Direction | S1 (LAN) | S2 (Wi-Fi) | S3 (cancel) | S4 (拔网) | S5 (race) |
|---|---|---|---|---|---|
| **A(macOS) → B(Linux)** | ☐ | ☐ | ☐ | ☐ | ☐ |
| **B(Linux) → A(macOS)** | ☐ | ☐ | ☐ | ☐ | ☐ |

#### macOS-side command map
- Same as §2.1.

#### Linux X11 command map
- Copy file to clipboard: `xclip -selection clipboard -t text/uri-list -i /tmp/payload-200mib.bin`
- Compute sha256: `sha256sum /tmp/payload-200mib.bin`
- Watch link: `ss -u 'sport = :4252'`
- Disconnect: `sudo ip link set <iface> down`

#### Linux Wayland command map
- Copy file to clipboard: `wl-copy < /tmp/payload-200mib.bin` (auto-detects mime from extension)
- Other commands: same as X11.

#### Known platform quirks
- **X11 vs Wayland mismatch**: if A runs X11 and B runs Wayland, lan-mouse
  on B fails to read X11's clipboard (no IPC bridge). Both sides must use
  the same display protocol, or B must have `xclip` installed (XWayland
  fallback).

---

### 2.3 Windows ↔ Linux

**Setup** — A = Windows, B = Linux (X11 or Wayland). Same LAN / Wi-Fi.

| Direction | S1 (LAN) | S2 (Wi-Fi) | S3 (cancel) | S4 (拔网) | S5 (race) |
|---|---|---|---|---|---|
| **A(Win) → B(Linux)** | ☐ | ☐ | ☐ | ☐ | ☐ |
| **B(Linux) → A(Win)** | ☐ | ☐ | ☐ | ☐ | ☐ |

#### Windows / Linux command maps
- Same as §2.1 (Windows) / §2.2 (Linux).

---

## 3. Result capture template

For each peer group + direction + scenario, record one row:

```text
Group:      macOS ↔ Windows
Direction:  A(macOS) → B(Win)
Scenario:   S1 (200 MiB wired LAN)
Date:       2026-09-XX HH:MM
Operator:   <your name>

A-side (macOS) — sha256sum of source file:
  <sha256-A>  /tmp/payload-200mib.bin

B-side (Windows) — sha256sum of landed file:
  <sha256-B>  C:\Users\me\Downloads\lan-mouse\payload-200mib.bin

B-side landed file size:                                <N> bytes (expect 209715200)
sha256-A == sha256-B?                                   [YES / NO]
Transfer duration (A→B):                                <N.NN> s (budget < 30 s)
Re-inject visible in A log (set_files BackendCmd)?      [YES / NO / N/A if inject_to_clipboard=false]
.partial file present after transfer?                   [YES / NO]
Cancel/拔网 timing (if applicable):                       <N.NN> s

Evidence (paste 5-10 log lines from both sides):
  A: [INFO  lan_mouse::service] dispatching files: 1 entries, total 209715200 bytes, sha256=<sha>
  A: [INFO  lan_mouse::service] HTTP/3 GET /clipboard/file/<sha> → 200
  B: [INFO  lan_mouse::service] file apply succeeded: <path> size=209715200

Result:  ✅ PASS / ⚠️ PASS with caveat / ❌ FAIL
Notes:   (any caveats, e.g. "Wi-Fi cell congested during transfer")
```

After running all 30 cells, aggregate the timing / sha256 / cancel-latency /
拔网-latency / keepalive-interval / 60s-survival results into the M5
validator input.

### Pass-criteria summary

| Cell type | Expected outcome |
|---|---|
| S1 any direction | < 30 s; sha256 match; no .partial |
| S2 any direction | < 60 s; sha256 match; no .partial |
| S3 any direction | cancel latency < 1 s; no .partial; no full file |
| S4 any direction | FileTransferFailed event within 5 s; no .partial; reconnect within 2 s after network re-enable |
| S5 structural | 30 s + 60 s of silence, connection still alive; Pong interval ≤ 600 ms throughout |

---

## 4. Troubleshooting

### Symptom: S1 / S2 transfer > 30 s / 60 s budget

- **Check link type**: `netstat` / `lsof` should show the negotiated link
  speed. A 100 Mbps wired link has theoretical 12.5 MB/s → 200 MiB should
  take ~16 s. A Wi-Fi link at 50% of wired = 25 s. If your measurement is
  much higher, the link is likely Wi-Fi misclassified as wired (or vice
  versa).
- **Check CPU**: `htop` / Activity Monitor during the transfer — if a
  core is pinned at 100%, sha256 streaming is the bottleneck (200 MiB →
  ~5-8 s of pure sha256 on a modern x86). Re-run after closing heavy
  workloads.
- **Check disk**: a slow SSD / network share under `accept_dir` will cap
  the throughput. Use `/tmp` (tmpfs on macOS / Linux) for measurement.

### Symptom: S3 cancel latency > 1 s

- **Source-side**: A's dispatcher must observe the supersede within the
  next 500 ms tick after the overwrite. If you copy-cancel-copy with
  spacing > 500 ms, two dispatches happen, the second one cancelling
  the first. Verify the timing in A's log.
- **Receiver-side**: B's `apply_inbound_files_task` should observe the
  cancel oneshot within milliseconds of A's cancel push. If latency is
  > 1 s, check the StreamC link for backpressure.

### Symptom: S4 拔网 doesn't produce FileTransferFailed event

- **Wait longer**: the watchdog tolerance is 3.5 s (PONG_HEALTH_TIMEOUT).
  If the QUIC layer takes longer to detect the disconnect, the
  FileTransferFailed event may arrive 3-4 s after the network drop.
- **Check StreamC link**: if StreamC is in a different QUIC stream
  priority than the HTTP/3 file route, the disconnect signal may be
  delayed. This is a known race — see SUGGESTION-FIXED #S-12.

### Symptom: S5 30 s silence, connection drops

This is a **regression**. Check:
- `pong_health_watchdog` is alive: `grep "Pong health watchdog" /var/log/lan-mouse-A.log`
- `last_pong_at` is being updated: trace log should show one Pong per Ping
- QUIC keepalive is firing: `grep "keep_alive\|keepalive" /var/log/lan-mouse-A.log`

If `last_pong_at` updates correctly but the connection still drops, file
a bug in `next/SUGGESTION.md` with the full log excerpt.

### Symptom: .partial file remains after S4

If `keep_partial=false` (default), the daemon should remove `.partial`
on sha256 mismatch / cancel / 拔网. If the file remains, check
`src/service.rs::write_and_verify_file_blocking` — the cleanup branch
should fire. If `keep_partial=true`, the file is intentionally retained
(postmortem).

### Symptom: B-side doesn't auto-reconnect after S4

- The supervisor (`spawn_peer_supervisor`) should reconnect within 1-2 s
  of network re-enable. If it doesn't, check `record_retry_failure`
  backoff (1s → 2s → 4s → 8s cap) and `MAX_RETRY_FAILURES_BEFORE_OFFLINE`
  (5 failures → log error).

---

## 6. Out of scope (do NOT execute as part of this matrix)

- **Text clipboard** — M1b scope; see `tests/manual/clipboard-text.md`.
- **Image clipboard** — M2b scope; see `tests/manual/clipboard-image.md`.
- **HTML / RTF clipboard format negotiation** — out of PLAN-2 scope.
- **Multi-file clipboard** (multiple files in one Cmd+C) — M3a already
  supports multi-file; the matrix above tests **1 file** for timing
  purity. Multi-file scenarios can be added in a follow-up.
- **断点续传** — HTTP/3 `?range=` interface exists (M3a) but no
  daemon-side resume logic. If 拔网 hits mid-transfer, the file must be
  re-sent in full (per PLAN §0 Out of Scope).
- **macOS TCC permissions** for clipboard — covered in `clipboard-text.md`
  §4.
- **GUI Toaster notifications** for FileTransferFailed — out of
  STEP-5.2 scope (M5 STEP-5.3 lands Vue IPC binding for the toast; the
  CLI binding is STEP-5.5). The 拔网 真机 test verifies the **backend
  event push**, not the **GUI surface**.

---

> **本文档定稿时间**：2026-09-13
> **作者**：plan-step-executor
> **下一步**：人类真机测试 → M5 STEP-5.3（Vue IPC 绑定 + ClipboardConfigChanged IPC）