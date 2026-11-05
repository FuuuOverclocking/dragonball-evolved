use std::future::poll_fn;
use std::io;
use std::mem::{MaybeUninit, size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::task::{Poll, ready};

use tokio::io::Interest;
use tokio::net::UnixStream;

use crate::{Body, Fds, MAX_FDS, invalid};

const CONTROL_WORDS: usize =
    (size_of::<libc::cmsghdr>() + MAX_FDS * size_of::<RawFd>()).div_ceil(size_of::<usize>());

pub(crate) async fn receive(
    stream: &UnixStream,
    bytes: &mut [u8],
    fds: &mut Fds,
) -> io::Result<usize> {
    // Each connection has one reader, so the single-waker path needs no async waiter list.
    poll_fn(|cx| {
        loop {
            ready!(stream.poll_read_ready(cx))?;
            match stream.try_io(Interest::READABLE, || {
                let mut control = [MaybeUninit::<usize>::uninit(); CONTROL_WORDS];
                recv_once(stream.as_raw_fd(), bytes, fds, &mut control)
            }) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                result => return Poll::Ready(result),
            }
        }
    })
    .await
}

fn recv_once(
    fd: RawFd,
    bytes: &mut [u8],
    fds: &mut Fds,
    control: &mut [MaybeUninit<usize>],
) -> io::Result<usize> {
    loop {
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        // All pointers reference live, aligned buffers for the duration of recvmsg.
        let mut msg: libc::msghdr = unsafe { zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(control);
        let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let mut invalid_control = false;
        // recvmsg supplies initialized cmsghdr records; adopt every installed FD before reporting errors.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let header_len = libc::CMSG_LEN(0) as usize;
                    if (*cmsg).cmsg_len < header_len {
                        invalid_control = true;
                    } else {
                        let data_len = (*cmsg).cmsg_len - header_len;
                        let count = data_len / size_of::<RawFd>();
                        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                        for index in 0..count {
                            let owned = OwnedFd::from_raw_fd(data.add(index).read_unaligned());
                            if fds.push(owned).is_err() {
                                invalid_control = true;
                            }
                        }
                        invalid_control |= data_len % size_of::<RawFd>() != 0;
                    }
                } else {
                    invalid_control = true;
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        if invalid_control || msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 {
            return Err(invalid(
                "truncated, excessive or unsupported ancillary data",
            ));
        }
        return Ok(n as usize);
    }
}

pub(crate) async fn send(stream: &UnixStream, head: &[u8], body: &Body) -> io::Result<()> {
    let mut rights = [0; MAX_FDS];
    for (index, slot) in rights[..body.fds().len()].iter_mut().enumerate() {
        *slot = body
            .fds()
            .get(index)
            .ok_or_else(|| invalid("cannot send vacant FD slots"))?
            .as_raw_fd();
    }
    let mut written = 0;
    let total = head.len() + body.len();
    while written < total {
        let (header, payload) = if written < head.len() {
            (&head[written..], body.as_bytes())
        } else {
            (&[][..], &body.as_bytes()[written - head.len()..])
        };
        let descriptors = if written == 0 {
            &rights[..body.fds().len()]
        } else {
            &[]
        };
        let n = poll_fn(|cx| {
            loop {
                ready!(stream.poll_write_ready(cx))?;
                match stream.try_io(Interest::WRITABLE, || {
                    send_once(stream.as_raw_fd(), header, payload, descriptors)
                }) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => return Poll::Ready(result),
                }
            }
        })
        .await?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        written += n;
    }
    Ok(())
}

fn send_once(fd: RawFd, header: &[u8], payload: &[u8], rights: &[RawFd]) -> io::Result<usize> {
    loop {
        let mut iovs = [
            libc::iovec {
                iov_base: header.as_ptr().cast_mut().cast(),
                iov_len: header.len(),
            },
            libc::iovec {
                iov_base: payload.as_ptr().cast_mut().cast(),
                iov_len: payload.len(),
            },
        ];
        let mut control = [0usize; CONTROL_WORDS];
        // sendmsg reads these buffers synchronously and never retains their pointers.
        let mut msg: libc::msghdr = unsafe { zeroed() };
        msg.msg_iov = iovs.as_mut_ptr();
        msg.msg_iovlen = iovs.len();
        if !rights.is_empty() {
            unsafe {
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(size_of_val(rights) as u32) as usize;
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(size_of_val(rights) as u32) as usize;
                std::ptr::copy_nonoverlapping(
                    rights.as_ptr().cast::<u8>(),
                    libc::CMSG_DATA(cmsg),
                    size_of_val(rights),
                );
            }
        }
        let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn truncated_control_data_does_not_leak_descriptors() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let rights = [socket.as_raw_fd(); 3];
        assert_eq!(
            send_once(sender.as_raw_fd(), b"x", &[], &rights).unwrap(),
            1
        );
        drop(socket);

        let control_len = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as usize;
        let mut control =
            vec![MaybeUninit::<usize>::uninit(); control_len.div_ceil(size_of::<usize>())];
        let mut fds = Fds::default();
        let error = recv_once(receiver.as_raw_fd(), &mut [0], &mut fds, &mut control).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!fds.is_empty());
        assert!(fds.len() < rights.len());
        drop(fds);
        peer.set_nonblocking(true).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
