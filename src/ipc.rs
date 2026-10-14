//! Nonblocking local sequenced packets, with credentials on every message.
//! File-descriptor transfer is refused; received descriptors are closed even
//! when ancillary data or the payload is invalid.

use crate::protocol::{self, MAX_PACKET, Packet};
use nix::libc;
use nix::time::{ClockId, clock_gettime};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io,
    mem::{offset_of, size_of},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Credentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}
pub fn monotonic() -> io::Result<Duration> {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(io::Error::from)?;
    Ok(Duration::new(time.tv_sec() as u64, time.tv_nsec() as u32))
}
fn credentials(fd: &impl AsFd) -> io::Result<Credentials> {
    let mut value: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: live socket and initialized output of the encoded size.
    if unsafe {
        libc::getsockopt(
            fd.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut value as *mut libc::ucred).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<libc::ucred>() || value.pid <= 0 {
        return Err(protocol::invalid("socket has no local peer credentials"));
    }
    Ok(Credentials {
        pid: value.pid as u32,
        uid: value.uid,
        gid: value.gid,
    })
}
fn option(fd: i32, name: i32, value: i32) -> io::Result<()> {
    // SAFETY: scalar option and correctly sized live integer pointer.
    if unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            (&value as *const i32).cast(),
            size_of::<i32>() as libc::socklen_t,
        )
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn socket() -> io::Result<OwnedFd> {
    // SAFETY: no pointers; success returns a new owned socket descriptor.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    option(raw, libc::SO_PASSCRED, 1)?;
    Ok(fd)
}
fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if !path.is_absolute() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(protocol::invalid(
            "local socket requires a bounded absolute path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    Ok((
        address,
        (offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t,
    ))
}

pub struct Listener {
    fd: OwnedFd,
}
impl Listener {
    /// The service owns directory locking, permissions and endpoint cleanup.
    /// Binding never removes an existing node or accepts an abstract address.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let fd = socket()?;
        let (address, length) = address(path)?;
        // SAFETY: initialized Unix address with exact terminated pathname length.
        if unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: live bound socket and bounded backlog.
        if unsafe { libc::listen(fd.as_raw_fd(), 16) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }
    pub fn accept(&self) -> io::Result<Option<Channel>> {
        // SAFETY: null optional address output; success creates a new descriptor.
        let raw = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Channel::from_fd(fd).map(Some)
    }
}
impl AsFd for Listener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// One owner per runtime directory. Only an exclusively held lease permits
/// removal of a stale, same-owner socket; ordinary files are preserved.
pub struct Endpoint {
    listener: Listener,
    directory: File,
    _lease: File,
    inode: u64,
}
impl Endpoint {
    pub fn bind(directory: &Path, public: bool) -> io::Result<Self> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory)?;
        let stat = directory.metadata()?;
        let owner = unsafe { libc::geteuid() };
        if stat.uid() != owner || stat.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control directory requires protected owner permissions",
            ));
        }
        let base = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(base.join("control.lock"))?;
        let stat = lease.metadata()?;
        if !stat.is_file() || stat.uid() != owner || stat.mode() & 0o077 != 0 || stat.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control lease is not a private owner file",
            ));
        }
        // SAFETY: retained private lease descriptor and nonblocking exclusive lock.
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let path = base.join("control.sock");
        match fs::symlink_metadata(&path) {
            Ok(stat) if stat.file_type().is_socket() && stat.uid() == owner => {
                fs::remove_file(&path)?
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "control endpoint is not an owned stale socket",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = Listener::bind(&path)?;
        let mut endpoint = Self {
            listener,
            directory,
            _lease: lease,
            inode: fs::symlink_metadata(&path)?.ino(),
        };
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if public { 0o666 } else { 0o600 }),
        )?;
        endpoint.inode = fs::symlink_metadata(&path)?.ino();
        Ok(endpoint)
    }
    pub fn accept(&self) -> io::Result<Option<Channel>> {
        self.listener.accept()
    }
}
impl AsFd for Endpoint {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        let path = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
            .join("control.sock");
        if fs::symlink_metadata(&path)
            .is_ok_and(|stat| stat.ino() == self.inode && stat.file_type().is_socket())
        {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
pub struct Channel {
    fd: OwnedFd,
    peer: Credentials,
    received: u64,
    sent: u64,
    failed: bool,
}
impl Channel {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let fd = socket()?;
        let (address, length) = address(path)?;
        // SAFETY: initialized address and correctly bounded path length.
        if unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Self::from_fd(fd)
    }
    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        option(fd.as_raw_fd(), libc::SO_PASSCRED, 1)?;
        let peer = credentials(&fd)?;
        Ok(Self {
            fd,
            peer,
            received: 0,
            sent: 0,
            failed: false,
        })
    }
    /// Private in-process transport for isolated/protocol tests and trusted
    /// supervisor/worker control; credentials still fence every packet.
    pub fn pair() -> io::Result<(Self, Self)> {
        let mut pair = [-1; 2];
        // SAFETY: initialized output for two newly owned descriptors.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let left = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let right = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        Ok((Self::from_fd(left)?, Self::from_fd(right)?))
    }
    pub fn peer(&self) -> Credentials {
        self.peer
    }
    pub(crate) fn into_fd(self) -> OwnedFd {
        self.fd
    }
    /// A socketpair records its creator as SO_PEERCRED. After the supervised
    /// fork, require every packet to come from the actual owned child instead.
    pub(crate) fn expect_worker(&mut self, pid: u32) {
        self.peer.pid = pid;
    }
    pub fn send<T: Serialize>(&mut self, message: &T, at: Duration) -> io::Result<bool> {
        if self.failed {
            return Err(protocol::invalid("IPC channel has failed"));
        }
        let next = self
            .sent
            .checked_add(1)
            .ok_or_else(|| protocol::invalid("packet sequence exhausted"))?;
        let packet = Packet {
            version: protocol::VERSION,
            sequence: next,
            at,
            message,
        };
        let bytes = protocol::encode(&packet)?;
        // SAFETY: live socket and bounded immutable bytes; MSG_NOSIGNAL prevents
        // SIGPIPE and MSG_DONTWAIT guarantees this cannot stall input forwarding.
        let written = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::Interrupted
            {
                return Ok(false);
            }
            self.failed = true;
            return Err(error);
        }
        if written as usize != bytes.len() {
            self.failed = true;
            return Err(protocol::invalid("partial sequenced-packet write"));
        }
        self.sent = next;
        Ok(true)
    }
    pub fn receive<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Option<Packet<T>>> {
        if self.failed {
            return Err(protocol::invalid("IPC channel has failed"));
        }
        let result = self.read_packet();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn read_packet<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Option<Packet<T>>> {
        let mut bytes = vec![0u8; MAX_PACKET];
        // usize storage supplies cmsghdr alignment on both target architectures.
        // Linux allows at most 253 descriptors in one SCM_RIGHTS payload.
        // Hold the full payload plus credentials, including ABI translation
        // under user-mode emulation, so no installed descriptor is hidden by
        // ancillary truncation. Every delivered descriptor is closed below.
        let mut control = [0usize; 256];
        let mut vector = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = size_of_val(&control);
        // SAFETY: initialized vectors/control buffer with exact writable sizes.
        let read = unsafe {
            libc::recvmsg(
                self.fd.as_raw_fd(),
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::Interrupted
            {
                return Ok(None);
            }
            return Err(error);
        }
        let mut sender = None;
        let mut forbidden = false;
        // SAFETY: CMSG traversal operates inside the kernel-populated bounded
        // aligned control buffer. All installed rights are closed before errors.
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                let item = &*header;
                let base = libc::CMSG_LEN(0) as usize;
                if item.cmsg_len < base {
                    forbidden = true;
                    break;
                }
                let length = item.cmsg_len - base;
                if item.cmsg_level == libc::SOL_SOCKET && item.cmsg_type == libc::SCM_RIGHTS {
                    for index in 0..length / size_of::<i32>() {
                        let fd = libc::CMSG_DATA(header)
                            .cast::<i32>()
                            .add(index)
                            .read_unaligned();
                        libc::close(fd);
                    }
                    forbidden = true;
                } else if item.cmsg_level == libc::SOL_SOCKET
                    && item.cmsg_type == libc::SCM_CREDENTIALS
                    && length == size_of::<libc::ucred>()
                    && sender.is_none()
                {
                    let value = libc::CMSG_DATA(header)
                        .cast::<libc::ucred>()
                        .read_unaligned();
                    if value.pid <= 0 {
                        forbidden = true;
                    }
                    sender = Some(Credentials {
                        pid: value.pid as u32,
                        uid: value.uid,
                        gid: value.gid,
                    });
                } else {
                    forbidden = true;
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
        }
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || forbidden {
            return Err(protocol::invalid(
                "truncated packet or forbidden ancillary data",
            ));
        }
        if sender != Some(self.peer) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "packet sender differs from registered socket peer",
            ));
        }
        bytes.truncate(read as usize);
        let packet: Packet<T> = protocol::decode(&bytes)?;
        let next = self
            .received
            .checked_add(1)
            .ok_or_else(|| protocol::invalid("packet sequence exhausted"))?;
        packet.validate_header(next, monotonic()?)?;
        self.received = next;
        Ok(Some(packet))
    }
}
impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
