use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::BytesMut;
use futures::{AsyncRead, Stream};
use nimiq_serde::{Deserialize, DeserializeError, SerializedSize as _};
use pin_project::pin_project;

use super::header::Header;

/// Try to read, such that at most `wanted_len` bytes are in the buffer.
///
/// This will return `Poll::Pending` until the buffer has `wanted_len` bytes in
/// it. This returns `Poll::Ready(Ok(false))` in case of EOF.
fn read_to_buf<R>(
    mut reader: Pin<&mut R>,
    buffer: &mut BytesMut,
    wanted_len: usize,
    cx: &mut Context<'_>,
) -> Poll<Result<bool, std::io::Error>>
where
    R: AsyncRead,
{
    let mut len_buffer_read = buffer.len();
    if buffer.len() < wanted_len {
        buffer.resize(wanted_len, 0);
    }
    while len_buffer_read < wanted_len {
        match AsyncRead::poll_read(
            reader.as_mut(),
            cx,
            &mut buffer[len_buffer_read..wanted_len],
        ) {
            // EOF
            Poll::Ready(Ok(0)) => {
                buffer.resize(len_buffer_read, 0);
                return Poll::Ready(Ok(false));
            }

            // Data was read
            Poll::Ready(Ok(read)) => {
                len_buffer_read += read;
            }

            // An error occurred
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),

            // Reader is not ready
            Poll::Pending => {
                buffer.resize(len_buffer_read, 0);
                return Poll::Pending;
            }
        }
    }
    Poll::Ready(Ok(true))
}

/// TODO: Generalize over a type `H: Header`, which is `Deserialize` and has a getter for the length of the message.
#[derive(Clone, Debug)]
enum ReaderState {
    Head,
    Data { header: Header },
}

#[pin_project]
pub struct MessageReader<R, M> {
    #[pin]
    inner: R,

    state: ReaderState,

    buffer: BytesMut,

    _message_type: PhantomData<M>,
}

impl<R, M> MessageReader<R, M> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            state: ReaderState::Head,
            buffer: BytesMut::with_capacity(1024), // TODO: initial size?
            _message_type: PhantomData,
        }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    pub fn into_other<N>(self) -> MessageReader<R, N>
    where
        N: Deserialize,
    {
        if let ReaderState::Data { .. } = &ReaderState::Head {
            panic!("MessageReader can't be converted while data is being read.");
        }

        MessageReader {
            inner: self.inner,
            state: ReaderState::Head,
            buffer: self.buffer,
            _message_type: PhantomData,
        }
    }
}

fn unexpected_eof<T>() -> Poll<Option<Result<T, DeserializeError>>> {
    Poll::Ready(Some(Err(DeserializeError::unexpected_end())))
}

impl<R, M> Stream for MessageReader<R, M>
where
    R: AsyncRead,
    M: Deserialize + std::fmt::Debug,
{
    type Item = Result<M, DeserializeError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let self_projected = self.project();

        let (new_state, message) = match &self_projected.state {
            ReaderState::Head => {
                // Read header. This returns `Poll::Pending` until all header bytes have been read.
                match read_to_buf(
                    self_projected.inner,
                    self_projected.buffer,
                    Header::SIZE,
                    cx,
                ) {
                    // Wait for more data.
                    Poll::Pending => return Poll::Pending,

                    // An error occurred.
                    Poll::Ready(Err(e)) => {
                        return {
                            error!(error = %e, "Inner AsyncRead returned an error");
                            Poll::Ready(Some(Err(DeserializeError::unexpected_end())))
                        }
                    }

                    // EOF while reading the header.
                    Poll::Ready(Ok(false)) => {
                        if self_projected.buffer.is_empty() {
                            // No partial message, so this is the end of the stream
                            return Poll::Ready(None);
                        } else {
                            return unexpected_eof();
                        }
                    }

                    // Finished reading the header, and we didn't reach EOF.
                    Poll::Ready(Ok(true)) => {}
                }

                // Decode the header: 16 bit length big-endian
                // This will also advance the read position after the header.
                let header = match Header::deserialize_from_vec(self_projected.buffer) {
                    Ok(header) => header,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                };

                // Reset the buffer
                self_projected.buffer.clear();

                // Change reader state to read the data next.
                (ReaderState::Data { header }, None)
            }
            ReaderState::Data { header } => {
                let n = header.size as usize;

                // Read data. This returns `Poll::Pending` until all data bytes have been read.
                // The argument to `read_to_buf` is `n + 2`, because it takes the expected number of bytes read in
                // total, which includes the header.
                match read_to_buf(self_projected.inner, self_projected.buffer, n, cx) {
                    // Wait for more data.
                    Poll::Pending => return Poll::Pending,

                    // An error occurred.
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Some(Err(DeserializeError::unexpected_end())))
                    }

                    // EOF while reading the data.
                    Poll::Ready(Ok(false)) => return unexpected_eof(),

                    // Finished reading the message
                    Poll::Ready(Ok(true)) => (),
                }

                // Decode the message, the read position of the buffer is already at the start of the message.
                let message = match M::deserialize_from_vec(self_projected.buffer) {
                    Ok(message) => message,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                };

                // Reset the reader state to read a header next.
                *self_projected.state = ReaderState::Head;

                // Reset the buffer
                self_projected.buffer.clear();

                (ReaderState::Head, Some(message))
            }
        };

        *self_projected.state = new_state;

        if let Some(message) = message {
            Poll::Ready(Some(Ok(message)))
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {

    use bytes::{BufMut, BytesMut};
    use futures::{io::Cursor, StreamExt};
    use nimiq_serde::{Deserialize, Serialize, SerializedSize};
    use nimiq_test_log::test;

    use super::{Header, MessageReader};

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    struct TestMessage {
        pub foo: u32,
        pub bar: String,
    }

    fn put_message<M: Serialize>(buf: &mut BytesMut, message: &M) {
        let n = message.serialized_size();
        buf.reserve(n + Header::SIZE);
        let header = Header::new(n as u32);

        let mut w = buf.writer();
        header.serialize_to_writer(&mut w).unwrap();
        message.serialize_to_writer(&mut w).unwrap();
    }

    #[test(tokio::test)]
    pub async fn it_can_read_a_message() {
        let test_message = TestMessage {
            foo: 42,
            bar: "Hello World".to_owned(),
        };

        let mut data = BytesMut::new();
        put_message(&mut data, &test_message);
        let mut reader = MessageReader::new(Cursor::new(&data));

        assert_eq!(reader.next().await, Some(Ok(test_message)));
        assert_eq!(reader.next().await, None);
    }

    #[test(tokio::test)]
    pub async fn it_can_read_multiple_messages() {
        let m1 = TestMessage {
            foo: 42,
            bar: "Hello World".to_owned(),
        };
        let m2 = TestMessage {
            foo: 420,
            bar: "foobar".to_owned(),
        };

        let mut data = BytesMut::new();
        put_message(&mut data, &m1);
        put_message(&mut data, &m2);

        let mut reader = MessageReader::new(Cursor::new(&data));

        assert_eq!(reader.next().await, Some(Ok(m1)));
        assert_eq!(reader.next().await, Some(Ok(m2)));
        assert_eq!(reader.next().await, None);
    }
}
