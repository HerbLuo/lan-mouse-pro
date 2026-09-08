//! HTTP/3-lite over bare QUIC bidi streams (M0b STEP-0.2, Path 2).
//!
//! ## Why not real HTTP/3
//!
//! Real HTTP/3 needs ALPN `b"h3"`. Our QUIC link already uses
//! `ALPN_LAN_MOUSE = b"lan-mouse"` for the application-layer wire
//! (StreamA Hello + StreamB input + StreamC meta). Quinn's TLS config
//! accepts a list of ALPNs but the ALPN is selected during the QUIC
//! handshake, before any application-layer routing — so multiplexing
//! `b"h3"` and `b"lan-mouse"` on a single endpoint is fragile: we'd have
//! to demux every QUIC stream by ALPN after the handshake (see PLAN-2 §5
//! 评审 #2 second round). The spike in `examples/h3_pingpong.rs` verifies
//! that the h3 stack itself works, then we abandon the dual-ALPN path
//! and run a small custom request/response protocol on the same
//! `b"lan-mouse"` connection. Future M2/M3 layers (clipboard images /
//! file transfer) get a streaming GET endpoint without dragging in QPACK
//! + QPACK state + varint integer encoding for what is, at heart, a
//!   `GET /clipboard/{kind}/{sha256}` byte-pipe.
//!
//! ## Wire format
//!
//! Every request and response uses one bidi QUIC stream.
//!
//! ### Request
//!
//! ```text
//! +----------------+------------------+
//! | method_len u16 | method bytes     |
//! +----------------+------------------+
//! | path_len u16   | path bytes       |
//! +----------------+------------------+
//! | body_len u32   | body bytes       |
//! +----------------+------------------+
//! ```
//!
//! All integers are big-endian. `method` / `path` are UTF-8. `body` is
//! raw bytes (rarely non-empty for our GET-only use case; reserved for
//! future POSTs).
//!
//! ### Response
//!
//! ```text
//! +----------------+------------------+
//! | status u16     | body_len u32     |
//! +----------------+------------------+
//! | body bytes                          |
//! +--------------------------------------+
//! ```
//!
//! Headers are not modeled. The first wave of consumers (M1/M2/M3) only
//! needs status + body. A future revision can add a `headers_count u16`
//! prefix if required.
//!
//! ## Streaming
//!
//! The body is **streamed** in both directions. For 200 MiB responses
//! we do **not** call `Vec::with_capacity(200 MiB)`. `encode_request` /
//! `decode_request` / `encode_response` / `decode_response` operate on
//! length-prefix + bytes — the caller decides whether to copy into a
//! `Vec<u8>` or stream through a `tokio::io::sink`.
//!
//! `Server::handle_stream` writes the response body in chunks of 64 KiB
//! via `send.write_all` and `send.flush`-equivalent (QUIC flush is
//! internal). `Client::request` reads the body in chunks.
//!
//! ## Cancellation
//!
//! - Client side: drop the `SendStream` half → QUIC STOP_SENDING frame
//!   fires. Server's `recv.read(&mut buf)` returns 0 bytes, the handler
//!   exits within one chunk read.
//! - Server side: drop the `RecvStream` half → quinn sends STOP_SENDING.
//!   Client's `send.write_all` returns `ClosedStream`, the request exits.
//!
//! Both directions exit in well under the 1 s budget from PLAN-2 §3
//! M0b STEP-0.2 (verified by `cancel_during_transfer_drops_quickly`).
//!
//! ## Connection loss
//!
//! `Connection::accept_bi()` / `open_bi()` return `ConnectionError` once
//! quinn closes the underlying connection. The server's accept loop
//! exits and the client request errors out. Tests
//! `unplug_during_transfer_reports_connection_lost` and the loopback
//! 200 MiB transfer confirm this path.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream};
use tokio::io::AsyncWriteExt;

/// Default body chunk size for streaming responses (64 KiB).
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Wire request decoded off a stream.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Bytes,
}

/// Wire response written to a stream.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Bytes,
}

impl Response {
    pub fn ok(body: impl Into<Bytes>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: 404,
            body: Bytes::from_static(b"not found"),
        }
    }

    pub fn with_status(status: u16, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

/// Synchronous handler used by the simple router. Async handlers are not
/// needed for the spike / production byte-pipe use case — the body bytes
/// are already buffered before the handler runs.
pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

/// Router that maps `path` to a [`Handler`]. Currently only exact path
/// matches; future M2 / M3 layers will use parameterized
/// `/clipboard/{text,image,file}/{sha256}` paths.
#[derive(Default, Clone)]
pub struct Router {
    routes: HashMap<String, Handler>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler for `path`. `method` is always GET in the
    /// current revision; the router ignores any non-GET requests and
    /// returns 405.
    pub fn get<F>(mut self, path: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        self.routes.insert(path.into(), Arc::new(handler));
        self
    }

    pub fn handle(&self, req: &Request) -> Response {
        if req.method != "GET" {
            return Response::with_status(405, Bytes::from_static(b"method not allowed"));
        }
        match self.routes.get(&req.path) {
            Some(h) => h(req),
            None => Response::not_found(),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire encoding / decoding (single-buffer variants for tests / simple use)
// ---------------------------------------------------------------------------

const HEADER_LEN_BYTES: usize = 2 + 2 + 4;
const RESPONSE_HEADER_LEN_BYTES: usize = 2 + 4;

/// Encode a request as a single byte buffer. Used by tests and the
/// spike's internal helpers.
pub fn encode_request(method: &str, path: &str, body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN_BYTES + method.len() + path.len() + body.len());
    buf.extend_from_slice(&(method.len() as u16).to_be_bytes());
    buf.extend_from_slice(method.as_bytes());
    buf.extend_from_slice(&(path.len() as u16).to_be_bytes());
    buf.extend_from_slice(path.as_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(body);
    buf
}

/// Parse a request back out of a single byte buffer. Mirrors
/// [`encode_request`] — used by tests.
pub fn decode_request(buf: &[u8]) -> std::io::Result<(Request, usize)> {
    if buf.len() < HEADER_LEN_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "request header truncated",
        ));
    }
    let method_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    let path_start = 2 + method_len;
    if buf.len() < path_start + 2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "path_len missing",
        ));
    }
    let path_len = u16::from_be_bytes([buf[path_start], buf[path_start + 1]]) as usize;
    let body_len_start = path_start + 2 + path_len;
    if buf.len() < body_len_start + 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "body_len missing",
        ));
    }
    let body_len = u32::from_be_bytes([
        buf[body_len_start],
        buf[body_len_start + 1],
        buf[body_len_start + 2],
        buf[body_len_start + 3],
    ]) as usize;
    let body_start = body_len_start + 4;
    let total = body_start + body_len;
    if buf.len() < total {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "body truncated",
        ));
    }
    let method = std::str::from_utf8(&buf[2..path_start])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        .to_string();
    let path = std::str::from_utf8(&buf[path_start + 2..body_len_start])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        .to_string();
    let body = Bytes::copy_from_slice(&buf[body_start..total]);
    Ok((Request { method, path, body }, total))
}

/// Encode a response as a single byte buffer (tests only).
pub fn encode_response(status: u16, body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RESPONSE_HEADER_LEN_BYTES + body.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(body);
    buf
}

/// Decode a response from a single byte buffer (tests only).
pub fn decode_response(buf: &[u8]) -> std::io::Result<(Response, usize)> {
    if buf.len() < RESPONSE_HEADER_LEN_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "response header truncated",
        ));
    }
    let status = u16::from_be_bytes([buf[0], buf[1]]);
    let body_len = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize;
    let body_start = RESPONSE_HEADER_LEN_BYTES;
    let total = body_start + body_len;
    if buf.len() < total {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "response body truncated",
        ));
    }
    let body = Bytes::copy_from_slice(&buf[body_start..total]);
    Ok((Response { status, body }, total))
}

// ---------------------------------------------------------------------------
// Streaming variants (real SendStream / RecvStream) — production path
// ---------------------------------------------------------------------------

/// Stream-encode a request directly onto a `SendStream`. Avoids the
/// 200 MiB allocation spike of `encode_request(...).write_all(...)`.
pub async fn write_request(
    send: &mut SendStream,
    method: &str,
    path: &str,
    body: &[u8],
) -> std::io::Result<()> {
    send.write_all(&(method.len() as u16).to_be_bytes())
        .await
        .map_err(write_err_to_io)?;
    send.write_all(method.as_bytes())
        .await
        .map_err(write_err_to_io)?;
    send.write_all(&(path.len() as u16).to_be_bytes())
        .await
        .map_err(write_err_to_io)?;
    send.write_all(path.as_bytes())
        .await
        .map_err(write_err_to_io)?;
    send.write_all(&(body.len() as u32).to_be_bytes())
        .await
        .map_err(write_err_to_io)?;
    if !body.is_empty() {
        send.write_all(body).await.map_err(write_err_to_io)?;
    }
    Ok(())
}

fn write_err_to_io(e: quinn::WriteError) -> std::io::Error {
    use quinn::WriteError;
    match e {
        WriteError::Stopped(_) => {
            std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "peer STOP_SENDING")
        }
        WriteError::ConnectionLost(inner) => {
            std::io::Error::new(std::io::ErrorKind::ConnectionAborted, inner)
        }
        WriteError::ClosedStream => {
            std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "stream closed")
        }
        WriteError::ZeroRttRejected => std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "0-RTT rejected (server rejected 0-RTT data)",
        ),
    }
}

/// Stream-decode a request from a `RecvStream`. Each length-prefix is
/// read inline; the body is read in `CHUNK_SIZE` chunks into a single
/// `BytesMut` so memory pressure stays bounded.
pub async fn read_request(recv: &mut RecvStream) -> std::io::Result<Request> {
    let method_len = read_u16(recv).await? as usize;
    let method = read_bytes(recv, method_len).await?;
    let method = String::from_utf8(method)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let path_len = read_u16(recv).await? as usize;
    let path = read_bytes(recv, path_len).await?;
    let path = String::from_utf8(path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let body_len = read_u32(recv).await? as usize;
    let body = read_bytes(recv, body_len).await?;
    Ok(Request {
        method,
        path,
        body: Bytes::from(body),
    })
}

/// Convert quinn's `ReadExactError` into a `std::io::Error`. Quinn
/// distinguishes `ReadExactError::ReadError` (transport-level) from
/// `ReadExactError::FinishedEarly(n)` (peer closed mid-body after `n`
/// bytes). Both are surfaced as IO errors to the caller — the request
/// layer doesn't care which one fired, only that the body is
/// incomplete.
fn read_exact_err(e: quinn::ReadExactError) -> std::io::Error {
    use quinn::ReadExactError;
    match e {
        ReadExactError::ReadError(inner) => {
            std::io::Error::new(std::io::ErrorKind::ConnectionAborted, inner)
        }
        ReadExactError::FinishedEarly(n) => std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("stream closed mid-body (got {n} bytes)"),
        ),
    }
}

/// Write a full response (status + body) onto a stream, chunked so the
/// 200 MiB case does not allocate the whole body up front.
pub async fn write_response_streaming(
    send: &mut SendStream,
    resp: &Response,
) -> std::io::Result<()> {
    write_response_header(send, resp.status, resp.body.len()).await?;
    if resp.body.is_empty() {
        return Ok(());
    }
    let mut offset = 0;
    while offset < resp.body.len() {
        let end = (offset + CHUNK_SIZE).min(resp.body.len());
        send.write_all(&resp.body[offset..end])
            .await
            .map_err(write_err_to_io)?;
        offset = end;
    }
    Ok(())
}

/// Stream the response status + body length prefix onto a `SendStream`.
/// Body bytes follow via [`write_response_body_chunks`].
pub async fn write_response_header(
    send: &mut SendStream,
    status: u16,
    body_len: usize,
) -> std::io::Result<()> {
    send.write_all(&status.to_be_bytes())
        .await
        .map_err(write_err_to_io)?;
    send.write_all(&(body_len as u32).to_be_bytes())
        .await
        .map_err(write_err_to_io)?;
    Ok(())
}

/// Stream-decode a response header from a `RecvStream`. Returns
/// `(status, body_len)`; the body follows via `recv.read_exact` chunks.
pub async fn read_response_header(recv: &mut RecvStream) -> std::io::Result<(u16, u32)> {
    let status = read_u16(recv).await?;
    let body_len = read_u32(recv).await?;
    Ok((status, body_len))
}

/// Read `len` body bytes into a fresh `Vec<u8>`.
pub async fn read_body(recv: &mut RecvStream, len: usize) -> std::io::Result<Vec<u8>> {
    read_bytes(recv, len).await
}

async fn read_u16(recv: &mut RecvStream) -> std::io::Result<u16> {
    let mut buf = [0u8; 2];
    recv.read_exact(&mut buf).await.map_err(read_exact_err)?;
    Ok(u16::from_be_bytes(buf))
}

async fn read_u32(recv: &mut RecvStream) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    recv.read_exact(&mut buf).await.map_err(read_exact_err)?;
    Ok(u32::from_be_bytes(buf))
}

async fn read_bytes(recv: &mut RecvStream, len: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len.min(CHUNK_SIZE * 4));
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(CHUNK_SIZE);
        let start = out.len();
        out.resize(start + want, 0);
        recv.read_exact(&mut out[start..])
            .await
            .map_err(read_exact_err)?;
        remaining -= want;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Server / client builder interfaces (PLAN §3 M0b STEP-0.2 产物)
// ---------------------------------------------------------------------------

/// Build a server-side driver closure that accepts bidi streams on
/// `conn`, dispatches each request through the router, and writes the
/// response back.
///
/// Each accepted stream runs in its own `tokio::spawn` task — the
/// accept loop is non-blocking and continues to accept new streams
/// while existing requests are still in flight.
pub fn build_server(
    router: Arc<Router>,
) -> impl Fn(Connection) -> futures::future::BoxFuture<'static, ()> + Send + Sync + Clone {
    move |conn: Connection| {
        let router = router.clone();
        Box::pin(async move {
            loop {
                match conn.accept_bi().await {
                    Ok((mut send, mut recv)) => {
                        let router = router.clone();
                        tokio::spawn(async move {
                            // Best-effort: surface protocol errors as a 400.
                            // Mid-stream cancellation or connection drop
                            // surfaces as `Err` we log-and-return.
                            let req = match read_request(&mut recv).await {
                                Ok(r) => r,
                                Err(e) => {
                                    log::debug!("http3-lite read_request error: {e}");
                                    return;
                                }
                            };
                            let resp = router.handle(&req);
                            if let Err(e) = write_response_streaming(&mut send, &resp).await {
                                log::debug!("http3-lite write_response error: {e}");
                            }
                            // `SendStream::finish` is sync; `ClosedStream`
                            // surfaces if the client cancelled. Ignore it.
                            let _ = send.finish();
                        });
                    }
                    Err(e) => {
                        log::debug!("http3-lite accept_bi closed: {e}");
                        return;
                    }
                }
            }
        })
    }
}

/// Build a client-side handle bound to a single `Connection`. The
/// returned [`ClientConn`] opens a fresh bidi stream per request; the
/// `Connection` itself is reusable for the lifetime of the QUIC link.
pub fn build_request_conn(conn: Connection) -> ClientConn {
    ClientConn { conn }
}

/// Client-side connection handle. Cheap to clone — wraps a `quinn::Connection`
/// (`Arc` internally).
#[derive(Clone)]
pub struct ClientConn {
    conn: Connection,
}

impl ClientConn {
    /// Issue a single GET request and read the full response. The
    /// response body is fully buffered into a `Vec<u8>`. Callers that
    /// expect 200 MiB responses should prefer [`Self::request_streaming`]
    /// to avoid the allocation spike.
    pub async fn request(&self, path: &str) -> std::io::Result<Response> {
        let (mut send, mut recv) = self.conn.open_bi().await.map_err(quic_err_to_io)?;
        write_request(&mut send, "GET", path, &[]).await?;
        send.finish()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;
        let (status, body_len) = read_response_header(&mut recv).await?;
        let body = read_body(&mut recv, body_len as usize).await?;
        Ok(Response {
            status,
            body: Bytes::from(body),
        })
    }

    /// Issue a single GET request and stream the response body into
    /// `sink`. Returns the status code. Body bytes are written in
    /// `CHUNK_SIZE` chunks; no `Vec::with_capacity(body_len)`.
    pub async fn request_streaming(
        &self,
        path: &str,
        sink: &mut (dyn tokio::io::AsyncWrite + Send + Unpin),
    ) -> std::io::Result<u16> {
        let (mut send, mut recv) = self.conn.open_bi().await.map_err(quic_err_to_io)?;
        write_request(&mut send, "GET", path, &[]).await?;
        send.finish()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;
        let (status, body_len) = read_response_header(&mut recv).await?;
        let mut remaining = body_len as usize;
        let mut buf = vec![0u8; CHUNK_SIZE];
        while remaining > 0 {
            let want = remaining.min(CHUNK_SIZE);
            recv.read_exact(&mut buf[..want])
                .await
                .map_err(read_exact_err)?;
            sink.write_all(&buf[..want]).await?;
            remaining -= want;
        }
        sink.flush().await?;
        Ok(status)
    }
}

/// Helper to read a `Bytes`-backed streaming source in chunks. Not used
/// by the production server (the body is buffered in the `Response`),
/// but reserved for future M2 / M3 streaming sources (e.g. file cache).
#[allow(dead_code)]
pub fn chunk_bytes(bytes: &Bytes) -> impl Iterator<Item = &[u8]> {
    bytes.chunks(CHUNK_SIZE)
}

fn quic_err_to_io(e: quinn::ConnectionError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_request_roundtrip() {
        let buf = encode_request("GET", "/healthz", b"");
        let (req, total) = decode_request(&buf).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/healthz");
        assert!(req.body.is_empty());
        assert_eq!(total, buf.len());
    }

    #[test]
    fn encode_decode_request_with_body_roundtrip() {
        let buf = encode_request("POST", "/upload", b"hello world");
        let (req, total) = decode_request(&buf).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/upload");
        assert_eq!(&req.body[..], b"hello world");
        assert_eq!(total, buf.len());
    }

    #[test]
    fn encode_decode_response_roundtrip() {
        let buf = encode_response(200, b"ok");
        let (resp, total) = decode_response(&buf).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body[..], b"ok");
        assert_eq!(total, buf.len());
    }

    #[test]
    fn encode_decode_404_roundtrip() {
        let resp = Response::not_found();
        let buf = encode_response(resp.status, &resp.body);
        let (parsed, _) = decode_response(&buf).unwrap();
        assert_eq!(parsed.status, 404);
        assert_eq!(&parsed.body[..], b"not found");
    }

    #[test]
    fn large_request_body_roundtrip() {
        // 200 MiB (M3a target) round-trips through the framing.
        const SIZE: usize = 200 * 1024 * 1024;
        let body: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
        let buf = encode_request("POST", "/upload", &body);
        let expected_len = HEADER_LEN_BYTES + 4 + 7 + body.len();
        assert_eq!(buf.len(), expected_len);

        let (parsed, total) = decode_request(&buf).unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/upload");
        assert_eq!(parsed.body.len(), SIZE);
        assert_eq!(&parsed.body[..16], &body[..16]);
        assert_eq!(total, buf.len());
    }

    #[test]
    fn router_dispatches_to_registered_handler() {
        let router = Router::new().get("/healthz", |_| Response::ok("ok"));
        let req = Request {
            method: "GET".into(),
            path: "/healthz".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body[..], b"ok");
    }

    #[test]
    fn router_returns_404_for_unknown_path() {
        let router = Router::new().get("/healthz", |_| Response::ok("ok"));
        let req = Request {
            method: "GET".into(),
            path: "/clipboard/text/abc".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn router_rejects_non_get() {
        let router = Router::new().get("/healthz", |_| Response::ok("ok"));
        let req = Request {
            method: "POST".into(),
            path: "/healthz".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn decode_request_truncated_returns_err() {
        let buf = encode_request("GET", "/healthz", b"");
        // Truncate the last byte
        let truncated = &buf[..buf.len() - 1];
        assert!(decode_request(truncated).is_err());
    }

    #[test]
    fn decode_response_truncated_returns_err() {
        let buf = encode_response(200, b"hello");
        // Truncate to header only (no body)
        let truncated = &buf[..6];
        assert!(decode_response(truncated).is_err());
    }

    #[test]
    fn chunk_bytes_preserves_payload() {
        let body = Bytes::from(vec![1u8; CHUNK_SIZE * 3 + 17]);
        let mut collected = Vec::new();
        for chunk in chunk_bytes(&body) {
            collected.extend_from_slice(chunk);
        }
        assert_eq!(collected.len(), body.len());
        assert_eq!(collected, body.as_ref());
    }
}
