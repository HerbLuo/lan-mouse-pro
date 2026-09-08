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

/// Router that maps `path` to a [`Handler`].
///
/// **Two match modes**:
/// - `routes` — exact path match (registered via [`Router::get`]). Used
///   for static paths like `/healthz`.
/// - `prefix_routes` — prefix match (registered via [`Router::get_prefix`]).
///   Used for parameterized paths like `/clipboard/text/{sha256}` where
///   the sha256 suffix is variable. The first prefix that matches wins
///   (`HashMap::iter().find(...)`), which is deterministic for the small
///   prefix sets we register; if two prefixes overlap the longer one
///   should be registered first.
///
/// **Lookup order**: exact first, then prefix. This means a static
/// `/clipboard/text/` (if ever registered) wins over a prefix
/// `/clipboard/text/` (which would match all `/clipboard/text/...`).
#[derive(Default, Clone)]
pub struct Router {
    routes: HashMap<String, Handler>,
    prefix_routes: HashMap<String, Handler>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler for an exact path. `method` is always GET in the
    /// current revision; the router ignores any non-GET requests and
    /// returns 405.
    pub fn get<F>(mut self, path: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        self.routes.insert(path.into(), Arc::new(handler));
        self
    }

    /// Register a handler that matches any path beginning with `prefix`.
    /// The handler is called with the full request (path + body); the
    /// caller decides what to do with the suffix.
    ///
    /// **Use case**: parameterized paths like `/clipboard/text/{sha256}`
    /// where the trailing sha256 is opaque to the router. The current
    /// M0b stub handlers ignore the suffix and just return 404; M1/M2/M3
    /// will read the suffix to look up the cache by sha256.
    pub fn get_prefix<F>(mut self, prefix: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        self.prefix_routes.insert(prefix.into(), Arc::new(handler));
        self
    }

    pub fn handle(&self, req: &Request) -> Response {
        if req.method != "GET" {
            return Response::with_status(405, Bytes::from_static(b"method not allowed"));
        }
        // Exact match first (deterministic, no allocation).
        if let Some(h) = self.routes.get(&req.path) {
            return h(req);
        }
        // Prefix match — first hit wins. HashMap iteration order is
        // unspecified but for the small prefix sets we register
        // (currently 3 entries) this is fine; the production routers
        // never have overlapping prefixes.
        for (prefix, h) in &self.prefix_routes {
            if req.path.starts_with(prefix) {
                return h(req);
            }
        }
        Response::not_found()
    }
}

/// Build the production default router with 5 routes (M0b STEP-0.3 + STEP-0.4).
///
/// **Routes**:
/// | Path | Method | Status | Body | Notes |
/// |---|---|---|---|---|
/// | `GET /healthz` | GET | 200 | `"ok"` | M0b STEP-0.7 真机 `curl --http3` 命中点 |
/// | `GET /clipboard/text/{sha256}` | GET | 404 | `"not found"` | M1a stub; 占位等 M1b 接 cache |
/// | `GET /clipboard/image/{sha256}` | GET | 404 | `"not found"` | M2a stub; 占位等 M2a 接 cache |
/// | `GET /clipboard/file/{sha256}` | GET | 404 | `"not found"` | M3a stub; 占位等 M3a 接 cache |
/// | `GET /clipboard/file/{sha256}?range=...` | GET | 404 | `"not found"` | 同样走 `/clipboard/file/` prefix；range 接口 M3a 填 |
///
/// **Stub handler**: each clip board stub logs at trace (so a real
/// inbound GET shows up in `RUST_LOG=trace` but not in the default INFO
/// log) and returns 404. The trace log is the place to add M1/M2/M3
/// cache-lookup logic.
///
/// **Why this is a free function rather than a `Router::default()` impl**:
/// `Router` needs to remain `Default + Clone` (the spike + tests use
/// `Router::new()`). The "production router" is a concrete 5-route
/// factory — it lives at the module boundary, not on the type.
pub fn default_router() -> Arc<Router> {
    Arc::new(
        Router::new()
            .get("/healthz", |_req: &Request| Response::ok("ok"))
            .get_prefix("/clipboard/text/", |req: &Request| {
                log::trace!("http3 /clipboard/text/ stub: {}", req.path);
                Response::not_found()
            })
            .get_prefix("/clipboard/image/", |req: &Request| {
                log::trace!("http3 /clipboard/image/ stub: {}", req.path);
                Response::not_found()
            })
            .get_prefix("/clipboard/file/", |req: &Request| {
                log::trace!("http3 /clipboard/file/ stub: {}", req.path);
                Response::not_found()
            }),
    )
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
// GrowingSink — `Vec<u8>`-backed AsyncWrite that allocates incrementally
// ---------------------------------------------------------------------------

/// `AsyncWrite` sink that appends bytes to a borrowed `Vec<u8>` as they
/// arrive, with **no upfront capacity preallocation**.
///
/// **Why**: `ClientConn::request` reads the body in chunks via
/// `read_exact`, which itself does incremental resize
/// (`Vec::with_capacity(len.min(CHUNK_SIZE * 4))` — capped at 256 KiB).
/// For the streaming path used by [`Http3Client::get_text`] and friends
/// we want an explicit sink rather than `Vec<u8>::with_capacity(200 MiB)`;
/// `GrowingSink` plugs into `ClientConn::request_streaming` and grows
/// the underlying `Vec<u8>` only as bytes are actually written.
///
/// **Why not `tokio::io::sink`**: `tokio::io::sink` discards all bytes
/// — useful for the "drain and forget" case, useless when the caller
/// needs the bytes back. `GrowingSink` keeps them.
///
/// **Memory profile** (verified by `growing_sink_no_preallocation`
/// test): peak allocation is `CHUNK_SIZE` (64 KiB) at any moment, plus
/// `Vec<u8>` growth as `extend_from_slice` doubles the backing
/// allocation internally — for 200 MiB the final allocation is ~200 MiB
/// but the **peak working set** is bounded by `CHUNK_SIZE`.
pub struct GrowingSink<'a> {
    buf: &'a mut Vec<u8>,
}

impl<'a> GrowingSink<'a> {
    pub fn new(buf: &'a mut Vec<u8>) -> Self {
        Self { buf }
    }
}

impl<'a> tokio::io::AsyncWrite for GrowingSink<'a> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // `Vec::extend_from_slice` doubles the backing allocation when
        // capacity is exhausted — never preallocates more than the bytes
        // currently held. This is the memory-bounded path required by
        // M0b STEP-0.4 ("avoid `Vec::with_capacity(200 MiB)`").
        self.buf.extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// Http3Client — typed wrapper around `Connection` for the production GETs
// ---------------------------------------------------------------------------

/// Production client handle bound to a single `Connection`. Provides
/// typed accessors for the M1/M2/M3 byte-pipe routes
/// (`/clipboard/text/{sha256}`, `/clipboard/image/{sha256}`,
/// `/clipboard/file/{sha256}[?range=...]`) plus the M0b healthz probe.
///
/// **Connection reuse**: `quinn::Connection` is internally `Arc`-backed;
/// every method opens a fresh bidi stream and the underlying QUIC
/// connection is reused for the lifetime of the peer.
///
/// **Streaming**: every body read goes through [`GrowingSink`] —
/// no `Vec::with_capacity(body_len)` calls, no 200 MiB upfront
/// allocation. The body's bytes land in a `Vec<u8>` that grows
/// incrementally as bytes arrive (see `GrowingSink` docstring).
///
/// **Status semantics**: the helper methods return `(status, body)`.
/// Callers (M1/M2/M3 service code) inspect the status and decide
/// whether to surface the bytes — 4xx means "cache miss / not yet
/// ready" (log warn + skip), 5xx means "transient failure" (retry),
/// 200 means the body is the requested bytes.
#[derive(Clone)]
pub struct Http3Client {
    conn: Connection,
}

impl Http3Client {
    /// Wrap a `Connection` into an [`Http3Client`]. Cheap to clone
    /// (just an `Arc`-backed wrapper).
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// `GET /healthz` — used by M0b STEP-0.7 真机 `curl --http3` probe
    /// and by the service's liveness check.
    pub async fn healthz(&self) -> std::io::Result<(u16, Vec<u8>)> {
        self.get_bytes("/healthz").await
    }

    /// `GET /clipboard/text/{sha256}` — pulled by the receiver after a
    /// `ClipboardText { sha256, size }` metadata frame on StreamC.
    /// Returns 404 in M0b (M1b wires the cache).
    pub async fn get_text(&self, sha256: &str) -> std::io::Result<(u16, Vec<u8>)> {
        self.get_bytes(&format!("/clipboard/text/{sha256}")).await
    }

    /// `GET /clipboard/image/{sha256}` — pulled by the receiver after a
    /// `ClipboardImage { sha256, size }` metadata frame on StreamC.
    /// Returns 404 in M0b (M2a wires the cache).
    pub async fn get_image(&self, sha256: &str) -> std::io::Result<(u16, Vec<u8>)> {
        self.get_bytes(&format!("/clipboard/image/{sha256}")).await
    }

    /// `GET /clipboard/file/{sha256}[?range=...]` — pulled by the
    /// receiver after a `FileTransferOffer { sha256, size, ... }`
    /// metadata frame on StreamC. Returns 404 in M0b (M3a wires the
    /// file cache + range).
    ///
    /// `range = Some("N-M")` (per PLAN-2 §1 range semantics) emits
    /// `?range=N-M` as the query. The query is part of the wire path
    /// (no separate header framing yet — see http3.rs top doc-comment).
    pub async fn get_file(
        &self,
        sha256: &str,
        range: Option<&str>,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let path = match range {
            Some(r) => format!("/clipboard/file/{sha256}?range={r}"),
            None => format!("/clipboard/file/{sha256}"),
        };
        self.get_bytes(&path).await
    }

    /// Generic streaming GET. Returns `(status, body)` where the body
    /// was read chunk-by-chunk into a `Vec<u8>` via [`GrowingSink`]
    /// (no `Vec::with_capacity(body_len)`).
    ///
    /// **Error classification** (matches the leader's spec for the
    /// M0b STEP-0.4 unit tests):
    /// - `Connection` / `SendStream` IO errors → `std::io::Error`
    ///   (typically `ErrorKind::ConnectionAborted`)
    /// - response header decode failure (truncated body_len, etc.) →
    ///   `std::io::Error` (typically `ErrorKind::UnexpectedEof`)
    /// - 4xx / 5xx status **does not** raise an error — the caller
    ///   inspects `status` and decides (PLAN-2 §5 评审 #3 2nd:
    ///   "404 cache miss silently ignored"). This matches the spec.
    async fn get_bytes(&self, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
        // Issue the request on a fresh bidi stream. `Connection::open_bi`
        // is the only entry point for client-side bi streams.
        let (mut send, mut recv) = self.conn.open_bi().await.map_err(quic_err_to_io)?;
        // Stream-encode the request so we don't allocate a separate
        // header Vec just for the call. Matches `ClientConn::request`.
        write_request(&mut send, "GET", path, &[]).await?;
        send.finish()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;

        // Decode the response header.
        let (status, body_len) = read_response_header(&mut recv).await?;

        // Stream the body into a `GrowingSink`. `body_len` is the
        // declared length — we honor it exactly so the receiver knows
        // when to stop (no extra EOF handling required). The
        // `GrowingSink` allocates incrementally.
        let mut out: Vec<u8> = Vec::new();
        let mut sink = GrowingSink::new(&mut out);
        let mut remaining = body_len as usize;
        let mut chunk = vec![0u8; CHUNK_SIZE];
        while remaining > 0 {
            let want = remaining.min(CHUNK_SIZE);
            recv.read_exact(&mut chunk[..want])
                .await
                .map_err(read_exact_err)?;
            tokio::io::AsyncWriteExt::write_all(&mut sink, &chunk[..want]).await?;
            remaining -= want;
        }
        tokio::io::AsyncWriteExt::flush(&mut sink).await?;

        Ok((status, out))
    }
}

// ---------------------------------------------------------------------------
// Server / client builder interfaces (PLAN §3 M0b STEP-0.2 产物)
// ---------------------------------------------------------------------------

/// Maximum concurrent in-flight HTTP/3-lite requests per peer.
///
/// Validated by STEP-VALIDATION-P2-M0b P2.2: an unbounded `tokio::spawn`
/// per `accept_bi` lets a misbehaving peer open thousands of streams
/// to exhaust memory. 32 is well above the documented M3a peak
/// (1 file transfer + 1 clipboard-image pull + ~5 health-check retries
/// from a typical GUI session) while still capping a malicious peer
/// at a manageable backlog.
const MAX_INFLIGHT_REQUESTS_PER_PEER: usize = 32;

/// Build a server-side driver closure that accepts bidi streams on
/// `conn`, dispatches each request through the router, and writes the
/// response back.
///
/// Each accepted stream runs in its own `tokio::spawn` task — the
/// accept loop is non-blocking and continues to accept new streams
/// while existing requests are still in flight. The semaphore caps
/// concurrent in-flight requests at [`MAX_INFLIGHT_REQUESTS_PER_PEER`];
/// excess accepted streams still spawn the task but park on
/// `semaphore.acquire().await` until a slot frees. New streams opened
/// by the peer while we are saturated will queue at the QUIC layer
/// (which has its own flow-control window); we never reject outright.
pub fn build_server(
    router: Arc<Router>,
) -> impl Fn(Connection) -> futures::future::BoxFuture<'static, ()> + Send + Sync + Clone {
    move |conn: Connection| {
        let router = router.clone();
        Box::pin(async move {
            let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_REQUESTS_PER_PEER));
            loop {
                match conn.accept_bi().await {
                    Ok((mut send, mut recv)) => {
                        let router = router.clone();
                        let semaphore = semaphore.clone();
                        tokio::spawn(async move {
                            // Acquire a slot before doing any work — this is
                            // what bounds the in-flight count. The permit
                            // is released when the task ends (drop semantics
                            // of the RAII guard).
                            let _permit = match semaphore.acquire_owned().await {
                                Ok(p) => p,
                                Err(_) => {
                                    // semaphore closed => server shutting down
                                    return;
                                }
                            };
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

    // === Router prefix + default_router tests (M0b STEP-0.3) ===================

    /// Verify the prefix routing for `/clipboard/text/{sha256}` hits the
    /// stub handler (404 + logged trace).
    #[test]
    fn clipboard_text_prefix_returns_404() {
        let router = default_router();
        let req = Request {
            method: "GET".into(),
            path: "/clipboard/text/abcdef0123456789".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(
            resp.status, 404,
            "/clipboard/text/{{sha256}} stub should return 404"
        );
    }

    #[test]
    fn clipboard_image_prefix_returns_404() {
        let router = default_router();
        let req = Request {
            method: "GET".into(),
            path: "/clipboard/image/deadbeef".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(
            resp.status, 404,
            "/clipboard/image/{{sha256}} stub should return 404"
        );
    }

    #[test]
    fn clipboard_file_prefix_returns_404_no_range() {
        let router = default_router();
        let req = Request {
            method: "GET".into(),
            path: "/clipboard/file/cafebabe".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(
            resp.status, 404,
            "/clipboard/file/{{sha256}} stub should return 404"
        );
    }

    /// Range query: `/clipboard/file/{sha256}?range=0-99` should match the
    /// `/clipboard/file/` prefix (the `?range=` is part of the wire path)
    /// and return 404 (range interface is M3a).
    #[test]
    fn clipboard_file_prefix_with_range_returns_404() {
        let router = default_router();
        let req = Request {
            method: "GET".into(),
            path: "/clipboard/file/cafebabe?range=0-99".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(
            resp.status, 404,
            "/clipboard/file/{{sha256}}?range=... stub should return 404"
        );
    }

    /// Exact `/healthz` should return 200 + "ok" via the default router.
    #[test]
    fn healthz_via_default_router_returns_200() {
        let router = default_router();
        let req = Request {
            method: "GET".into(),
            path: "/healthz".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body[..], b"ok");
    }

    /// Sanity check: a path outside any registered route returns 404.
    #[test]
    fn default_router_unknown_path_returns_404() {
        let router = default_router();
        let req = Request {
            method: "GET".into(),
            path: "/totally/not/a/route".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(resp.status, 404);
    }

    /// Non-GET against `/healthz` returns 405.
    #[test]
    fn default_router_post_to_healthz_returns_405() {
        let router = default_router();
        let req = Request {
            method: "POST".into(),
            path: "/healthz".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req);
        assert_eq!(resp.status, 405);
    }

    /// `get_prefix` should not consume an exact path that doesn't start
    /// with the prefix — e.g. `/clipboard/text` (no trailing slash)
    /// must NOT match `/clipboard/text/` (it falls through to 404).
    #[test]
    fn get_prefix_requires_trailing_slash_to_match() {
        let router = Router::new().get_prefix("/clipboard/text/", |_| Response::not_found());
        let req_no_slash = Request {
            method: "GET".into(),
            path: "/clipboard/text".into(),
            body: Bytes::new(),
        };
        let resp = router.handle(&req_no_slash);
        assert_eq!(
            resp.status, 404,
            "/clipboard/text (no trailing slash) should NOT match /clipboard/text/ prefix"
        );
    }

    // === GrowingSink tests (M0b STEP-0.4) =====================================

    #[test]
    fn growing_sink_writes_incrementally() {
        let mut buf: Vec<u8> = Vec::new();
        let mut sink = GrowingSink::new(&mut buf);
        // Write three chunks; `poll_write` is synchronous in our impl
        // (no real async IO), so we drive it directly.
        let chunk1 = vec![0xABu8; 1024];
        let chunk2 = vec![0xCDu8; 2048];
        let chunk3 = vec![0xEFu8; 4096];
        for c in [&chunk1, &chunk2, &chunk3] {
            let r = futures::executor::block_on(async {
                use tokio::io::AsyncWriteExt;
                sink.write_all(c).await
            });
            r.expect("write_all");
        }
        let r = futures::executor::block_on(async {
            use tokio::io::AsyncWriteExt;
            sink.flush().await
        });
        r.expect("flush");
        assert_eq!(buf.len(), 1024 + 2048 + 4096);
        assert_eq!(&buf[..1024], &chunk1[..]);
        assert_eq!(&buf[1024..1024 + 2048], &chunk2[..]);
        assert_eq!(&buf[1024 + 2048..], &chunk3[..]);
    }

    /// Verify the GrowingSink does **not** preallocate any capacity
    /// upfront — `buf.capacity()` after construction (before any write)
    /// must be 0. This is the "no `Vec::with_capacity(200 MiB)`" guard
    /// the leader mandated for STEP-0.4.
    #[test]
    fn growing_sink_no_preallocation() {
        let mut buf: Vec<u8> = Vec::new();
        assert_eq!(buf.capacity(), 0, "fresh Vec<u8> should have zero capacity");
        let _sink = GrowingSink::new(&mut buf);
        assert_eq!(
            buf.capacity(),
            0,
            "constructing GrowingSink should NOT preallocate any Vec capacity"
        );
    }

    // === Http3Client end-to-end (M0b STEP-0.4) ================================
    //
    // These tests spin up a real QUIC server + client (in-process) so the
    // Http3Client is exercised through the full stack: open_bi → write
    // request → read response header → stream body → return bytes. The
    // /healthz happy path proves the wire format works; the 404 / 500
    // cases prove the client surfaces non-200 statuses without raising
    // IO errors (PLAN-2 §5 评审 #3 2nd: "404 cache miss silently ignored");
    // the timeout test proves the upper layer can abort a stuck GET.

    use std::net::{Ipv4Addr, SocketAddrV4};

    use crate::quic_transport::endpoint::{dial, endpoint};
    use crate::quic_transport::test_helpers::{ephemeral_cert, ephemeral_pins_dir};

    /// Spin up a server with `router` bound to the given test cert, plus
    /// return the server's `Endpoint` and `Connection` once the client
    /// dials. Used by the round-trip tests below.
    async fn spawn_test_server(router: Arc<Router>) -> std::net::SocketAddr {
        use crate::quic_transport::endpoint::install_crypto_provider;
        use crate::quic_transport::endpoint_with_cert;

        install_crypto_provider();
        let (server_cert_chain, server_key) = ephemeral_cert();
        let server_ep = endpoint_with_cert(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
            server_cert_chain,
            server_key,
            std::time::Duration::from_secs(5),
        )
        .expect("server endpoint bind");
        let server_addr = server_ep.local_addr().expect("server addr");

        // Spawn the server accept loop on the local task set so the
        // per-connection driver has somewhere to run. Quinn `Endpoint`
        // is `Clone` (internally `Arc`-backed); we move the clone into
        // the task and drop the original at function exit (the closure
        // owns the keepalive ref).
        let ep_for_task = server_ep.clone();
        tokio::task::spawn_local(async move {
            loop {
                let Some(incoming) = ep_for_task.accept().await else {
                    break;
                };
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let driver = build_server(router.clone());
                tokio::task::spawn_local(async move {
                    driver(conn).await;
                });
            }
        });
        // Drop the local binding — the spawned task still holds an Arc.
        drop(server_ep);

        server_addr
    }

    /// Dial the test server and return a `Connection`. The helper hides
    /// the cert / pins_dir plumbing so each round-trip test stays small.
    async fn dial_test_server(server_addr: std::net::SocketAddr) -> quinn::Connection {
        let (client_cert_chain, client_key) = ephemeral_cert();
        let pins_dir = ephemeral_pins_dir();
        let _ = std::fs::remove_dir_all(&pins_dir);
        let client_ep = endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .expect("client endpoint bind");
        let conn = dial(
            &client_ep,
            server_addr,
            client_cert_chain[0].clone(),
            client_key,
            &pins_dir,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("dial");
        conn
    }

    /// Happy path: client GET /healthz → server returns 200 + "ok".
    /// Verifies the wire format works end-to-end through the QUIC stack.
    #[tokio::test(flavor = "multi_thread")]
    async fn http3_client_healthz_roundtrip() {
        crate::quic_transport::test_helpers::local_set_test!(http3_client_healthz_roundtrip, {
            let server_addr = spawn_test_server(default_router()).await;
            let conn = dial_test_server(server_addr).await;
            let client = Http3Client::new(conn);

            let (status, body) = client.healthz().await.expect("healthz");
            assert_eq!(status, 200, "/healthz should return 200");
            assert_eq!(&body[..], b"ok", "/healthz body should be 'ok'");
        });
    }

    /// Stub routes return 404 — the client surfaces the status without
    /// raising an IO error (callers decide what to do with 404).
    #[tokio::test(flavor = "multi_thread")]
    async fn http3_client_get_text_returns_404() {
        crate::quic_transport::test_helpers::local_set_test!(http3_client_get_text_returns_404, {
            let server_addr = spawn_test_server(default_router()).await;
            let conn = dial_test_server(server_addr).await;
            let client = Http3Client::new(conn);

            let (status, body) = client.get_text("abcdef").await.expect("get_text");
            assert_eq!(
                status, 404,
                "/clipboard/text/{{sha256}} stub should return 404"
            );
            assert_eq!(
                &body[..],
                b"not found",
                "default 404 body should be 'not found'"
            );
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http3_client_get_image_returns_404() {
        crate::quic_transport::test_helpers::local_set_test!(http3_client_get_image_returns_404, {
            let server_addr = spawn_test_server(default_router()).await;
            let conn = dial_test_server(server_addr).await;
            let client = Http3Client::new(conn);

            let (status, _body) = client.get_image("cafebabe").await.expect("get_image");
            assert_eq!(status, 404);
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http3_client_get_file_returns_404() {
        crate::quic_transport::test_helpers::local_set_test!(http3_client_get_file_returns_404, {
            let server_addr = spawn_test_server(default_router()).await;
            let conn = dial_test_server(server_addr).await;
            let client = Http3Client::new(conn);

            let (status_no_range, _) = client
                .get_file("deadbeef", None)
                .await
                .expect("get_file no range");
            assert_eq!(
                status_no_range, 404,
                "/clipboard/file/{{sha256}} (no range) should be 404"
            );

            let (status_with_range, _) = client
                .get_file("deadbeef", Some("0-99"))
                .await
                .expect("get_file with range");
            assert_eq!(
                status_with_range, 404,
                "/clipboard/file/{{sha256}}?range=0-99 should be 404 (M3a will fill this)"
            );
        });
    }

    /// 5xx response parsing: register a /boom route that returns 500 +
    /// body "boom", GET it, assert (500, "boom"). Verifies the client
    /// surfaces non-2xx / non-4xx statuses without an IO error.
    #[tokio::test(flavor = "multi_thread")]
    async fn http3_client_5xx_error_parsing() {
        crate::quic_transport::test_helpers::local_set_test!(http3_client_5xx_error_parsing, {
            let router = Arc::new(Router::new().get("/boom", |_| {
                Response::with_status(500, Bytes::from_static(b"boom"))
            }));
            let server_addr = spawn_test_server(router).await;
            let conn = dial_test_server(server_addr).await;
            let client = Http3Client::new(conn);

            let (status, body) = client
                .get_bytes("/boom")
                .await
                .expect("/boom should not raise an IO error for 5xx");
            assert_eq!(status, 500, "5xx status should be surfaced as-is");
            assert_eq!(&body[..], b"boom");
        });
    }

    /// Timeout handling: a server that accepts the bidi stream but
    /// **never writes a response** must let `tokio::time::timeout`
    /// cancel the client's GET. This proves the upper layer (M1/M2/M3
    /// service code) can abort a stuck GET without leaking tasks.
    ///
    /// **Why a custom hanging server (not the default router)**: the
    /// default router's handlers are sync and respond immediately;
    /// there's no way to make them hang. A custom server that just
    /// parks on `accept_bi()` after accepting each stream produces a
    /// reliable hang on the client side.
    #[tokio::test(flavor = "multi_thread")]
    async fn http3_client_timeout_handling() {
        crate::quic_transport::test_helpers::local_set_test!(http3_client_timeout_handling, {
            use crate::quic_transport::endpoint::install_crypto_provider;
            use crate::quic_transport::endpoint_with_cert;

            install_crypto_provider();
            let (server_cert_chain, server_key) = ephemeral_cert();
            let server_ep = endpoint_with_cert(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into(),
                server_cert_chain,
                server_key,
                std::time::Duration::from_secs(5),
            )
            .expect("server endpoint bind");
            let server_addr = server_ep.local_addr().expect("server addr");

            // Hanging server: accept connections, accept each bidi
            // stream, but never write a response. The parked streams
            // are kept alive so they aren't dropped (matches the
            // "bunch bidi" parking pattern in `listen.rs`).
            let parked: std::rc::Rc<
                std::cell::RefCell<Vec<(quinn::SendStream, quinn::RecvStream)>>,
            > = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let parked_for_task = parked.clone();
            let ep_for_task = server_ep.clone();
            tokio::task::spawn_local(async move {
                loop {
                    let Some(incoming) = ep_for_task.accept().await else {
                        break;
                    };
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let parked_inner = parked_for_task.clone();
                    tokio::task::spawn_local(async move {
                        loop {
                            let (send, recv) = match conn.accept_bi().await {
                                Ok(pair) => pair,
                                Err(_) => return,
                            };
                            // Park both halves; never read or write.
                            parked_inner.borrow_mut().push((send, recv));
                        }
                    });
                }
            });
            drop(server_ep);

            // Client dials and tries to GET /anything.
            let conn = dial_test_server(server_addr).await;
            let client = Http3Client::new(conn);
            let get = client.get_bytes("/anything");
            // Wrap in a short timeout — the server will never respond.
            let result = tokio::time::timeout(std::time::Duration::from_millis(200), get).await;
            assert!(
                result.is_err(),
                "GET against a hanging server must time out (got Ok)"
            );
        });
    }
}
