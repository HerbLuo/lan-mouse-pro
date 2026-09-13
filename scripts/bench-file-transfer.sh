#!/usr/bin/env bash
# scripts/bench-file-transfer.sh
#
# Helper for `tests/manual/file-transfer.md` (M5 STEP-5.2 真机回归).
#
# Generates a 200 MiB random fixture, computes its sha256, then waits for
# the file to land on the configured `accept_dir` (default
# `~/Downloads/lan-mouse/`), then prints timing + sha256 verification.
#
# This script is run **once per peer** to capture the source / sink
# timings. The actual clipboard copy (Cmd+C / Ctrl-C / xclip / wl-copy)
# is the operator's responsibility — this script does NOT push to the
# clipboard.
#
# Usage:
#   scripts/bench-file-transfer.sh setup                   # generate + sha256
#   scripts/bench-file-transfer.sh measure [accept_dir]     # wait + verify + timing
#
# Environment:
#   PAYLOAD_PATH    override fixture path (default /tmp/payload-200mib.bin)
#   ACCEPT_DIR      override accept_dir (default ~/Downloads/lan-mouse)
#   TIMEOUT_SECS    max wait for file to land (default 90)
#
# Examples:
#   $ scripts/bench-file-transfer.sh setup
#   # (operator copies /tmp/payload-200mib.bin to clipboard on A side)
#   $ scripts/bench-file-transfer.sh measure
#   # (on B side, after the daemon has landed the file)

set -euo pipefail

PAYLOAD_PATH="${PAYLOAD_PATH:-/tmp/payload-200mib.bin}"
ACCEPT_DIR="${ACCEPT_DIR:-$HOME/Downloads/lan-mouse}"
TIMEOUT_SECS="${TIMEOUT_SECS:-90}"
LANDED_PATH="${ACCEPT_DIR}/payload-200mib.bin"
EXPECTED_SIZE=$((200 * 1024 * 1024))   # 200 MiB exact

cmd_setup() {
    if [ -e "$PAYLOAD_PATH" ]; then
        echo "[bench] $PAYLOAD_PATH already exists, sha256:"
        sha256sum "$PAYLOAD_PATH"
        echo "[bench] delete it first to regenerate: rm $PAYLOAD_PATH"
        exit 0
    fi

    echo "[bench] generating 200 MiB fixture at $PAYLOAD_PATH (dd if=/dev/urandom)"
    if dd if=/dev/urandom of="$PAYLOAD_PATH" bs=1M count=200 status=none; then
        :
    else
        echo "[bench] FAIL: dd failed"
        exit 1
    fi

    local actual_size
    actual_size=$(wc -c < "$PAYLOAD_PATH" | tr -d ' ')
    if [ "$actual_size" != "$EXPECTED_SIZE" ]; then
        echo "[bench] FAIL: expected $EXPECTED_SIZE bytes, got $actual_size"
        exit 1
    fi

    echo "[bench] sha256:"
    sha256sum "$PAYLOAD_PATH"
    echo "[bench] copy $PAYLOAD_PATH to your clipboard (Cmd+C / Ctrl-C / xclip / wl-copy)"
    echo "[bench] on B side, run: scripts/bench-file-transfer.sh measure"
}

cmd_measure() {
    local start_epoch start_ns end_epoch end_ns elapsed

    if [ ! -d "$ACCEPT_DIR" ]; then
        mkdir -p "$ACCEPT_DIR" || {
            echo "[bench] FAIL: cannot create $ACCEPT_DIR"
            exit 1
        }
    fi

    echo "[bench] waiting up to ${TIMEOUT_SECS}s for $LANDED_PATH to appear"
    echo "[bench] (start a 200 MiB transfer on A side if you haven't already)"

    # Record start NOW (operator may have started transfer slightly earlier,
    # but we measure from this script's invocation).
    start_epoch=$(date +%s)
    start_ns=$(date +%N)

    local waited=0
    while [ ! -e "$LANDED_PATH" ]; do
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -ge "$TIMEOUT_SECS" ]; then
            echo "[bench] FAIL: $LANDED_PATH did not appear within ${TIMEOUT_SECS}s"
            echo "[bench] check daemon log for errors (clipboard backend, mTLS pairing, accept_dir permissions)"
            exit 1
        fi
    done

    end_epoch=$(date +%s)
    end_ns=$(date +%N)
    elapsed=$(awk -v s="$start_epoch" -v sns="$start_ns" -v e="$end_epoch" -v ens="$end_ns" \
        'BEGIN { printf "%.3f", (e - s) + (ens - sns) / 1e9 }')

    echo "[bench] file landed after ${elapsed}s"

    # Verify size
    local actual_size
    actual_size=$(wc -c < "$LANDED_PATH" | tr -d ' ')
    if [ "$actual_size" != "$EXPECTED_SIZE" ]; then
        echo "[bench] FAIL: expected $EXPECTED_SIZE bytes, got $actual_size"
        exit 1
    fi
    echo "[bench] size OK: $actual_size bytes"

    # Verify sha256 if source available
    if [ -e "$PAYLOAD_PATH" ]; then
        local src_sha dst_sha
        src_sha=$(sha256sum "$PAYLOAD_PATH" | awk '{print $1}')
        dst_sha=$(sha256sum "$LANDED_PATH" | awk '{print $1}')
        if [ "$src_sha" = "$dst_sha" ]; then
            echo "[bench] sha256 OK: $dst_sha"
        else
            echo "[bench] FAIL: sha256 mismatch"
            echo "  source: $src_sha"
            echo "  landed: $dst_sha"
            exit 1
        fi
    else
        echo "[bench] source fixture not present at $PAYLOAD_PATH — skipping sha256 check"
        echo "[bench] landed sha256: $(sha256sum "$LANDED_PATH" | awk '{print $1}')"
    fi

    # Check for stray .partial files
    if ls "${LANDED_PATH}.partial"* 1>/dev/null 2>&1; then
        echo "[bench] WARN: stray .partial files remain (default keep_partial=false should clean them):"
        ls -la "${LANDED_PATH}.partial"* || true
    fi

    # Budget check (200 MiB wired LAN: < 30 s; Wi-Fi: < 60 s)
    local budget="${BUDGET_SECS:-30}"
    local elapsed_int
    elapsed_int=$(awk -v e="$elapsed" 'BEGIN { printf "%d", e }')
    if [ "$elapsed_int" -lt "$budget" ]; then
        echo "[bench] PASS: ${elapsed}s < ${budget}s budget"
    else
        echo "[bench] WARN: ${elapsed}s exceeds ${budget}s budget (Wi-Fi expected if link is wireless)"
    fi
}

cmd_default() {
    cat <<EOF
bench-file-transfer.sh — M5 STEP-5.2 helper

Usage:
  $0 setup                              # generate 200 MiB fixture + sha256
  $0 measure [accept_dir]               # wait for landed file + verify

Environment:
  PAYLOAD_PATH    override fixture path         (default: /tmp/payload-200mib.bin)
  ACCEPT_DIR      override accept_dir           (default: ~/Downloads/lan-mouse)
  TIMEOUT_SECS    max wait for file to land     (default: 90)
  BUDGET_SECS     timing budget for PASS line   (default: 30; bump to 60 for Wi-Fi)

Examples:
  # A side
  PAYLOAD_PATH=/tmp/my-file.bin $0 setup
  # (operator: copy to clipboard)
  # (then on B side:)
  ACCEPT_DIR=/Users/me/Downloads/lan-mouse $0 measure
EOF
}

case "${1:-}" in
    setup)
        cmd_setup
        ;;
    measure)
        if [ -n "${2:-}" ]; then
            ACCEPT_DIR="$2"
        fi
        cmd_measure
        ;;
    *)
        cmd_default
        exit 1
        ;;
esac