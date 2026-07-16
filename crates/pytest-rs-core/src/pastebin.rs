//! `--pastebin=all`: tee the whole session's real stdout (fd 1) into an
//! in-memory buffer without touching any of the engine's own print call
//! sites. Unlike upstream (which monkeypatches a Python-level
//! TerminalReporter._tw.write), pytest-rs's native mode writes straight to
//! fd 1 via println!/print!, so the tee has to happen at the OS descriptor
//! level: dup the current fd 1 (whatever it points to — a real terminal, or
//! an outer pytester capture file), splice a pipe in its place, and relay
//! every byte read back to the original destination while also buffering it.

use std::io::Write;
use std::thread::JoinHandle;

pub struct PastebinCapture {
    real_fd: libc::c_int,
    handle: Option<JoinHandle<Vec<u8>>>,
}

impl PastebinCapture {
    /// Start teeing fd 1. Safety: only ever called once per session, from
    /// the main thread, before any worker threads that might also write to
    /// fd 1 are spawned.
    #[allow(unsafe_code)]
    pub fn start() -> Option<Self> {
        unsafe {
            let real_fd = libc::dup(1);
            if real_fd < 0 {
                return None;
            }
            let mut fds: [libc::c_int; 2] = [0, 0];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                libc::close(real_fd);
                return None;
            }
            let (read_fd, write_fd) = (fds[0], fds[1]);
            if libc::dup2(write_fd, 1) < 0 {
                libc::close(real_fd);
                libc::close(read_fd);
                libc::close(write_fd);
                return None;
            }
            libc::close(write_fd);
            let handle = std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = libc::read(read_fd, chunk.as_mut_ptr().cast(), chunk.len());
                    if n <= 0 {
                        break;
                    }
                    let bytes = &chunk[..n as usize];
                    libc::write(real_fd, bytes.as_ptr().cast(), bytes.len());
                    buffer.extend_from_slice(bytes);
                }
                libc::close(read_fd);
                buffer
            });
            Some(Self {
                real_fd,
                handle: Some(handle),
            })
        }
    }

    /// Stop teeing, restore fd 1 to its original destination, and return
    /// everything that was written during the session as raw bytes (upstream
    /// itself pipes the tee through a "w+b" tempfile, so create_new_paste's
    /// caller always hands it bytes, not str).
    #[allow(unsafe_code)]
    pub fn stop(mut self) -> Vec<u8> {
        let _ = std::io::stdout().flush();
        unsafe {
            libc::dup2(self.real_fd, 1);
            libc::close(self.real_fd);
        }
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}
