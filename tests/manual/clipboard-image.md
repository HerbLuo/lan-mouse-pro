# Cross-platform clipboard image end-to-end test (M2b STEP-2b.3)

> **Goal**: validate the full M2b clipboard image pipeline — PNG / JPG / DIB
> byte-level fidelity, macOS TIFF→PNG normalisation (评审 #2 3rd), Windows
> DIB round-trip (评审 #4 + #3 3rd), and image-branch loopback LRU (32 / 60 s
> per M2a STEP-2a.4) — across all three cross-platform peer combinations:
>
> - **macOS ↔ Windows** (DIB direct path on Windows side; PNG normalisation
>   on macOS side; SHA-256 byte-level identical for Windows ↔ Windows)
> - **macOS ↔ Linux** (PNG path on both sides; SHA-256 byte-level identical)
> - **Windows ↔ Linux** (DIB→PNG fallback on Linux; "visually consistent"
>   rather than byte-level identical — see §4 troubleshooting)
>
> All peers run `lan-mouse` daemons with `enable_clipboard_to = true`,
> pre-validated QUIC mTLS pairing, and the M2a / M2b backends from
> STEP-2a.1 / 2a.2 / 2a.3 / 2a.4 / 2b.1 / 2b.2.
>
> **Owner**: human operator (this template is the checklist; image clipboard
> events touch OS pasteboard / Win32 / X11 / Wayland and cannot be exercised
> from a headless CI matrix).
>
> **Related STEPs**: 2a.1 (image trait + mime detection), 2a.2 (macOS NSPasteboard
> image + TIFF→PNG normalisation), 2a.3 (image outbound + HTTP/3 image route),
> 2a.4 (image inbound + image loopback LRU 32/60s), 2b.1 (Windows CF_DIBV5 +
> macOS NSImage spike + image-crate fallback), 2b.2 (Linux X11 / Wayland
> image + DIB→PNG fallback).

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
RUST_LOG="lan_mouse=debug,lan_mouse::service::clipboard=trace,lan_mouse::clipboard::macos=trace" \
  ./target/release/lan-mouse

# ─── Windows (PowerShell) ───
$env:RUST_LOG = "lan_mouse=debug,lan_mouse::service::clipboard=trace,lan_mouse::clipboard::windows=trace"
.\target\release\lan-mouse.exe

# ─── Linux (X11) ───
RUST_LOG="lan_mouse=debug,lan_mouse::service::clipboard=trace,lan_mouse::clipboard::linux=trace" \
  ./target/release/lan-mouse

# ─── Linux (Wayland) ───
# same command; lan-mouse auto-detects wl-paste / wl-copy vs xclip (XWayland fallback)
```

### 0.3 Pairing + clipboard config

Verify each machine's `config.toml` contains a peer entry and the
image-clipboard switches are enabled:

```toml
[[clients]]
host = "peer.local"
port = 4252
trust_address = true
enable_clipboard_to = true   # ← must be true for cross-device image sync

[clipboard]
# daemon-global; defaults are fine for this matrix
# ignore_images = false       # default: sync images enabled
```

**Daemon log check** — within 5 s of launch, both sides should print:

```text
[INFO  lan_mouse] connected to peer: <peer>.local:4252
[INFO  lan_mouse::clipboard] backend ready: <macos: NSPasteboard | windows: OpenClipboard | linux: xclip / wl-paste>
[TRACE lan_mouse::service::clipboard] hit rate: skip=0 allow=0 (0/0)   # no traffic yet
```

### 0.4 Per-platform image clipboard command reference

| Action | macOS | Windows (PowerShell) | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| **Capture full screen** | `screencapture -x -t png /tmp/4k.png` | Snipping Tool → New (auto-clipboard) | `gnome-screenshot -f /tmp/4k.png` / `spectacle -b -o /tmp/4k.png` | `wlr-screencopy` or `grim /tmp/4k.png` |
| **Copy file to clipboard** | `osascript -e 'set the clipboard to (read (POSIX file "/tmp/4k.png") as «class PNGf»)'` (or open in Preview.app, Cmd-A → Cmd-C) | `Get-Item C:\tmp\4k.png \| Set-Clipboard` (PowerShell 7+ only) — fallback: open in Paint → Ctrl-A → Ctrl-C | `xclip -selection clipboard -t image/png -i /tmp/4k.png` | `wl-copy < /tmp/4k.png` (auto-detect mime from extension) |
| **Paste to file (image)** | `osascript -e 'set pngData to (the clipboard as «class PNGf»)' -e 'set f to open for access (POSIX file "/tmp/back.png") with write permission' -e 'write pngData to f' -e 'close access f'` (or open Preview.app, Cmd-N → Cmd-S to file) | `Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::GetImage().Save('C:\tmp\back.png', [System.Drawing.Imaging.ImageFormat]::Png)` | `xclip -selection clipboard -t image/png -o > /tmp/back.png` | `wl-paste --type image/png > /tmp/back.png` |
| **Compute sha256** | `shasum -a 256 /tmp/4k.png` | `Get-FileHash -Algorithm SHA256 C:\tmp\4k.png \| Select-Object -ExpandProperty Hash` | `sha256sum /tmp/4k.png` | `sha256sum /tmp/4k.png` |
| **Read first 16 bytes (xxd)** | `xxd /tmp/4k.png \| head -1` | `Format-Hex C:\tmp\4k.png -Count 16 \| Format-Table` | `xxd /tmp/4k.png \| head -1` | `xxd /tmp/4k.png \| head -1` |

> **Daemon log on every OS clipboard image change** (≈ 500 ms tick):
> `[DEBUG lan_mouse::service::clipboard] image change detected sha256=<hex> size=<N> mime=<png|jpg|dib>`

### 0.5 Image-magic detection (sanity check the fixtures)

| Format | First 4 bytes (hex) | `file(1)` magic name |
|---|---|---|
| PNG | `89 50 4E 47` (`\x89PNG`) | PNG image data |
| JPEG | `FF D8 FF` (start-of-image) | JPEG image data (JFIF/Exif) |
| BMP | `42 4D` (`BM`) | PC bitmap |
| DIB (CF_DIBV5 raw, no file header) | `28 00 00 00` (BITMAPINFOHEADER biSize = 40) → `6C 00 00 00` (BITMAPV5HEADER biSize = 124) | (no standard `file(1)` magic) |

Quick sanity on each fixture:

```bash
# ─── Source side (any peer) ───
file /tmp/4k.png           # → "PNG image data, 3840 x 2160, 8-bit/color RGBA, non-interlaced"
file /tmp/1080p.jpg        # → "JPEG image data, JFIF standard ..."
xxd /tmp/4k.png | head -1  # first line of hex dump — confirm 89 50 4E 47
```

### 0.6 Fixture preparation (one-time, on any peer with shell access)

```bash
# ─── macOS (screencapture native) ───
screencapture -x -t png /tmp/4k.png
# (or use an open-source 4K sample: e.g. `curl -o /tmp/4k.png https://example.com/4k.png`)

# ─── Linux (ImageMagick fallback for a deterministic 4K fixture) ───
magick -size 3840x2160 gradient: /tmp/4k.png    # deterministic 3840×2160 RGBA gradient

# ─── 1080p JPG fixture (any platform) ───
magick -size 1920x1080 plasma: /tmp/1080p.jpg   # deterministic 1920×1080 JPG
# OR
curl -o /tmp/1080p.jpg https://example.com/sample.jpg

# Compute reference sha256
sha256sum /tmp/4k.png /tmp/1080p.jpg | tee /tmp/fixtures.sha256
```

> **Determinism**: PNG screenshots taken via `screencapture` may include the
> mouse cursor, transient UI overlays, or wall-clock-driven wallpaper — this
> breaks byte-level fidelity across runs. Use the **ImageMagick fixture**
> (`magick gradient:`) for the canonical byte-level pass, and reserve
> `screencapture` for a single human-judgement run per platform.

---

## 1. Test scenarios (apply to each peer group in §2)

Each peer group (§2) runs the **four scenarios** below in order. Mark each
checkbox with ✅ / ❌ + one-line evidence (log excerpt / `sha256sum` output)
before moving on. If any scenario fails, copy the daemon log snippet into
`next/SUGGESTION.md` and pause the matrix.

### Scenario S1 — 4K screenshot (5–15 MiB PNG, byte-level identity)

**Covers**: 2a.2 macOS NSPasteboard `.png` direct read + TIFF→PNG fallback,
2a.3 image outbound + HTTP/3 image route, 2a.4 image inbound + image LRU
loopback, 2b.1 Windows `CF_DIBV5` direct round-trip (Windows ↔ Windows),
2b.2 Linux X11/Wayland PNG path.

#### 1.1 Generate 4K PNG fixture

```bash
# ─── A side (any peer with ImageMagick — deterministic gradient) ───
magick -size 3840x2160 gradient: /tmp/4k.png
wc -c /tmp/4k.png                                  # → expect ~6–8 KiB (gradient compresses well)
# If ImageMagick not installed: screencapture -x -t png /tmp/4k.png (5-15 MiB, non-deterministic)

# Or use a more realistic 4K screenshot (random-pixel, no compression)
magick -size 3840x2160 plasma: -depth 8 /tmp/4k.png   # ~1-2 MiB
sha256sum /tmp/4k.png                                 # → <sha_a>  ← SAVE THIS
xxd /tmp/4k.png | head -1                             # → confirm 89 50 4E 47 (PNG magic)
```

#### 1.2 Push to clipboard

```bash
# ─── A side ───
# macOS: Preview.app open → File → Open → /tmp/4k.png → Cmd-A → Cmd-C
#         OR: osascript -e 'set the clipboard to (read (POSIX file "/tmp/4k.png") as «class PNGf»)'
# Windows: open /tmp/4k.png in Paint → Ctrl-A → Ctrl-C
#          OR: PowerShell 7+: Get-Item /tmp/4k.png | Set-Clipboard
# Linux X11: xclip -selection clipboard -t image/png -i /tmp/4k.png
# Linux Wayland: wl-copy < /tmp/4k.png
<COPY_FILE_TO_CLIPBOARD> /tmp/4k.png

# Wait 1 s for daemon tick (500 ms poll) + StreamC push + HTTP/3 pull initiation
sleep 1
```

**Expected log (A side)**:

```text
[DEBUG lan_mouse::service::clipboard] image change detected sha256=<sha_a> size=<N> mime=image/png
[DEBUG lan_mouse::service::clipboard] sending ClipboardImage (meta) to peer sha256=<sha_a> mime=image/png
[DEBUG lan_mouse::service::clipboard] clipboard cache: evicted prev outbound sha=<sha_prev>
[TRACE lan_mouse::service::clipboard] clipboard cache: inserted sha256=<sha_a> size=<N>
```

**Expected log (B side)**:

```text
[DEBUG lan_mouse::service::clipboard] inbound ClipboardImage (meta) sha256=<sha_a> mime=image/png
[DEBUG lan_mouse::service::clipboard] HTTP/3 GET /clipboard/image/<sha_a> → 200
[DEBUG lan_mouse::service::clipboard] apply_inbound_clipboard_image (size=<N>)
[DEBUG lan_mouse::service::clipboard] mark_local_image_write sha256=<sha_a>  # image LRU (32/60s)
[TRACE lan_mouse::service::clipboard] hit rate: skip=0 allow=1 (0%)
```

#### 1.3 Verify B side round-trip

```bash
# ─── B side ───
# macOS: Preview.app New from Clipboard → File → Save As /tmp/back.png
#         OR: osascript snippet from §0.4 row "Paste to file (image)"
# Windows: PowerShell [System.Windows.Forms.Clipboard]::GetImage().Save('C:\tmp\back.png', …)
# Linux X11: xclip -selection clipboard -t image/png -o > /tmp/back.png
# Linux Wayland: wl-paste --type image/png > /tmp/back.png
<PASTE_TO_FILE> /tmp/back.png

sha256sum /tmp/back.png                            # → <sha_b>
xxd /tmp/back.png | head -1                        # → confirm 89 50 4E 47 (PNG magic preserved)

diff /tmp/4k.png /tmp/back.png                     # → exit 0 (byte-identical)
[ "$sha_a" = "$sha_b" ] && echo "BYTE-IDENTICAL ✓" || echo "MISMATCH ✗"
```

**Pass criteria**:
1. `diff` exits 0; `sha256sum` matches A-side
2. The apply log line appears within 2 s of the source-side `change detected`
3. PNG magic `89 50 4E 47` preserved on B side (no format conversion to BMP / JPEG)
4. **Exception** (document, do not fail): `Windows ↔ Linux` and
   `Linux → Windows` byte-level identity is **not guaranteed** — Linux
   backend converts DIB→PNG via `image crate` (#S-4 alpha limitation
   transparency detail); see §4 troubleshooting.

---

### Scenario S2 — 1080p JPG (≈ 100–500 KiB, byte-level identity for PNG/JPG path)

**Covers**: 2a.2 macOS JPEG read (fallback path), 2a.3 image outbound
(JPG mime on wire), 2a.4 image inbound, 2b.1 Windows BMP path / JPG
unsupported path (image crate fallback).

#### 2.1 Generate 1080p JPG fixture

```bash
# ─── A side ───
magick -size 1920x1080 plasma: -quality 85 /tmp/1080p.jpg
wc -c /tmp/1080p.jpg                                # → ~100-500 KiB (quality 85)
sha256sum /tmp/1080p.jpg                            # → <sha_a>
xxd /tmp/1080p.jpg | head -1                        # → confirm FF D8 FF (JPEG magic)
```

#### 2.2 Push to clipboard

```bash
# Same as S1 §1.2 (copy file → clipboard)
<COPY_FILE_TO_CLIPBOARD> /tmp/1080p.jpg
sleep 1
```

**Expected log (A side)**:

```text
[DEBUG lan_mouse::service::clipboard] image change detected sha256=<sha_a> size=<N> mime=image/jpeg
[DEBUG lan_mouse::service::clipboard] sending ClipboardImage (meta) to peer sha256=<sha_a> mime=image/jpeg
[DEBUG lan_mouse::service::clipboard] clipboard cache: inserted sha256=<sha_a> size=<N>
```

> **Note on macOS**: macOS clipboard typically holds image data as PNG
> internally (Preview.app, Safari, Finder re-encode to PNG). The wire mime
> label depends on the source app; in practice `image/png` is more common
> than `image/jpeg`. The daemon logs the actual mime it observed; both are
> accepted on the receive side.

**Expected log (B side)**:

```text
[DEBUG lan_mouse::service::clipboard] inbound ClipboardImage (meta) sha256=<sha_a> mime=image/jpeg
[DEBUG lan_mouse::service::clipboard] HTTP/3 GET /clipboard/image/<sha_a> → 200
[DEBUG lan_mouse::service::clipboard] apply_inbound_clipboard_image (size=<N>)
[DEBUG lan_mouse::service::clipboard] mark_local_image_write sha256=<sha_a>
[TRACE lan_mouse::service::clipboard] hit rate: skip=0 allow=1 (0%)
```

#### 2.3 Verify B side round-trip

```bash
# ─── B side (paste to file) ───
<PASTE_TO_FILE> /tmp/back.jpg

sha256sum /tmp/back.jpg                              # → <sha_b>
xxd /tmp/back.jpg | head -1                          # → confirm FF D8 FF (or PNG if receiver re-encoded)

diff /tmp/1080p.jpg /tmp/back.jpg                     # → exit 0 (byte-identical)
[ "$sha_a" = "$sha_b" ] && echo "BYTE-IDENTICAL ✓" || echo "MISMATCH ✗"
```

**Pass criteria**:
1. `diff` exits 0; `sha256sum` matches A-side
2. JPEG magic `FF D8 FF` preserved on B side (no PNG re-encode)
3. **Exception**: `Windows` receiver with `image/jpeg` mime → windows.rs
   `set_image(Mime::Jpeg)` returns `Err(Unsupported)` (only PNG / DIB paths
   implemented per 2b.1 scope). Daemon logs warn + skips; B's clipboard
   remains empty. Mark as **"known unsupported"**, not fail.

---

### Scenario S3 — Preview.app TIFF→PNG normalisation (macOS-only, all 3 peer directions)

**Covers**: 2a.2 macOS `data(forType: .tiff)` fallback → `image` crate
TIFF decode → forced PNG re-encode. The fallback path is the one case
where the **source daemon** rewrites the clipboard image bytes — without
this normalisation, Preview.app copies (TIFF only) would never reach
byte-level identity on any peer.

#### 3.1 macOS side: capture Preview.app TIFF clipboard

```bash
# ─── A side = macOS ───
# 1. Open /tmp/4k.png in Preview.app
open /tmp/4k.png
# 2. Wait for Preview.app to render (~1 s)
sleep 1
# 3. Use the Rectangular Selection tool (Cmd-Option-drag a region)
#    then press Cmd-C to copy selection as TIFF.
#    Alternative: Tools → Select All (Cmd-A) → Cmd-C
#    Alternative: cmd-line (headless, deterministic):
osascript -e '
  tell application "Preview"
    activate
    delay 0.3
  end tell
  tell application "System Events"
    keystroke "a" using command down
    delay 0.2
    keystroke "c" using command down
  end tell
'
sleep 1
```

**Expected log (macOS side — the SOURCE)**:

```text
[DEBUG lan_mouse::service::clipboard] image change detected sha256=<sha_tiff> size=<N> mime=image/tiff
# OR (after image crate TIFF decode):
[DEBUG lan_mouse::clipboard::macos] TIFF-only source — normalising via image crate
[DEBUG lan_mouse::clipboard::macos] TIFF → PNG re-encode: <N1> bytes → <N2> bytes
[DEBUG lan_mouse::service::clipboard] image change detected sha256=<sha_png> size=<N2> mime=image/png
[DEBUG lan_mouse::service::clipboard] sending ClipboardImage (meta) to peer sha256=<sha_png> mime=image/png
```

> **What "TIFF→PNG" looks like in the log**: the first `change detected`
> line shows `mime=image/tiff` with the original TIFF sha; the second
> `change detected` line shows `mime=image/png` with the re-encoded PNG
> sha. **The wire payload is the PNG** (`sha_png`), never the TIFF
> (`sha_tiff`) — this is the byte-level-identity guarantee per PLAN §3
> 评审 #2 3rd.

#### 3.2 Verify B side round-trip

```bash
# ─── B side (any peer) ───
sleep 1
<PASTE_TO_FILE> /tmp/back.png

sha256sum /tmp/back.png                              # → <sha_b>
xxd /tmp/back.png | head -1                          # → confirm 89 50 4E 47 (PNG, not TIFF)

[ "<sha_png>" = "<sha_b>" ] && echo "BYTE-IDENTICAL ✓" || echo "MISMATCH ✗"
```

**Pass criteria**:
1. `sha_b == sha_png` (the re-encoded PNG from step 3.1, **not** the
   original TIFF `sha_tiff`)
2. PNG magic `89 50 4E 47` on B side — TIFF `49 49 2A 00` (little-endian
   Intel byte order) must **not** appear
3. The two log lines on the macOS source are both visible (TIFF read +
   PNG re-encode + wire push)

> **Why this scenario only runs from macOS**: TIFF source is
> macOS-specific (Preview.app). The reverse direction (B=macOS) does not
> apply; the matrix section §2 marks this scenario as **"macOS source
> only"** for the "B→A" direction.

---

### Scenario S4 — Image loopback LRU (no jitter on rapid re-push)

**Covers**: 2a.4 image-branch loopback LRU (32 entries / 60 s TTL per
M2a STEP-2a.4) — image writes are expensive, so the LRU is smaller than
the text branch's 128 / 60 s. After A pushes an image to B, A's local
copy of the same fingerprint must **not** cause A to push again when
the OS tick re-detects the image change.

#### 4.1 Push image, wait, verify LRU mark

```bash
# ─── A side ───
<COPY_FILE_TO_CLIPBOARD> /tmp/4k.png
sleep 1
# Verify A's log shows mark_local_image_write
grep "mark_local_image_write sha256=$(sha256sum /tmp/4k.png | awk '{print $1}')" /var/log/lan-mouse-A.log
# → expect 1 line on A side
```

#### 4.2 Re-paste on A side (simulating "Preview.app reopened / user pasted in same app")

```bash
# ─── A side (same machine) ───
# macOS: Cmd-V on Preview.app (re-pastes the same image)
# Windows: Ctrl-V on Paint (re-pastes the same image)
# Linux X11: xdotool key ctrl+v (or paste in gimp / pinta)
# Linux Wayland: wtype -M ctrl v (or paste in gimp / pinta)
sleep 0.5
# Verify A's log shows image change detected + loopback skip (NOT push)
grep -E "image change detected sha256=($(sha256sum /tmp/4k.png | awk '{print $1}'))" /var/log/lan-mouse-A.log | wc -l
# → expect 2 (first push + second local re-paste)
grep -E "loopback.*image.*skip" /var/log/lan-mouse-A.log | wc -l
# → expect 1 (the re-paste is suppressed)
```

**Expected log (A side)**:

```text
# t0: outbound push to B
[DEBUG lan_mouse::service::clipboard] image change detected sha256=<sha> size=<N>
[DEBUG lan_mouse::service::clipboard] sending ClipboardImage (meta) to peer

# t+0.5: local re-paste in Preview.app — change detected, but LRU marks skip
[DEBUG lan_mouse::service::clipboard] image change detected sha256=<sha> size=<N>
[DEBUG lan_mouse::service::clipboard] loopback LRU hit — skip image push

# t+1.0 (B still receives the t0 push — no jitter expected on B)
[TRACE lan_mouse::service::clipboard] hit rate: skip=1 allow=1 (50%)
```

**Pass criteria**:
1. A's daemon log shows **exactly 2** `image change detected sha256=<sha>`
   lines (t0 + t+0.5)
2. A's daemon log shows **exactly 1** `loopback LRU hit — skip image push`
   line (the t+0.5 re-paste is suppressed)
3. A's `sending ClipboardImage` line count is **1** (only the t0 push; the
   t+0.5 is suppressed)
4. B's daemon log shows **exactly 1** `inbound ClipboardImage` + **exactly
   1** `apply_inbound_clipboard_image` (B receives once, no jitter)
5. **No `apply_inbound_clipboard_image` on A's side** — A did not write
   the image back to its own clipboard (loopback skip pre-empts the
   outbound → inbound round-trip)

> **Distinguishing 2a.4 image LRU vs 2a.4 text LRU**: image LRU uses
> 32 entries / 60 s (smaller because image writes are expensive); text
> LRU uses 128 / 60 s. The hit-rate trace line is shared; the skip
> message says "image" or "text" to distinguish.

---

## 2. Cross-platform peer-pair groups

Each group runs §1 S1 + S2 + S3 (macOS-source only) + S4 in each direction.
The total matrix is **24 cells** (3 groups × 2 directions × 4 scenarios).

### 2.1 macOS ↔ Windows

**Setup** — A = macOS, B = Windows (PowerShell). Both machines on the same
LAN; mDNS resolving `mac-a.local` / `win-b`; firewall 4252/UDP open.

| Direction | S1 (4K) | S2 (1080p JPG) | S3 (TIFF→PNG) | S4 (image loopback) |
|---|---|---|---|---|
| **A(macOS) → B(Win)** | ☐ | ☐ | ☐ | ☐ |
| **B(Win) → A(macOS)** | ☐ | ☐ | n/a (macOS-source only) | ☐ |

#### macOS-side command map
- Capture: `screencapture -x -t png /tmp/4k.png` (or open PNG in Preview.app)
- Copy file: open in Preview.app, Cmd-A → Cmd-C (creates PNG-clipboard) **OR**
  `osascript -e 'set the clipboard to (read (POSIX file "/tmp/4k.png") as «class PNGf»)'`
- Paste to file: open Preview.app, Cmd-N (from clipboard) → Cmd-S to /tmp/back.png
  **OR** `osascript` snippet from §0.4 row "Paste to file (image)"

#### Windows-side command map
- Capture: Snipping Tool → New → auto-clipboard
- Copy file: open in Paint → Ctrl-A → Ctrl-C **OR** PowerShell 7+ `Set-Clipboard -Path C:\tmp\4k.png`
- Paste to file: PowerShell `[System.Windows.Forms.Clipboard]::GetImage().Save('C:\tmp\back.png', …)`

#### Known platform quirks
- **Windows JPEG receive**: `windows.rs::set_image(Mime::Jpeg)` returns
  `Err(Unsupported)` per 2b.1 scope (only PNG / DIB paths implemented).
  Mark S2 as **"known unsupported"** if the receiver is Windows, not fail.
- **Windows DIB payload** (B→A direction, S1): Windows source pushes CF_DIBV5
  raw bytes → wire mime = `application/x-dib` → macOS receiver dispatches
  `set_dib_image` (macos.rs) → `image crate` decode + re-encode PNG → drops
  to NSImage-crate fallback. **Byte-level identity is NOT guaranteed** for
  this direction (PLAN §3 评审 #3 3rd "image-crate fallback is lossy").
  Mark the B(Win)→A(macOS) S1 cell as **"visually consistent only"**.
- **Windows ↔ Windows** (not in this matrix, but for reference): CF_DIBV5
  direct round-trip → 100% byte-level identity. Tested via Microsoft Paint
  Snipping Tool in earlier validation runs.

---

### 2.2 macOS ↔ Linux

**Setup** — A = macOS, B = Linux (X11 or Wayland). Both on same LAN; mDNS
resolving `mac-a.local` / `linux-b.local`; firewall 4252/UDP open.

| Direction | S1 (4K) | S2 (1080p JPG) | S3 (TIFF→PNG) | S4 (image loopback) |
|---|---|---|---|---|
| **A(macOS) → B(Linux)** | ☐ | ☐ | ☐ | ☐ |
| **B(Linux) → A(macOS)** | ☐ | ☐ | n/a (macOS-source only) | ☐ |

#### macOS-side command map
- Same as §2.1.

#### Linux X11 command map
- Capture: `gnome-screenshot -f /tmp/4k.png` / `spectacle -b -o /tmp/4k.png`
  / `flameshot gui -p /tmp/4k.png`
- Copy file: `xclip -selection clipboard -t image/png -i /tmp/4k.png`
- Paste to file: `xclip -selection clipboard -t image/png -o > /tmp/back.png`
- sha256: `sha256sum /tmp/back.png`

#### Linux Wayland command map
- Capture: `grim /tmp/4k.png` (sway) / `wlr-screencopy` (wayfire)
- Copy file: `wl-copy < /tmp/4k.png` (auto-detects mime from extension)
- Paste to file: `wl-paste --type image/png > /tmp/back.png`
- sha256: `sha256sum /tmp/back.png`

#### Known platform quirks
- **X11 vs Wayland mismatch**: if A runs X11 and B runs Wayland, lan-mouse
  on B fails to read X11's clipboard (no IPC bridge). Both sides must use
  the same display protocol, or the B side must have `xclip` installed
  (XWayland fallback per 2b.2).
- **Linux receiver DIB bytes** (B→A direction, S1): if Windows sends
  `application/x-dib` wire label (not in this macOS↔Linux pair, but
  analogous), linux.rs `set_dib_image` → `image crate` decode + re-encode
  PNG → "visually consistent". macOS source never produces DIB.
- **Tool detection on Linux**: on first clipboard read, daemon logs
  `tool detected: xclip` or `tool detected: wl-paste`. If neither is on
  `$PATH`, daemon logs `clipboard backend unavailable: install xclip or
  wl-clipboard` and clipboard sync on that side is silently disabled
  (mouse / keyboard sharing continues to work).

---

### 2.3 Windows ↔ Linux

**Setup** — A = Windows (PowerShell), B = Linux (X11 or Wayland). Same
LAN; firewall 4252/UDP open; Windows hostname resolvable from Linux
(`win-a.local` or via `win-a` NetBIOS).

| Direction | S1 (4K) | S2 (1080p JPG) | S3 (TIFF→PNG) | S4 (image loopback) |
|---|---|---|---|---|
| **A(Win) → B(Linux)** | ☐ | ☐ | n/a (macOS-source only) | ☐ |
| **B(Linux) → A(macOS equivalent)** | ☐ | ☐ | n/a (macOS-source only) | ☐ |
| **B(Linux) → A(Win)** | ☐ | ☐ | n/a (macOS-source only) | ☐ |

#### Windows-side command map
- Capture: Snipping Tool → New → auto-clipboard
- Copy file: PowerShell 7+ `Set-Clipboard -Path C:\tmp\4k.png` **OR** open in Paint → Ctrl-A → Ctrl-C
- Paste to file: PowerShell `[System.Windows.Forms.Clipboard]::GetImage().Save('C:\tmp\back.png', …)`

#### Linux-side command map
- Same as §2.2.

#### Known platform quirks
- **Windows → Linux DIB bytes** (A→B, S1): Windows source pushes CF_DIBV5 →
  wire mime = `application/x-dib` → Linux receiver `set_dib_image` →
  `image crate` decode + re-encode PNG → "visually consistent", NOT
  byte-level identical (#S-4 alpha limitation). Mark this cell as
  **"visually consistent only"**.
- **Linux → Windows PNG bytes** (B→A, S1): Linux source pushes PNG → wire
  mime = `image/png` → Windows receiver `set_image(Mime::Png)` → PNG
  re-encode to DIB via `image crate` (24-bit RGB, no alpha per #S-4) →
  "visually consistent", NOT byte-level identical. Mark this cell as
  **"visually consistent only"**.
- **Windows JPEG receive** (A→B direction, S2): Windows-side linux
  receiver processes JPEG mime label as PNG via `apply_inbound_image_bytes`
  fallback (`Mime::from_label(jpg).unwrap_or(Mime::Png)`). Result: bytes
  are written verbatim to the Linux clipboard with `image/jpeg` mime
  label; `xclip -t image/png -o` will fail, `xclip -t image/jpeg -o` will
  succeed.

---

## 3. Result capture template

For each peer group + direction + scenario, record one row:

```text
Group:       macOS ↔ Windows
Direction:   A(macOS) → B(Win)
Scenario:    S1 (4K screenshot, deterministic ImageMagick fixture)
Date:        2026-09-XX HH:MM
Operator:    <your name>
Source side (macOS) — sha256sum of /tmp/4k.png:
  <sha256-a>  /tmp/4k.png
Sink side (Windows) — sha256sum of C:\tmp\back.png:
  <sha256-b>  C:\tmp\back.png
PNG magic on B side (first 4 bytes hex):  89 50 4E 47 ✓ / ✗
sha256-a == sha256-b?                      [YES / NO / "visually consistent only"]
diff exit code:                             [0 / 1 / 127]
Time to first byte:                        [<N>] ms (from daemon log timestamp diff)
Apply log appeared:                        [YES / NO]
SUGGESTION #S-4 alpha limitation noted?   [YES / NO / N/A]

Evidence (paste 5-10 log lines):
  [DEBUG lan_mouse::service::clipboard] image change detected sha256=...
  [DEBUG lan_mouse::service::clipboard] sending ClipboardImage (meta) to peer ...
  [DEBUG lan_mouse::service::clipboard] HTTP/3 GET /clipboard/image/<sha> → 200
  ...

Result:  ✅ PASS / ⚠️ PASS with caveat (visually consistent only) / ❌ FAIL
Notes:   (any caveats, e.g. "first attempt failed with 404 cache miss; second attempt clean")
```

After running all 24 cells (3 groups × 2 directions × 4 scenarios, with
S3 only for macOS-source directions = 22 effective cells), aggregate
the sha256 results, count of `apply_inbound_clipboard_image` events,
and any `WARN` / `ERROR` log lines into the M2b validator input.

### Pass-criteria summary

| Cell type | Expected outcome |
|---|---|
| S1 macOS → any (PNG path) | byte-identical (`diff` exit 0) |
| S1 Windows → macOS (DIB → image-crate fallback) | "visually consistent only" (#S-4 noted) |
| S1 Windows → Linux (DIB → image-crate fallback) | "visually consistent only" |
| S1 Linux → Windows (PNG → image-crate BMP encoder) | "visually consistent only" (#S-4 noted) |
| S1 Windows ↔ Windows (CF_DIBV5 direct) | byte-identical (out of this matrix, baseline) |
| S2 PNG push to PNG-capable receiver | byte-identical |
| S2 JPG push to Windows receiver | "known unsupported" (windows.rs JPEG path returns Err) |
| S2 JPG push to macOS / Linux receiver | byte-identical (PNG re-encode → Jpeg is preserved as bytes; receiver reads back as PNG via `apply_inbound_image_bytes` fallback) |
| S3 macOS Preview.app TIFF source → any | byte-identical (PNG normalisation makes it so) |
| S3 reverse direction (B → macOS) | n/a (macOS-source only) |
| S4 any direction | LRU skip count = 1 per re-paste, push count = 1 per source |

---

## 4. Troubleshooting

### Symptom: B's clipboard stays empty after S1 / S2

Check A's log for `sending ClipboardImage (meta)` — if missing, the
dispatcher didn't pick up the change. Likely causes:
- **macOS Accessibility / Input Monitoring permission** not yet granted
  to the daemon. Open System Settings → Privacy & Security → Input
  Monitoring and add the daemon binary. **Image clipboard** requires
  the same permission as text clipboard.
- **`enable_clipboard_to = false`** in the peer entry of `config.toml` —
  fix and restart daemon.
- **Image mime detection failed** (rare): if A's app puts the image on
  the clipboard in a format without magic bytes (e.g., NSColor object
  on macOS), the daemon returns `None` from `current_image()` and skips.
  Try copying via a different app (Preview.app, Safari).

Check B's log for `HTTP/3 GET /clipboard/image/<sha>`:
- **`→ 404 cache miss`**: 2a.3 active eviction race — A already pushed a
  newer sha. Run the scenario again with bigger `sleep` between pushes.
- **No GET at all**: the inbound Meta event was not delivered — check
  the StreamC connection is open (`netstat -an | grep 4252` or
  `ss -u 'sport = :4252'`).

### Symptom: B's clipboard lands as garbage bytes (e.g., not PNG magic)

The HTTP/3 GET succeeded but `apply_inbound_clipboard_image` wrote
wrong bytes. This indicates a sha mismatch between A's cache key and
B's HTTP/3 lookup — file this as a bug in `next/SUGGESTION.md` with
the sha values from both sides.

If B receives `application/x-dib` wire label but its backend returns
`Err(Unsupported)` (e.g., Linux with `image crate` not built), the
daemon logs warn + skips. B's clipboard remains unchanged. Mark as
**"receiver unsupported"**, not fail.

### Symptom: S1 Windows → Linux / Linux → Windows shows byte-level mismatch

**This is expected** per PLAN §3 评审 #3 3rd + #S-4:
- Windows → Linux: CF_DIBV5 → Linux `set_dib_image` → `image crate` decode
  → re-encode PNG → bytes differ from DIB but visually consistent.
- Linux → Windows: PNG → Windows `set_image(Mime::Png)` → `image crate`
  PNG→BMP encoder → BITMAPINFOHEADER 24-bit RGB → bytes differ from PNG
  but visually consistent.

**Do not mark as fail.** Use the "visually consistent only" cell outcome.

To confirm "visually consistent", diff the dimensions and a quick pixel
sample:

```bash
# Compare dimensions
magick identify /tmp/4k.png /tmp/back.png
# → expect same `3840x2160` lines for both

# Compare a 100x100 region from the center
magick /tmp/4k.png -crop 100x100+1920+1080 /tmp/a.png
magick /tmp/back.png -crop 100x100+1920+1080 /tmp/b.png
compare -metric AE /tmp/a.png /tmp/b.png /tmp/diff.png
# → expect "0" or very small number (sub-pixel rendering differs)
```

### Symptom: S3 Preview.app TIFF normalisation log missing on macOS source

The TIFF fallback path runs only if `data(forType: .png)` returns `None`
on the macOS pasteboard. If the user copies a PNG (not TIFF) into
Preview.app's clipboard, the source log shows `mime=image/png` directly
with no TIFF intermediate line. This is correct — Preview.app gives
both PNG and TIFF representations; PNG is preferred per 2a.2 §1.

To force the TIFF path: use `osascript` to grab only the TIFF class:

```bash
osascript -e 'set tiffData to (the clipboard as «class TIFF»)' \
          -e 'set f to open for access (POSIX file "/tmp/raw.tiff") with write permission' \
          -e 'write tiffData to f' \
          -e 'close access f'
```

Then check that `file /tmp/raw.tiff` shows "TIFF image data" and the
daemon log shows the TIFF→PNG re-encode step.

### Symptom: S4 image loopback LRU skip missing

A's daemon log should show `loopback LRU hit — skip image push` within
500 ms of the second `image change detected` line. If missing:
- **A's daemon was just restarted** between the push and the re-paste:
  `mark_local_image_write` is in-memory only; restart clears it.
- **The re-paste happened > 60 s after the original push**: the LRU
  TTL (60 s) has expired; the re-paste is treated as a fresh event.
- **The re-paste is a different image (different sha)**: this is NOT
  loopback; A correctly pushes the new image.

### Symptom: S2 JPG pushes to Windows receiver — B's clipboard stays empty

**Expected behaviour** per 2b.1 scope: `windows.rs::set_image(Mime::Jpeg)`
returns `Err(Unsupported)`; daemon logs warn + skips. Mark the cell as
**"known unsupported — JPEG path not implemented on Windows"**, not fail.

If the JPG push is a hard requirement for the user's workflow, the
workaround is:
1. macOS / Linux side re-encode JPG → PNG before pushing (daemon does
   not auto-reencode on the source side per PLAN §3 STEP-2a.2 — TIFF→PNG
   is the only auto-reencode path).
2. OR: implement Windows `set_image(Mime::Jpeg)` via `image crate` JPEG
   decode → PNG → BMP encode (extension of 2b.1, out of M2b scope).

---

## 5. M2b milestone gate (final checklist)

After all 24 cells of the matrix pass, run the following to close out
M2b:

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

# ─── 5.3 M2b completion log ───
# Attach the §3 result template (24 rows) + the static check output to
# next/STEP-P2-M2b-2b.3.md as the human-validation appendix.
```

**M2b milestone closure requires all of:**

| Item | Owner | Pass criteria |
|---|---|---|
| Static checks (5.1) | executor | 0 errors / 0 clippy warnings / 0 fmt diff |
| Cross-platform check (5.2) | executor or CI | 4/4 pass (or document missing toolchain) |
| S1 4K screenshot 6/6 cells | human | byte-identical (or "visually consistent only" for Windows↔Linux pairs per #S-4) |
| S2 1080p JPG 6/6 cells | human | byte-identical (or "known unsupported" for Windows-receiver cells per 2b.1) |
| S3 Preview.app TIFF→PNG 3/3 cells (macOS-source only) | human | sha_png matches B-side PNG bytes |
| S4 image loopback 6/6 cells | human | LRU skip count = 1 per re-paste, push count = 1 per source |
| SUGGESTION.md cleanup | executor | all 24 cells clean → no new entries |

When every box above is checked, M2b is ready for validator review.

---

## 6. Out of scope (do NOT execute as part of this matrix)

- **File copy** (200 MiB round-trip via HTTP/3) — M3a / M3b scope.
- **GUI Toaster / accept-reject flow** — M4 scope.
- **HTML / RTF clipboard format negotiation** — out of PLAN-2 scope
  entirely.
- **Multi-image clipboard** (some apps put multiple image formats on
  the clipboard simultaneously, e.g., PNG + TIFF); lan-mouse takes the
  priority order PNG → TIFF → BMP and pushes only one (the highest
  priority available). Future milestone may push all with mime hints.
- **Clipboard history** (recent N items with timestamps) — out of PLAN-2
  scope, addressed in PLAN-M3 backlog.
- **Wayland portal restrictions** (`wlr-data-control` protocol version
  ≥ 2 for clipboard reads) — out of M2b scope; use `wl-paste` / `wl-copy`
  tools as documented. If portal permissions are missing, wl-paste
  returns "Permission denied" and daemon logs `clipboard backend unavailable`.
- **Cross-machine clock skew** — both peers should be NTP-synced for the
  hit-rate timestamp log lines to align with reality (image LRU TTL
  uses wall-clock 60 s).
- **Windows MSVC compile** — this template's tests assume MSVC is built
  in CI `windows-latest` job; local zig-cross validation (gnu ABI) is
  sufficient for the executor's gate.

---

> **本文档定稿时间**：2026-09-10
> **作者**：plan-step-executor
> **下一步**：人类真机测试 → M2b 2b.4 收尾（fmt / clippy / build / 三平台编译）
