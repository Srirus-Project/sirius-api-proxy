//! Bounds decoded Master table bytes held for reads and responses.
//!
//! Every table read loads, hashes and checks a whole file (up to `master_registry::MAX_JSON`)
//! and keeps the bytes until the connection has taken the last of them. A process-wide gate
//! admits at most `TABLE_READS` of them: about 24 MB with real tables of about 1.5 MB, 16 x 64
//! MiB in theory. Waiters queue in FIFO order for `TABLE_WAIT`, then answer 503
//! `master_unavailable`. A 200 hands its body over in `CHUNK` copies and keeps its permit until
//! the last one is taken or the body is dropped, so slow clients hold permits; any other
//! response releases it at once. Manifests, history and bundles (which have their own gate) are
//! not admitted here. Nothing is configurable.
use crate::error::AppError;
use axum::{
    body::{Body, Bytes, HttpBody},
    response::Response,
};
use bytes::Buf;
use hyper::body::{Frame, SizeHint};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock,
    },
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const TABLE_READS: usize = 16;
const TABLE_WAIT: Duration = Duration::from_secs(5);
const WARN_EVERY_SECS: u64 = 60;
/// Largest data frame handed to the connection. hyper queues a frame whole and drops the body
/// once it reports its end, so one frame holding the table would release the permit while the
/// table still waits on a client that does not read. Smaller frames are only polled as the
/// connection drains its write buffer, and copies keep a queued tail from pinning the whole
/// table after the permit is gone.
const CHUNK: usize = 64 * 1024;
static TABLES: LazyLock<Gate> = LazyLock::new(|| Gate {
    permits: Arc::new(Semaphore::new(TABLE_READS)),
    wait: TABLE_WAIT,
});

#[derive(Clone)]
pub(crate) struct Gate {
    permits: Arc<Semaphore>,
    wait: Duration,
}
impl Gate {
    /// The process-wide table gate; every region of one process shares its budget.
    pub(crate) fn tables() -> Gate {
        TABLES.clone()
    }
    #[cfg(test)]
    pub(crate) fn new(permits: usize, wait: Duration) -> Gate {
        Gate {
            permits: Arc::new(Semaphore::new(permits)),
            wait,
        }
    }
    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.permits.available_permits()
    }
    /// Waits up to the gate's budget; the database read deadline, if any, starts afterwards.
    pub(crate) async fn admit(&self) -> Result<Admission, AppError> {
        match tokio::time::timeout(self.wait, self.permits.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Admission {
                _permit: Arc::new(permit),
            }),
            _ => {
                warn_busy();
                Err(AppError::MasterUnavailable)
            }
        }
    }
}
/// A busy 503 shares its code with integrity failures; this line tells them apart.
fn warn_busy() {
    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    static LAST: AtomicU64 = AtomicU64::new(0);
    // Offset by the interval so the first rejection always logs.
    let now = ORIGIN.elapsed().as_secs() + WARN_EVERY_SECS;
    let last = LAST.load(Ordering::Relaxed);
    if now >= last + WARN_EVERY_SECS
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            error_code = "master_read_busy",
            "Master table read admission wait exceeded; answering 503"
        );
    }
}

/// One admitted table read. Clones share the permit, so a clone moved into a blocking read
/// keeps it until that read finishes even if the request is cancelled.
#[derive(Clone)]
pub(crate) struct Admission {
    _permit: Arc<OwnedSemaphorePermit>,
}
impl Admission {
    /// A 200 carries the permit in its body; 304 and errors release it here.
    pub(crate) fn attach(self, response: Response) -> Response {
        if response.status() != 200 {
            return response;
        }
        let (parts, inner) = response.into_parts();
        Response::from_parts(
            parts,
            Body::new(Held {
                inner,
                pending: Bytes::new(),
                _permit: self,
            }),
        )
    }
}
/// `attach` for routes that admit only table reads.
pub(crate) fn attach(admission: Option<Admission>, response: Response) -> Response {
    match admission {
        Some(admission) => admission.attach(response),
        None => response,
    }
}

/// Forwards `size_hint` too, so the response keeps its Content-Length.
struct Held {
    inner: Body,
    /// The rest of an oversized data frame, handed on in `CHUNK` copies.
    pending: Bytes,
    _permit: Admission,
}
impl HttpBody for Held {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        if this.pending.is_empty() {
            match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) if data.len() > CHUNK => this.pending = data,
                    Ok(data) => return Poll::Ready(Some(Ok(Frame::data(data)))),
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                other => return Poll::Ready(other),
            }
        }
        let n = this.pending.len().min(CHUNK);
        let chunk = Bytes::copy_from_slice(&this.pending[..n]);
        this.pending.advance(n);
        if this.pending.is_empty() {
            // Free the table now; only copies stay queued once the permit is released.
            this.pending = Bytes::new();
        }
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }
    fn is_end_stream(&self) -> bool {
        self.pending.is_empty() && self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        let inner = self.inner.size_hint();
        let pending = self.pending.len() as u64;
        let mut hint = SizeHint::new();
        hint.set_lower(inner.lower() + pending);
        if let Some(upper) = inner.upper() {
            hint.set_upper(upper + pending);
        }
        hint
    }
}
