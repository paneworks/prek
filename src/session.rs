//! Active-seat registration from kernel socket credentials and logind.
//!
//! A UID or a client-supplied session name is insufficient. The agent must be
//! inside the verified compositor's actual login session. Shared user services
//! need a separate broker-authorized delegation protocol, not implemented here.
//! Registration proves membership and activity, never an unlocked desktop.
//! Callers must still obtain lock-provider evidence and recheck registration
//! before accepting context or dispatching an action.

use crate::{
    context::{Binding, SessionKind},
    hyprland::Client,
    transport::Consumer,
};
use nix::libc;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    mem::size_of,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    ptr::NonNull,
    time::{Duration, Instant},
};

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn checked(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::from_raw_os_error(-result))
    } else {
        Ok(result)
    }
}

#[derive(Debug)]
struct Process {
    pid: u32,
    start: u64,
    uid: u32,
    handle: OwnedFd,
}
impl Process {
    fn open(pid: u32, uid: u32) -> io::Result<Self> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(denied("invalid peer process"));
        }
        // SAFETY: pidfd_open takes scalar arguments and returns a new descriptor.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut process = Self {
            pid,
            uid,
            start: 0,
            handle: unsafe { OwnedFd::from_raw_fd(fd as i32) },
        };
        process.start = process.birth()?;
        process.check()?;
        Ok(process)
    }
    fn birth(&self) -> io::Result<u64> {
        fs::read_to_string(format!("/proc/{}/stat", self.pid))?
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("process birth is missing"))
    }
    fn check(&self) -> io::Result<()> {
        let mut poll = libc::pollfd {
            fd: self.handle.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: initialized descriptor and zero timeout; no blocking wait.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if ready != 0
            || fs::metadata(format!("/proc/{}", self.pid))?.uid() != self.uid
            || self.birth()? != self.start
        {
            return Err(denied("registered process exited or changed identity"));
        }
        Ok(())
    }
}

/// Credentials can only be obtained from an established local socket.
#[derive(Debug)]
pub struct Peer {
    process: Process,
    socket: OwnedFd,
}
impl Peer {
    pub fn from_socket(socket: &impl AsFd) -> io::Result<Self> {
        let mut kind: libc::c_int = 0;
        let mut kind_length = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: live borrowed descriptor and correctly sized integer output.
        if unsafe {
            libc::getsockopt(
                socket.as_fd().as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut libc::c_int).cast(),
                &mut kind_length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if kind_length as usize != size_of::<libc::c_int>()
            || !matches!(kind, libc::SOCK_STREAM | libc::SOCK_SEQPACKET)
        {
            return Err(denied(
                "registration requires a connected local stream or packet socket",
            ));
        }
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: live borrowed socket and correctly sized writable output.
        if unsafe {
            libc::getsockopt(
                socket.as_fd().as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != size_of::<libc::ucred>() || credentials.pid <= 0 {
            return Err(denied("local socket has no process credentials"));
        }
        let peer = Self {
            process: Process::open(credentials.pid as u32, credentials.uid)?,
            socket: socket.as_fd().try_clone_to_owned()?,
        };
        peer.check()?;
        Ok(peer)
    }
    pub fn pid(&self) -> u32 {
        self.process.pid
    }
    pub fn uid(&self) -> u32 {
        self.process.uid
    }
    fn check(&self) -> io::Result<()> {
        self.process.check()?;
        let mut poll = libc::pollfd {
            fd: self.socket.as_raw_fd(),
            events: libc::POLLRDHUP,
            revents: 0,
        };
        // SAFETY: retained socket and initialized descriptor, zero timeout.
        if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if poll.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(denied("registered agent disconnected"));
        }
        Ok(())
    }
}

struct Library {
    handle: NonNull<libc::c_void>,
    _file: File,
}
impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: successful dlopen reference, released exactly once after use.
        unsafe {
            libc::dlclose(self.handle.as_ptr());
        }
    }
}
type PidString = unsafe extern "C" fn(libc::pid_t, *mut *mut libc::c_char) -> i32;
type SessionString = unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_char) -> i32;
type SessionUid = unsafe extern "C" fn(*const libc::c_char, *mut libc::uid_t) -> i32;
type SessionBool = unsafe extern "C" fn(*const libc::c_char) -> i32;
type SeatActive =
    unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_char, *mut libc::uid_t) -> i32;

/// Loads an absolute, policy-selected system libsystemd without searching for
/// its path or taking over logind's device-controller role.
pub struct Logind {
    pid_session: PidString,
    session_seat: SessionString,
    session_class: SessionString,
    session_type: SessionString,
    session_state: SessionString,
    session_uid: SessionUid,
    session_active: SessionBool,
    session_remote: SessionBool,
    seat_active: SeatActive,
    _library: Library,
}
impl Logind {
    pub fn open(library: &Path) -> io::Result<Self> {
        if !library.is_absolute() {
            return Err(denied("logind library must be an absolute policy path"));
        }
        let canonical = fs::canonicalize(library)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(canonical)?;
        let stat = file.metadata()?;
        if !stat.is_file() || stat.uid() != 0 || stat.mode() & 0o022 != 0 {
            return Err(denied("logind library must be a protected root-owned file"));
        }
        let path = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
        // SAFETY: trusted, retained library file and a valid terminated path.
        let handle =
            NonNull::new(unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "system libsystemd could not be loaded",
                    )
                })?;
        let library = Library {
            handle,
            _file: file,
        };
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                // SAFETY: the policy library implements the documented sd-login
                // ABI. Every pointer is checked and the library remains owned.
                let pointer = unsafe {
                    libc::dlsym(
                        library.handle.as_ptr(),
                        concat!($name, "\0").as_ptr().cast(),
                    )
                };
                if pointer.is_null() {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        concat!("missing libsystemd entry point: ", $name),
                    ));
                }
                unsafe { std::mem::transmute::<*mut libc::c_void, $ty>(pointer) }
            }};
        }
        Ok(Self {
            pid_session: symbol!("sd_pid_get_session", PidString),
            session_seat: symbol!("sd_session_get_seat", SessionString),
            session_class: symbol!("sd_session_get_class", SessionString),
            session_type: symbol!("sd_session_get_type", SessionString),
            session_state: symbol!("sd_session_get_state", SessionString),
            session_uid: symbol!("sd_session_get_uid", SessionUid),
            session_active: symbol!("sd_session_is_active", SessionBool),
            session_remote: symbol!("sd_session_is_remote", SessionBool),
            seat_active: symbol!("sd_seat_get_active", SeatActive),
            _library: library,
        })
    }
    fn process_session(&self, process: &Process) -> io::Result<String> {
        process.check()?;
        allocated(|out| unsafe { (self.pid_session)(process.pid as libc::pid_t, out) })
    }
    fn facts(&self, peer: &Peer, compositor: &Process) -> io::Result<Facts> {
        peer.check()?;
        compositor.check()?;
        if peer.uid() != compositor.uid {
            return Err(denied("agent UID differs from compositor"));
        }
        let session = self.process_session(compositor)?;
        if self.process_session(&peer.process)? != session {
            return Err(denied("agent is outside the compositor's login session"));
        }
        let key = CString::new(session.as_str()).map_err(|_| invalid("invalid session ID"))?;
        let value =
            |function: SessionString| allocated(|out| unsafe { function(key.as_ptr(), out) });
        let seat = value(self.session_seat)?;
        let mut uid = 0;
        checked(unsafe { (self.session_uid)(key.as_ptr(), &mut uid) })?;
        let seat_key = CString::new(seat.as_str()).map_err(|_| invalid("invalid seat ID"))?;
        let mut active_uid = 0;
        let active_session = allocated(|out| unsafe {
            (self.seat_active)(seat_key.as_ptr(), out, &mut active_uid)
        })?;
        let facts = Facts {
            session,
            seat,
            uid,
            active_uid,
            active_session,
            kind: value(self.session_class)?,
            session_type: value(self.session_type)?,
            state: value(self.session_state)?,
            active: checked(unsafe { (self.session_active)(key.as_ptr()) })? == 1,
            remote: checked(unsafe { (self.session_remote)(key.as_ptr()) })? != 0,
        };
        facts.validate(compositor.uid)?;
        peer.check()?;
        compositor.check()?;
        Ok(facts)
    }
    pub fn register(&self, peer: Peer, client: &Client) -> io::Result<Registration> {
        let checked_at = Instant::now();
        client.check_binding()?;
        let Consumer::Hyprland { pid, start, .. } = client.consumer() else {
            unreachable!()
        };
        let compositor = Process::open(pid, client.uid())?;
        if compositor.start != start {
            return Err(denied("compositor process was replaced"));
        }
        let facts = self.facts(&peer, &compositor)?;
        // Independent reads bracket registration; no partial or inconsistent
        // seat switch grants a binding. Later changes revoke this lifetime.
        if self.facts(&peer, &compositor)? != facts {
            return Err(denied("logind session changed during registration"));
        }
        client.check_binding()?;
        let binding = Binding {
            seat: facts.seat.clone(),
            session: facts.session.clone(),
            uid: facts.uid,
            compositor_pid: pid,
            compositor_start: start,
            kind: if facts.kind == "greeter" {
                SessionKind::Greeter
            } else {
                SessionKind::User
            },
        };
        let registration = Registration {
            peer,
            compositor,
            facts,
            binding,
            consumer: client.consumer(),
            checked_at,
            revoked: false,
        };
        registration.binding()?;
        Ok(registration)
    }
}

fn allocated(call: impl FnOnce(*mut *mut libc::c_char) -> i32) -> io::Result<String> {
    let mut pointer = std::ptr::null_mut();
    let result = call(&mut pointer);
    let value = if result < 0 {
        Err(io::Error::from_raw_os_error(-result))
    } else if pointer.is_null() {
        Err(invalid("logind returned no value"))
    } else {
        // SAFETY: documented sd-login output is a malloc-owned C string.
        let length = unsafe { libc::strnlen(pointer, 257) };
        if length == 0 || length > 256 {
            Err(invalid("logind value exceeds bounds"))
        } else {
            // SAFETY: length is inside the terminated sd-login allocation.
            let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
            if bytes.iter().any(u8::is_ascii_control) {
                Err(invalid("invalid logind text"))
            } else {
                std::str::from_utf8(bytes)
                    .map(str::to_owned)
                    .map_err(|_| invalid("invalid logind text"))
            }
        }
    };
    // SAFETY: sd-login allocation, or NULL; free accepts both.
    unsafe {
        libc::free(pointer.cast());
    }
    value
}

#[derive(Debug, Eq, PartialEq)]
struct Facts {
    session: String,
    seat: String,
    uid: u32,
    active_uid: u32,
    active_session: String,
    kind: String,
    session_type: String,
    state: String,
    active: bool,
    remote: bool,
}
impl Facts {
    fn validate(&self, expected_uid: u32) -> io::Result<()> {
        if self.uid != expected_uid
            || self.active_uid != self.uid
            || self.active_session != self.session
            || !self.active
            || self.remote
            || self.state != "active"
            || !matches!(self.session_type.as_str(), "wayland" | "tty")
            || !matches!(
                self.kind.as_str(),
                "user" | "user-early" | "user-light" | "user-early-light" | "greeter"
            )
        {
            return Err(denied(
                "compositor has no permitted active local login session",
            ));
        }
        Ok(())
    }
}

/// Private membership proof; a failed recheck permanently revokes this token.
#[derive(Debug)]
pub struct Registration {
    peer: Peer,
    compositor: Process,
    facts: Facts,
    binding: Binding,
    consumer: Consumer,
    checked_at: Instant,
    revoked: bool,
}
impl Registration {
    pub fn binding(&self) -> io::Result<&Binding> {
        if self.revoked {
            Err(denied("session registration has been revoked"))
        } else if self.checked_at.elapsed() > Duration::from_millis(250) {
            Err(denied("session registration needs a fresh check"))
        } else {
            Ok(&self.binding)
        }
    }
    pub fn check(&mut self, logind: &Logind, client: &Client) -> io::Result<()> {
        if self.revoked {
            return Err(denied("session registration has been revoked"));
        }
        let checked_at = Instant::now();
        let result = (|| {
            client.check_binding()?;
            if client.uid() != self.binding.uid || client.consumer() != self.consumer {
                return Err(denied("registration compositor changed"));
            }
            if logind.facts(&self.peer, &self.compositor)? != self.facts {
                return Err(denied("active session changed"));
            }
            if logind.facts(&self.peer, &self.compositor)? != self.facts {
                return Err(denied("active session changed during recheck"));
            }
            client.check_binding()?;
            if checked_at.elapsed() > Duration::from_millis(250) {
                return Err(denied("session inspection exceeded its freshness budget"));
            }
            Ok(())
        })();
        if result.is_err() {
            self.revoked = true;
        } else {
            self.checked_at = checked_at;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    #[test]
    fn credentials_are_kernel_derived_and_process_handles_stay_live() {
        let (left, _right) = UnixStream::pair().unwrap();
        let peer = Peer::from_socket(&left).unwrap();
        assert_eq!(peer.pid(), std::process::id());
        assert_eq!(peer.uid(), unsafe { libc::geteuid() });
        peer.process.check().unwrap();
        assert!(Process::open(0, peer.uid()).is_err());
    }
    #[test]
    fn socket_disconnect_revokes_a_still_living_peer() {
        let (left, right) = UnixStream::pair().unwrap();
        let peer = Peer::from_socket(&left).unwrap();
        drop(right);
        peer.process.check().unwrap();
        assert!(peer.check().is_err());
        let (datagram, _other) = std::os::unix::net::UnixDatagram::pair().unwrap();
        assert!(Peer::from_socket(&datagram).is_err());
    }
    #[test]
    fn active_seat_uid_class_and_locality_are_all_required() {
        let valid = || Facts {
            session: "1".into(),
            seat: "seat0".into(),
            uid: 1000,
            active_uid: 1000,
            active_session: "1".into(),
            kind: "user".into(),
            session_type: "wayland".into(),
            state: "active".into(),
            active: true,
            remote: false,
        };
        valid().validate(1000).unwrap();
        for change in 0..7 {
            let mut facts = valid();
            match change {
                0 => facts.active_uid = 1001,
                1 => facts.active_session = "2".into(),
                2 => facts.kind = "manager".into(),
                3 => facts.session_type = "unspecified".into(),
                4 => facts.state = "closing".into(),
                5 => facts.active = false,
                _ => facts.remote = true,
            }
            assert!(facts.validate(1000).is_err());
        }
        assert!(valid().validate(1001).is_err());
    }
    #[test]
    fn library_path_requires_explicit_system_policy() {
        assert!(Logind::open(Path::new("libsystemd.so.0")).is_err());
        assert!(Logind::open(Path::new("/does-not-exist/libsystemd.so.0")).is_err());
    }
}
