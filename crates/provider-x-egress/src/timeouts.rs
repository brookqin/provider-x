use std::{
    error::Error,
    fmt,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming};
use pin_project_lite::pin_project;
use tokio::time::{Instant, Sleep};

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Debug)]
struct StreamIdleTimeout;

impl fmt::Display for StreamIdleTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("upstream response stream idle timeout")
    }
}

impl Error for StreamIdleTimeout {}

pin_project! {
    pub(crate) struct IdleTimeoutBody {
        #[pin]
        inner: Incoming,
        #[pin]
        sleep: Sleep,
        timeout: Duration,
    }
}

impl IdleTimeoutBody {
    pub(crate) fn new(inner: Incoming, timeout: Duration) -> Self {
        Self {
            inner,
            sleep: tokio::time::sleep(timeout),
            timeout,
        }
    }
}

impl Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                this.sleep.as_mut().reset(Instant::now() + *this.timeout);
                return Poll::Ready(Some(Ok(frame)));
            }
            Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(Box::new(error)))),
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Pending => {}
        }

        if this.sleep.poll(context).is_ready() {
            return Poll::Ready(Some(Err(Box::new(StreamIdleTimeout))));
        }
        Poll::Pending
    }
}
