#!/usr/bin/env bash
#
# quic_smoke.sh — end-to-end shell smoke (placeholder)
#
# Design intent: spawn two `lan-mouse-cli` processes (or two daemons + daemons),
# have them exchange a small batch of Motion + KeyboardKey events on localhost,
# and exit 0 if everything completes within the timeout.
#
# **M1 status note (2026-08-31)**:
#
# - `lan-mouse-cli` is IPC-only (see `lan-mouse-cli/src/lib.rs`). It connects to
#   a running daemon over the IPC socket and cannot itself initiate a QUIC
#   peer connection. A pure-CLI two-process flow would require a daemon in
#   the middle, which brings up a third process and per-test config state
#   that is brittle to automate reliably.
# - The integration tests under `tests/quic_smoke.rs` already cover the
#   transport-level round-trip in-process with deterministic assertions.
#
# Until the GUI / CLI is wired to actually exercise the QUIC peer-session
# path end-to-end (planned for a future post-M1 wiring pass), this script **SKIPs**
# with an informative message rather than falsely passing or falsely
# failing CI. Leader is encouraged to run `cargo test -p lan-mouse --test
# quic_smoke` as the source of truth for §7.2 acceptance.
#
# When wired up later (e.g. via `LANMOUSE_SMOKE_MODE=cli` env flag), the
# script should:
#   1. start two `lan-mouse` daemons (each with its own XDG_DATA_HOME so
#      cert keys are independent), on different ports
#   2. add the peers to each other via `lan-mouse-cli add-client ...`
#   3. activate clients on both sides (`lan-mouse-cli activate ...`)
#   4. inject Motion + KeyboardKey events via the daemon's
#      `lan-mouse-cli send-event` (or equivalent) subcommand
#   5. observe peer-side received-event logs / counters and assert they
#      match
#
# In the meantime: this script always exits 0 with a SKIP note.

set -eu

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

cat <<'NOTE'
[quic_smoke.sh] SKIP — see header comment.

The end-to-end shell smoke requires a CLI/daemon top-loop that drives the
QUIC peer-session path against a real input source. That wiring is queued
for a future wiring pass; the authoritative transport-level coverage today lives in:

    cargo test -p lan-mouse --test quic_smoke         # 2 tests
    cargo test -p lan-mouse --test input_channel_routing   # 7 tests

Run those as the source of truth for §7.2 acceptance. This script will
evolve into a true end-to-end harness once the CLI gains a send-event
subcommand and the daemon exposes a deterministic event-sink.
NOTE

exit 0
