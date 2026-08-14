use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming};
use pin_project_lite::pin_project;
use protocol_openai_chat_completions::ChatSseDecoder;
use tokio::time::{Instant, Sleep};

use crate::timeouts::BoxError;

pin_project! {
    pub(crate) struct ChatCompletionBody {
        #[pin]
        inner: Incoming,
        #[pin]
        sleep: Sleep,
        timeout: Duration,
        decoder: ChatSseDecoder,
        pending: VecDeque<Bytes>,
        upstream_done: bool,
    }
}

impl ChatCompletionBody {
    pub(crate) fn new(inner: Incoming, decoder: ChatSseDecoder, timeout: Duration) -> Self {
        Self {
            inner,
            sleep: tokio::time::sleep(timeout),
            timeout,
            decoder,
            pending: VecDeque::new(),
            upstream_done: false,
        }
    }
}

impl Body for ChatCompletionBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        if let Some(data) = this.pending.pop_front() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        if *this.upstream_done {
            return Poll::Ready(None);
        }
        loop {
            match this.inner.as_mut().poll_frame(context) {
                Poll::Ready(Some(Ok(frame))) => {
                    this.sleep.as_mut().reset(Instant::now() + *this.timeout);
                    if let Ok(data) = frame.into_data() {
                        let events = match this.decoder.push(&data) {
                            Ok(events) => events,
                            Err(error) => return Poll::Ready(Some(Err(Box::new(error)))),
                        };
                        this.pending.extend(
                            events
                                .into_iter()
                                .map(|event| Bytes::from(format!("data: {event}\n\n"))),
                        );
                        if let Some(data) = this.pending.pop_front() {
                            return Poll::Ready(Some(Ok(Frame::data(data))));
                        }
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(Box::new(error))));
                }
                Poll::Ready(None) => {
                    let events = match this.decoder.finish() {
                        Ok(events) => events,
                        Err(error) => return Poll::Ready(Some(Err(Box::new(error)))),
                    };
                    this.pending.extend(
                        events
                            .into_iter()
                            .map(|event| Bytes::from(format!("data: {event}\n\n"))),
                    );
                    *this.upstream_done = true;
                    return this.pending.pop_front().map_or(Poll::Ready(None), |data| {
                        Poll::Ready(Some(Ok(Frame::data(data))))
                    });
                }
                Poll::Pending => break,
            }
        }
        if this.sleep.poll(context).is_ready() {
            return Poll::Ready(Some(Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upstream Chat Completions stream idle timeout",
            )))));
        }
        Poll::Pending
    }
}
