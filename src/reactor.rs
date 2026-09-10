//! Single-threaded readiness through `io_uring`, with a poll compatibility backend.
//! Only readiness requests enter the ring: the kernel never borrows Rust buffers.
#![allow(unsafe_code)]

use io_uring::{IoUring, opcode, squeue, types};
use std::{io, os::fd::RawFd, time::Duration};

pub struct Reactor {
    uring: Option<Uring>,
    required: bool,
    fallback: Vec<libc::pollfd>,
}

impl Reactor {
    pub fn new() -> io::Result<Self> {
        let mode = std::env::var_os("RTCH_IO_BACKEND").unwrap_or_else(|| "auto".into());
        let required = mode == "uring";
        let uring = if mode == "poll" {
            None
        } else if mode == "auto" || required {
            match Uring::new() {
                Ok(ring) => Some(ring),
                Err(e) if !required && unavailable(&e) => None,
                Err(e) => return Err(e),
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RTCH_IO_BACKEND must be auto, uring, or poll",
            ));
        };
        Ok(Self {
            uring,
            required,
            fallback: Vec::new(),
        })
    }

    /// Forget a descriptor before closing it; late completions cannot reach a
    /// new connection that reuses the same descriptor number.
    pub fn remove(&mut self, fd: RawFd) -> io::Result<()> {
        if let Some(ring) = &mut self.uring
            && let Some(index) = ring.watches.iter().position(|w| w.fd == fd)
        {
            ring.cancel(index)?;
        }
        Ok(())
    }

    /// An events mask of zero disables the descriptor, including hangups.
    pub fn wait(&mut self, fds: &mut [libc::pollfd], timeout: u32) -> io::Result<()> {
        for fd in &mut *fds {
            fd.revents = 0;
        }
        if let Some(ring) = &mut self.uring {
            match ring.wait(fds, timeout) {
                Ok(()) => return Ok(()),
                Err(e) if !self.required && unavailable(&e) => self.uring = None,
                Err(e) => return Err(e),
            }
        }
        // poll reports HUP even for events=0. Match the ring's disabled watches.
        self.fallback.clear();
        self.fallback.extend(fds.iter().map(|fd| libc::pollfd {
            fd: if fd.events == 0 { -1 } else { fd.fd },
            events: fd.events,
            revents: 0,
        }));
        crate::os::poll(
            &mut self.fallback,
            i32::try_from(timeout).unwrap_or(i32::MAX),
        )?;
        for (fd, ready) in fds.iter_mut().zip(&self.fallback) {
            fd.revents = ready.revents;
        }
        Ok(())
    }
}

fn unavailable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EOPNOTSUPP | libc::EINVAL)
    )
}

struct Watch {
    fd: RawFd,
    events: i16,
    token: u64,
}

struct Uring {
    ring: IoUring,
    watches: Vec<Watch>,
    next_token: u64,
}

impl Uring {
    fn new() -> io::Result<Self> {
        // At most 66 live descriptors, plus cancellations and replacement polls.
        let ring = IoUring::new(256)?;
        if !ring.params().is_feature_ext_arg() {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        Ok(Self {
            ring,
            watches: Vec::with_capacity(66),
            next_token: 0,
        })
    }

    fn enqueue(&mut self, entry: &squeue::Entry) -> io::Result<()> {
        loop {
            // SAFETY: only PollAdd/PollRemove entries reach this helper. They
            // contain integers, no pointers to user memory. Never use SQPOLL;
            // descriptor changes occur on this thread between wait calls.
            if unsafe { self.ring.submission().push(entry) }.is_ok() {
                return Ok(());
            }
            match self.ring.submit() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn cancel(&mut self, index: usize) -> io::Result<()> {
        let watch = self.watches.swap_remove(index);
        // Token zero belongs exclusively to cancellation acknowledgements.
        self.enqueue(&opcode::PollRemove::new(watch.token).build())
    }

    fn wait(&mut self, fds: &mut [libc::pollfd], timeout: u32) -> io::Result<()> {
        let mut index = 0;
        while index < self.watches.len() {
            let watch = &self.watches[index];
            if fds
                .iter()
                .any(|fd| fd.fd == watch.fd && fd.events == watch.events)
            {
                index += 1;
            } else {
                self.cancel(index)?;
            }
        }
        for fd in fds.iter().filter(|fd| fd.fd >= 0 && fd.events != 0) {
            if self.watches.iter().any(|watch| watch.fd == fd.fd) {
                continue;
            }
            self.next_token = self
                .next_token
                .checked_add(1)
                .ok_or_else(|| io::Error::other("io_uring token space exhausted"))?;
            let entry =
                opcode::PollAdd::new(types::Fd(fd.fd), u32::from(fd.events.cast_unsigned()))
                    .build()
                    .user_data(self.next_token);
            self.enqueue(&entry)?;
            self.watches.push(Watch {
                fd: fd.fd,
                events: fd.events,
                token: self.next_token,
            });
        }
        let timeout = types::Timespec::from(Duration::from_millis(u64::from(timeout)));
        let args = types::SubmitArgs::new().timespec(&timeout);
        match self.ring.submitter().submit_with_args(1, &args) {
            Ok(_) => {}
            Err(e)
                if e.kind() == io::ErrorKind::Interrupted
                    || e.raw_os_error() == Some(libc::ETIME) => {}
            Err(e) => return Err(e),
        }
        for completion in self.ring.completion() {
            let token = completion.user_data();
            // Cancelled watches can complete successfully before cancellation
            // wins the race. Their tokens are already gone and must be ignored.
            let Some(index) = self.watches.iter().position(|w| w.token == token) else {
                continue;
            };
            let watch = self.watches.swap_remove(index);
            let result = completion.result();
            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result));
            }
            if let Some(fd) = fds.iter_mut().find(|fd| fd.fd == watch.fd) {
                fd.revents |= i16::try_from(result).map_err(io::Error::other)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
        time::Instant,
    };

    fn check(mut reactor: Reactor) {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let mut fds = [libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let start = Instant::now();
        reactor.wait(&mut fds, 20).unwrap();
        assert_eq!(fds[0].revents, 0);
        assert!(start.elapsed() >= Duration::from_millis(10));

        for byte in 0..100_u8 {
            writer.write_all(&[byte]).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                reactor.wait(&mut fds, 20).unwrap();
                if fds[0].revents & libc::POLLIN != 0 {
                    break;
                }
                assert!(Instant::now() < deadline);
            }
            let mut received = [0];
            reader.read_exact(&mut received).unwrap();
            assert_eq!(received, [byte]);
            // Arm an idle request, then cancel it and reuse the descriptor.
            reactor.wait(&mut fds, 1).unwrap();
            reactor.remove(reader.as_raw_fd()).unwrap();
        }

        // A queued completion for an old mask must not leak into a new mask.
        reactor.wait(&mut fds, 1).unwrap();
        writer.write_all(b"x").unwrap();
        fds[0].events = libc::POLLOUT;
        for _ in 0..3 {
            reactor.wait(&mut fds, 20).unwrap();
            assert_eq!(fds[0].revents & libc::POLLIN, 0);
        }
        fds[0].events = 0;
        drop(writer);
        for _ in 0..3 {
            reactor.wait(&mut fds, 1).unwrap();
            assert_eq!(
                fds[0].revents, 0,
                "disabled descriptors must not spin on HUP"
            );
        }
        fds[0].events = libc::POLLIN;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            reactor.wait(&mut fds, 20).unwrap();
            if fds[0].revents & libc::POLLHUP != 0 {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        let mut remaining = Vec::new();
        reader.read_to_end(&mut remaining).unwrap();
        assert_eq!(remaining, b"x");
    }

    #[test]
    fn poll_readiness_timeout_cancellation_and_hangup() {
        check(Reactor {
            uring: None,
            required: false,
            fallback: Vec::new(),
        });
    }

    #[test]
    fn uring_readiness_timeout_cancellation_and_hangup() {
        let uring = match Uring::new() {
            Ok(ring) => ring,
            Err(e)
                if unavailable(&e)
                    && std::env::var_os("RTCH_IO_BACKEND").is_none_or(|m| m != "uring") =>
            {
                eprintln!("io_uring unavailable: {e}; set RTCH_IO_BACKEND=uring to require it");
                return;
            }
            Err(e) => panic!("io_uring initialization failed: {e}"),
        };
        check(Reactor {
            uring: Some(uring),
            required: true,
            fallback: Vec::new(),
        });
    }
}
