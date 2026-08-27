use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

/// Default maximum size of one newline-delimited ACP frame.
pub const DEFAULT_LINE_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) struct BoundedLines<R> {
    reader: Pin<Box<R>>,
    buffer: Vec<u8>,
    limit: usize,
    finished: bool,
}

impl<R> BoundedLines<R> {
    pub(crate) fn new(reader: R, limit: usize) -> Self {
        Self {
            reader: Box::pin(reader),
            buffer: Vec::new(),
            limit,
            finished: false,
        }
    }

    fn take_line(&mut self, terminated: bool) -> io::Result<String> {
        if terminated && self.buffer.last() == Some(&b'\r') {
            self.buffer.pop();
        }
        String::from_utf8(std::mem::take(&mut self.buffer)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        })
    }

    fn overflow(&mut self) -> Poll<Option<io::Result<String>>> {
        self.finished = true;
        Poll::Ready(Some(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ACP line exceeds configured {}-byte limit", self.limit),
        ))))
    }
}

impl<R: futures::AsyncBufRead> futures::Stream for BoundedLines<R> {
    type Item = io::Result<String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }

        loop {
            let this = &mut *self;
            let available = match this.reader.as_mut().poll_fill_buf(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Ok(available)) => available,
            };
            if available.is_empty() {
                this.finished = true;
                return if this.buffer.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(this.take_line(false)))
                };
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let payload_bytes = newline.unwrap_or(available.len());
            let Some(payload_len) = this.buffer.len().checked_add(payload_bytes) else {
                return this.overflow();
            };
            if payload_len > this.limit {
                return this.overflow();
            }

            let consumed = newline.map_or(available.len(), |position| position + 1);
            this.buffer.extend_from_slice(&available[..payload_bytes]);
            this.reader.as_mut().consume(consumed);
            if newline.is_some() {
                return Poll::Ready(Some(this.take_line(true)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    #[tokio::test]
    async fn accepts_the_exact_limit_and_strips_line_endings() {
        let limit = 64;
        let mut input = vec![b'x'; limit - 1];
        input.extend_from_slice(b"\r\nnext");
        let reader = futures::io::BufReader::with_capacity(8, futures::io::Cursor::new(input));
        let mut lines = BoundedLines::new(reader, limit);

        assert_eq!(lines.next().await.unwrap().unwrap(), "x".repeat(limit - 1));
        assert_eq!(lines.next().await.unwrap().unwrap(), "next");
        assert!(lines.next().await.is_none());
    }

    #[tokio::test]
    async fn rejects_oversize_without_retaining_more_than_the_limit() {
        let limit = 64;
        let mut input = vec![b'x'; limit + 1];
        input.push(b'\n');
        let reader = futures::io::BufReader::with_capacity(8, futures::io::Cursor::new(input));
        let mut lines = BoundedLines::new(reader, limit);

        let error = lines.next().await.unwrap().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("64-byte limit"));
        assert!(lines.buffer.len() <= limit);
        assert!(lines.next().await.is_none());
    }
}
