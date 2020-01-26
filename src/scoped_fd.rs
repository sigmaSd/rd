use nix::fcntl::open;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use nix::unistd::close;
use nix::NixPath;
use std::os::unix::io::RawFd;

pub struct ScopedFd {
    fd: RawFd,
}

impl ScopedFd {
    pub fn new() -> Self {
        ScopedFd { fd: -1 }
    }

    pub fn from_raw(fd: RawFd) -> Self {
        ScopedFd { fd: fd }
    }

    pub fn open_from_path<P: ?Sized + NixPath>(path: &P, oflag: OFlag, mode: Mode) -> Self {
        let rawfd = open(path, oflag, mode).unwrap();
        ScopedFd { fd: rawfd }
    }

    pub fn close(&mut self) {
        if self.fd >= 0 {
            close(self.fd).unwrap();
        }

        self.fd = -1;
    }

    pub fn is_open(&self) -> bool {
        self.fd >= 0
    }

    pub fn as_raw(&self) -> RawFd {
        self.fd
    }

    pub fn extract(&mut self) -> RawFd {
        let result = self.fd;
        self.fd = -1;
        result
    }
}

impl Drop for ScopedFd {
    fn drop(&mut self) {
        self.close()
    }
}
