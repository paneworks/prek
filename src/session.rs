//! Active-seat registration from kernel socket credentials and logind.
//!
//! A UID or a client-supplied session name is insufficient. The agent must be
//! inside the verified compositor's actual login session. A compositor that a
//! user manager runs outside any session, such as under uwsm, is bound to the
//! user's display session only while logind records that exact process as the
//! session's controller, and its agent must share the compositor's cgroup.
//! Registration proves membership and activity, never an unlocked desktop.
//! Callers must still read logind's lock hint and recheck registration before
//! accepting context or dispatching an action.

use crate::{
    compositor::Compositor,
    context::{Binding, SessionKind},
    transport::Consumer,
};
use nix::libc;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    mem::size_of,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
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
    fn exited(&self) -> io::Result<bool> {
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
        Ok(ready != 0)
    }
    /// Fails unless the pidfd is live before and after the owner is read.
    fn check(&self) -> io::Result<()> {
        if self.exited()?
            || fs::metadata(format!("/proc/{}", self.pid))?.uid() != self.uid
            || self.exited()?
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

/// Logind's state directories; every session, seat and user fact is read from them.
const STATE: [&str; 3] = [
    "/run/systemd/sessions",
    "/run/systemd/seats",
    "/run/systemd/users",
];

/// Count that advances whenever the watched directories may have changed, and
/// on every read while they cannot be watched.
struct Generation {
    directories: Vec<PathBuf>,
    changes: Option<Changes>,
    count: u64,
}
impl Generation {
    fn new(directories: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            directories: directories.into_iter().map(Into::into).collect(),
            changes: None,
            count: 0,
        }
    }
    fn current(&mut self) -> u64 {
        let unchanged = match self.changes.as_ref().map(Changes::drain) {
            Some(Ok(changed)) => !changed,
            Some(Err(_)) => {
                self.changes = None;
                false
            }
            None => {
                self.changes = Changes::open(&self.directories).ok();
                false
            }
        };
        if !unchanged {
            self.count += 1;
        }
        self.count
    }
}

/// Inotify watch on directories whose entries may change.
pub(crate) struct Changes(OwnedFd);
impl AsFd for Changes {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Changes {
    pub(crate) fn open(directories: &[PathBuf]) -> io::Result<Self> {
        // SAFETY: inotify_init1 takes scalar flags and returns a new descriptor.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let changes = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MODIFY
            | libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_ONLYDIR
            | libc::IN_DONT_FOLLOW;
        for directory in directories {
            let path = CString::new(directory.as_os_str().as_bytes())
                .map_err(|_| invalid("invalid state directory"))?;
            // SAFETY: live inotify descriptor and terminated path.
            if unsafe { libc::inotify_add_watch(changes.0.as_raw_fd(), path.as_ptr(), mask) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(changes)
    }
    /// Drains queued events; whether any arrived. A lost watch or an overflowed
    /// queue is an error, after which the watch must be reopened.
    pub(crate) fn drain(&self) -> io::Result<bool> {
        const HEADER: usize = size_of::<libc::inotify_event>();
        let mut buffer = [0u8; 4096];
        let mut changed = false;
        loop {
            // SAFETY: live descriptor and writable buffer of the given length.
            let read =
                unsafe { libc::read(self.0.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EAGAIN) {
                    return Ok(changed);
                }
                return Err(error);
            }
            let read = read as usize;
            if read == 0 {
                return Ok(changed);
            }
            changed = true;
            let mut offset = 0;
            while offset + HEADER <= read {
                // SAFETY: a complete event header lies inside the bytes read.
                let event = unsafe {
                    std::ptr::read_unaligned(
                        buffer[offset..].as_ptr().cast::<libc::inotify_event>(),
                    )
                };
                if event.mask
                    & (libc::IN_IGNORED
                        | libc::IN_Q_OVERFLOW
                        | libc::IN_DELETE_SELF
                        | libc::IN_MOVE_SELF
                        | libc::IN_UNMOUNT)
                    != 0
                {
                    return Err(invalid("directory watch was lost"));
                }
                offset += HEADER + event.len as usize;
            }
        }
    }
}

pub(crate) struct Library {
    handle: NonNull<libc::c_void>,
    _file: File,
}
impl Library {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = Self::pin(path)?;
        if file.metadata()?.uid() != 0 {
            return Err(denied("logind library must be a protected root-owned file"));
        }
        Self::load(file)
    }
    #[cfg(test)]
    pub(crate) fn open_fixture(path: &Path) -> io::Result<Self> {
        Self::load(Self::pin(path)?)
    }
    fn pin(path: &Path) -> io::Result<File> {
        if !path.is_absolute() {
            return Err(denied("logind library must be an absolute policy path"));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(fs::canonicalize(path)?)?;
        let stat = file.metadata()?;
        if !stat.is_file() || stat.mode() & 0o022 != 0 {
            return Err(denied("logind library must be a protected root-owned file"));
        }
        Ok(file)
    }
    fn load(file: File) -> io::Result<Self> {
        let path = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
        // SAFETY: the selected library file and descriptor path remain retained.
        let handle =
            NonNull::new(unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "system libsystemd could not be loaded",
                    )
                })?;
        Ok(Self {
            handle,
            _file: file,
        })
    }
    pub(crate) fn symbol(&self, name: &std::ffi::CStr) -> io::Result<*mut libc::c_void> {
        // SAFETY: the live retained library and terminated symbol name remain owned.
        let pointer = unsafe { libc::dlsym(self.handle.as_ptr(), name.as_ptr()) };
        if pointer.is_null() {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("missing libsystemd entry point: {}", name.to_string_lossy()),
            ))
        } else {
            Ok(pointer)
        }
    }
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
type UidString = unsafe extern "C" fn(libc::uid_t, *mut *mut libc::c_char) -> i32;
type SessionString = unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_char) -> i32;
type SessionUid = unsafe extern "C" fn(*const libc::c_char, *mut libc::uid_t) -> i32;
type SessionBool = unsafe extern "C" fn(*const libc::c_char) -> i32;
type SeatActive =
    unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_char, *mut libc::uid_t) -> i32;

/// Loads an absolute, policy-selected system libsystemd without searching for
/// its path or taking over logind's device-controller role.
pub struct Logind {
    pid_session: PidString,
    pid_cgroup: PidString,
    uid_display: UidString,
    session_seat: SessionString,
    session_class: SessionString,
    session_type: SessionString,
    session_state: SessionString,
    session_uid: SessionUid,
    session_active: SessionBool,
    session_remote: SessionBool,
    seat_active: SeatActive,
    library: Library,
    lock_hints: Option<crate::sleep_watch::LockHints>,
    lock_epoch: u64,
    lock_seen: Option<Seen>,
    generation: Generation,
}
/// Lock-hint connection and the session property changes it had counted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Seen {
    epoch: u64,
    changes: u64,
}
impl Logind {
    pub fn open(library: &Path) -> io::Result<Self> {
        let library = Library::open(library)?;
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                // SAFETY: the policy library implements the documented sd-login
                // ABI. Every pointer is checked and the library remains owned.
                let name =
                    std::ffi::CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).unwrap();
                let pointer = library.symbol(name)?;
                unsafe { std::mem::transmute::<*mut libc::c_void, $ty>(pointer) }
            }};
        }
        Ok(Self {
            pid_session: symbol!("sd_pid_get_session", PidString),
            pid_cgroup: symbol!("sd_pid_get_cgroup", PidString),
            uid_display: symbol!("sd_uid_get_display", UidString),
            session_seat: symbol!("sd_session_get_seat", SessionString),
            session_class: symbol!("sd_session_get_class", SessionString),
            session_type: symbol!("sd_session_get_type", SessionString),
            session_state: symbol!("sd_session_get_state", SessionString),
            session_uid: symbol!("sd_session_get_uid", SessionUid),
            session_active: symbol!("sd_session_is_active", SessionBool),
            session_remote: symbol!("sd_session_is_remote", SessionBool),
            seat_active: symbol!("sd_seat_get_active", SeatActive),
            library,
            lock_hints: None,
            lock_epoch: 0,
            lock_seen: None,
            generation: Generation::new(STATE),
        })
    }
    /// Advances whenever logind's state may have changed.
    fn generation(&mut self) -> u64 {
        self.generation.current()
    }
    /// Drains logind's change notifications; whether a watched state directory
    /// or a session property changed, or a watch was lost, since the last call.
    pub fn changed(&mut self) -> bool {
        let watched = self.generation.changes.is_some();
        let before = self.generation.count;
        let advanced = self.generation.current() != before;
        let lock = match self.lock_hints.as_mut().map(|bus| bus.changes()) {
            Some(Ok(changes)) => Some(Seen {
                epoch: self.lock_epoch,
                changes,
            }),
            Some(Err(_)) => {
                self.lock_hints = None;
                None
            }
            None => None,
        };
        let lock_changed = lock != self.lock_seen;
        self.lock_seen = lock;
        watched && advanced || lock_changed
    }
    /// Descriptors that become ready when logind's state may have changed,
    /// with the poll events each needs.
    pub fn poll_sources(&self) -> Vec<(BorrowedFd<'_>, i16)> {
        let mut sources: Vec<_> = self
            .generation
            .changes
            .iter()
            .map(|changes| (changes.0.as_fd(), libc::POLLIN))
            .collect();
        if let Some(bus) = &self.lock_hints {
            sources.push((bus.as_fd(), bus.events().unwrap_or(libc::POLLIN)));
        }
        sources
    }
    fn process_session(&self, process: &Process) -> io::Result<String> {
        process.check()?;
        allocated(|out| unsafe { (self.pid_session)(process.pid as libc::pid_t, out) })
    }
    fn process_cgroup(&self, process: &Process) -> io::Result<String> {
        process.check()?;
        allocated(|out| unsafe { (self.pid_cgroup)(process.pid as libc::pid_t, out) })
    }
    /// Display session and cgroup of a compositor its user manager runs outside
    /// any session; the controller is resolved to a process at registration.
    fn controlled_session(
        &self,
        peer: &Peer,
        compositor: &Process,
    ) -> io::Result<(String, Option<String>, Option<String>)> {
        let cgroup = self.process_cgroup(compositor)?;
        if !user_manager_cgroup(&cgroup, compositor.uid) {
            return Err(denied(
                "compositor is outside both a login session and its user manager",
            ));
        }
        let agent = self.process_session(&peer.process);
        if !matches!(&agent, Err(error) if error.raw_os_error() == Some(libc::ENODATA))
            || self.process_cgroup(&peer.process)? != cgroup
        {
            return Err(denied("agent is outside the compositor's systemd unit"));
        }
        let session =
            allocated(|out| unsafe { (self.uid_display)(compositor.uid as libc::uid_t, out) })?;
        let controller = controller(&session)?;
        Ok((session, Some(controller), Some(cgroup)))
    }
    fn facts(&self, peer: &Peer, compositor: &Process) -> io::Result<Facts> {
        peer.check()?;
        compositor.check()?;
        if peer.uid() != compositor.uid {
            return Err(denied("agent UID differs from compositor"));
        }
        let (session, controller, cgroup) = match self.process_session(compositor) {
            Err(error) if error.raw_os_error() == Some(libc::ENODATA) => {
                self.controlled_session(peer, compositor)?
            }
            session => {
                let session = session?;
                if self.process_session(&peer.process)? != session {
                    return Err(denied("agent is outside the compositor's login session"));
                }
                (session, None, None)
            }
        };
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
            controller,
            cgroup,
        };
        facts.validate(compositor.uid)?;
        peer.check()?;
        compositor.check()?;
        Ok(facts)
    }
    /// Lock hint of `session` through one retained bus connection, queried again
    /// only after logind announced a session property change; any failure
    /// drops the connection, the cached object path and the cached hint.
    fn locked(
        &mut self,
        session: &str,
        path: &mut Option<CString>,
        cached: &mut Option<(Seen, bool)>,
    ) -> io::Result<bool> {
        let result = (|| {
            if self.lock_hints.is_none() {
                self.lock_epoch = self
                    .lock_epoch
                    .checked_add(1)
                    .ok_or_else(|| invalid("lock-hint connections exhausted"))?;
                self.lock_hints = Some(crate::sleep_watch::LockHints::open(&self.library)?);
            }
            let bus = self.lock_hints.as_mut().unwrap();
            let mut seen = Seen {
                epoch: self.lock_epoch,
                changes: bus.changes()?,
            };
            if let Some((at, locked)) = *cached
                && at == seen
            {
                return Ok(locked);
            }
            if path.is_none() {
                let id = CString::new(session).map_err(|_| invalid("invalid session ID"))?;
                *path = Some(bus.session_path(&id)?);
            }
            // Drains signals read along with each reply and reads the hint again
            // when one arrived.
            for _ in 0..4 {
                let locked = bus.locked(path.as_ref().unwrap())?;
                let changes = bus.changes()?;
                if changes == seen.changes {
                    *cached = Some((seen, locked));
                    return Ok(locked);
                }
                seen.changes = changes;
            }
            Err(invalid("logind lock hint kept changing"))
        })();
        if result.is_err() {
            self.lock_hints = None;
            *path = None;
            *cached = None;
        }
        result
    }
    pub fn register(&mut self, peer: Peer, client: &Compositor) -> io::Result<Registration> {
        let checked_at = Instant::now();
        client.check_binding()?;
        let (pid, start) = (client.pid(), client.start());
        let compositor = Process::open(pid, client.uid())?;
        if compositor.start != start {
            return Err(denied("compositor process was replaced"));
        }
        let generation = self.generation();
        let cgroups = cgroups(&peer, &compositor)?;
        let facts = self.facts(&peer, &compositor)?;
        // Unique bus names are never reused, so rechecks compare only the name.
        if let Some(name) = &facts.controller {
            let name = CString::new(name.as_str()).map_err(|_| invalid("invalid bus name"))?;
            if crate::sleep_watch::connection_pid(&self.library, &name)? != compositor.pid {
                return Err(denied(
                    "the display session's controller is not the compositor",
                ));
            }
        }
        // Independent reads bracket registration; no partial or inconsistent
        // seat switch grants a binding. Later changes revoke this lifetime.
        if self.facts(&peer, &compositor)? != facts {
            return Err(denied("logind session changed during registration"));
        }
        if self::cgroups(&peer, &compositor)? != cgroups {
            return Err(denied(
                "a registered process moved cgroups during registration",
            ));
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
            generation,
            cgroups,
            session_path: None,
            lock: None,
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

/// Raw cgroup membership of the agent and the compositor, each read while its
/// process stays live.
fn cgroups(peer: &Peer, compositor: &Process) -> io::Result<[String; 2]> {
    let read = |process: &Process| -> io::Result<String> {
        process.check()?;
        let membership = fs::read_to_string(format!("/proc/{}/cgroup", process.pid))?;
        process.check()?;
        Ok(membership)
    };
    Ok([read(&peer.process)?, read(compositor)?])
}

fn user_manager_cgroup(cgroup: &str, uid: u32) -> bool {
    cgroup
        .strip_prefix(&format!("/user.slice/user-{uid}.slice/user@{uid}.service/"))
        .is_some_and(|unit| !unit.is_empty())
}

/// Controller bus name of `session` from logind's root-owned session state.
fn controller(session: &str) -> io::Result<String> {
    const LIMIT: u64 = 16 * 1024;
    if session.is_empty()
        || session.len() > 64
        || !session.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(invalid("invalid session ID"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(format!("/run/systemd/sessions/{session}"))?;
    let stat = file.metadata()?;
    if !stat.is_file() || stat.uid() != 0 || stat.mode() & 0o022 != 0 || stat.len() > LIMIT {
        return Err(denied("logind session state is not protected"));
    }
    let mut state = String::new();
    file.take(LIMIT).read_to_string(&mut state)?;
    controller_name(&state)
}
fn controller_name(state: &str) -> io::Result<String> {
    let mut names = state
        .lines()
        .filter_map(|line| line.strip_prefix("CONTROLLER="));
    let unique = |name: &str| {
        name.strip_prefix(':')
            .and_then(|name| name.split_once('.'))
            .is_some_and(|(left, right)| {
                [left, right].iter().all(|part| {
                    (1..=10).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit())
                })
            })
    };
    match (names.next(), names.next()) {
        (Some(name), None) if unique(name) => Ok(name.to_owned()),
        (None, _) => Err(denied(
            "the compositor does not control the display session",
        )),
        _ => Err(invalid("logind session controller is malformed")),
    }
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
    controller: Option<String>,
    cgroup: Option<String>,
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

/// How long a passed registration check covers the further checks of one
/// request burst, such as an agent's context renewal.
const RECHECK: Duration = Duration::from_millis(100);

/// Private membership proof; a failed recheck permanently revokes this token.
#[derive(Debug)]
pub struct Registration {
    peer: Peer,
    compositor: Process,
    facts: Facts,
    binding: Binding,
    consumer: Consumer,
    checked_at: Instant,
    generation: u64,
    cgroups: [String; 2],
    session_path: Option<CString>,
    lock: Option<(Seen, bool)>,
    revoked: bool,
}
impl Registration {
    /// Logind's lock hint for the bound session.
    pub fn locked(&mut self, logind: &mut Logind) -> io::Result<bool> {
        logind.locked(&self.facts.session, &mut self.session_path, &mut self.lock)
    }
    /// Pidfds of the agent and the compositor; each becomes readable when its
    /// process exits.
    pub fn poll_sources(&self) -> [BorrowedFd<'_>; 2] {
        [
            self.peer.process.handle.as_fd(),
            self.compositor.handle.as_fd(),
        ]
    }
    /// Whether the agent or the compositor has exited.
    pub fn exited(&self) -> bool {
        !matches!(
            (self.peer.process.exited(), self.compositor.exited()),
            (Ok(false), Ok(false))
        )
    }
    pub fn binding(&self) -> io::Result<&Binding> {
        if self.revoked {
            Err(denied("session registration has been revoked"))
        } else if self.checked_at.elapsed() > Duration::from_millis(250) {
            Err(denied("session registration needs a fresh check"))
        } else {
            Ok(&self.binding)
        }
    }
    pub fn check(&mut self, logind: &mut Logind, client: &Compositor) -> io::Result<()> {
        if self.revoked {
            return Err(denied("session registration has been revoked"));
        }
        let generation = logind.generation();
        // A check that passed within RECHECK stands while both processes live
        // and logind's state is unchanged.
        if self.checked_at.elapsed() < RECHECK && generation == self.generation && !self.exited() {
            return Ok(());
        }
        let checked_at = Instant::now();
        let result = (|| {
            client.check_binding()?;
            if client.uid() != self.binding.uid || client.consumer() != self.consumer {
                return Err(denied("registration compositor changed"));
            }
            // Logind's facts are reread only after its state directories changed.
            self.peer.check()?;
            if generation != self.generation {
                if logind.facts(&self.peer, &self.compositor)? != self.facts {
                    return Err(denied("active session changed"));
                }
                if logind.facts(&self.peer, &self.compositor)? != self.facts {
                    return Err(denied("active session changed during recheck"));
                }
            }
            if cgroups(&self.peer, &self.compositor)? != self.cgroups {
                return Err(denied("a registered process moved cgroups"));
            }
            self.peer.check()?;
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
            self.generation = generation;
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod native_probe;

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
        let deadline = Instant::now() + Duration::from_secs(5);
        while peer.check().is_ok() {
            assert!(
                Instant::now() < deadline,
                "the closed peer was never reported"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
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
            controller: None,
            cgroup: None,
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
    fn a_sessionless_compositor_needs_its_user_manager_and_exactly_one_controller() {
        let unit = "/user.slice/user-1000.slice/user@1000.service/session.slice/wayland-wm@compositor.desktop.service";
        assert!(user_manager_cgroup(unit, 1000));
        for cgroup in [
            "/user.slice/user-1000.slice/user@1000.service/",
            "/user.slice/user-1000.slice/session-2.scope",
            "/system.slice/display-manager.service",
        ] {
            assert!(!user_manager_cgroup(cgroup, 1000), "{cgroup}");
        }
        assert!(!user_manager_cgroup(unit, 1001));
        let state =
            "# This is private data. Do not parse.\nUID=1000\nCONTROLLER=:1.20096\nDEVICES=13:66\n";
        assert_eq!(controller_name(state).unwrap(), ":1.20096");
        assert_eq!(
            controller_name("UID=1000\n").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        for state in [
            "CONTROLLER=:1.2\nCONTROLLER=:1.3\n",
            "CONTROLLER=org.example\n",
            "CONTROLLER=:1.\n",
            "CONTROLLER=:1.2 \n",
        ] {
            assert_eq!(
                controller_name(state).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert!(controller("../5157").is_err());
    }
    #[test]
    fn library_path_requires_explicit_system_policy() {
        assert!(Logind::open(Path::new("libsystemd.so.0")).is_err());
        assert!(Logind::open(Path::new("/does-not-exist/libsystemd.so.0")).is_err());
    }
    #[test]
    #[ignore = "needs the system bus, a logind session and PREK_LOGIND_LIBRARY"]
    fn a_live_lock_hint_is_queried_once_until_logind_announces_a_change() {
        let library = std::env::var_os("PREK_LOGIND_LIBRARY").unwrap();
        let mut logind = Logind::open(Path::new(&library)).unwrap();
        let session = allocated(|out| unsafe { (logind.pid_session)(0, out) }).unwrap();
        let (mut path, mut cached) = (None, None);
        let locked = logind.locked(&session, &mut path, &mut cached).unwrap();
        let seen = cached.unwrap().0;
        assert!(
            logind.changed(),
            "opening the bus connection is reported once"
        );
        assert!(!logind.changed());
        assert!(!logind.poll_sources().is_empty());
        for _ in 0..20 {
            assert_eq!(
                logind.locked(&session, &mut path, &mut cached).unwrap(),
                locked
            );
        }
        assert_eq!(
            cached.unwrap().0,
            seen,
            "an unchanged session is not queried again"
        );
        println!("session {session} locked={locked} {seen:?}");
    }
    #[test]
    fn the_state_generation_advances_only_on_changes_and_reopens_a_lost_watch() {
        let root = std::env::temp_dir().join(format!("prek-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let [sessions, seats] = ["sessions", "seats"].map(|name| root.join(name));
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&seats).unwrap();
        let mut generation = Generation::new([&sessions, &seats]);
        let first = generation.current();
        assert_eq!(generation.current(), first);
        fs::write(sessions.join(".c3.tmp"), b"STATE=active\n").unwrap();
        fs::rename(sessions.join(".c3.tmp"), sessions.join("c3")).unwrap();
        let renamed = generation.current();
        assert!(renamed > first);
        assert_eq!(generation.current(), renamed);
        fs::write(seats.join("seat0"), b"ACTIVE=c3\n").unwrap();
        let written = generation.current();
        assert!(written > renamed);
        fs::remove_dir_all(&seats).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while generation.changes.is_some() {
            assert!(
                Instant::now() < deadline,
                "the removed directory was never reported"
            );
            generation.current();
            std::thread::sleep(Duration::from_millis(1));
        }
        let lost = generation.count;
        assert!(lost > written);
        let reopened = generation.current();
        assert!(
            reopened > lost,
            "a missing directory keeps every read fresh"
        );
        assert!(generation.current() > reopened);
        fs::create_dir(&seats).unwrap();
        let watched = generation.current();
        assert_eq!(generation.current(), watched);
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn an_exited_process_fails_its_check() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let process = Process::open(child.id(), unsafe { libc::geteuid() }).unwrap();
        process.check().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            process.check().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
