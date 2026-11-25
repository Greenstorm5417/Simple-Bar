//! Shared helpers for the non-blocking IPC sockets in the event loop.

use std::io::{self, ErrorKind, Read};

/// Upper bound for unread bytes kept from one peer.
pub(crate) const MAX_BUFFERED: usize = 1 << 20;

/// Append everything the socket has ready to `buf`.
///
/// Returns `Ok(())` once the socket would block, and an error when the
/// peer hung up (`UnexpectedEof`), the read failed, or the peer sent more
/// than [`MAX_BUFFERED`] unread bytes.
pub(crate) fn read_available(stream: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_BUFFERED {
                    return Err(ErrorKind::OutOfMemory.into());
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eof_is_an_error() {
        let mut buf = Vec::new();
        let err = read_available(&mut &b"abc"[..], &mut buf).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert_eq!(buf, b"abc");
    }
}
