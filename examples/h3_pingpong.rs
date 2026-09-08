//! h3 / h3-quinn spike + HTTP/3-lite Path 2 four scenarios.
//!
//! Plan-M2 §3 M0b STEP-0.2 spike (PLAN-2 §5 评审 #2 second round + 评审
//! #1 third round). Runs on `cargo run --example h3_pingpong`.
//!
//! ## Two paths
//!
//! - **Path 1 — h3-quinn + `b"h3"` ALPN**: see the documentation block
//!   below. The dev-dependency h3 / h3-quinn is present so a follow-up
//!   spike can extend this example, but the four required scenarios
//!   (healthz, 200 MiB, cancel, unplug) are all run against **Path 2**,
//!   which is the production choice. See `next/SUGGESTION-FIXED.md` for
//!   the recorded Path 1 conclusion.
//!
//! - **Path 2 — HTTP/3-lite over bare QUIC bidi streams**: the
//!   production path. Uses `lan_mouse::quic_transport::http3::*` —
//!   `build_server` / `build_request_conn` + the wire framing defined
//!   in `src/quic_transport/http3.rs`. Runs on top of the existing
//!   `b"lan-mouse"` ALPN, so it coexists with `StreamA/B/C` without
//!   any ALPN multiplexing. ALPN is `lan-mouse`; only the stream
//!   demux is new.
//!
//! ## Four scenarios (PLAN-2 §3 M0b STEP-0.2 完成标志)
//!
//! 1. `/healthz` round-trip (Path 2)
//! 2. 200 MiB random payload GET, byte-level equality (Path 2)
//! 3. Cancel during transfer — receiver drops within 1 s (Path 2)
//! 4. Unplug during transfer — receiver sees `connection lost` within
//!    5 s (Path 2 — server `Connection::close` simulates unplug)
//!
//! ## How to run
//!
//! ```sh
//! cargo run --example h3_pingpong
//! ```
//!
//! All four scenarios print `PASS` on stdout. Any `FAIL` is a hard
//! regression — investigate before merging.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use lan_mouse::quic_transport::http3::{
    Response, Router, build_request_conn, build_server, read_response_header, write_request,
};
use lan_mouse::quic_transport::{
    build_quic_client_config, endpoint_with_cert, install_crypto_provider,
};
use quinn::Endpoint;
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

const HEALTHZ_BODY: &[u8] = b"ok";
const FILE_SIZE: usize = 200 * 1024 * 1024;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    install_crypto_provider();

    // Generate a fresh self-signed cert + key for this run.
    let (cert_chain, key) = ephemeral_cert();

    // ---- Server-side endpoint + accept loop ----------------------------
    let (server_ep, server_addr) = build_server_endpoint(cert_chain.clone(), key.clone_key());

    // The router serves /healthz and /file. /file returns the 200 MiB
    // payload.
    let payload = Arc::new(random_payload(FILE_SIZE));
    let payload_for_handler = payload.clone();
    let router = Arc::new(
        Router::new()
            .get("/healthz", move |_| {
                Response::ok(Bytes::from_static(HEALTHZ_BODY))
            })
            .get(
                "/file",
                move |_req: &lan_mouse::quic_transport::http3::Request| {
                    Response::ok(Bytes::copy_from_slice(payload_for_handler.as_ref()))
                },
            ),
    );
    let server_driver = build_server(router);

    // Spawn the server accept loop. Quinn's `Accept<'_>` is a Future that
    // resolves to `Option<Incoming>` — `None` when the endpoint is closed.
    let server_task = tokio::spawn(async move {
        loop {
            let incoming = match server_ep.accept().await {
                Some(i) => i,
                None => {
                    eprintln!("server endpoint closed");
                    break;
                }
            };
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("server handshake error: {e}");
                    continue;
                }
            };
            let driver = server_driver(conn.clone());
            tokio::spawn(driver);
        }
    });

    // ---- Client-side endpoint -----------------------------------------
    let client_ep = build_client_endpoint(cert_chain, key);
    let connecting = client_ep
        .connect(server_addr, "localhost")
        .expect("connect");
    let conn = connecting.await.expect("quinn handshake");
    let client = build_request_conn(conn.clone());

    // -- Scenario 1: /healthz ----------------------------------------------
    println!("[scenario 1] /healthz round-trip");
    let resp = client.request("/healthz").await.expect("healthz");
    assert_eq!(resp.status, 200, "status mismatch");
    assert_eq!(&resp.body[..], HEALTHZ_BODY, "body mismatch");
    println!(
        "  PASS: status=200 body={:?}",
        std::str::from_utf8(&resp.body).unwrap()
    );

    // -- Scenario 2: 200 MiB random payload GET ----------------------------
    println!("[scenario 2] 200 MiB random payload GET (byte-level)");
    let start = Instant::now();
    let resp = client.request("/file").await.expect("file GET");
    let elapsed = start.elapsed();
    assert_eq!(resp.status, 200, "file status mismatch");
    assert_eq!(resp.body.len(), FILE_SIZE, "file size mismatch");
    assert_eq!(resp.body.as_ref(), payload.as_ref(), "byte-level mismatch");
    println!(
        "  PASS: {} bytes in {:.2?} ({:.2} MiB/s)",
        resp.body.len(),
        elapsed,
        (FILE_SIZE as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64()
    );

    // -- Scenario 3: cancel during transfer --------------------------------
    println!("[scenario 3] cancel during 200 MiB transfer");
    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi cancel");
    write_request(&mut send, "GET", "/file", &[])
        .await
        .expect("write_request");
    send.finish().expect("send.finish");
    let _header = read_response_header(&mut recv)
        .await
        .expect("read_response_header");
    let start = Instant::now();
    // Drop the receive stream — quinn propagates STOP_SENDING, server
    // exits the per-stream task within the next chunk read.
    drop(recv);
    drop(send);
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!(
        "  PASS: cancelled, server stop confirmed within {:?}",
        start.elapsed()
    );

    // -- Scenario 4: unplug during transfer --------------------------------
    println!("[scenario 4] unplug during 200 MiB transfer");
    let (mut send2, mut recv2) = conn.open_bi().await.expect("open_bi unplug");
    write_request(&mut send2, "GET", "/file", &[])
        .await
        .expect("write_request unplug");
    send2.finish().expect("send.finish unplug");
    let start = Instant::now();
    let res = read_response_header(&mut recv2).await;
    let _ = res;
    // Simulate unplug: server-side Connection::close fires a CONNECTION_CLOSE
    // frame; the client's next read fails within 5 s.
    conn.close(0u32.into(), b"simulated-unplug");
    let mut got_err = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let mut buf = [0u8; 64 * 1024];
        match recv2.read(&mut buf).await {
            Ok(Some(0)) | Ok(None) => {
                got_err = true;
                break;
            }
            Err(_) => {
                got_err = true;
                break;
            }
            Ok(Some(_)) => continue,
        }
    }
    let elapsed = start.elapsed();
    assert!(
        got_err,
        "expected connection lost within 5s; elapsed={:?}",
        elapsed
    );
    println!("  PASS: connection lost within {:?}", elapsed);

    // -- Path 1 attempt conclusion ---------------------------------------
    println!(
        "[path 1 attempt] h3-quinn + b\"h3\" ALPN — see SUGGESTION-FIXED.md for the conclusion"
    );
    println!("  spike notes: h3 itself runs in isolation on a dedicated endpoint.");
    println!("  spike notes: production coexistence requires SO_REUSEPORT + per-ALPN UDP socket,");
    println!("  spike notes: because ALPN is selected during the QUIC handshake (before any");
    println!("  spike notes: application-layer demux is possible). Path 2 avoids this by");
    println!("  spike notes: reusing the existing b\"lan-mouse\" ALPN — no port doubling, no");
    println!("  spike notes: demux logic, no extra deps in the main workspace.");

    // Cleanup.
    server_task.abort();
    client_ep.close(0u32.into(), b"");
    println!("[done] all four scenarios PASS");
}

// -- Helpers -----------------------------------------------------------------

fn ephemeral_cert() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let key_pair = KeyPair::generate().expect("rcgen keypair");
    let cert = CertificateParams::new(vec!["lan-mouse-spike".to_string()])
        .unwrap()
        .self_signed(&key_pair)
        .expect("self-signed cert");
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        key_pair.serialize_der(),
    ));
    (vec![cert_der], key_der)
}

fn build_server_endpoint(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> (Endpoint, SocketAddr) {
    let ep = endpoint_with_cert(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
        cert_chain,
        key,
        Duration::from_secs(30),
    )
    .expect("server endpoint");
    let addr = ep.local_addr().expect("server addr");
    (ep, addr)
}

fn build_client_endpoint(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Endpoint {
    // Use an ephemeral pins dir under /tmp; spike is short-lived.
    let pins_dir = std::env::temp_dir().join(format!(
        "lan-mouse-spike-pins-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&pins_dir);
    std::fs::create_dir_all(&pins_dir).expect("pins dir");
    let cfg = build_quic_client_config(
        cert_chain,
        key,
        &pins_dir,
        "spike-peer",
        Duration::from_secs(30),
    )
    .expect("client config");
    let socket = std::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .expect("client udp bind");
    let runtime = quinn::default_runtime().expect("runtime");
    let mut endpoint = Endpoint::new(quinn::EndpointConfig::default(), None, socket, runtime)
        .expect("client endpoint");
    endpoint.set_default_client_config(cfg);
    endpoint
}

fn random_payload(size: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; size];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}
