#[cfg(not(any(feature = "_rt-std", feature = "_rt-tokio", feature = "_rt-async-std")))]
use std::{path::Path, time::Duration};

#[cfg(feature = "_rt-async-std")]
mod rt_async_std;
#[cfg(feature = "_rt-std")]
mod rt_std;
#[cfg(feature = "_rt-tokio")]
mod rt_tokio;

pub const MAX_BUFFER_SIZE: usize = 64;

#[cfg(unix)]
// Unused on cfgs where the other StdUnixStream definition is selected; kept for the _rt-std path.
#[allow(dead_code)]
pub(crate) type StdUnixStream = std::os::unix::net::UnixStream;

#[cfg(not(unix))]
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct StdUnixStream;

#[cfg(not(unix))]
#[allow(dead_code)]
impl StdUnixStream {
    pub(crate) fn connect<P: AsRef<std::path::Path>>(_path: P) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unix sockets not supported on Windows",
        ))
    }
    pub(crate) fn set_nonblocking(&self, _b: bool) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(not(unix))]
impl std::io::Read for StdUnixStream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

#[cfg(not(unix))]
impl std::io::Write for StdUnixStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) struct SocketAgent {
    #[cfg(feature = "_rt-std")]
    stream: StdUnixStream,
    #[cfg(feature = "_rt-tokio")]
    stream: tokio::net::UnixStream,
    #[cfg(feature = "_rt-async-std")]
    stream: async_std::os::unix::net::UnixStream,
}

#[cfg(not(any(feature = "_rt-std", feature = "_rt-tokio", feature = "_rt-async-std")))]
impl SocketAgent {
    #[allow(unused)]
    pub(crate) fn new<P: AsRef<Path>>(_socket_path: P, _timeout: Duration) -> crate::Result<Self> {
        crate::missing_rt!()
    }
}
