//! Cross-platform clipboard text end-to-end test stub (M1b STEP-1b.4)
//!
//! **Status**: STUB — these tests are not wired to live code paths.
//!
//! The actual byte-level clipboard sync is validated by **human-operated
//! end-to-end runs** documented in `tests/manual/clipboard-text.md`. This
//! integration test file exists to:
//!
//! 1. Reserve a stable location for future in-process coverage of the
//!    StreamC round-trip and HTTP/3 cache-miss pull paths.
//! 2. Pin the public API surface the real tests will exercise once they
//!    are un-stubbed.
//! 3. Compile under `cargo test --workspace` so the slot is discoverable
//!    by `cargo test` discovery but does not block the suite.
//!
//! All tests in this module are `#[ignore]`-marked; they do not run on
//! `cargo test --workspace`. To opt in once the harness is built, run:
//!
//! ```bash
//! cargo test --workspace --test clipboard_text_e2e -- --ignored --nocapture
//! ```
//!
//! ## Why a stub?
//!
//! **StreamC round-trip** in production goes through `quinn::Connection`
//! combined with QUIC handshake and TLS — reproducing that in an
//! in-process test duplicates the existing `tests/quic_smoke.rs`
//! harness and adds clipboard-specific plumbing
//! (`PeerSession::send_stream_c` and `StreamEvent::ClipboardMeta`).
//! Building the harness requires the `PeerSession` test seams that
//! aren't currently exposed.
//!
//! **HTTP/3 cache-miss pull** in production goes through the full
//! `Http3Client::get_text` to `Router::handle` to `clipboard_text_route`
//! path. The route handler takes `Arc<Router>` and an explicit cache
//! handle; building the test seam needs the dispatcher to expose a
//! `Service::clipboard_cache` accessor (currently private).
//!
//! When the production code is refactored to expose those seams (likely
//! in M1b/M2a refinement PRs), this file's tests should be un-stubbed.
//!
//! ## What is being pinned
//!
//! - `lan_mouse_proto::ProtoEvent::ClipboardText` round-trip via
//!   `from_content` + `is_inline` + `Vec<u8>` codec (1b.1).
//! - `lan_mouse::quic_transport::http3::encode_request` /
//!   `encode_response` / `decode_request` / `decode_response` framing for
//!   `GET /clipboard/text/{sha256}` (1b.2).
//! - 404 silent handling on cache miss (1b.2 reviewer #3 second round).
//! - Active eviction contract (1b.2 reviewer #3 second round) — pinned
//!   at unit-test layer in `src/service.rs::register_pending_clipboard_request`
//!   because `src/clipboard/cache.rs` is `pub(crate)` and not reachable
//!   from `tests/`.
//!
//! ## Out of scope here
//!
//! - Real `PeerSession` end-to-end (covered by `tests/quic_smoke.rs`).
//! - LRU loopback skip semantics (covered by `src/service.rs` unit tests
//!   `lru_fingerprints_tests`).
//! - Per-platform clipboard backend byte-fidelity (Windows OpenClipboard,
//!   macOS NSPasteboard, Linux xclip / wl-paste) — requires OS-level
//!   sandbox, run on real machines via `tests/manual/clipboard-text.md`.

use lan_mouse_proto::{CLIPBOARD_TEXT_INLINE_LIMIT, ProtoEvent};

/// Helper: derive sha256 fingerprint from a byte slice for stub use.
///
/// In production this lives in `lan-mouse-proto` (already uses sha2 0.10);
/// for the stub we re-derive locally so we don't drag in private helpers.
fn sha256_stub(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

// ─── StreamC round-trip stub ────────────────────────────────────────────────

/// Verify that a 1 MiB text pushed via the dispatcher round-trips through
/// `ClipboardText::from_content` + `Vec<u8>` codec + decode, producing
/// byte-identical content on the receive side.
///
/// **Stub**: not wired to `PeerSession::send_stream_c`. Requires the
/// in-process StreamC harness from `tests/quic_smoke.rs` extended with
/// clipboard semantics (1b.1 + 1b.2).
#[test]
#[ignore = "stub: requires in-process StreamC harness; see file header"]
fn one_megabyte_text_round_trips_via_stream_c() {
    // ── Setup ────────────────────────────────────────────────────────────
    // Generate 1 MiB random text (mirrors `tests/manual/clipboard-text.md`
    // §S2.1 so the stub agrees with the human checklist).
    let payload: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    assert_eq!(payload.len(), 1_048_576);
    assert!(payload.len() > CLIPBOARD_TEXT_INLINE_LIMIT);

    let sha = sha256_stub(&payload);

    // ── Source side: dispatcher constructs ClipboardText ─────────────────
    // Production code: `service::clipboard_dispatcher` calls
    //   ClipboardText::from_content(fingerprint, sha256, payload)
    // and ships it via `PeerSession::send_stream_c`.
    let event = ProtoEvent::ClipboardText(lan_mouse_proto::ClipboardText::from_content(
        sha, // fingerprint (here aliased to sha256 for stub simplicity)
        sha, // sha256
        payload.clone(),
    ));
    let ct_inline = match &event {
        ProtoEvent::ClipboardText(ct) => ct.is_inline(),
        _ => panic!("expected ClipboardText variant"),
    };
    assert!(!ct_inline, "1 MiB must take the meta path");

    // ── Wire: encode via Vec<u8> var-codec (1b.1) ────────────────────────
    let wire: Vec<u8> = Vec::<u8>::from(event.clone());
    assert!(
        wire.len() < payload.len() / 2,
        "meta path must NOT inline the 1 MiB bytes (got wire len = {})",
        wire.len()
    );

    // ── Sink side: decode + verify ───────────────────────────────────────
    let decoded = ProtoEvent::try_from(wire.as_slice()).expect("decode");
    let ct = match decoded {
        ProtoEvent::ClipboardText(ct) => ct,
        other => panic!("expected ClipboardText variant, got {other:?}"),
    };
    assert!(!ct.is_inline(), "decoded event must remain meta");
    assert_eq!(ct.sha256, sha);

    // ── HTTP/3 pull (would happen on the receiver) ───────────────────────
    // Production code: receiver sees `ClipboardText` Meta branch,
    // calls `Http3Client::get_text(hex)`, gets back bytes, calls
    // `apply_inbound_clipboard_text`. The stub stops at codec level;
    // the HTTP/3 path is pinned separately in `http3_cache_miss_returns_404`.
    let pulled_stub_bytes = payload.clone(); // ← stands in for Http3Client result
    assert_eq!(pulled_stub_bytes, payload, "byte-level identity must hold");
}

// ─── HTTP/3 cache-miss stub ─────────────────────────────────────────────────

/// Verify that `Http3Client::get_text` returns `(404, empty body)` on cache
/// miss and that the dispatcher treats it as silent (no panic, no error
/// propagation).
///
/// **Stub**: currently constructs the request frame via
/// `encode_request` + decode; does not stand up a real HTTP/3 server.
/// Once the in-process harness from `tests/quic_smoke.rs` is extended
/// to host a `Router`, the test should be un-stubbed to spin up a
/// `PeerSession` + `Router` pair.
#[test]
#[ignore = "stub: requires in-process HTTP/3 Router harness; see file header"]
fn http3_cache_miss_returns_404_silently() {
    use lan_mouse::quic_transport::http3::{
        Response, decode_request, decode_response, encode_request, encode_response,
    };

    // ── Encode a GET /clipboard/text/<sha> request ───────────────────────
    let sha_hex = "0000000000000000000000000000000000000000000000000000000000000000";
    let path = format!("/clipboard/text/{sha_hex}");
    let wire = encode_request("GET", &path, &[]);

    // ── Decode the request ───────────────────────────────────────────────
    let (req, _consumed) = decode_request(&wire).expect("decode req");
    assert_eq!(req.method, "GET");
    assert_eq!(req.path, path);

    // ── Simulate cache miss: a real `Router::handle` returns 404 here ────
    // Production code path (not invoked in stub):
    //   let router = default_router_with_cache(Arc::new(Mutex::new(ClipboardCache::new())));
    //   let resp = router.handle(&req);
    //
    // `Response::not_found()` includes a `"not found"` body — production
    // dispatcher's `(status, body.len())` match treats any non-200 as
    // skip regardless of body length, so the body bytes are irrelevant
    // to the contract. We use `with_status(404, b"")` here to pin
    // *empty body* explicitly.
    let simulated_resp = Response::with_status(404, Vec::<u8>::new());

    // ── Encode + decode the 404 response ─────────────────────────────────
    let resp_bytes = encode_response(simulated_resp.status, &simulated_resp.body);
    let (decoded_resp, _consumed) = decode_response(&resp_bytes).expect("decode resp");
    assert_eq!(decoded_resp.status, 404, "cache miss must surface as 404");
    assert!(
        decoded_resp.body.is_empty(),
        "404 response body must be empty (dispatcher treats body length 0 as silent skip)"
    );

    // ── Dispatcher contract pin (1b.2 reviewer #3 second round) ──────────
    // Production `service::handle_clipboard_inbound` receives
    // `Ok((status, body))` from `Http3Client::get_text`. The match arm:
    //
    //   match (status, body.len()) {
    //       (200, n) if n > 0 => apply_inbound_clipboard_text(body),
    //       _ => { warn!("cache miss"); skip; }
    //   }
    //
    // The stub pins that 404 + empty body lands in the `_` arm and does
    // not panic. Full coverage is in `src/service.rs::inbound_clipboard_text_*_tests`.
    let dispatcher_outcome = match (decoded_resp.status, decoded_resp.body.len()) {
        (200, n) if n > 0 => "apply",
        _ => "silent-skip",
    };
    assert_eq!(dispatcher_outcome, "silent-skip", "404 must not panic");
}

// ─── Active eviction stub (documented; not wired) ──────────────────────────

/// Verify that source-side `cache.remove(prev_sha)` happens **before** the
/// next push so a receiver that races the pull sees 404 instead of stale
/// bytes.
///
/// **Stub**: `ClipboardCache` lives in `src/clipboard/cache.rs` and is
/// `pub(crate)` — not reachable from `tests/`. The active eviction
/// contract is pinned at the dispatcher level by
/// `src/service.rs::register_pending_clipboard_request` (see STEP-P2-M1b-1b.2
/// §3.1 "active_eviction_concurrent_with_lookup_old_returns_miss").
///
/// This stub exists for documentation only — `#[ignore]`d and the body
/// is empty. Once the `clipboard` module is promoted to `pub` (likely
/// in M2a when `clipboard::Backend` becomes a public surface for the
/// GUI Toaster), this test should be un-stubbed and the body moved over
/// verbatim from `src/service.rs`.
#[test]
#[ignore = "stub: src/clipboard is pub(crate); see file header for the contract pin location"]
fn active_eviction_concurrent_with_lookup_old_returns_miss() {
    // The actual contract is verified by:
    //   src/service.rs::register_pending_clipboard_request
    //   (commit 7abb275, M1b STEP-1b.2)
    //
    // That unit test uses `crate::clipboard::cache::ClipboardCache`
    // directly inside the `lan-mouse` crate where the `pub(crate)` gate
    // is satisfied. From `tests/` we cannot reach it, so we leave the
    // contract pin at the unit-test layer.
}

// ─── Sanity checks that DO run on every `cargo test` ───────────────────────

/// Sanity: `lan_mouse_proto::ProtoEvent` codec round-trip works for an
/// inline-size ClipboardText. This is a thin re-export of the unit test
/// coverage in `lan-mouse-proto/src/lib.rs`; included here so that the
/// stub file itself contributes at least one passing test and stays a
/// real part of the test suite (rather than 100% ignored).
#[test]
fn inline_clipboard_text_codec_round_trip() {
    let payload = b"hello clipboard".to_vec();
    let sha = sha256_stub(&payload);
    let event = ProtoEvent::ClipboardText(lan_mouse_proto::ClipboardText::from_content(
        sha,
        sha,
        payload.clone(),
    ));
    let ct_is_inline = match &event {
        ProtoEvent::ClipboardText(ct) => ct.is_inline(),
        _ => panic!("expected ClipboardText variant"),
    };
    assert!(ct_is_inline);

    let wire: Vec<u8> = Vec::<u8>::from(event.clone());
    let decoded = ProtoEvent::try_from(wire.as_slice()).expect("decode");
    let ct = match decoded {
        ProtoEvent::ClipboardText(ct) => ct,
        other => panic!("expected ClipboardText variant, got {other:?}"),
    };
    assert_eq!(ct.sha256, sha);
}

/// Sanity: a payload at the boundary `CLIPBOARD_TEXT_INLINE_LIMIT` is
/// still inlined (≤ limit), but `CLIPBOARD_TEXT_INLINE_LIMIT + 1` is not.
#[test]
fn inline_boundary_round_trip() {
    let at_limit = vec![b'x'; CLIPBOARD_TEXT_INLINE_LIMIT];
    let over_limit = vec![b'x'; CLIPBOARD_TEXT_INLINE_LIMIT + 1];

    let sha_at = sha256_stub(&at_limit);
    let ev_at = ProtoEvent::ClipboardText(lan_mouse_proto::ClipboardText::from_content(
        sha_at,
        sha_at,
        at_limit.clone(),
    ));
    let at_inline = match &ev_at {
        ProtoEvent::ClipboardText(ct) => ct.is_inline(),
        _ => panic!("expected ClipboardText variant"),
    };
    assert!(at_inline);

    let sha_over = sha256_stub(&over_limit);
    let ev_over = ProtoEvent::ClipboardText(lan_mouse_proto::ClipboardText::from_content(
        sha_over,
        sha_over,
        over_limit.clone(),
    ));
    let over_inline = match &ev_over {
        ProtoEvent::ClipboardText(ct) => ct.is_inline(),
        _ => panic!("expected ClipboardText variant"),
    };
    assert!(!over_inline);
}
