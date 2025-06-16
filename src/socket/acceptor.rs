use std::{
    env,
    fs::{self, File},
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, RawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use rustix::{
    fs::{FlockOperation, flock},
    io::Errno,
};
use thiserror::Error;
use tokio::net::UnixListener;

use crate::socket::connection::Connection;

/// Socket errors.
#[derive(Debug, Error)]
pub enum AcceptorError {
    #[error("no available socket candidates")]
    NoAvailableSocket,
    #[error("XDG_RUNTIME_DIR is not set or invalid")]
    RuntimeDirInvalid,
    #[error("socket is already in use")]
    SocketInUse,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Wayland connection acceptor.
#[derive(Debug)]
pub struct Acceptor {
    /// The underlying listener.
    listener: UnixListener,

    /// Name of the bound socket.
    socket_name: String,

    /// Path to the bound socket.
    bind_path: PathBuf,
    /// Path to the lock file.
    lock_path: PathBuf,

    /// Lock file, held as guard.
    _lock: File,
}

impl Acceptor {
    /// Attempts to bind to a socket from the range `wayland-0` to `wayland-32`.
    ///
    /// The socket will be created under the `XDG_RUNTIME_DIR`.
    pub fn bind_auto() -> Result<Self, AcceptorError> {
        Self::bind_range("wayland", 0..33)
    }

    /// Attempts to bind to a socket from a range of candidates.
    ///
    /// Candidate socket names are constructed by appending a numeric index
    /// to the given basename (e.g. `basename-0`, `basename-1`, ...).
    ///
    /// The socket will be created under the `XDG_RUNTIME_DIR`.
    pub fn bind_range<I>(basename: &str, range: I) -> Result<Self, AcceptorError>
    where
        I: IntoIterator<Item = usize>,
    {
        let xdg = Self::xdg_runtime_dir()?;

        for index in range {
            match Self::bind(&xdg, format!("{basename}-{index}")) {
                // Successfully bound to a socket, return.
                Ok(acceptor) => return Ok(acceptor),

                // Lock is in use, try next candidate.
                Err(AcceptorError::SocketInUse) => continue,

                // Other errors, abort.
                Err(err) => return Err(err),
            }
        }

        Err(AcceptorError::NoAvailableSocket)
    }

    /// Binds to a socket with the given name.
    ///
    /// The socket will be created under the `XDG_RUNTIME_DIR`.
    pub fn bind_name<S>(socket_name: S) -> Result<Self, AcceptorError>
    where
        S: AsRef<str> + Into<String>,
    {
        Self::bind(&Self::xdg_runtime_dir()?, socket_name)
    }

    /// Binds to a socket with the given name in the given directory.
    pub fn bind<P, S>(directory: P, socket_name: S) -> Result<Self, AcceptorError>
    where
        P: AsRef<Path>,
        S: AsRef<str> + Into<String>,
    {
        // Build paths.
        let (bind_path, lock_path) = Self::build_paths(directory.as_ref(), socket_name.as_ref());

        // Acquire the lock first.
        let _lock = Self::lock_file(&lock_path)?;

        // Clean up any stale socket file from a previous crashed run.
        // It is safe to delete it now because we hold the exclusive lock.
        Self::cleanup_stale_socket(&bind_path)?;

        // Bind and listen
        let listener = UnixListener::bind(&bind_path)?;

        Ok(Acceptor {
            listener,
            socket_name: socket_name.into(),
            bind_path,
            lock_path,
            _lock,
        })
    }

    /// Returns the name of the bound socket.
    pub fn socket_name(&self) -> &str {
        &self.socket_name
    }

    /// Accepts a new connection.
    pub async fn accept(&self) -> Result<Connection, AcceptorError> {
        let (stream, _) = self.listener.accept().await?;
        Ok(Connection::from(stream))
    }

    /// Returns (bind_path, lock_path).
    fn build_paths(directory: &Path, socket_name: &str) -> (PathBuf, PathBuf) {
        (
            directory.join(socket_name),
            // We use string formatting to append ".lock" rather than `Path::with_extension`.
            // If `socket_name` already contains a dot (e.g., "custom.sock"), `with_extension`
            // would replace the existing extension (yielding "custom.lock") instead of
            // appending to it (yielding the desired "custom.sock.lock").
            directory.join(format!("{socket_name}.lock")),
        )
    }

    fn lock_file(lock_path: &Path) -> Result<File, AcceptorError> {
        // We loop to handle a classic race condition: another process might unlink
        // the file right between our `open()` and `flock()`. If this happens, we
        // could end up holding a lock on a deleted ("ghost") file while another
        // process locks a newly created file at the exact same path.
        // We verify the inode to detect this anomaly and retry if necessary.
        loop {
            // Open lock file.
            let lock = File::options()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(lock_path)?;

            // Try to acquire a non-blocking exclusive lock.
            match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(_) => {
                    // Lock acquired successfully, proceed.
                }
                Err(err) if err == Errno::WOULDBLOCK || err == Errno::AGAIN => {
                    // Lock is currently held by another process.
                    return Err(AcceptorError::SocketInUse);
                }
                Err(err) => {
                    // For any other error, return it as a standard I/O error.
                    return Err(AcceptorError::Io(err.into()));
                }
            }

            // Get the metadata of the currently opened file descriptor.
            let fd_meta = lock.metadata()?;

            // Get the metadata of the file currently residing at the path on disk.
            let path_meta = match fs::metadata(lock_path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // The file was removed between open() and flock(), retry.
                    continue;
                }
                Err(err) => return Err(err.into()),
            };

            // Verify that the device ID and inode match exactly.
            if fd_meta.dev() == path_meta.dev() && fd_meta.ino() == path_meta.ino() {
                // Success, return the locked file as guard.
                return Ok(lock);
            }

            // The file was replaced (race condition), retry.
        }
    }

    fn cleanup_stale_socket(bind_path: &Path) -> Result<(), AcceptorError> {
        match fs::remove_file(bind_path) {
            Ok(_) => {
                // The file existed and was successfully removed.
                // The environment is now clean.
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // The file didn't exist in the first place,
                // which is also a perfectly expected state.
                Ok(())
            }
            Err(e) => {
                // Acquired the lock but can't delete the socket file?
                // This is a genuine exceptional condition (e.g., insufficient permissions).
                Err(AcceptorError::Io(e))
            }
        }
    }

    fn xdg_runtime_dir() -> Result<PathBuf, AcceptorError> {
        let dir = env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .map_err(|_| AcceptorError::RuntimeDirInvalid)?;

        if !dir.is_absolute() {
            return Err(AcceptorError::RuntimeDirInvalid);
        }

        Ok(dir)
    }
}

impl AsRawFd for Acceptor {
    fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }
}

impl AsFd for Acceptor {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.bind_path);
        let _ = fs::remove_file(&self.lock_path);
    }
}
