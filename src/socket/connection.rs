use std::{
    collections::VecDeque,
    io::{self, IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
};

use rustix::{
    io::retry_on_intr,
    net::{
        RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
        SendAncillaryMessage, SendFlags, recvmsg, send, sendmsg,
    },
};
use tokio::{io::Interest, net::UnixStream};

/// Maximum number of FDs that can be sent/received in a single message.
///
/// Taken from libwayland.
pub const MAX_FDS_OUT: usize = 28;

/// A Wayland connection.
#[derive(Debug)]
pub struct Connection(UnixStream);

impl Connection {
    /// Sends all `bytes` and `fds` to completion.
    ///
    /// # Invariants
    ///
    /// `bytes.len() >= fds.len().div_ceil(MAX_FDS_OUT)` should hold to ensure
    /// every FD chunk has at least one byte to piggyback on.
    pub async fn send_all(&self, mut bytes: &[u8], mut fds: &[OwnedFd]) -> io::Result<()> {
        debug_assert!(
            bytes.len() >= fds.len().div_ceil(MAX_FDS_OUT),
            "not enough bytes to carry all FD chunks"
        );

        // FDs must piggyback on at least 1 byte. When there are more than MAX_FDS_OUT
        // FDs, drain them in chunks of MAX_FDS_OUT, each carried by a single byte.
        while fds.len() > MAX_FDS_OUT {
            let n = self.send(&bytes[..1], &fds[..MAX_FDS_OUT]).await?;
            bytes = &bytes[n..];
            fds = &fds[MAX_FDS_OUT..];
        }

        // At most MAX_FD_NUM FDs remain; send as many bytes as possible.
        // If there are FDs, they are all guaranteed sent after this call returns.
        let n = self.send(bytes, fds).await?;
        bytes = &bytes[n..];

        // Send any remaining bytes with no FDs attached.
        while !bytes.is_empty() {
            let n = self.send(bytes, &[]).await?;
            bytes = &bytes[n..];
        }

        Ok(())
    }

    /// Asynchronously sends raw bytes and attached FDs.
    ///
    /// This is the asynchronous wrapper around [`Self::try_send`]. It will yield
    /// execution to the async runtime if the underlying socket is not ready.
    /// **It internally absorbs any `WouldBlock` errors**, meaning it will only return
    /// once data is actually sent or a fatal I/O error occurs.
    ///
    /// See [`Self::try_send`] for invariants, FD semantics, and underlying OS behavior.
    pub async fn send(&self, bytes: &[u8], fds: &[OwnedFd]) -> io::Result<usize> {
        loop {
            self.0.writable().await?;

            match self
                .0
                .try_io(Interest::WRITABLE, || self.try_send(bytes, fds))
            {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Asynchronously receives a byte stream and extracts attached FDs.
    ///
    /// This is the asynchronous wrapper around [`Self::try_recv`]. It will yield
    /// execution to the async runtime if there is no data available.
    /// **It internally absorbs any `WouldBlock` errors**, meaning it will only return
    /// once data is successfully read, EOF is reached, or a fatal I/O error occurs.
    ///
    /// See [`Self::try_recv`] for invariants, EOF semantics, and underlying OS behavior.
    pub async fn recv(&self, bytes: &mut [u8], fds: &mut VecDeque<OwnedFd>) -> io::Result<usize> {
        loop {
            self.0.readable().await?;

            match self
                .0
                .try_io(Interest::READABLE, || self.try_recv(bytes, fds))
            {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Attempts to send raw bytes and attached FDs in a non-blocking manner.
    ///
    /// # Returns
    ///
    /// - **`Ok(bytes_sent)`**: If `bytes_sent > 0`, all FDs are guaranteed by the OS to have
    ///   been sent, even if `bytes_sent < bytes.len()`.
    /// - **`Err(WouldBlock)`**: Socket buffer is full; 0 bytes and 0 FDs were sent.
    /// - **`Err(e)`**: A fatal I/O error occurred.
    ///
    /// # Invariants
    ///
    /// - `bytes` must not be empty if `fds` is non-empty: FDs must piggyback on at least one byte to be reliably delivered.
    /// - `fds.len()` must not exceed `MAX_FDS_OUT`.
    pub fn try_send(&self, bytes: &[u8], fds: &[OwnedFd]) -> io::Result<usize> {
        debug_assert!(
            fds.is_empty() || !bytes.is_empty(),
            "FDs must be attached to at least one byte"
        );
        debug_assert!(fds.len() <= MAX_FDS_OUT, "send too many FDs");

        #[cfg(not(any(target_os = "macos", target_os = "redox")))]
        let flags = SendFlags::DONTWAIT | SendFlags::NOSIGNAL;
        #[cfg(any(target_os = "macos", target_os = "redox"))]
        let flags = SendFlags::DONTWAIT;

        if !fds.is_empty() {
            let iov = [IoSlice::new(bytes)];
            let mut cmsg_space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_OUT))];
            let mut cmsg_buffer = SendAncillaryBuffer::new(&mut cmsg_space);
            cmsg_buffer.push(SendAncillaryMessage::ScmRights(as_borrowed_fds(fds)));

            Ok(retry_on_intr(|| {
                sendmsg(self, &iov, &mut cmsg_buffer, flags)
            })?)
        } else {
            Ok(retry_on_intr(|| send(self, bytes, flags))?)
        }
    }

    /// Attempts to receive a byte stream and extract attached FDs in a non-blocking manner.
    ///
    /// # Returns
    ///
    /// - **`Ok(n)`**: Bytes read into `bytes`. `Ok(0)` means the peer has closed the
    ///   connection (EOF). Any received FDs are appended to the back of `fds`.
    /// - **`Err(WouldBlock)`**: No data available yet.
    /// - **`Err(e)`**: A fatal I/O error occurred.
    ///
    /// # Invariants
    ///
    /// - `bytes` must not be empty.
    pub fn try_recv(&self, bytes: &mut [u8], fds: &mut VecDeque<OwnedFd>) -> io::Result<usize> {
        debug_assert!(!bytes.is_empty(), "recv empty byte slice");

        #[cfg(not(any(target_os = "macos", target_os = "redox")))]
        let flags = RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC;
        #[cfg(any(target_os = "macos", target_os = "redox"))]
        let flags = RecvFlags::DONTWAIT;

        let mut iov = [IoSliceMut::new(bytes)];
        let mut cmsg_space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_OUT))];
        let mut cmsg_buffer = RecvAncillaryBuffer::new(&mut cmsg_space);

        let msg = retry_on_intr(|| recvmsg(self, &mut iov[..], &mut cmsg_buffer, flags))?;

        let received_fds = cmsg_buffer
            .drain()
            .filter_map(|cmsg| match cmsg {
                RecvAncillaryMessage::ScmRights(rights) => Some(rights),
                _ => None,
            })
            .flatten();

        fds.extend(received_fds);

        #[cfg(any(target_os = "macos", target_os = "redox"))]
        for fd in fds.iter() {
            if let Ok(flags) = rustix::io::fcntl_getfd(fd) {
                let _ = rustix::io::fcntl_setfd(fd, flags | rustix::io::FdFlags::CLOEXEC);
            }
        }

        Ok(msg.bytes)
    }
}

impl From<UnixStream> for Connection {
    fn from(stream: UnixStream) -> Self {
        // macOS doesn't have MSG_NOSIGNAL, but has SO_NOSIGPIPE instead
        #[cfg(target_os = "macos")]
        let _ = rustix::net::sockopt::set_socket_nosigpipe(&stream, true);
        Self(stream)
    }
}

impl AsFd for Connection {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Reinterprets a slice of [`OwnedFd`] as a slice of [`BorrowedFd`].
const fn as_borrowed_fds(fds: &[OwnedFd]) -> &[BorrowedFd<'_>] {
    // SAFETY:
    // Both types are `#[repr(transparent)]` over a `RawFd` (`i32`), so they are
    // layout-identical. The output lifetime is bound to the input, so the FDs
    // cannot be dropped while the borrow is live.
    unsafe { std::slice::from_raw_parts(fds.as_ptr() as *const BorrowedFd, fds.len()) }
}
