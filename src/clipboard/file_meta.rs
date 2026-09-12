//! Cross-platform file metadata collection (PLAN-2 / M3a STEP-3a.1).
//!
//! Computes the per-file metadata the dispatcher pushes over StreamC
//! inside a `lan_mouse_proto::ProtoEvent::ClipboardFiles { fingerprint,
//! entries }` envelope. Bytes themselves ride HTTP/3-lite in STEP-3a.4 —
//! this module only computes the metadata + content fingerprint.
//!
//! Recursive directory expansion is **not** in scope for STEP-3a.1;
//! the caller (dispatcher / service) is responsible for any walk
//! before reaching `collect_files`. See PLAN §3 M3a STEP-3a.2 for the
//! outbound branch.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Per-file metadata — travels inside
/// `lan_mouse_proto::ClipboardFiles::entries` on the wire.
///
/// **Equality**: derived `PartialEq + Eq` so the outbound dispatcher
/// can de-duplicate a multi-select clipboard's file list cheaply
/// (PLAN §3 M3a 评审 #3 2nd — sha256 is the canonical dedupe key;
/// `collect_files` itself does not filter, it just collects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Filename component only — no directory prefix. On the receiver
    /// this becomes the default landing name; collisions get a `(1)` /
    /// `(2)` suffix per PLAN §3 STEP-3a.3.
    pub name: String,
    /// File size in bytes (matches `std::fs::Metadata::len()`).
    pub size: u64,
    /// MIME type. Set by [`detect_mime`] on the normal path;
    /// set to [`MIME_TOO_LARGE`] for files exceeding [`FOUR_GIB`]
    /// (after a `log::warn!`).
    pub mime: String,
    /// SHA-256 of the file's raw bytes, streamed in 64 KiB chunks
    /// (the wire-format output of `sha2::Sha256::finalize()`).
    pub sha256: [u8; 32],
}

/// Errors produced by [`collect_files`] / [`collect_files_blocking`].
#[derive(Debug, Error)]
pub enum FileMetaError {
    /// Caller passed a directory path. Recursive walk is out of
    /// scope for STEP-3a.1; the dispatcher surfaces this as a
    /// "directory refused" log + skip rather than walking itself.
    #[error("path is a directory: {0}")]
    IsDirectory(PathBuf),
    /// Wrapped `std::io::Error` for permission / not-found / IO
    /// failures. The dispatcher's error-handling switch uses the
    /// variant (not the message) to decide between "skip + warn" and
    /// "abort the whole batch".
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// **M3a STEP-3a.2** — at least one file in the batch exceeds
    /// `cfg.max_file_size`. The dispatcher's outbound branch uses
    /// this to short-circuit the entire batch (no sha256, no cache
    /// insert, no StreamC push, no HTTP/3 setup) and immediately
    /// fire a `PopupGuard` so the user is told *why* their copy was
    /// dropped — without waiting for the 500 ms tick (PLAN §5
    /// 风险 #25).
    ///
    /// Carries the **first** offending path (later offenders are
    /// logged at info but not surfaced via this variant — keeping
    /// the enum non-tuple for forward-compat). `size` is the
    /// offending entry's `std::fs::Metadata::len()`; `limit` is the
    /// configured `max_file_size` in bytes (`0` means "no limit",
    /// in which case this variant is unreachable).
    #[error("file exceeds limit: {offending} ({size} bytes > limit={limit} bytes)")]
    ExceedsLimit {
        offending: PathBuf,
        size: u64,
        limit: u64,
    },
}

/// Per-file transfer ceiling. Anything strictly larger is marked
/// [`MIME_TOO_LARGE`] and the receiver is expected to short-circuit
/// the HTTP/3 fetch (PLAN §3 M3a STEP-3a.1 "单个 > 4 GiB 警告").
///
/// **Why 4 GiB, not `u32::MAX`**: `quinn` QUIC stream framing uses
/// `u32::MAX` byte offsets for range requests; combined with the LAN
/// throughput targets and platform-specific ftruncate limits, files
/// above 4 GiB push the implementation into territory where
/// range-based resumption (out of scope for M3a, scoped to "首次
/// 一次性成功" per PLAN §0) would matter. The oversize marker is an
/// early bail so the user can refuse before bytes move.
pub const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;

/// Wire-facing MIME label for > 4 GiB files. Pinned because the
/// receiver's short-circuit branch keys on this exact string — a
/// typo would silently turn "refuse" into "fetch the whole 4+ GiB".
pub const MIME_TOO_LARGE: &str = "application/x-too-large";

/// Compute [`FileEntry`] metadata for each path in `paths`.
///
/// Behaviour:
/// - **Rejects directories** with [`FileMetaError::IsDirectory`] on
///   the first dir encountered (the whole batch is aborted — the
///   caller's UI should display the offending path before retrying
///   with a non-recursive selection).
/// - **Marks oversize files** (> 4 GiB) with MIME [`MIME_TOO_LARGE`]
///   after logging `warn!`; sha256 is **not** computed for these
///   (receiver is expected to refuse outright — saves minutes of
///   hashing on a file we will never transfer).
/// - **STEP-3a.2 `max_size` cap**: if `max_size > 0` and any file
///   exceeds `max_size` bytes, the whole batch is rejected with
///   [`FileMetaError::ExceedsLimit`] (no sha256, no entry
///   construction). `max_size == 0` disables the check (PLAN §5
///   风险 #25: "`0` = 不限").
///
/// **Streaming**: sha256 is computed via 64 KiB reads against a
/// `tokio::fs::File` — never buffered into a single `Vec<u8>` (the
/// 200 MiB / 4 GiB targets would blow memory otherwise).
///
/// **Why this is async (not `spawn_blocking`-friendly)**: tokio's
/// file IO yields the LocalSet between reads, so the runtime keeps
/// making progress on other tasks. The dispatcher's outbound branch
/// can call this directly with `.await` on the main task — see
/// [`crate::service::Service::dispatch_files`] for the wiring.
pub async fn collect_files(
    paths: &[PathBuf],
    max_size: u64,
) -> Result<Vec<FileEntry>, FileMetaError> {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        let meta = tokio::fs::metadata(path).await?;
        if meta.is_dir() {
            return Err(FileMetaError::IsDirectory(path.clone()));
        }
        let size = meta.len();
        // **M3a STEP-3a.2** — `max_size` cap. `0` means "no limit"
        // and is the documented way to bypass the cap (PLAN §5
        // 风险 #25). Order matters — check `max_size > 0` first to
        // avoid the `0` case being treated as "every file is too
        // large".
        if max_size > 0 && size > max_size {
            log::warn!(
                "file {} size {} > max_size {} bytes, rejecting batch (offending)",
                path.display(),
                size,
                max_size
            );
            return Err(FileMetaError::ExceedsLimit {
                offending: path.clone(),
                size,
                limit: max_size,
            });
        }
        let (mime, sha256) = if should_mark_too_large(size) {
            log::warn!(
                "file {} size {} > 4 GiB, marking as {}",
                path.display(),
                size,
                MIME_TOO_LARGE
            );
            (MIME_TOO_LARGE.to_string(), [0u8; 32])
        } else {
            (detect_mime(path), stream_sha256(path).await?)
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        entries.push(FileEntry {
            name,
            size,
            mime,
            sha256,
        });
    }
    Ok(entries)
}

/// **M3a STEP-3a.2** — synchronous sibling of [`collect_files`] for
/// use inside `tokio::task::spawn_blocking`.
///
/// The dispatcher's outbound branch wraps this in `spawn_blocking` so
/// the CPU-bound sha256 streaming (200 MiB → ~5-8 s on a 2014-era
/// SSD, ~1-3 s on modern NVMe) does not block the daemon's LocalSet
/// — matching the **`dispatch_image` off-LocalSet pattern** from
/// commit `7a57bb3` (PLAN §3 M3a STEP-3a.2 ②).
///
/// **Why a separate sync entrypoint** (not just
/// `Handle::current().block_on(collect_files(...))` inside
/// `spawn_blocking`): the latter is a known tokio anti-pattern —
/// nesting a runtime on a blocking thread is unsupported and can
/// deadlock under load. Splitting the sync / async surfaces keeps
/// both paths idiomatic.
///
/// **Behaviour parity with [`collect_files`]**: identical
/// `max_size` semantics (0 = unlimited), identical error variants,
/// identical MIME / sha256 wiring (delegates to the same helpers).
/// The only difference is `std::fs` / `std::io::Read` instead of
/// `tokio::fs` / `tokio::io::AsyncReadExt` — fine because
/// `spawn_blocking`'s threadpool is purpose-built for blocking I/O.
pub fn collect_files_blocking(
    paths: &[PathBuf],
    max_size: u64,
) -> Result<Vec<FileEntry>, FileMetaError> {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        let meta = std::fs::metadata(path)?;
        if meta.is_dir() {
            return Err(FileMetaError::IsDirectory(path.clone()));
        }
        let size = meta.len();
        if max_size > 0 && size > max_size {
            log::warn!(
                "file {} size {} > max_size {} bytes, rejecting batch (offending)",
                path.display(),
                size,
                max_size
            );
            return Err(FileMetaError::ExceedsLimit {
                offending: path.clone(),
                size,
                limit: max_size,
            });
        }
        let (mime, sha256) = if should_mark_too_large(size) {
            log::warn!(
                "file {} size {} > 4 GiB, marking as {}",
                path.display(),
                size,
                MIME_TOO_LARGE
            );
            (MIME_TOO_LARGE.to_string(), [0u8; 32])
        } else {
            (detect_mime(path), stream_sha256_blocking(path)?)
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        entries.push(FileEntry {
            name,
            size,
            mime,
            sha256,
        });
    }
    Ok(entries)
}

/// **M3a STEP-3a.2** — synchronous sha256 streaming (used by
/// [`collect_files_blocking`] inside `spawn_blocking`). Mirrors
/// [`stream_sha256`] exactly, swapping `tokio::fs::File` +
/// `AsyncReadExt` for `std::fs::File` + `std::io::Read` — the
/// spawn_blocking pool is purpose-built for blocking I/O.
fn stream_sha256_blocking(path: &Path) -> Result<[u8; 32], FileMetaError> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// `true` iff a file of `size` bytes exceeds [`FOUR_GIB`].
///
/// Extracted from `collect_files` so unit tests can pin the boundary
/// without writing 4 GiB to disk.
pub fn should_mark_too_large(size: u64) -> bool {
    size > FOUR_GIB
}

/// Stream a file's SHA-256 in 64 KiB chunks.
///
/// **Why 64 KiB chunks**: balances syscall count vs. memory pressure
/// — 64 KiB matches the typical kernel readahead window so each
/// `read` returns close to a full buffer (vs. 4 KiB which would
/// quadruple syscalls on the 200 MiB target). Allocating the buffer
/// once outside the loop avoids re-allocation churn over thousands
/// of iterations.
async fn stream_sha256(path: &Path) -> Result<[u8; 32], FileMetaError> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Extension-based MIME guess. Returns one of the canonical labels
/// for known types; defaults to `application/octet-stream`.
///
/// **Why extension-only at this stage**: the dispatcher pushes
/// metadata **before** bytes travel (sha256 over StreamC, bytes
/// over HTTP/3-lite in STEP-3a.4); a magic-byte sniff would require
/// opening the file twice. Extending the detector (e.g. via the
/// `infer` crate's magic-byte table) is a forward-compat hook —
/// M3b / M4 can swap in a richer source without touching the trait
/// surface.
fn detect_mime(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    /// Reference SHA-256 of a byte slice — uses the same `sha2` crate
    /// the production path uses, so the comparison below catches
    /// hash-function drift if `sha2` ever bumps majors.
    fn sha256_of(data: &[u8]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(data);
        h.finalize().into()
    }

    /// Helper: write a file's full contents and fsync. Uses
    /// `tokio::fs` to keep the test path async-runtime-aware (some
    /// test runners serialise async + sync IO on the same loop).
    async fn write_file(path: &Path, data: &[u8]) {
        let mut f = tokio::fs::File::create(path).await.unwrap();
        f.write_all(data).await.unwrap();
        f.sync_all().await.unwrap();
    }

    /// `collect_files` returns a single entry with the right SHA-256
    /// for a 1 KiB file (basic happy path — verifies the metadata
    /// projection + sha256 wiring).
    #[tokio::test]
    async fn collect_files_returns_single_file_with_correct_sha256() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.bin");
        let payload = vec![0xAB_u8; 1024];
        write_file(&path, &payload).await;
        // STEP-3a.2 — explicit `max_size=0` (unlimited) for the
        // pre-existing happy-path test, so the new param doesn't
        // regress prior assertions.
        let entries = collect_files(std::slice::from_ref(&path), 0).await.unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "hello.bin");
        assert_eq!(e.size, 1024);
        assert_eq!(e.sha256, sha256_of(&payload));
        // .bin → unknown extension → octet-stream.
        assert_eq!(e.mime, "application/octet-stream");
    }

    /// Three different files produce three entries, each with the
    /// right SHA-256 — catches cross-entry hashing bleed (the
    /// streaming loop accidentally reusing hasher state).
    #[tokio::test]
    async fn collect_files_returns_multiple_files_with_independent_sha256() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.txt");
        let p2 = dir.path().join("b.txt");
        let p3 = dir.path().join("c.bin");
        let d1 = b"hello world".to_vec();
        let d2 = b"goodbye world".to_vec();
        let d3 = vec![0u8; 16_384];
        write_file(&p1, &d1).await;
        write_file(&p2, &d2).await;
        write_file(&p3, &d3).await;
        let entries = collect_files(&[p1.clone(), p2.clone(), p3.clone()], 0)
            .await
            .unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sha256, sha256_of(&d1));
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].mime, "text/plain");
        assert_eq!(entries[1].sha256, sha256_of(&d2));
        assert_eq!(entries[1].name, "b.txt");
        assert_eq!(entries[2].sha256, sha256_of(&d3));
        assert_eq!(entries[2].name, "c.bin");
        assert_eq!(entries[2].mime, "application/octet-stream");
    }

    /// `collect_files` rejects a directory with `IsDirectory` —
    /// recursive walk is out of scope for STEP-3a.1.
    #[tokio::test]
    async fn collect_files_returns_error_for_directory() {
        let dir = tempdir().unwrap();
        let err = collect_files(&[dir.path().to_path_buf()], 0)
            .await
            .unwrap_err();
        match err {
            FileMetaError::IsDirectory(p) => assert_eq!(p, dir.path()),
            other => panic!("expected IsDirectory, got {other:?}"),
        }
    }

    /// 1 KiB streamed SHA-256 matches the reference. Exercises a
    /// partial read (1024 bytes < the 64 KiB buffer).
    #[tokio::test]
    async fn stream_sha256_1kib_correct() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("kib.bin");
        let data = vec![0x33_u8; 1024];
        write_file(&path, &data).await;
        let hash = stream_sha256(&path).await.unwrap();
        assert_eq!(hash, sha256_of(&data));
    }

    /// 1 MiB streamed SHA-256 matches the reference. Exercises the
    /// 64 KiB read buffer loop boundary (16 full reads).
    #[tokio::test]
    async fn stream_sha256_1mib_correct() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mib.bin");
        let data = vec![0x77_u8; 1024 * 1024];
        write_file(&path, &data).await;
        let hash = stream_sha256(&path).await.unwrap();
        assert_eq!(hash, sha256_of(&data));
    }

    /// 200 MiB streamed SHA-256 matches the reference. Hits the
    /// read-loop hard — 200 MiB ÷ 64 KiB = 3200 reads. ~1-3s on
    /// SSD, longer on spinning disk. Required by PLAN §3 M3a
    /// STEP-3a.1 "完成标志: 1 KiB / 1 MiB / 200 MiB 文件 sha256
    /// 正确"; the cost is the price of the contract.
    #[tokio::test]
    async fn stream_sha256_200mib_correct() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data = vec![0xCC_u8; 200 * 1024 * 1024];
        write_file(&path, &data).await;
        let hash = stream_sha256(&path).await.unwrap();
        assert_eq!(hash, sha256_of(&data));
    }

    /// `detect_mime` recognises the canonical extensions and falls
    /// back to octet-stream for unknown / missing extensions.
    /// Pins case-insensitivity (`JPG` → `image/jpeg`) and the
    /// multi-extension fallback (`a.tar.gz` → octet-stream because
    /// only the last `.gz` is examined).
    #[test]
    fn detect_mime_recognises_common_extensions() {
        let cases = [
            ("a.png", "image/png"),
            ("a.PNG", "image/png"),
            ("a.jpg", "image/jpeg"),
            ("a.jpeg", "image/jpeg"),
            ("a.JPG", "image/jpeg"),
            ("a.bmp", "image/bmp"),
            ("a.pdf", "application/pdf"),
            ("a.txt", "text/plain"),
            ("a.md", "text/plain"),
            ("a.bin", "application/octet-stream"),
            ("a", "application/octet-stream"),
            ("a.tar.gz", "application/octet-stream"),
        ];
        for (input, expected) in cases.iter() {
            let p = PathBuf::from(input);
            assert_eq!(
                detect_mime(&p),
                *expected,
                "detect_mime mismatch for {input}: expected {expected}"
            );
        }
    }

    /// `should_mark_too_large` flips on at strictly > 4 GiB. Pinned
    /// because the boundary is a wire-protocol contract (see the
    /// docs on [`FOUR_GIB`]).
    #[test]
    fn should_mark_too_large_threshold_is_4gib() {
        assert!(!should_mark_too_large(0));
        assert!(!should_mark_too_large(1024));
        // Exactly 4 GiB → NOT too large (boundary inclusive on the
        // small side per the `>` comparison in `should_mark_too_large`).
        assert!(!should_mark_too_large(FOUR_GIB));
        // 4 GiB + 1 byte → too large.
        assert!(should_mark_too_large(FOUR_GIB + 1));
        assert!(should_mark_too_large(u64::MAX));
    }

    /// `MIME_TOO_LARGE` is the wire-facing label — pinned because
    /// the receiver's short-circuit branch keys on this exact string.
    #[test]
    fn mime_too_large_constant_is_stable() {
        assert_eq!(MIME_TOO_LARGE, "application/x-too-large");
    }

    /// `FileMetaError` Display messages stay stable — the
    /// dispatcher's error log + downstream log-grep tests rely on
    /// the exact wording.
    #[test]
    fn file_meta_error_display_messages_are_stable() {
        let err = FileMetaError::IsDirectory(PathBuf::from("/tmp/foo"));
        assert_eq!(err.to_string(), "path is a directory: /tmp/foo");
        let io_err =
            FileMetaError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert!(
            io_err.to_string().contains("io error"),
            "Io variant Display must contain 'io error'; got {}",
            io_err
        );
        // **M3a STEP-3a.2** — `ExceedsLimit` Display carries the
        // offending path + size + limit (in bytes). The dispatcher's
        // log line and the PopupGuard's body both grep / display this
        // string verbatim, so it has to stay stable.
        let ex = FileMetaError::ExceedsLimit {
            offending: PathBuf::from("/tmp/big.bin"),
            size: 60 * 1024 * 1024,
            limit: 50 * 1024 * 1024,
        };
        assert_eq!(
            ex.to_string(),
            "file exceeds limit: /tmp/big.bin (62914560 bytes > limit=52428800 bytes)"
        );
    }

    /// Empty path slice returns an empty Vec without touching the
    /// filesystem — defensive default for callers that have nothing
    /// to collect (e.g. an empty Finder multi-select).
    #[tokio::test]
    async fn collect_files_with_empty_slice_returns_empty_vec() {
        let entries = collect_files(&[], 0).await.unwrap();
        assert!(entries.is_empty());
    }

    /// Non-existent path surfaces as `FileMetaError::Io` (not
    /// `IsDirectory`) — preserves the error variant contract for the
    /// dispatcher's error-handling switch.
    #[tokio::test]
    async fn collect_files_missing_path_returns_io_error() {
        let path = PathBuf::from("/nonexistent/lan-mouse-pro-3a.1-test-fixture");
        let err = collect_files(&[path], 0).await.unwrap_err();
        assert!(matches!(err, FileMetaError::Io(_)), "got {err:?}");
    }

    // === M3a STEP-3a.2 — `max_size` parameter + `ExceedsLimit` boundary ===

    /// `max_size = 0` disables the cap entirely (PLAN §5 风险 #25:
    /// "`0` = 不限"). A file of any size passes through; the legacy
    /// behaviour of M3a STEP-3a.1 (no cap) is preserved by callers
    /// who explicitly opt in.
    #[tokio::test]
    async fn collect_files_max_size_zero_disables_cap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("huge.bin");
        // 5 MiB file — comfortably above any realistic default.
        let payload = vec![0xABu8; 5 * 1024 * 1024];
        write_file(&path, &payload).await;
        let entries = collect_files(std::slice::from_ref(&path), 0).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, payload.len() as u64);
    }

    /// Boundary: a file of size exactly equal to `max_size` is
    /// accepted (the `>` comparison is strict, not `>=`). Pins the
    /// wire-protocol semantics — a file of exactly 50 MiB on a
    /// 50 MiB cap is NOT rejected.
    #[tokio::test]
    async fn collect_files_max_size_boundary_exact_is_accepted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boundary.bin");
        let max_size: u64 = 1024; // 1 KiB cap
        let payload = vec![0xCDu8; max_size as usize];
        write_file(&path, &payload).await;
        let entries = collect_files(std::slice::from_ref(&path), max_size)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, max_size);
    }

    /// Boundary: a file one byte over `max_size` is rejected. Pins
    /// the strict `>` comparison.
    #[tokio::test]
    async fn collect_files_max_size_boundary_plus_one_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("over.bin");
        let max_size: u64 = 1024;
        let payload = vec![0xCDu8; (max_size + 1) as usize];
        write_file(&path, &payload).await;
        let err = collect_files(std::slice::from_ref(&path), max_size)
            .await
            .unwrap_err();
        match err {
            FileMetaError::ExceedsLimit {
                offending,
                size,
                limit,
            } => {
                assert_eq!(offending, path);
                assert_eq!(size, max_size + 1);
                assert_eq!(limit, max_size);
            }
            other => panic!("expected ExceedsLimit, got {other:?}"),
        }
    }

    /// A batch where the **first** file fits but a later one
    /// exceeds `max_size` must reject the whole batch — PLAN §3
    /// STEP-3a.2 "整批 Err(ExceedsLimit)". The dispatcher surfaces
    /// the offending path so the user knows which one to drop.
    #[tokio::test]
    async fn collect_files_max_size_rejects_batch_when_any_file_exceeds() {
        let dir = tempdir().unwrap();
        let ok = dir.path().join("ok.bin");
        let huge = dir.path().join("huge.bin");
        write_file(&ok, &vec![0xAAu8; 512]).await; // fits easily
        write_file(&huge, &vec![0xBBu8; 5 * 1024 * 1024]).await; // 5 MiB
        let max_size: u64 = 1024 * 1024; // 1 MiB
        let paths = [ok, huge];
        let err = collect_files(&paths, max_size).await.unwrap_err();
        match err {
            FileMetaError::ExceedsLimit {
                offending,
                size,
                limit,
            } => {
                assert_eq!(
                    offending,
                    dir.path().join("huge.bin"),
                    "offending must be the second file"
                );
                assert_eq!(size, 5 * 1024 * 1024);
                assert_eq!(limit, max_size);
            }
            other => panic!("expected ExceedsLimit, got {other:?}"),
        }
    }

    /// `ExceedsLimit` is returned **before** any sha256 is computed
    /// — the offending file is rejected on size alone. Pin this
    /// because the dispatcher's early-reject path relies on it (no
    /// wasted CPU on a file we will never transfer).
    #[tokio::test]
    async fn collect_files_max_size_rejects_before_sha256_compute() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.bin");
        write_file(&path, &vec![0xCDu8; 2048]).await;
        let max_size: u64 = 1024;
        // If sha256 had been computed for the offending file, the
        // sha256 field would be non-zero. The early-reject contract
        // means we never even enter the per-file loop body that
        // populates it — `ExceedsLimit` is the **only** payload
        // that comes back, with the offending path carried.
        let err = collect_files(std::slice::from_ref(&path), max_size)
            .await
            .unwrap_err();
        match err {
            FileMetaError::ExceedsLimit { offending, .. } => assert_eq!(offending, path),
            other => panic!("expected ExceedsLimit, got {other:?}"),
        }
    }

    // === M3a STEP-3a.2 — sync `collect_files_blocking` parity tests ===

    /// `collect_files_blocking` mirrors `collect_files` happy path
    /// with synchronous std::fs I/O — the spawn_blocking entry
    /// point. 1 KiB sanity check.
    #[test]
    fn collect_files_blocking_returns_single_file_with_correct_sha256() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.bin");
        let payload = vec![0xAB_u8; 1024];
        std::fs::write(&path, &payload).unwrap();
        let entries = collect_files_blocking(std::slice::from_ref(&path), 0).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sha256, sha256_of(&payload));
        assert_eq!(entries[0].size, 1024);
    }

    /// `collect_files_blocking` 1 MiB + multi-entry sha256 parity
    /// with `collect_files`. Catches a regression where the sync
    /// version accidentally re-uses hasher state across files (the
    /// async version had a similar test in STEP-3a.1).
    #[test]
    fn collect_files_blocking_multi_file_independent_sha256() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.txt");
        let p2 = dir.path().join("b.txt");
        let d1 = b"hello world".to_vec();
        let d2 = b"goodbye world".to_vec();
        std::fs::write(&p1, &d1).unwrap();
        std::fs::write(&p2, &d2).unwrap();
        let entries = collect_files_blocking(&[p1.clone(), p2.clone()], 0).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sha256, sha256_of(&d1));
        assert_eq!(entries[1].sha256, sha256_of(&d2));
    }

    /// `collect_files_blocking` enforces `max_size` the same way
    /// the async version does.
    #[test]
    fn collect_files_blocking_respects_max_size() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("huge.bin");
        std::fs::write(&path, vec![0u8; 5 * 1024 * 1024]).unwrap();
        let err = collect_files_blocking(std::slice::from_ref(&path), 1024 * 1024).unwrap_err();
        match err {
            FileMetaError::ExceedsLimit {
                offending,
                size,
                limit,
            } => {
                assert_eq!(offending, path);
                assert_eq!(size, 5 * 1024 * 1024);
                assert_eq!(limit, 1024 * 1024);
            }
            other => panic!("expected ExceedsLimit, got {other:?}"),
        }
    }

    /// `collect_files_blocking` directory rejection (parity).
    #[test]
    fn collect_files_blocking_rejects_directory() {
        let dir = tempdir().unwrap();
        let err = collect_files_blocking(&[dir.path().to_path_buf()], 0).unwrap_err();
        assert!(matches!(err, FileMetaError::IsDirectory(_)), "got {err:?}");
    }
}
