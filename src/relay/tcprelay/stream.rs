//! Stream protocol implementation

use std::{
    cmp,
    io,
    marker::Unpin,
    pin::Pin,
    task::{Context, Poll},
};

use crate::crypto::{new_stream, BoxStreamCipher, CipherType, CryptoMode};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::ready;
use tokio::prelude::*;

use super::BUFFER_SIZE;

const DUMMY_BUFFER: [u8; BUFFER_SIZE] = [0u8; BUFFER_SIZE];

/// Reader wrapper that will decrypt data automatically
pub struct DecryptedReader {
    buffer: BytesMut,
    cipher: BoxStreamCipher,
    pos: usize,
    got_final: bool,
    incoming_buffer: Vec<u8>,
}

impl DecryptedReader {
    pub fn new(t: CipherType, key: &[u8], iv: &[u8]) -> DecryptedReader {
        let cipher = new_stream(t, key, iv, CryptoMode::Decrypt);
        let buffer_size = cipher.buffer_size(&DUMMY_BUFFER);
        DecryptedReader {
            buffer: BytesMut::with_capacity(buffer_size),
            cipher,
            pos: 0,
            got_final: false,
            incoming_buffer: vec![0u8; BUFFER_SIZE],
        }
    }

    pub fn poll_read_decrypted<R>(
        &mut self,
        ctx: &mut Context<'_>,
        r: &mut R,
        dst: &mut [u8],
    ) -> Poll<io::Result<usize>>
    where
        R: AsyncRead + Unpin,
    {
        while self.pos >= self.buffer.len() {
            if self.got_final {
                return Poll::Ready(Ok(0));
            }

            let n = ready!(Pin::new(&mut *r).poll_read(ctx, &mut self.incoming_buffer))?;

            // Reset pointers
            self.buffer.clear();
            self.pos = 0;

            if n == 0 {
                // Finialize block
                self.buffer.reserve(self.buffer_size(&[]));
                self.cipher.finalize(&mut self.buffer)?;
                self.got_final = true;
            } else {
                let data = &self.incoming_buffer[..n];
                // Ensure we have enough space
                let buffer_len = self.buffer_size(data);
                self.buffer.reserve(buffer_len);
                self.cipher.update(data, &mut self.buffer)?;
            }
        }

        let remaining_len = self.buffer.len() - self.pos;
        let n = cmp::min(dst.len(), remaining_len);
        (&mut dst[..n]).copy_from_slice(&self.buffer[self.pos..self.pos + n]);
        self.pos += n;
        Poll::Ready(Ok(n))
    }

    fn buffer_size(&self, data: &[u8]) -> usize {
        self.cipher.buffer_size(data)
    }
}

enum EncryptWriteStep {
    Nothing,
    Writing(BytesMut),
}

/// Writer wrapper that will encrypt data automatically
pub struct EncryptedWriter {
    cipher: BoxStreamCipher,
    steps: EncryptWriteStep,
    iv: Option<Bytes>,
}

impl EncryptedWriter {
    /// Creates a new EncryptedWriter
    pub fn new(t: CipherType, key: &[u8], iv: Bytes) -> EncryptedWriter {
        EncryptedWriter {
            cipher: new_stream(t, key, &iv, CryptoMode::Encrypt),
            steps: EncryptWriteStep::Nothing,
            iv: Some(iv),
        }
    }

    pub fn poll_write_encrypted<W>(&mut self, ctx: &mut Context<'_>, w: &mut W, data: &[u8]) -> Poll<io::Result<usize>>
    where
        W: AsyncWrite + Unpin,
    {
        ready!(self.poll_write_all_encrypted(ctx, w, data))?;
        Poll::Ready(Ok(data.len()))
    }

    fn poll_write_all_encrypted<W>(&mut self, ctx: &mut Context<'_>, w: &mut W, data: &[u8]) -> Poll<io::Result<()>>
    where
        W: AsyncWrite + Unpin,
    {
        // FIXME: How about finalize?

        loop {
            match self.steps {
                EncryptWriteStep::Nothing => {
                    // Send the first packet with iv
                    let iv_length = match self.iv {
                        Some(ref i) => i.len(),
                        None => 0,
                    };

                    let mut buf = BytesMut::with_capacity(iv_length + self.buffer_size(data));

                    // Put iv first
                    if let Some(i) = self.iv.take() {
                        buf.extend(i);
                    }

                    self.cipher_update(data, &mut buf)?;

                    self.steps = EncryptWriteStep::Writing(buf);
                }
                EncryptWriteStep::Writing(ref mut buf) => {
                    while buf.remaining() > 0 {
                        let n = ready!(Pin::new(&mut *w).poll_write_buf(ctx, buf))?;
                        if n == 0 {
                            use std::io::ErrorKind;
                            return Poll::Ready(Err(ErrorKind::UnexpectedEof.into()));
                        }
                    }

                    self.steps = EncryptWriteStep::Nothing;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }

    fn cipher_update<B: BufMut>(&mut self, data: &[u8], buf: &mut B) -> io::Result<()> {
        self.cipher.update(data, buf).map_err(From::from)
    }

    #[allow(dead_code)]
    fn cipher_finalize<B: BufMut>(&mut self, buf: &mut B) -> io::Result<()> {
        self.cipher.finalize(buf).map_err(From::from)
    }

    fn buffer_size(&self, data: &[u8]) -> usize {
        self.cipher.buffer_size(data)
    }
}
