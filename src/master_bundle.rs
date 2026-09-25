//! Build a verified pinned archive before sending any successful response bytes.
use crate::{error::AppError, master_registry as registry};
use axum::{body::Body, http::HeaderMap, response::Response};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    io::{self, Seek, Write},
    pin::Pin,
    sync::{Arc, LazyLock},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{OwnedSemaphorePermit, Semaphore},
};
const MAX_ARCHIVE: u64 = 528 * 1024 * 1024;
static GATE: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));
pub(crate) fn permit() -> Result<OwnedSemaphorePermit, AppError> {
    GATE.clone()
        .try_acquire_owned()
        .map_err(|_| AppError::MasterUnavailable)
}
struct Writer {
    file: std::fs::File,
    hash: Sha256,
    len: u64,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > MAX_ARCHIVE.saturating_sub(self.len) {
            return Err(io::Error::other("archive limit"));
        }
        let n = self.file.write(bytes)?;
        self.hash.update(&bytes[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
struct Archive {
    tar: tar::Builder<Writer>,
    permit: Arc<OwnedSemaphorePermit>,
}
impl Archive {
    fn new(permit: OwnedSemaphorePermit) -> io::Result<Self> {
        Ok(Self {
            tar: tar::Builder::new(Writer {
                file: tempfile::tempfile()?,
                hash: Sha256::new(),
                len: 0,
            }),
            permit: Arc::new(permit),
        })
    }
    fn append(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        self.tar.append_data(&mut header, name, bytes)
    }
    fn finish(self) -> io::Result<Bundle> {
        let mut writer = self.tar.into_inner()?;
        writer.flush()?;
        writer.file.rewind()?;
        Ok(Bundle {
            file: writer.file,
            etag: format!("\"{:x}\"", writer.hash.finalize()),
            len: writer.len,
            permit: self.permit,
        })
    }
}
pub(crate) struct Bundle {
    file: std::fs::File,
    etag: String,
    len: u64,
    permit: Arc<OwnedSemaphorePermit>,
}
struct Reader {
    file: tokio::fs::File,
    _permit: Arc<OwnedSemaphorePermit>,
}
impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_read(cx, buf)
    }
}
/// The callback must read tables from the selected snapshot, never from CURRENT.
pub(crate) async fn build<F, Fut>(
    manifest: registry::PublishedManifest,
    mut load: F,
    permit: OwnedSemaphorePermit,
) -> Result<Bundle, AppError>
where
    F: FnMut(registry::File) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, AppError>>,
{
    tokio::time::timeout(Duration::from_secs(120), async move {
        manifest
            .validate(&manifest.scope)
            .map_err(|_| AppError::MasterUnavailable)?;
        let bytes = serde_json::to_vec(&manifest).map_err(|_| AppError::MasterUnavailable)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(AppError::MasterUnavailable);
        }
        let mut archive = tokio::task::spawn_blocking(move || {
            let mut archive = Archive::new(permit)?;
            archive.append("metadata/manifest.json", &bytes)?;
            Ok::<_, io::Error>(archive)
        })
        .await
        .map_err(|_| AppError::MasterUnavailable)?
        .map_err(|_| AppError::MasterUnavailable)?;
        for file in manifest.files {
            let bytes = load(file.clone()).await?;
            archive = tokio::task::spawn_blocking(move || {
                if bytes.len() as u64 != file.size || registry::digest(&bytes) != file.sha256 {
                    return Err(io::Error::other("invalid table"));
                }
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map_err(|_| io::Error::other("invalid table"))?;
                archive.append(&format!("tables/{}", file.name), &bytes)?;
                Ok::<_, io::Error>(archive)
            })
            .await
            .map_err(|_| AppError::MasterUnavailable)?
            .map_err(|_| AppError::MasterUnavailable)?;
        }
        tokio::task::spawn_blocking(move || archive.finish())
            .await
            .map_err(|_| AppError::MasterUnavailable)?
            .map_err(|_| AppError::MasterUnavailable)
    })
    .await
    .map_err(|_| AppError::MasterUnavailable)?
}
impl Bundle {
    pub(crate) fn response(
        self,
        headers: HeaderMap,
        version: &str,
        content_hash: &str,
    ) -> Result<Response, AppError> {
        let unchanged = headers
            .get("if-none-match")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|v| {
                v.split(',').any(|s| {
                    let s = s.trim();
                    s == "*" || s.strip_prefix("W/").unwrap_or(s) == self.etag
                })
            });
        let mut response = Response::builder()
            .status(if unchanged { 304 } else { 200 })
            .header("content-type", "application/x-tar")
            .header("etag", &self.etag)
            .header("cache-control", "private, no-cache")
            .header("x-master-version", version)
            .header("x-master-content-sha256", content_hash)
            .header(
                "content-disposition",
                format!("attachment; filename=\"master-{content_hash}.tar\""),
            );
        if !unchanged {
            response = response.header("content-length", self.len);
        }
        let body = if unchanged {
            Body::empty()
        } else {
            let reader = Reader {
                file: tokio::fs::File::from_std(self.file),
                _permit: self.permit,
            };
            Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
                reader,
                64 * 1024,
            ))
        };
        response.body(body).map_err(|_| AppError::MasterUnavailable)
    }
}
