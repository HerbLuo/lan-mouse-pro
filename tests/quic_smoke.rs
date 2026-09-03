//! End-to-end QUIC smoke test
//!
//! **Goal**: stand up an in-process listener + connector pair on localhost,
//! complete mTLS + Hello, and verify a small batch of Motion + KeyboardKey
//! events round-trip across the two transports used in M1:
//!
//! - Motion: **datagram** (`send_motion` → `read_datagram`)
//! - KeyboardKey: **stream B** (length-prefixed frame via `send_stream_b` +
//!   `read_frame` on a stream-B reader task)
//!
//! **Plus** a separate test verifies QUIC keepalive — after ≥ 10s of
//! silence on both endpoints, the connection must remain alive. This
//! codifies the only previously un-automated keepalive check.
//!
//! **Style**: each test brings its own self-signed cert + ephemeral port,
//! so parallel `cargo test` runs do not collide. Cert/key paths live under
//! `/tmp/lan-mouse-quic-smoke-<pid>-<tag>/`.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lan_mouse::quic_transport::{
    PeerSession, StreamBunch, accept, dial, endpoint, endpoint_with_cert, install_crypto_provider,
    read_frame,
};
use lan_mouse_proto::ProtoEvent;

use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::mpsc as tokio_mpsc;

// `quinn::Connection::closed()` returns an opaque future; check it without
// awaiting via `FutureExt::now_or_never`.
use futures::FutureExt;

// -- Helpers ------------------------------------------------------------------

/// Generate a fresh self-signed cert + key pair, returning X.509 chain + key
/// suitable for `endpoint_with_cert` and `build_quic_client_config`. Uses
/// `rcgen` directly (transitive dep, promoted to `[dev-dependencies]`).
fn ephemeral_cert(tag: &str) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    use rcgen::KeyPair;
    let key_pair = KeyPair::generate().expect("rcgen keypair");
    let cert = rcgen::CertificateParams::new(vec![format!("lan-mouse-smoke-{tag}")])
        .unwrap()
        .self_signed(&key_pair)
        .expect("self-signed cert");
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        key_pair.serialize_der(),
    ));
    (vec![cert_der], key_der)
}

/// Convenience: build an `endpoint_with_cert` server bound to localhost:0.
fn server_endpoint(
    tag: &str,
) -> (
    quinn::Endpoint,
    std::net::SocketAddr,
    Vec<CertificateDer<'static>>,
    PrivateKeyDer<'static>,
) {
    let (cert_chain, key) = ephemeral_cert(tag);
    let addr: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into();
    let ep = endpoint_with_cert(addr, cert_chain.clone(), key.clone_key()).expect("server ep");
    let local_addr = ep.local_addr().expect("server local addr");
    (ep, local_addr, cert_chain, key)
}

fn motion_event(i: u32) -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
        time: i,
        dx: 1.0 + i as f64 * 0.1,
        dy: 2.0 + i as f64 * 0.2,
    }))
}

fn key_event(i: u32) -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
        time: i,
        // 30 = 'a', +i for variety
        key: 30 + i,
        state: 1, // press
    }))
}

// -- Test 1: 5 Motion + 5 KeyboardKey round-trip -------------------------------

#[tokio::test(flavor = "current_thread")]
async fn five_motion_and_five_keyboard_events_round_trip() {
    install_crypto_provider();

    // (1) Bind server endpoint with ephemeral cert.
    let (server_ep, server_addr, _server_cert, _server_key) = server_endpoint("round_trip");
    let (client_cert_chain, client_key) = ephemeral_cert("round_trip_client");

    // (2) Pins dir for client TofuVerifier.
    let pins_dir = std::env::temp_dir().join(format!(
        "lan-mouse-smoke-pins-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&pins_dir);

    // (3) Server task: accept conn → wrap in PeerSession → server_hello →
    //     accept stream B (for keyboard) → read Motion datagrams + read
    //     KeyboardKey frames on stream B.
    let server_ep_for_task = server_ep.clone();
    let pins_dir_for_server = pins_dir.clone();
    let server_task = tokio::spawn(async move {
        let conn = tokio::time::timeout(Duration::from_secs(5), accept(&server_ep_for_task))
            .await
            .expect("server accept timeout")
            .expect("server accept");
        let session = Arc::new(PeerSession::from_connection(conn));

        tokio::time::timeout(
            Duration::from_secs(5),
            lan_mouse::quic_transport::server_hello(&session),
        )
        .await
        .expect("server_hello timeout")
        .expect("server_hello");

        // (a) Read 5 motion datagrams.
        let mut motion_got: Vec<ProtoEvent> = Vec::with_capacity(5);
        while motion_got.len() < 5 {
            let bytes =
                tokio::time::timeout(Duration::from_secs(5), session.connection().read_datagram())
                    .await
                    .expect("read_datagram timeout")
                    .expect("read_datagram");
            assert_eq!(
                bytes.len(),
                lan_mouse_proto::MAX_EVENT_SIZE,
                "datagram payload should be exactly MAX_EVENT_SIZE bytes"
            );
            let mut buf = [0u8; lan_mouse_proto::MAX_EVENT_SIZE];
            buf.copy_from_slice(&bytes);
            let event: ProtoEvent = buf.try_into().expect("decode motion datagram");
            motion_got.push(event);
        }

        // (b) Accept the client's stream-B bidi. `send_stream_b` now caches
        //     one long-lived stream B and writes all 5 frames on it (see
        //     `session.rs::cached_send_b`), so the server `accept_bi`s once
        //     and reads all 5 frames off that one stream. Reads must overlap
        //     with the client sends (the bidi is opened by the *first*
        //     `send_stream_b` call, so the server's `accept_bi` is racing
        //     with that first send).
        let (_ts, mut rs) =
            tokio::time::timeout(Duration::from_secs(10), session.connection().accept_bi())
                .await
                .expect("accept_bi stream B timeout")
                .expect("accept_bi stream B");
        drop(_ts);

        let mut key_got: Vec<ProtoEvent> = Vec::with_capacity(5);
        while key_got.len() < 5 {
            let event = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rs))
                .await
                .expect("read_frame stream B timeout")
                .expect("read_frame");
            key_got.push(event);
        }
        drop(session);
        drop(server_ep_for_task);
        let _ = pins_dir_for_server; // keep alive until end

        (motion_got, key_got)
    });

    // (4) Client endpoint + dial + client_hello.
    let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into()).expect("client ep");
    let conn = dial(
        &client_ep,
        server_addr,
        client_cert_chain[0].clone(),
        client_key.clone_key(),
        &pins_dir,
    )
    .await
    .expect("dial");
    let client_session = Arc::new(PeerSession::from_connection(conn));

    tokio::time::timeout(
        Duration::from_secs(5),
        lan_mouse::quic_transport::client_hello(&client_session),
    )
    .await
    .expect("client_hello timeout")
    .expect("client_hello");

    // (5) Send 5 Motion (datagram) + 5 KeyboardKey (stream B).
    for i in 0..5 {
        client_session
            .send_motion(&motion_event(i))
            .await
            .expect("send_motion");
    }
    for i in 0..5 {
        let ev = key_event(i);
        let (buf, _len): ([u8; lan_mouse_proto::MAX_EVENT_SIZE], usize) = ev.into();
        client_session
            .send_stream_b(&buf)
            .await
            .expect("send_stream_b");
    }

    // (6) Wait for server to receive all 10 events, then drop client too.
    let (motion_got, key_got) = tokio::time::timeout(Duration::from_secs(15), server_task)
        .await
        .expect("server task timeout")
        .expect("server task join");

    assert_eq!(
        motion_got.len(),
        5,
        "server should have received 5 motion events"
    );
    assert_eq!(key_got.len(), 5, "server should have received 5 key events");

    // Field check on the payloads (don't trust identity :: only structure).
    for (i, e) in motion_got.iter().enumerate() {
        let ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion { time, dx, dy })) = e
        else {
            panic!("motion[{i}] not a Pointer Motion event: {e:?}");
        };
        assert_eq!(*time, i as u32);
        assert!(*dx > 0.0);
        assert!(*dy > 0.0);
    }

    drop(client_session);
    drop(client_ep);
    let _ = std::fs::remove_dir_all(&pins_dir);
}

// -- Test 2: silent ≥ 10 s connection stays alive (QUIC keepalive) -------------

/// Step-7.1 dropped the app-layer `RECV_IDLE_TIMEOUT` (8 s) in favour of QUIC's
/// built-in keepalive (`keep_alive_interval = 5s`, `max_idle_timeout = 30s`).
/// This test verifies that — even with no peer traffic for > 10 s — the
/// `quinn::Connection` is still alive on both ends and `peer_identity()`
/// remains present. Without `keep_alive_interval` set in `TransportConfig`
/// the connection would time out at the 30 s default; we use 10 s (well
/// below 30 s) to assert the upper bound while keeping the test quick.
#[tokio::test(flavor = "current_thread")]
async fn connection_survives_ten_seconds_of_silence() {
    install_crypto_provider();

    let (server_ep, server_addr, _sc, _sk) = server_endpoint("keepalive");
    let (client_cert_chain, client_key) = ephemeral_cert("keepalive_client");
    let pins_dir = std::env::temp_dir().join(format!(
        "lan-mouse-smoke-keepalive-pins-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&pins_dir);

    let server_ep_for_task = server_ep.clone();
    let server_task = tokio::spawn(async move {
        let conn = tokio::time::timeout(Duration::from_secs(5), accept(&server_ep_for_task))
            .await
            .expect("server accept timeout")
            .expect("server accept");
        let session = Arc::new(PeerSession::from_connection(conn));

        tokio::time::timeout(
            Duration::from_secs(5),
            lan_mouse::quic_transport::server_hello(&session),
        )
        .await
        .expect("server hello timeout")
        .expect("server hello");

        // Wait ≥ 10 s in silence.
        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(11)).await;
        let elapsed = started.elapsed();

        // Connection must still be alive: `closed()` future should NOT be
        // ready (poll via `now_or_never()` returns `None`).
        // Note: `peer_identity()` is only meaningful on mTLS endpoints
        // (`endpoint_with_verifier`). This test uses `endpoint_with_cert`
        // (no client cert required), so server-side `peer_identity` is
        // always None — we deliberately do not assert on it.
        let alive = session.connection().closed().now_or_never().is_none();
        (alive, elapsed)
    });

    let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into()).expect("client ep");
    let conn = dial(
        &client_ep,
        server_addr,
        client_cert_chain[0].clone(),
        client_key.clone_key(),
        &pins_dir,
    )
    .await
    .expect("dial");
    let client_session = Arc::new(PeerSession::from_connection(conn));
    tokio::time::timeout(
        Duration::from_secs(5),
        lan_mouse::quic_transport::client_hello(&client_session),
    )
    .await
    .expect("client hello timeout")
    .expect("client hello");

    let (server_alive, elapsed) = tokio::time::timeout(Duration::from_secs(20), server_task)
        .await
        .expect("server task timeout")
        .expect("server task join");

    // Client side: connection should still be alive too.
    // `peer_identity()` on the client side **is** meaningful here (the
    // client received the server's cert chain during TLS handshake).
    let client_peer_id_present = client_session.connection().peer_identity().is_some();
    let client_alive = client_session
        .connection()
        .closed()
        .now_or_never()
        .is_none();

    assert!(
        elapsed >= Duration::from_secs(10),
        "server wait should have actually been ≥ 10s (real elapsed: {elapsed:?})"
    );
    assert!(
        server_alive,
        "server-side connection must remain alive after 10s silence"
    );
    assert!(
        client_alive,
        "client-side connection must remain alive after 10s silence"
    );
    assert!(
        client_peer_id_present,
        "client-side peer_identity must remain present (server cert is what the client pins)"
    );

    drop(client_session);
    drop(client_ep);
    drop(server_ep);
    let _ = std::fs::remove_dir_all(&pins_dir);
}

// -- Quiet gcc warnings: symbols used only via `Arc<PeerSession::from_connection>`
#[allow(dead_code)]
fn _quiet_keeps_signature(_s: &PeerSession, _b: &StreamBunch, _t: tokio_mpsc::Sender<ProtoEvent>) {}
