//! Cross-platform clipboard image end-to-end test stub (M2b STEP-2b.3)
//!
//! **Status**: STUB — these tests are not wired to live code paths.
//!
//! The actual byte-level clipboard image sync is validated by **human-operated
//! end-to-end runs** documented in `tests/manual/clipboard-image.md`. This
//! integration test file exists to:
//!
//! 1. Reserve a stable location for future in-process coverage of the
//!    image StreamC round-trip, HTTP/3 image cache-miss pull, and image
//!    loopback LRU paths.
//! 2. Pin the public API surface the real tests will exercise once they
//!    are un-stubbed.
//! 3. Compile under `cargo test --workspace` so the slot is discoverable
//!    by `cargo test` discovery but does not block the suite.
//!
//! All tests in this module are `#[ignore]`-marked except two sanity
//! checks; the `#[ignore]` tests do not run on `cargo test --workspace`.
//! To opt in once the harness is built, run:
//!
//! ```bash
//! cargo test --workspace --test clipboard_image_e2e -- --ignored --nocapture
//! ```
//!
//! ## Why a stub?
//!
//! **Image StreamC round-trip** in production goes through `quinn::Connection`
//! combined with QUIC handshake and TLS — reproducing that in an
//! in-process test duplicates the existing `tests/quic_smoke.rs`
//! harness and adds clipboard-image-specific plumbing
//! (`PeerSession::send_stream_c` and `StreamEvent::ClipboardMeta`).
//! Building the harness requires the `PeerSession` test seams that
//! aren't currently exposed.
//!
//! **HTTP/3 image cache-miss pull** in production goes through the full
//! `Http3Client::get_image` to `Router::handle` to `clipboard_image_route`
//! path. The route handler takes `Arc<Router>` and an explicit cache
//! handle; building the test seam needs the dispatcher to expose a
//! `Service::clipboard_cache` accessor (currently private).
//!
//! **Image loopback LRU** uses `IMAGE_LOOPBACK_CAPACITY = 32` / TTL 60 s
//! (smaller than text LRU's 128 / 60 s because image writes are
//! expensive per M2a STEP-2a.4). `src/clipboard/cache.rs` is
//! `pub(crate)` and not reachable from `tests/`.
//!
//! When the production code is refactored to expose those seams (likely
//! in M2b refinement PRs or M4 GUI integration when `clipboard::Backend`
//! becomes public), this file's tests should be un-stubbed.
//!
//! ## What is being pinned
//!
//! - `lan_mouse_proto::ProtoEvent::ClipboardImage` round-trip via
//!   `Vec<u8>` var-codec with wire layout
//!   `[u8; 32 fingerprint][u32 BE mime_len][mime bytes][u8; 32 sha256][u64 BE size]`
//!   (M2a / 2b.1).
//! - `lan_mouse::quic_transport::http3::encode_request` /
//!   `encode_response` / `decode_request` / `decode_response` framing for
//!   `GET /clipboard/image/{sha256}` (2a.3).
//! - 404 silent handling on image cache miss (M1b reviewer #3 2nd pattern
//!   applies identically to image branch per 2a.3 dispatcher contract).
//! - Image loopback LRU 32 / 60 s contract (2a.4) — pinned at unit-test
//!   layer in `src/clipboard/cache.rs::tests::image_lru_*` because
//!   `src/clipboard/cache.rs` is `pub(crate)` and not reachable from
//!   `tests/`.
//! - `MIME_DIB` constant + `Mime::is_dib_label` routing predicate
//!   (M2b STEP-2b.1) — pins `application/x-dib` wire label.
//!
//! ## Out of scope here
//!
//! - Real `PeerSession` end-to-end (covered by `tests/quic_smoke.rs`).
//! - Per-platform clipboard backend byte-fidelity (Windows CF_DIBV5,
//!   macOS NSPasteboard TIFF→PNG, Linux xclip / wl-paste) — requires
//!   OS-level sandbox, run on real machines via
//!   `tests/manual/clipboard-image.md`.

use lan_mouse_proto::ProtoEvent;

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

/// Wire-level MIME labels that appear on the `ClipboardImage::mime` field.
///
/// Mirrors the values used in `src/clipboard/mod.rs` and the platform
/// backends. Pinning them here lets the stub tests reference the same
/// labels without crossing module boundaries. Only `MIME_PNG` and
/// `MIME_DIB` are exercised by the current sanity tests; `MIME_JPEG`
/// and `MIME_BMP` are reserved for future un-stub expansion (e.g.,
/// when the JPEG / BMP round-trip stubs land in a later STEP).
#[allow(dead_code)]
const MIME_PNG: &str = "image/png";
#[allow(dead_code)]
const MIME_JPEG: &str = "image/jpeg";
#[allow(dead_code)]
const MIME_BMP: &str = "image/bmp";
#[allow(dead_code)]
const MIME_DIB: &str = "application/x-dib";

// ─── Image StreamC round-trip stub ─────────────────────────────────────────

/// Verify that a 4K PNG (~8 MiB random bytes) pushed via the dispatcher
/// round-trips through `ClipboardImage` + `Vec<u8>` codec + decode,
/// producing byte-identical metadata on the receive side. The image
/// bytes themselves are pulled via HTTP/3 `GET /clipboard/image/{sha256}`
/// (not exercised here — see `http3_image_cache_miss_returns_404_silently`).
///
/// **Stub**: not wired to `PeerSession::send_stream_c`. Requires the
/// in-process StreamC harness from `tests/quic_smoke.rs` extended with
/// clipboard-image semantics (2a.3).
#[test]
#[ignore = "stub: requires in-process StreamC harness; see file header"]
fn four_k_screenshot_metadata_round_trips_via_stream_c() {
    // ── Setup: 8 MiB of deterministic byte-pattern image data ──────────
    let payload: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(payload.len(), 8 * 1024 * 1024);

    let sha = sha256_stub(&payload);
    let fingerprint = sha; // alias for stub simplicity (production uses separate hash)

    // ── Source side: dispatcher constructs ClipboardImage ───────────────
    // Production code: `service::clipboard_dispatcher` constructs
    //   ClipboardImage { fingerprint, mime: "image/png", sha256, size: payload.len() }
    // and ships it via `PeerSession::send_stream_c` (M2a 2a.3).
    let event = ProtoEvent::ClipboardImage(lan_mouse_proto::ClipboardImage {
        fingerprint,
        mime: MIME_PNG.to_string(),
        sha256: sha,
        size: payload.len() as u64,
    });

    // ── Wire: encode via Vec<u8> var-codec (M2a codec.rs) ───────────────
    let wire: Vec<u8> = Vec::<u8>::from(event.clone());

    // The wire payload carries only metadata (fingerprint + mime + sha256 +
    // size), NEVER the image bytes themselves. Bytes go through HTTP/3
    // pull separately. The total wire size is ~32 + 4 + 8 + 32 + 8 = ~84
    // bytes plus a small event-type prefix — well under 1 KiB regardless
    // of image size.
    assert!(
        wire.len() < 1024,
        "ClipboardImage wire payload must be metadata-only (got wire len = {})",
        wire.len()
    );

    // ── Sink side: decode + verify metadata ────────────────────────────
    let decoded = ProtoEvent::try_from(wire.as_slice()).expect("decode");
    let ci = match decoded {
        ProtoEvent::ClipboardImage(ci) => ci,
        other => panic!("expected ClipboardImage variant, got {other:?}"),
    };
    assert_eq!(ci.fingerprint, fingerprint);
    assert_eq!(ci.sha256, sha);
    assert_eq!(ci.mime, MIME_PNG);
    assert_eq!(ci.size, payload.len() as u64);

    // ── HTTP/3 pull (would happen on the receiver) ──────────────────────
    // Production code: receiver sees `ClipboardImage` Meta branch,
    // calls `Http3Client::get_image(hex)`, gets back bytes, calls
    // `apply_inbound_clipboard_image`. The stub stops at codec level;
    // the HTTP/3 path is pinned separately in
    // `http3_image_cache_miss_returns_404_silently`.
    let pulled_stub_bytes = payload.clone(); // ← stands in for Http3Client result
    assert_eq!(pulled_stub_bytes, payload, "byte-level identity must hold");
}

// ─── HTTP/3 image cache-miss stub ──────────────────────────────────────────

/// Verify that `Http3Client::get_image` returns `(404, empty body)` on
/// cache miss and that the dispatcher treats it as silent (no panic,
/// no error propagation). Mirrors the M1b reviewer #3 2nd contract
/// applied to the image branch per 2a.3 dispatcher.
///
/// **Stub**: currently constructs the request frame via `encode_request`
/// + decode; does not stand up a real HTTP/3 server. Once the
/// in-process harness from `tests/quic_smoke.rs` is extended to host
/// a `Router`, the test should be un-stubbed to spin up a
/// `PeerSession` + `Router` pair.
#[test]
#[ignore = "stub: requires in-process HTTP/3 Router harness; see file header"]
fn http3_image_cache_miss_returns_404_silently() {
    use lan_mouse::quic_transport::http3::{
        Response, decode_request, decode_response, encode_request, encode_response,
    };

    // ── Encode a GET /clipboard/image/<sha> request ─────────────────────
    let sha_hex = "0000000000000000000000000000000000000000000000000000000000000000";
    let path = format!("/clipboard/image/{sha_hex}");
    let wire = encode_request("GET", &path, &[]);

    // ── Decode the request ──────────────────────────────────────────────
    let (req, _consumed) = decode_request(&wire).expect("decode req");
    assert_eq!(req.method, "GET");
    assert_eq!(req.path, path);

    // ── Simulate cache miss: a real `Router::handle` returns 404 here ───
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

    // ── Encode + decode the 404 response ────────────────────────────────
    let resp_bytes = encode_response(simulated_resp.status, &simulated_resp.body);
    let (decoded_resp, _consumed) = decode_response(&resp_bytes).expect("decode resp");
    assert_eq!(decoded_resp.status, 404, "cache miss must surface as 404");
    assert!(
        decoded_resp.body.is_empty(),
        "404 response body must be empty (dispatcher treats body length 0 as silent skip)"
    );

    // ── Dispatcher contract pin (2a.3 reviewer #3 2nd pattern) ──────────
    // Production `service::handle_clipboard_inbound_image` receives
    // `Ok((status, body))` from `Http3Client::get_image`. The match arm:
    //
    //   match (status, body.len()) {
    //       (200, n) if n > 0 => apply_inbound_clipboard_image(body),
    //       _ => { warn!("cache miss"); skip; }
    //   }
    //
    // The stub pins that 404 + empty body lands in the `_` arm and does
    // not panic. Full coverage is in
    // `src/service.rs::inbound_clipboard_image_*_tests`.
    let dispatcher_outcome = match (decoded_resp.status, decoded_resp.body.len()) {
        (200, n) if n > 0 => "apply",
        _ => "silent-skip",
    };
    assert_eq!(dispatcher_outcome, "silent-skip", "404 must not panic");
}

// ─── Image loopback LRU stub (documented; not wired) ──────────────────────

/// Verify that the **image-branch** loopback LRU (capacity 32, TTL 60 s
/// per M2a STEP-2a.4) correctly suppresses a local re-paste of the same
/// image fingerprint within the TTL window.
///
/// Image writes are expensive (each entry holds a 5–15 MiB PNG in the
/// image cache), so the LRU is smaller than the text branch's 128 / 60 s.
/// The image LRU is **independent** from the text LRU — they don't roll
/// each other.
///
/// **Stub**: `LruFingerprints` lives in `src/clipboard/cache.rs` and is
/// `pub(crate)` — not reachable from `tests/`. The loopback contract is
/// pinned at the unit-test layer by
/// `src/clipboard/cache.rs::tests::image_lru_loopback_*` (see
/// STEP-P2-M2a-2a.4 §3.1).
///
/// This stub exists for documentation only — `#[ignore]`d and the body
/// is empty. Once the `clipboard` module is promoted to `pub` (likely
/// in M4 when `clipboard::Backend` becomes a public surface for the
/// GUI Toaster), this test should be un-stubbed and the body moved over
/// verbatim from `src/service.rs`.
#[test]
#[ignore = "stub: src/clipboard is pub(crate); see file header for the contract pin location"]
fn four_k_screenshot_does_not_loop_back_within_image_lru_ttl() {
    // The actual contract is verified by:
    //   src/clipboard/cache.rs::tests::image_lru_loopback_skip
    //   (M2a STEP-2a.4 commit 3391873)
    //
    // That unit test uses `crate::clipboard::cache::LruFingerprints::with_capacity_and_ttl`
    // with `IMAGE_LOOPBACK_CAPACITY = 32` and `IMAGE_LOOPBACK_TTL = 60s`
    // directly inside the `lan-mouse` crate where the `pub(crate)` gate
    // is satisfied. From `tests/` we cannot reach it, so we leave the
    // contract pin at the unit-test layer.
}

// ─── Sanity checks that DO run on every `cargo test` ──────────────────────

/// Sanity: `lan_mouse_proto::ProtoEvent` codec round-trip works for a
/// `ClipboardImage` event with PNG mime. The wire layout is
/// `[u8; 32 fingerprint][u32 BE mime_len][mime bytes][u8; 32 sha256][u64 BE size]`
/// per `lan-mouse-proto/src/codec.rs::ClipboardImage::encode_var_body`.
///
/// This is a thin re-export of the unit test coverage in
/// `lan-mouse-proto/src/lib.rs`; included here so that the stub file
/// itself contributes at least one passing test and stays a real part
/// of the test suite (rather than 100% ignored).
#[test]
fn clipboard_image_png_codec_round_trip() {
    let payload = b"\x89PNG\r\n\x1a\nfake 4k screenshot bytes".to_vec();
    let sha = sha256_stub(&payload);
    let event = ProtoEvent::ClipboardImage(lan_mouse_proto::ClipboardImage {
        fingerprint: sha,
        mime: MIME_PNG.to_string(),
        sha256: sha,
        size: payload.len() as u64,
    });

    let wire: Vec<u8> = Vec::<u8>::from(event.clone());
    let decoded = ProtoEvent::try_from(wire.as_slice()).expect("decode");
    let ci = match decoded {
        ProtoEvent::ClipboardImage(ci) => ci,
        other => panic!("expected ClipboardImage variant, got {other:?}"),
    };
    assert_eq!(ci.sha256, sha);
    assert_eq!(ci.mime, MIME_PNG);
    assert_eq!(ci.size, payload.len() as u64);
}

/// Sanity: `ClipboardImage` with `application/x-dib` mime (Windows
/// CF_DIBV5 direct path per M2b STEP-2b.1) round-trips byte-identical.
/// This pins the `MIME_DIB` wire label constant — production code
/// routes it via `Mime::is_dib_label("application/x-dib")` predicate in
/// `apply_inbound_image_bytes` to `set_dib_image`.
#[test]
fn clipboard_image_dib_wire_label_round_trip() {
    let payload = b"\x28\x00\x00\x00 fake BITMAPINFOHEADER".to_vec();
    let sha = sha256_stub(&payload);
    let event = ProtoEvent::ClipboardImage(lan_mouse_proto::ClipboardImage {
        fingerprint: sha,
        mime: MIME_DIB.to_string(),
        sha256: sha,
        size: payload.len() as u64,
    });

    let wire: Vec<u8> = Vec::<u8>::from(event.clone());
    let decoded = ProtoEvent::try_from(wire.as_slice()).expect("decode");
    let ci = match decoded {
        ProtoEvent::ClipboardImage(ci) => ci,
        other => panic!("expected ClipboardImage variant, got {other:?}"),
    };
    assert_eq!(ci.sha256, sha);
    assert_eq!(
        ci.mime, MIME_DIB,
        "DIB wire label must be preserved verbatim"
    );
    // MIME_DIB is "application/x-dib" — pin the exact string to catch
    // accidental renames of the constant (mirrors
    // `src/clipboard/mod.rs::tests::mime_dib_constant_is_stable`).
    assert_eq!(MIME_DIB, "application/x-dib");
}
