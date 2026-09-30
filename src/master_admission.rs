//! Bounds decoded Master table bytes held for reads and responses.
//!
//! Every table read loads, hashes and checks a whole file (up to `master_registry::MAX_JSON`)
//! and keeps the bytes until the client has received them. A process-wide gate admits at most
//! `TABLE_READS` of them: about 24 MB with real tables of about 1.5 MB, 16 x 64 MiB in theory.
//! Waiters queue in FIFO order for `TABLE_WAIT`, then answer 503 `master_unavailable`. A 200
//! keeps its permit until the body is sent or dropped, so slow clients hold permits; any other
//! response releases it at once. Manifests, history and bundles (which have their own gate) are
//! not admitted here. Nothing is configurable.
use crate::error::AppError;
use axum::{
    body::{Body, Bytes, HttpBody},
    response::Response,
};
use hyper::body::{Frame, SizeHint};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const TABLE_READS: usize = 16;
const TABLE_WAIT: Duration = Duration::from_secs(5);
const WARN_EVERY_SECS: u64 = 60;
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
    _permit: Admission,
}
impl HttpBody for Held {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
