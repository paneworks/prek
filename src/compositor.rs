//! The compositor process that serves one user's Wayland display socket.
//!
//! The binding is built from kernel facts only: the private runtime directory,
//! the display socket inode, `SO_PEERCRED` of a fresh connection, the process
//! start time and the executable inode. No Wayland request is ever sent.

use crate::transport::{Consumer, ConsumerEvidence, Descriptor};
use nix::libc;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    mem::{offset_of, size_of},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const CONNECT_TIME: Duration = Duration::from_millis(200);
const MAX_DISPLAY: usize = 160;

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn malformed(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Maps a vanished process to a lost binding.
fn lost(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) {
        denied("compositor process exited")
    } else {
        error
    }
}

fn beneath(directory: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/{name}", directory.as_raw_fd()))
}

/// The compositor process listening on one user's Wayland display socket.
#[derive(Debug)]
pub struct Compositor {
    runtime: File,
    socket_inode: u64,
    uid: u32,
    pid: u32,
    start: u64,
    executable: (u64, u64),
    display: String,
}

impl Compositor {
    /// Bind the process that listens on `display` inside `runtime`.
    ///
    /// `runtime` is the user's absolute `XDG_RUNTIME_DIR`, a directory owned by
    /// `uid` with no group or other access. `display` is a `WAYLAND_DISPLAY`
    /// socket name of 1..=160 bytes of `[A-Za-z0-9_.-]`, neither `.` nor `..`.
    /// `executable` is the trusted compositor ELF from root policy; the peer's
    /// `/proc/<pid>/exe` must be that same file. The probing connection is
    /// closed before this returns.
    pub fn open(runtime: &Path, display: &str, uid: u32, executable: &Path) -> io::Result<Self> {
        if !runtime.is_absolute() {
            return Err(malformed("runtime directory must be absolute"));
        }
        validate(display)?;
        let directory = private(runtime, uid)?;
        let expected = fs::metadata(executable)?;
        let expected = (expected.dev(), expected.ino());
        let (socket_inode, stream, pid) = endpoint(&directory, display, uid)?;
        let process = Process::open(pid)?;
        if process.executable()? != expected {
            return Err(denied("compositor executable differs from policy"));
        }
        let start = process.birth()?;
        drop(stream);
        Ok(Self {
            runtime: directory,
            socket_inode,
            uid,
            pid,
            start,
            executable: expected,
            display: display.into(),
        })
    }

    /// Require the same socket inode, process start time and executable
    /// without connecting; any difference is `PermissionDenied`.
    pub fn check_binding(&self) -> io::Result<()> {
        if socket_inode(&self.runtime, &self.display, self.uid)? != self.socket_inode {
            return Err(denied("compositor binding changed"));
        }
        let process = self.process()?;
        if process.executable().map_err(lost)? != self.executable {
            return Err(denied("compositor executable changed"));
        }
        Ok(())
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn start(&self) -> u64 {
        self.start
    }
    pub fn display(&self) -> &str {
        &self.display
    }
    pub fn consumer(&self) -> Consumer {
        Consumer::Compositor {
            pid: self.pid,
            start: self.start,
            display: self.display.clone(),
        }
    }

    /// Evidence that this compositor holds an event node udev published for
    /// `descriptor`; `None` while no such node is open in the compositor.
    pub fn device_evidence(
        &self,
        descriptor: &Descriptor,
        at: Duration,
    ) -> io::Result<Option<ConsumerEvidence>> {
        descriptor.validate().map_err(invalid)?;
        for node in crate::linux::published(&descriptor.marker)? {
            match self.holds_device(&node) {
                Ok(true) => {
                    return Ok(Some(ConsumerEvidence {
                        consumer: self.consumer(),
                        marker: descriptor.marker.clone(),
                        name: descriptor.name.clone(),
                        class: descriptor.class,
                        seat: descriptor.source.seat.clone(),
                        at,
                    }));
                }
                Ok(false) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// Whether the compositor process currently has a descriptor open on the
    /// character device `node`, matched by device number. Descriptors that
    /// close during the scan or are not character devices are ignored. A
    /// process start time that differs before or after the scan is
    /// `PermissionDenied`.
    pub fn holds_device(&self, node: &Path) -> io::Result<bool> {
        let target = fs::metadata(node)?;
        if !target.file_type().is_char_device() {
            return Err(malformed("device node must be a character device"));
        }
        let process = self.process()?;
        let held = process.holds(target.rdev());
        self.same(&process)?;
        held
    }

    fn process(&self) -> io::Result<Process> {
        let process = Process::open(self.pid).map_err(lost)?;
        self.same(&process)?;
        Ok(process)
    }
    fn same(&self, process: &Process) -> io::Result<()> {
        if process.birth().map_err(lost)? != self.start {
            return Err(denied("compositor process changed"));
        }
        Ok(())
    }
}

fn validate(display: &str) -> io::Result<()> {
    if display.is_empty()
        || display.len() > MAX_DISPLAY
        || matches!(display, "." | "..")
        || !display
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(malformed("invalid Wayland display name"));
    }
    Ok(())
}

fn private(path: &Path, uid: u32) -> io::Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let stat = directory.metadata()?;
    if stat.uid() != uid || stat.mode() & 0o077 != 0 {
        return Err(denied("runtime directory must be private to its user"));
    }
    Ok(directory)
}

/// Inode of the display socket, which must be a socket owned by `uid`.
fn socket_inode(directory: &File, display: &str, uid: u32) -> io::Result<u64> {
    let stat = fs::symlink_metadata(beneath(directory, display))?;
    if !stat.file_type().is_socket() || stat.uid() != uid {
        return Err(denied(
            "display endpoint must be a socket owned by its user",
        ));
    }
    Ok(stat.ino())
}

/// Socket inode, live connection and peer PID of a display socket owned by `uid`.
fn endpoint(directory: &File, display: &str, uid: u32) -> io::Result<(u64, UnixStream, u32)> {
    let path = beneath(directory, display);
    let inode = socket_inode(directory, display, uid)?;
    let stream = crate::fsuid::with_fsuid(uid, || connect(&path, Instant::now() + CONNECT_TIME))?;
    let peer = credentials(&stream)?;
    if socket_inode(directory, display, uid)? != inode {
        return Err(denied("display socket changed while connecting"));
    }
    if peer.uid != uid || peer.pid <= 0 {
        return Err(denied("display socket peer does not match its user"));
    }
    Ok((inode, stream, peer.pid as u32))
}

/// A `/proc/<pid>` handle that stays bound to one process instance.
struct Process(File);
impl Process {
    fn open(pid: u32) -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(format!("/proc/{pid}"))
            .map(Self)
    }
    /// Start time in clock ticks: field 22 of `stat`, counted after the last `)`.
    fn birth(&self) -> io::Result<u64> {
        fs::read_to_string(beneath(&self.0, "stat"))?
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("process start time is missing"))
    }
    fn executable(&self) -> io::Result<(u64, u64)> {
        let stat = fs::metadata(beneath(&self.0, "exe"))?;
        Ok((stat.dev(), stat.ino()))
    }
    fn holds(&self, rdev: u64) -> io::Result<bool> {
        let descriptors = beneath(&self.0, "fd");
        for entry in fs::read_dir(&descriptors)? {
            match character_device(&descriptors.join(entry?.file_name())) {
                Ok(Some(device)) if device == rdev => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
}

/// Device number of a descriptor target that is a character device, read from
/// cached attributes so no filesystem is asked to revalidate.
fn character_device(path: &Path) -> io::Result<Option<u64>> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| invalid("descriptor path contains NUL"))?;
    // SAFETY: statx is plain data; all-zero is a valid initial value.
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated path and a writable statx buffer for this call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_STATX_DONT_SYNC,
            libc::STATX_TYPE,
            &mut stat as *mut libc::statx,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::mode_t::from(stat.stx_mode) & libc::S_IFMT != libc::S_IFCHR {
        return Ok(None);
    }
    Ok(Some(libc::makedev(
        stat.stx_rdev_major,
        stat.stx_rdev_minor,
    )))
}

fn wait(fd: i32, events: i16, until: Instant) -> io::Result<()> {
    loop {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut poll = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: one initialized poll descriptor with a bounded timeout.
        let result = unsafe {
            libc::poll(
                &mut poll,
                1,
                remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if poll.revents & libc::POLLNVAL != 0 {
            return Err(invalid("socket descriptor is invalid"));
        }
        return Ok(());
    }
}

fn connect(path: &Path, until: Instant) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain data; all-zero is a valid empty address.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(malformed("display socket path exceeds bounds"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length = (offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    loop {
        if Instant::now() >= until {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // SAFETY: scalar arguments; success returns a new descriptor.
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: initialized address with its exact length.
        if unsafe { libc::connect(raw, (&address as *const libc::sockaddr_un).cast(), length) } == 0
        {
            return Ok(UnixStream::from(fd));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EAGAIN) => {
                // A full backlog leaves this socket unconnected; retry with a fresh one.
                drop(fd);
                std::thread::sleep(
                    until
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                );
            }
            Some(libc::EINTR) => {}
            Some(libc::EINPROGRESS) => {
                wait(raw, libc::POLLOUT, until)?;
                let mut status: libc::c_int = 0;
                let mut status_length = size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: live socket and initialized integer status output.
                if unsafe {
                    libc::getsockopt(
                        raw,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut status as *mut libc::c_int).cast(),
                        &mut status_length,
                    )
                } < 0
                {
                    return Err(io::Error::last_os_error());
                }
                if status != 0 {
                    return Err(io::Error::from_raw_os_error(status));
                }
                let stream = UnixStream::from(fd);
                stream.peer_addr()?;
                return Ok(stream);
            }
            _ => return Err(error),
        }
    }
}

fn credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    // SAFETY: ucred is plain data; all-zero is a valid initial value.
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: initialized credential output for the live connected socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<libc::ucred>() {
        return Err(invalid("invalid display socket peer credentials"));
    }
    Ok(peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::{fs::PermissionsExt, net::UnixListener},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::{self, JoinHandle},
    };

    const DISPLAY: &str = "wayland-test";

    fn uid() -> u32 {
        // SAFETY: geteuid takes no arguments and cannot fail.
        unsafe { libc::geteuid() }
    }
    fn executable() -> PathBuf {
        PathBuf::from(format!("/proc/{}/exe", std::process::id()))
    }

    struct Server {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Server {
        fn bind(path: &Path) -> Self {
            let listener = UnixListener::bind(path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => drop(stream),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                }
            });
            Self {
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct Runtime {
        path: PathBuf,
        server: Option<Server>,
    }
    impl Runtime {
        fn new(mode: u32) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "prek-compositor-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            let server = Server::bind(&path.join(DISPLAY));
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            Self {
                path,
                server: Some(server),
            }
        }
        fn open(&self, display: &str) -> io::Result<Compositor> {
            Compositor::open(&self.path, display, uid(), &executable())
        }
    }
    impl Drop for Runtime {
        fn drop(&mut self) {
            self.server.take();
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn open_binds_the_listening_process() {
        let runtime = Runtime::new(0o700);
        let compositor = runtime.open(DISPLAY).unwrap();
        assert_eq!(compositor.pid(), std::process::id());
        assert_eq!(compositor.uid(), uid());
        assert_eq!(compositor.display(), DISPLAY);
        assert!(compositor.start() > 0);
        compositor.check_binding().unwrap();
    }

    #[test]
    fn another_executable_is_denied() {
        let runtime = Runtime::new(0o700);
        let other = runtime.path.join("other");
        fs::write(&other, b"").unwrap();
        for executable in [Path::new("/proc/1/exe"), other.as_path()] {
            let error = Compositor::open(&runtime.path, DISPLAY, uid(), executable).unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::PermissionDenied,
                "{}",
                executable.display()
            );
        }
    }

    #[test]
    fn shared_or_foreign_runtime_directories_are_denied() {
        for mode in [0o710, 0o701, 0o770, 0o755] {
            let runtime = Runtime::new(mode);
            assert_eq!(
                runtime.open(DISPLAY).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "{mode:o}"
            );
        }
        let runtime = Runtime::new(0o700);
        let foreign =
            Compositor::open(&runtime.path, DISPLAY, uid().wrapping_add(1), &executable());
        assert_eq!(foreign.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn endpoint_must_be_an_owned_socket() {
        let runtime = Runtime::new(0o700);
        fs::write(runtime.path.join("plain"), b"").unwrap();
        std::os::unix::fs::symlink(runtime.path.join(DISPLAY), runtime.path.join("link")).unwrap();
        for display in ["plain", "link"] {
            assert_eq!(
                runtime.open(display).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "{display}"
            );
        }
        assert_eq!(
            runtime.open("missing").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn malformed_names_are_rejected() {
        let runtime = Runtime::new(0o700);
        let long = "w".repeat(MAX_DISPLAY + 1);
        for display in ["", ".", "..", "a/b", "a b", "wayland-0\0", long.as_str()] {
            assert_eq!(
                runtime.open(display).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{display:?}"
            );
        }
        let relative = Compositor::open(Path::new("run/user"), DISPLAY, uid(), &executable());
        assert_eq!(relative.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_replaced_socket_breaks_the_binding() {
        let mut runtime = Runtime::new(0o700);
        let compositor = runtime.open(DISPLAY).unwrap();
        let staged = runtime.path.join("staged");
        let replacement = Server::bind(&staged);
        fs::rename(&staged, runtime.path.join(DISPLAY)).unwrap();
        runtime.server = Some(replacement);
        assert_eq!(
            compositor.check_binding().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        runtime.open(DISPLAY).unwrap().check_binding().unwrap();
        runtime.server.take();
        fs::remove_file(runtime.path.join(DISPLAY)).unwrap();
        assert!(compositor.check_binding().is_err());
    }

    #[test]
    fn a_changed_process_start_revokes_the_binding() {
        let runtime = Runtime::new(0o700);
        let mut compositor = runtime.open(DISPLAY).unwrap();
        compositor.start += 1;
        assert_eq!(
            compositor.check_binding().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            compositor
                .holds_device(Path::new("/dev/null"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn held_devices_are_matched_by_device_number() {
        let runtime = Runtime::new(0o700);
        let compositor = runtime.open(DISPLAY).unwrap();
        let _null = File::open("/dev/null").unwrap();
        let alias = runtime.path.join("alias");
        std::os::unix::fs::symlink("/dev/null", &alias).unwrap();
        assert!(compositor.holds_device(Path::new("/dev/null")).unwrap());
        assert!(compositor.holds_device(&alias).unwrap());
        assert!(!compositor.holds_device(Path::new("/dev/full")).unwrap());
        assert_eq!(
            compositor.holds_device(&runtime.path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
