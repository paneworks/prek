use super::*;
use bus::{Bus, Callback, Connection, Message, Slot};
use std::{
    cell::RefCell,
    fs,
    io::{BufRead, BufReader},
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

#[derive(Clone, Copy)]
struct ServerApi {
    object: unsafe extern "C" fn(
        *mut Bus,
        *mut *mut Slot,
        *const libc::c_char,
        Option<Callback>,
        *mut libc::c_void,
    ) -> i32,
    request: unsafe extern "C" fn(*mut Bus, *const libc::c_char, u64) -> i32,
    release: unsafe extern "C" fn(*mut Bus, *const libc::c_char) -> i32,
    reply: unsafe extern "C" fn(*mut Message, *const libc::c_char, ...) -> i32,
    is_call: unsafe extern "C" fn(*mut Message, *const libc::c_char, *const libc::c_char) -> i32,
    emit: unsafe extern "C" fn(
        *mut Bus,
        *const libc::c_char,
        *const libc::c_char,
        *const libc::c_char,
        *const libc::c_char,
        ...
    ) -> i32,
    flush: unsafe extern "C" fn(*mut Bus) -> i32,
}
impl ServerApi {
    fn load(library: &crate::session::Library) -> Self {
        macro_rules! symbol {
            ($name:literal,$ty:ty) => {
                unsafe {
                    std::mem::transmute::<*mut libc::c_void, $ty>(
                        library
                            .symbol(
                                CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).unwrap(),
                            )
                            .unwrap(),
                    )
                }
            };
        }
        Self {
            object: symbol!(
                "sd_bus_add_object",
                unsafe extern "C" fn(
                    *mut Bus,
                    *mut *mut Slot,
                    *const libc::c_char,
                    Option<Callback>,
                    *mut libc::c_void,
                ) -> i32
            ),
            request: symbol!(
                "sd_bus_request_name",
                unsafe extern "C" fn(*mut Bus, *const libc::c_char, u64) -> i32
            ),
            release: symbol!(
                "sd_bus_release_name",
                unsafe extern "C" fn(*mut Bus, *const libc::c_char) -> i32
            ),
            reply: symbol!(
                "sd_bus_reply_method_return",
                unsafe extern "C" fn(*mut Message, *const libc::c_char, ...) -> i32
            ),
            is_call: symbol!(
                "sd_bus_message_is_method_call",
                unsafe extern "C" fn(*mut Message, *const libc::c_char, *const libc::c_char) -> i32
            ),
            emit: symbol!(
                "sd_bus_emit_signal",
                unsafe extern "C" fn(
                    *mut Bus,
                    *const libc::c_char,
                    *const libc::c_char,
                    *const libc::c_char,
                    *const libc::c_char,
                    ...
                ) -> i32
            ),
            flush: symbol!("sd_bus_flush", unsafe extern "C" fn(*mut Bus) -> i32),
        }
    }
}
struct Fake {
    api: Api,
    server: ServerApi,
    bus: *mut Bus,
    preparing: Cell<bool>,
    race_get: Cell<bool>,
    race_inhibit: Cell<bool>,
    pipes: RefCell<Vec<OwnedFd>>,
}
impl Fake {
    fn emit(&self, value: bool) -> i32 {
        self.preparing.set(value);
        unsafe {
            (self.server.emit)(
                self.bus,
                OBJECT.as_ptr(),
                MANAGER.as_ptr(),
                c"PrepareForSleep".as_ptr(),
                c"b".as_ptr(),
                i32::from(value),
            )
        }
    }
    fn read_string(&self, message: *mut Message) -> Option<&CStr> {
        let mut value: *const libc::c_char = std::ptr::null();
        if unsafe {
            (self.api.read)(
                message,
                b's' as libc::c_char,
                (&mut value as *mut *const libc::c_char).cast(),
            )
        } != 1
            || value.is_null()
        {
            return None;
        }
        Some(unsafe { CStr::from_ptr(value) })
    }
}
unsafe extern "C" fn method(
    message: *mut Message,
    data: *mut libc::c_void,
    _error: *mut libc::c_void,
) -> i32 {
    let state = unsafe { &*data.cast::<Fake>() };
    if unsafe {
        (state.server.is_call)(
            message,
            c"org.freedesktop.DBus.Properties".as_ptr(),
            c"Get".as_ptr(),
        )
    } == 1
    {
        if state.read_string(message) != Some(MANAGER)
            || state.read_string(message) != Some(c"PreparingForSleep")
        {
            return -libc::EINVAL;
        }
        if state.race_get.replace(false) && (state.emit(true) < 0 || state.emit(false) < 0) {
            return -libc::EIO;
        }
        return unsafe {
            (state.server.reply)(
                message,
                c"v".as_ptr(),
                c"b".as_ptr(),
                i32::from(state.preparing.get()),
            )
        };
    }
    if unsafe { (state.server.is_call)(message, MANAGER.as_ptr(), c"ListInhibitors".as_ptr()) } == 1
    {
        if state.pipes.borrow().is_empty() {
            return unsafe { (state.server.reply)(message, c"a(ssssuu)".as_ptr(), 0u32) };
        }
        return unsafe {
            (state.server.reply)(
                message,
                c"a(ssssuu)".as_ptr(),
                1u32,
                c"sleep".as_ptr(),
                c"Prek".as_ptr(),
                c"Quiesce input workers before sleep".as_ptr(),
                c"delay".as_ptr(),
                libc::geteuid(),
                std::process::id(),
            )
        };
    }
    if unsafe { (state.server.is_call)(message, MANAGER.as_ptr(), c"Inhibit".as_ptr()) } == 1 {
        for expected in [
            c"sleep",
            c"Prek",
            c"Quiesce input workers before sleep",
            c"delay",
        ] {
            if state.read_string(message) != Some(expected) {
                return -libc::EINVAL;
            }
        }
        if state.race_inhibit.replace(false) && state.emit(true) < 0 {
            return -libc::EIO;
        }
        let mut pipes = [-1; 2];
        if unsafe { libc::pipe2(pipes.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
            return -libc::EIO;
        }
        let read = unsafe { OwnedFd::from_raw_fd(pipes[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(pipes[1]) };
        state.pipes.borrow_mut().push(read);
        return unsafe { (state.server.reply)(message, c"h".as_ptr(), write.as_raw_fd()) };
    }
    -libc::ENOSYS
}
enum Control {
    Prepare(bool),
    Pair,
    Malformed,
    RaceGet,
    RaceInhibit,
    Release,
    Stop,
}
struct Server {
    commands: mpsc::Sender<(Control, mpsc::Sender<()>)>,
    active: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn start(library: PathBuf, address: CString, initial: bool) -> Self {
        let (commands, receiver) = mpsc::channel::<(Control, mpsc::Sender<()>)>();
        let (ready, started) = mpsc::channel();
        let active = Arc::new(AtomicUsize::new(0));
        let count = active.clone();
        let thread = thread::spawn(move || {
            let library = crate::session::Library::open_fixture(&library).unwrap();
            let api = Api::load(&library).unwrap();
            let server = ServerApi::load(&library);
            let bus = Connection::open(api, &address).unwrap();
            let state = Box::new(Fake {
                api,
                server,
                bus: bus.ptr.as_ptr(),
                preparing: Cell::new(initial),
                race_get: Cell::new(false),
                race_inhibit: Cell::new(false),
                pipes: RefCell::new(Vec::new()),
            });
            let mut slot = std::ptr::null_mut();
            checked(unsafe {
                (server.object)(
                    bus.ptr.as_ptr(),
                    &mut slot,
                    OBJECT.as_ptr(),
                    Some(method),
                    (&raw const *state).cast_mut().cast(),
                )
            })
            .unwrap();
            checked(unsafe { (server.request)(bus.ptr.as_ptr(), LOGIN.as_ptr(), 0) }).unwrap();
            ready.send(()).unwrap();
            loop {
                if let Ok((command, done)) = receiver.try_recv() {
                    match command {
                        Control::Prepare(value) => {
                            checked(state.emit(value)).unwrap();
                        }
                        Control::Pair => {
                            checked(state.emit(true)).unwrap();
                            checked(state.emit(false)).unwrap();
                        }
                        Control::Malformed => {
                            checked(unsafe {
                                (server.emit)(
                                    bus.ptr.as_ptr(),
                                    OBJECT.as_ptr(),
                                    MANAGER.as_ptr(),
                                    c"PrepareForSleep".as_ptr(),
                                    c"s".as_ptr(),
                                    c"not-a-boolean".as_ptr(),
                                )
                            })
                            .unwrap();
                        }
                        Control::RaceGet => state.race_get.set(true),
                        Control::RaceInhibit => state.race_inhibit.set(true),
                        Control::Release => {
                            checked(unsafe { (server.release)(bus.ptr.as_ptr(), LOGIN.as_ptr()) })
                                .unwrap();
                        }
                        Control::Stop => {
                            done.send(()).unwrap();
                            break;
                        }
                    }
                    checked(unsafe { (server.flush)(bus.ptr.as_ptr()) }).unwrap();
                    done.send(()).unwrap();
                }
                if checked(unsafe { (api.process)(bus.ptr.as_ptr(), std::ptr::null_mut()) })
                    .is_err()
                {
                    break;
                }
                let mut pipes = state.pipes.borrow_mut();
                pipes.retain(|pipe| {
                    let mut byte = 0u8;
                    (unsafe { libc::read(pipe.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) }) != 0
                });
                count.store(pipes.len(), Ordering::SeqCst);
                drop(pipes);
                thread::sleep(Duration::from_millis(1));
            }
            unsafe {
                (api.slot_unref)(slot);
            }
        });
        started.recv_timeout(Duration::from_secs(4)).unwrap();
        Self {
            commands,
            active,
            thread: Some(thread),
        }
    }
    fn send(&self, command: Control) {
        let (done, result) = mpsc::channel();
        self.commands.send((command, done)).unwrap();
        result.recv_timeout(Duration::from_secs(4)).unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let (done, result) = mpsc::channel();
        if self.commands.send((Control::Stop, done)).is_ok() {
            let _ = result.recv_timeout(Duration::from_secs(4));
        }
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            assert!(result.is_ok(), "private bus server failed");
        }
    }
}
struct PrivateBus {
    child: Child,
    root: PathBuf,
    address: CString,
}
impl PrivateBus {
    fn start() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "prek-sleep-bus-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let config = root.join("bus.conf");
        let address = CString::new(format!("unix:path={}", root.join("socket").display())).unwrap();
        let listen = address
            .to_str()
            .unwrap()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        fs::write(
            &config,
            format!(
                r#"<busconfig><type>session</type><auth>EXTERNAL</auth><listen>{listen}</listen>
<policy context="default"><allow user="*"/><allow own="*"/>
<allow send_destination="*"/><allow send_type="signal"/><allow receive_sender="*"/>
</policy></busconfig>"#
            ),
        )
        .unwrap();
        let child = Command::new("dbus-daemon")
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .arg(format!("--config-file={}", config.display()))
            .arg(format!("--address={}", address.to_str().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut value = Self {
            child,
            root,
            address,
        };
        let mut line = String::new();
        BufReader::new(value.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(
            line.starts_with(value.address.to_str().unwrap()),
            "owned bus failed to publish its configured address"
        );
        value
    }
}
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn until(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(
            Instant::now() < until,
            "private bus condition did not complete"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn open_watch(library: &Path, address: &CStr, uid: u32) -> io::Result<Watch> {
    Watch::open_inner(
        crate::session::Library::open_fixture(library)?,
        address,
        uid,
    )
}
#[test]
fn a_private_bus_reports_the_process_behind_a_connection_name() {
    let library = std::env::var_os("PREK_TEST_SYSTEMD_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/current-system/systemd/lib/libsystemd.so.0"));
    let daemon = PrivateBus::start();
    let library = crate::session::Library::open_fixture(&library).unwrap();
    let bus = Connection::open(Api::load(&library).unwrap(), &daemon.address).unwrap();
    assert_eq!(bus.pid(c"org.freedesktop.DBus").unwrap(), daemon.child.id());
    assert!(bus.pid(c":1.999999").is_err());
}
#[test]
fn actual_private_bus_pins_owner_preserves_queued_boundaries_and_owns_delay_descriptors() {
    let library = std::env::var_os("PREK_TEST_SYSTEMD_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/current-system/systemd/lib/libsystemd.so.0"));
    let daemon = PrivateBus::start();
    let fixture = daemon.root.join("fixture.so");
    fs::copy(&library, &fixture).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o400)).unwrap();
    crate::session::Library::open_fixture(&fixture).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(
            matches!(crate::session::Library::open(&fixture), Err(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        assert!(
            matches!(Watch::open(&fixture), Err(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
    }
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o622)).unwrap();
    assert!(crate::session::Library::open_fixture(&fixture).is_err());
    assert!(crate::session::Library::open(&fixture).is_err());
    let server = Server::start(library.clone(), daemon.address.clone(), false);
    let uid = unsafe { libc::geteuid() };
    assert!(
        matches!(open_watch(&library,&daemon.address,uid^1),Err(error) if error.kind()==io::ErrorKind::PermissionDenied)
    );
    let mut watch = open_watch(&library, &daemon.address, uid).unwrap();
    assert_eq!(watch.step().unwrap(), Snapshot::default());
    assert_eq!(server.active.load(Ordering::SeqCst), 0);
    let foreign_library = crate::session::Library::open_fixture(&library).unwrap();
    let foreign_api = Api::load(&foreign_library).unwrap();
    let foreign = Connection::open(foreign_api, &daemon.address).unwrap();
    let tools = ServerApi::load(&foreign_library);
    checked(unsafe {
        (tools.emit)(
            foreign.ptr.as_ptr(),
            OBJECT.as_ptr(),
            MANAGER.as_ptr(),
            c"PrepareForSleep".as_ptr(),
            c"b".as_ptr(),
            1i32,
        )
    })
    .unwrap();
    checked(unsafe { (tools.flush)(foreign.ptr.as_ptr()) }).unwrap();
    thread::sleep(Duration::from_millis(10));
    assert_eq!(watch.step().unwrap(), Snapshot::default());
    if uid != 0 {
        assert!(
            matches!(watch.delay(),Err(error) if error.kind()==io::ErrorKind::PermissionDenied)
        );
    }
    server.send(Control::Pair);
    until(|| {
        watch
            .step()
            .is_ok_and(|value| value.generation == 1 && !value.preparing)
    });
    let lease = watch.delay_inner().unwrap();
    assert_ne!(
        unsafe { libc::fcntl(lease._fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    until(|| server.active.load(Ordering::SeqCst) == 1);
    assert_eq!(
        crate::sleep_watch::native_probe::own_inhibitors(&mut watch).unwrap(),
        1
    );
    drop(lease);
    until(|| server.active.load(Ordering::SeqCst) == 0);
    assert_eq!(
        crate::sleep_watch::native_probe::own_inhibitors(&mut watch).unwrap(),
        0
    );
    server.send(Control::Prepare(true));
    until(|| watch.step().unwrap().preparing);
    let mut initial = open_watch(&library, &daemon.address, uid).unwrap();
    assert!(initial.step().unwrap().preparing);
    assert!(initial.delay_inner().is_err());
    assert!(watch.delay_inner().is_err());
    server.send(Control::Prepare(false));
    until(|| !watch.step().unwrap().preparing);
    server.send(Control::RaceGet);
    let mut raced = open_watch(&library, &daemon.address, uid).unwrap();
    assert!(raced.step().unwrap().generation >= 1);
    server.send(Control::RaceInhibit);
    let lease = raced.delay_inner().unwrap();
    until(|| raced.step().unwrap().preparing);
    until(|| server.active.load(Ordering::SeqCst) == 1);
    drop(lease);
    until(|| server.active.load(Ordering::SeqCst) == 0);
    server.send(Control::Prepare(false));
    until(|| !watch.step().unwrap().preparing);
    server.send(Control::Malformed);
    until(|| watch.step().is_err());
    server.send(Control::Prepare(false));
    assert!(watch.step().is_err());
    let mut fresh = open_watch(&library, &daemon.address, uid).unwrap();
    server.send(Control::Release);
    until(|| fresh.step().is_err());
    assert!(fresh.step().is_err());
}

#[test]
fn private_bus_disconnect_is_permanent_and_poll_descriptor_remains_owned() {
    let library = std::env::var_os("PREK_TEST_SYSTEMD_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/current-system/systemd/lib/libsystemd.so.0"));
    let mut daemon = PrivateBus::start();
    let _server = Server::start(library.clone(), daemon.address.clone(), false);
    let mut watch = open_watch(&library, &daemon.address, unsafe { libc::geteuid() }).unwrap();
    let fd = watch.as_fd().as_raw_fd();
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    until(|| watch.step().is_err());
    assert!(watch.step().is_err());
    assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
}

#[test]
fn input_worker_is_reaped_before_delay_release_on_error_unwind() {
    const CASE: &str =
        "sleep_watch::tests::native::input_worker_is_reaped_before_delay_release_on_error_unwind";
    if std::env::var("PREK_TEST_SLEEP_BARRIER").as_deref() == Ok("worker") {
        thread::sleep(Duration::from_secs(10));
        return;
    }
    let library = std::env::var_os("PREK_TEST_SYSTEMD_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/current-system/systemd/lib/libsystemd.so.0"));
    let daemon = PrivateBus::start();
    let server = Server::start(library.clone(), daemon.address.clone(), false);
    let mut watch = open_watch(&library, &daemon.address, unsafe { libc::geteuid() }).unwrap();
    let lease = watch.delay_inner().unwrap();
    let fd = lease._fd.as_raw_fd();
    until(|| server.active.load(Ordering::SeqCst) == 1);
    use std::sync::{Mutex, atomic::AtomicBool};
    let order = Arc::new(Mutex::new(Vec::new()));
    let valid = Arc::new(AtomicBool::new(true));
    struct Owner {
        fd: i32,
        order: Arc<Mutex<Vec<u8>>>,
        valid: Arc<AtomicBool>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.order.lock().unwrap().push(1);
            self.valid.fetch_and(
                unsafe { libc::fcntl(self.fd, libc::F_GETFD) } >= 0,
                Ordering::SeqCst,
            );
        }
    }
    struct Worker {
        inner: Option<crate::supervisor::Supervisor>,
        fd: i32,
        order: Arc<Mutex<Vec<u8>>>,
        valid: Arc<AtomicBool>,
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            self.order.lock().unwrap().push(2);
            self.valid.fetch_and(
                unsafe { libc::fcntl(self.fd, libc::F_GETFD) } >= 0,
                Ordering::SeqCst,
            );
            let identity = self.inner.as_ref().unwrap().worker_identity();
            drop(self.inner.take());
            self.valid
                .fetch_and(identity.confirmed_dead().unwrap_or(false), Ordering::SeqCst);
            self.valid.fetch_and(
                unsafe { libc::fcntl(self.fd, libc::F_GETFD) } >= 0,
                Ordering::SeqCst,
            );
        }
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CASE, "--test-threads=1"])
        .env("PREK_TEST_SLEEP_BARRIER", "worker")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let worker = crate::supervisor::Supervisor::spawn(command).unwrap();
    let identity = worker.worker_identity();
    let lifetime = Inhibited::new(
        Owner {
            fd,
            order: order.clone(),
            valid: valid.clone(),
        },
        Worker {
            inner: Some(worker),
            fd,
            order: order.clone(),
            valid: valid.clone(),
        },
        lease,
    );
    fn fail<T>(_owner: T) -> io::Result<()> {
        Err(io::ErrorKind::ConnectionAborted.into())
    }
    assert!(fail(lifetime).is_err());
    assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    assert!(valid.load(Ordering::SeqCst));
    assert!(identity.confirmed_dead().unwrap());
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    until(|| server.active.load(Ordering::SeqCst) == 0);
}

#[test]
fn same_login_guardian_reaps_unregistered_child_across_queued_sleep_boundary() {
    const MODE: &str = "PREK_TEST_SLEEP_GUARDIAN";
    const CASE: &str = "sleep_watch::tests::native::same_login_guardian_reaps_unregistered_child_across_queued_sleep_boundary";
    let command = |mode: &str, log: &Path| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", CASE, "--test-threads=1", "--nocapture"])
            .env(MODE, mode)
            .env("PREK_TEST_SLEEP_TRACE", log)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        command
    };
    if std::env::var(MODE).as_deref() == Ok("child") {
        let log = PathBuf::from(std::env::var_os("PREK_TEST_SLEEP_TRACE").unwrap());
        let old = fs::read_to_string(&log).unwrap_or_default();
        if let Some(pid) = old.lines().next().and_then(|line| line.parse::<i32>().ok()) {
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }
        use std::io::Write;
        let mut output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        writeln!(output, "{}", std::process::id()).unwrap();
        drop(output);
        if old.is_empty() {
            thread::sleep(Duration::from_secs(10));
        }
        return;
    }
    let root = std::env::temp_dir().join(format!("prek-sleep-guardian-{}", std::process::id()));
    if std::env::var(MODE).as_deref() != Ok("owner") {
        fs::create_dir(&root).unwrap();
        assert!(
            command("owner", &root.join("trace"))
                .status()
                .unwrap()
                .success()
        );
        fs::remove_dir_all(root).unwrap();
        return;
    }
    let log = PathBuf::from(std::env::var_os("PREK_TEST_SLEEP_TRACE").unwrap());
    let library = std::env::var_os("PREK_TEST_SYSTEMD_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/current-system/systemd/lib/libsystemd.so.0"));
    let daemon = PrivateBus::start();
    let server = Server::start(library.clone(), daemon.address.clone(), false);
    let watch = open_watch(&library, &daemon.address, unsafe { libc::geteuid() }).unwrap();
    let driver_log = log.clone();
    let (finished, wait_finished) = mpsc::channel();
    let driver = thread::spawn(move || {
        until(|| fs::read_to_string(&driver_log).is_ok_and(|text| text.lines().count() == 1));
        server.send(Control::Pair);
        until(|| fs::read_to_string(&driver_log).is_ok_and(|text| text.lines().count() == 2));
        wait_finished.recv_timeout(Duration::from_secs(4)).unwrap();
    });
    let result = crate::session_launcher::run_with_sleep(|| command("child", &log), Some(watch));
    finished.send(()).unwrap();
    driver.join().unwrap();
    result.unwrap();
    let entries = fs::read_to_string(log).unwrap();
    let pids = entries.lines().collect::<Vec<_>>();
    assert_eq!(pids.len(), 2);
    assert_ne!(pids[0], pids[1]);
}
