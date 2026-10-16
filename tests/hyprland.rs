#![cfg(target_os = "linux")]

use prek::hyprland::Client;
#[path = "support/power_tests.rs"]
mod power;
#[path = "support/hyprland_actions.rs"]
mod window_actions;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Server {
    directory: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(reply: &'static str, delay: Duration) -> Self {
        Self::with_reply(move |_| reply.into(), delay)
    }
    fn with_reply(reply: impl Fn(&[u8]) -> String + Send + 'static, delay: Duration) -> Self {
        Self::with_version(reply, delay, "0.55.4")
    }
    fn with_version(
        reply: impl Fn(&[u8]) -> String + Send + 'static,
        delay: Duration,
        version: &'static str,
    ) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "prek-ipc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        for path in [
            &directory,
            &directory.join("hypr"),
            &directory.join("hypr/test"),
        ] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let listener = UnixListener::bind(directory.join("hypr/test/.socket.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut bytes = [0; 128];
                    let length = stream.read(&mut bytes).unwrap_or(0);
                    if length == 0 {
                        continue;
                    }
                    if bytes[..length] == *b"j/version" {
                        let _ = stream.write_all(
                            serde_json::json!({"version":version})
                                .to_string()
                                .as_bytes(),
                        );
                    } else {
                        thread::sleep(delay);
                        let request = &bytes[..length];
                        let response = if request.starts_with(b"[[BATCH]]") {
                            std::str::from_utf8(&request[9..])
                                .unwrap()
                                .split(';')
                                .map(|command| reply(command.as_bytes()))
                                .collect::<Vec<_>>()
                                .join("\n\n\n")
                        } else {
                            reply(request)
                        };
                        let _ = stream.write_all(response.as_bytes());
                    }
                } else {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        });
        Self {
            directory,
            stop,
            thread: Some(worker),
        }
    }
    fn client(&self) -> Client {
        Client::open(&self.directory, "test", uid(), &test_executable()).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        fs::remove_dir_all(&self.directory).unwrap();
    }
}
fn uid() -> u32 {
    unsafe { nix::libc::geteuid() }
}
fn test_executable() -> PathBuf {
    // Under user-mode QEMU, peer credentials identify the emulator process.
    // Test that actual kernel image rather than weakening production checks.
    PathBuf::from(format!("/proc/{}/exe", std::process::id()))
}
const EMPTY: &str = "{\"keyboards\":[],\"mice\":[],\"touch\":[],\"switches\":[],\"tablets\":[]}";

#[test]
fn verified_0562_inventory_retains_typed_identity_and_peer_checks() {
    let server = Server::with_version(
        |_| {
            serde_json::json!({
                "keyboards":[{"address":"0x1234","name":"owned-test-keyboard",
                    "rules":"evdev","model":"pc105","layout":"us","variant":"","options":"",
                    "active_layout_index":0,"active_keymap":"English (US)",
                    "capsLock":false,"numLock":false,"main":true}],
                "mice":[],"tablets":[],"touch":[],"switches":[]
            })
            .to_string()
        },
        Duration::ZERO,
        "0.56.2",
    );
    let client = server.client();
    assert_eq!((client.version().minor, client.version().patch), (56, 2));
    assert_eq!(client.inventory().unwrap()["keyboards"][0]["layout"], "us");
    assert!(Client::open(&server.directory, "test", uid(), Path::new("/proc/1/exe")).is_err());
    let malformed = Server::with_version(
        |_| {
            serde_json::json!({"keyboards":[{"address":"0x0","name":"invalid"}],
            "mice":[],"tablets":[],"touch":[],"switches":[]})
            .to_string()
        },
        Duration::ZERO,
        "0.56.2",
    );
    assert!(malformed.client().inventory().is_err());
}

#[test]
fn private_socket_requests_verify_the_exact_peer_and_endpoint() {
    let server = Server::new(EMPTY, Duration::ZERO);
    let client = server.client();
    assert_eq!(client.version().minor, 55);
    assert!(
        client.inventory().unwrap()["touch"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let path = server.directory.join("hypr/test/.socket.sock");
    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    assert_eq!(
        client.inventory().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    drop(replacement);
}

#[test]
fn registration_rejects_wrong_executable_and_nonprivate_directories() {
    let server = Server::new(EMPTY, Duration::ZERO);
    assert_eq!(
        Client::open(&server.directory, "test", uid(), Path::new("/proc/1/exe"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    fs::set_permissions(
        server.directory.join("hypr/test"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(
        Client::open(&server.directory, "test", uid(), &test_executable())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(Client::open(&server.directory, "../test", uid(), &test_executable()).is_err());
}

#[test]
fn stalled_or_malformed_compositor_replies_fail_within_the_budget() {
    let server = Server::new(EMPTY, Duration::from_millis(500));
    let client = server.client();
    let started = Instant::now();
    assert_eq!(
        client.inventory().unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert!(started.elapsed() < Duration::from_millis(400));
    drop(server);
    let server = Server::new("not JSON", Duration::ZERO);
    assert!(server.client().inventory().is_err());
}

#[test]
fn witnessed_compositor_lock_latches_desktop_inhibition() {
    let server = Server::new("{\"locked\":true}", Duration::ZERO);
    let client = server.client();
    let mut sampler = prek::hyprland::ContextSampler::default();
    assert!(
        sampler
            .sample(&client, 1, prek::context::SessionKind::User)
            .is_err()
    );
    assert!(sampler.inhibited());
    drop(server);
    let server = Server::new("{\"locked\":false}", Duration::ZERO);
    assert!(
        sampler
            .sample(&server.client(), 2, prek::context::SessionKind::User)
            .is_err()
    );
    assert!(sampler.inhibited());
}

#[test]
fn context_queries_share_one_deadline() {
    let server = Server::new("{\"locked\":false}", Duration::from_millis(500));
    let client = server.client();
    let mut sampler = prek::hyprland::ContextSampler::default();
    let started = Instant::now();
    assert_eq!(
        sampler
            .sample(&client, 1, prek::context::SessionKind::User)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    assert!(started.elapsed() < Duration::from_millis(400));
    assert!(!sampler.inhibited());
}

#[test]
fn context_sampling_fences_output_reappearance_and_geometry_changes() {
    let topology = Arc::new(AtomicU64::new(1));
    let changing = Arc::clone(&topology);
    let server = Server::with_reply(
        move |request| match request {
            b"j/locked" => "{\"locked\":false}".into(),
            b"j/clients" => "[]".into(),
            b"j/layers" | b"j/activewindow" => "{}".into(),
            b"j/monitors" => {
                let phase = changing.load(Ordering::Relaxed);
                if phase == 0 {
                    return "[]".into();
                }
                serde_json::json!([{"id": 1, "name": "panel", "disabled": false,
                "width": 1080, "height": 1920, "scale": if phase == 3 { 3.0 } else { 2.0 },
                "x": 0, "y": 0, "transform": 0, "activeWorkspace": {"id": 1},
                "dpmsStatus": true}])
                .to_string()
            }
            _ => panic!("unexpected query"),
        },
        Duration::ZERO,
    );
    let client = server.client();
    let mut sampler = prek::hyprland::ContextSampler::default();
    let first = sampler
        .sample(&client, 7, prek::context::SessionKind::User)
        .unwrap();
    assert_eq!(first.state, prek::context::State::Desktop);
    assert_eq!(first.outputs[0].geometry_generation, 2);
    let same = sampler
        .sample(&client, 7, prek::context::SessionKind::User)
        .unwrap();
    assert_eq!(same.outputs[0].geometry_generation, 2);
    assert!(same.revision > first.revision);
    topology.store(0, Ordering::Relaxed);
    assert!(
        sampler
            .sample(&client, 7, prek::context::SessionKind::User)
            .unwrap()
            .outputs
            .is_empty()
    );
    topology.store(1, Ordering::Relaxed);
    assert_eq!(
        sampler
            .sample(&client, 7, prek::context::SessionKind::User)
            .unwrap()
            .outputs[0]
            .geometry_generation,
        3
    );
    topology.store(3, Ordering::Relaxed);
    let changed = sampler
        .sample(&client, 8, prek::context::SessionKind::Greeter)
        .unwrap();
    assert_eq!(changed.state, prek::context::State::Greeter);
    assert_eq!(changed.generation, 8);
    assert_eq!(changed.outputs[0].geometry_generation, 4);
}

#[test]
#[ignore = "requires an explicitly selected real compositor; read-only"]
fn stock_compositor_queries_use_the_verified_live_socket() {
    let runtime = PathBuf::from(std::env::var_os("PREK_HYPR_RUNTIME").unwrap());
    let instance = std::env::var("PREK_HYPR_INSTANCE").unwrap();
    let executable = PathBuf::from(std::env::var_os("PREK_HYPR_EXE").unwrap());
    let uid = std::env::var("PREK_HYPR_UID").unwrap().parse().unwrap();
    let client = Client::open(&runtime, &instance, uid, &executable).unwrap();
    assert!(client.inventory().unwrap().is_object());
    let snapshot = prek::hyprland::ContextSampler::default()
        .sample(&client, 1, prek::context::SessionKind::User)
        .unwrap();
    assert!(!snapshot.outputs.is_empty());
    assert_eq!(snapshot.state, prek::context::State::Desktop);
    println!(
        "verified stock Hyprland {:?}: {:?}",
        client.version(),
        client.consumer()
    );
}

#[path = "support/kernel.rs"]
mod kernel_fixture;

#[test]
#[ignore = "requires the disposable stock-compositor VM and root-owned runtime registry"]
fn stock_compositor_accepts_virtual_keyboard_and_verifies_its_keymap() {
    use prek::{
        input::EV_KEY,
        kernel::Virtual,
        transport::{Descriptor, KernelIdentity},
    };
    use std::collections::BTreeMap;
    assert_eq!(std::env::var("PREK_COMPOSITOR_VM").as_deref(), Ok("1"));
    let registry = kernel_fixture::authorized();
    let runtime = PathBuf::from(std::env::var_os("PREK_HYPR_RUNTIME").unwrap());
    let instance = std::env::var("PREK_HYPR_INSTANCE").unwrap();
    let executable = PathBuf::from(std::env::var_os("PREK_HYPR_EXE").unwrap());
    let uid = std::env::var("PREK_HYPR_UID").unwrap().parse().unwrap();
    let client = Client::open(&runtime, &instance, uid, &executable).unwrap();
    let mut original = kernel_fixture::fixture(false);
    original.capabilities.insert(EV_KEY, (1..=255).collect());
    let identity = KernelIdentity {
        bus: 3,
        vendor: original.vendor,
        product: original.product,
        version: 1,
    };
    let policy = BTreeMap::from([("kb_layout".into(), "us".into())]);
    let prepared = Descriptor::mirror(&original, identity, None, &BTreeMap::new(), policy).unwrap();
    assert!(
        client
            .device_evidence(&prepared, kernel_fixture::now())
            .unwrap()
            .is_none()
    );
    // Visible only inside this disposable VM. No key events are injected and
    // no originals are grabbed while testing the real compositor inventory.
    let output = Virtual::create(registry.prepare(prepared.clone()).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let evidence = loop {
        if !output.processed_metadata().unwrap() {
            assert!(
                Instant::now() < deadline,
                "virtual udev metadata did not become ready"
            );
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        if let Some(evidence) = client
            .device_evidence(&prepared, kernel_fixture::now())
            .unwrap()
        {
            break evidence;
        }
        assert!(
            Instant::now() < deadline,
            "stock compositor never accepted the virtual keyboard"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(evidence.settings["kb_layout"], "us");
    let wrong = Descriptor::mirror(
        &original,
        identity,
        None,
        &BTreeMap::new(),
        BTreeMap::from([("kb_layout".into(), "fr".into())]),
    )
    .unwrap();
    assert!(
        client
            .device_evidence(&wrong, kernel_fixture::now())
            .is_err()
    );
    drop(output);
    loop {
        if client
            .device_evidence(&prepared, kernel_fixture::now())
            .unwrap()
            .is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "fixture launched only by the compositor/session VM harness"]
fn logind_session_agent_fixture() {
    use std::os::unix::net::UnixStream;
    assert_eq!(
        std::env::var("PREK_SESSION_AGENT_FIXTURE").as_deref(),
        Ok("1")
    );
    let socket = std::env::var_os("PREK_AGENT_SOCKET").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stream = loop {
        if let Ok(stream) = UnixStream::connect(&socket) {
            break stream;
        }
        assert!(
            Instant::now() < deadline,
            "session test broker did not start"
        );
        thread::sleep(Duration::from_millis(25));
    };
    stream.write_all(b"ready").unwrap();
    let mut response = [0; 1];
    let received = stream.read(&mut response).unwrap();
    if received == 1 && response == *b"D" {
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        // Keep the process alive so registration must detect socket loss.
        thread::sleep(Duration::from_secs(5));
    } else {
        assert_eq!(received, 0);
    }
}

#[test]
#[ignore = "requires actual logind and the disposable stock-compositor VM"]
fn stock_session_registration_checks_membership_and_revokes_dead_agents() {
    use prek::session::{Logind, Peer};
    use std::{
        os::unix::{net::UnixStream, process::CommandExt},
        process::Command,
    };
    assert_eq!(std::env::var("PREK_COMPOSITOR_VM").as_deref(), Ok("1"));
    let socket = Path::new("/run/prek-session/agent.sock");
    let listener = UnixListener::bind(socket).unwrap();
    fs::set_permissions(socket, fs::Permissions::from_mode(0o666)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let accept = |listener: &UnixListener| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut greeting = [0; 5];
                    stream.read_exact(&mut greeting).unwrap();
                    assert_eq!(&greeting, b"ready");
                    break stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("agent socket: {error}"),
            }
        }
    };
    let agent = accept(&listener);
    let client = Client::open(
        &PathBuf::from(std::env::var_os("PREK_HYPR_RUNTIME").unwrap()),
        &std::env::var("PREK_HYPR_INSTANCE").unwrap(),
        std::env::var("PREK_HYPR_UID").unwrap().parse().unwrap(),
        &PathBuf::from(std::env::var_os("PREK_HYPR_EXE").unwrap()),
    )
    .unwrap();
    let logind = Logind::open(&PathBuf::from(
        std::env::var_os("PREK_LOGIND_LIBRARY").unwrap(),
    ))
    .unwrap();
    let peer = Peer::from_socket(&agent).unwrap();
    let agent_pid = peer.pid();
    let mut registration = logind.register(peer, &client).unwrap();
    let binding = registration.binding().unwrap();
    assert_eq!(binding.uid, 1000);
    assert_eq!(binding.seat, "seat0");
    assert!(!binding.session.is_empty());
    registration.check(&logind, &client).unwrap();

    // Same UID, separate service cgroup: UID ownership alone must not authorize
    // a process outside the compositor's login session.
    let outside_path = Path::new("/run/prek-session/outside.sock");
    let outside_listener = UnixListener::bind(outside_path).unwrap();
    fs::set_permissions(outside_path, fs::Permissions::from_mode(0o666)).unwrap();
    outside_listener.set_nonblocking(true).unwrap();
    let mut outsider = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "logind_session_agent_fixture"])
        .env("PREK_SESSION_AGENT_FIXTURE", "1")
        .env("PREK_AGENT_SOCKET", outside_path)
        .uid(1000)
        .gid(100)
        .spawn()
        .unwrap();
    let outside = accept(&outside_listener);
    let peer = Peer::from_socket(&outside).unwrap();
    assert_eq!(peer.uid(), 1000);
    assert!(logind.register(peer, &client).is_err());
    drop(outside);
    assert!(outsider.wait().unwrap().success());
    let (local, _other) = UnixStream::pair().unwrap();
    assert!(
        logind
            .register(Peer::from_socket(&local).unwrap(), &client)
            .is_err()
    );

    thread::sleep(Duration::from_millis(260));
    assert!(registration.binding().is_err());
    registration.check(&logind, &client).unwrap();
    assert!(registration.binding().is_ok());
    assert_eq!(
        unsafe { nix::libc::kill(agent_pid as i32, nix::libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if registration.check(&logind, &client).is_err() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(registration.binding().is_err());
    assert!(registration.check(&logind, &client).is_err());
    drop(agent);

    // Keep another live agent/compositor pair while the real active VT changes.
    // Losing activity revokes the old proof even after returning to this seat.
    let seat_path = Path::new("/run/prek-session/seat.sock");
    let seat_listener = UnixListener::bind(seat_path).unwrap();
    fs::set_permissions(seat_path, fs::Permissions::from_mode(0o666)).unwrap();
    seat_listener.set_nonblocking(true).unwrap();
    let mut seat_agent = accept(&seat_listener);
    let seat_agent_pid = Peer::from_socket(&seat_agent).unwrap().pid();
    let mut seat_registration = logind
        .register(Peer::from_socket(&seat_agent).unwrap(), &client)
        .unwrap();
    let chvt = std::env::var_os("PREK_TEST_CHVT").unwrap();
    assert!(Command::new(&chvt).arg("2").status().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if seat_registration.check(&logind, &client).is_err() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(seat_registration.binding().is_err());
    client.check_binding().unwrap();
    assert!(Command::new(&chvt).arg("1").status().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut refreshed = loop {
        if let Ok(registration) = logind.register(Peer::from_socket(&seat_agent).unwrap(), &client)
        {
            break registration;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    };
    assert!(seat_registration.check(&logind, &client).is_err());
    seat_agent.write_all(b"D").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if refreshed.check(&logind, &client).is_err() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { nix::libc::kill(seat_agent_pid as i32, 0) }, 0);
    assert!(refreshed.binding().is_err());
    drop(seat_agent);
    fs::remove_file(seat_path).unwrap();
    fs::remove_file(socket).unwrap();
    fs::remove_file(outside_path).unwrap();
}

#[test]
#[ignore = "synthetic idle source owned only by the disposable VM"]
fn broker_observer_source_fixture() {
    use evdev::{AttributeSet, KeyCode, uinput::VirtualDevice};
    use std::os::unix::fs::OpenOptionsExt;
    kernel_fixture::authorized();
    assert_eq!(std::env::var("PREK_COMPOSITOR_VM").as_deref(), Ok("1"));
    fn create_source() -> (VirtualDevice, PathBuf) {
        let keys = AttributeSet::from_iter([KeyCode::KEY_A]);
        let mut source = VirtualDevice::builder()
            .unwrap()
            .name("Prek broker fixture")
            .with_keys(&keys)
            .unwrap()
            .build()
            .unwrap();
        let node = source
            .enumerate_dev_nodes_blocking()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        (source, node)
    }
    let (source, node) = create_source();
    let mut source = Some(source);
    let mut witness = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open("/run/prek-control/source")
        .unwrap();
    writeln!(witness, "{}", node.display()).unwrap();
    witness.sync_all().unwrap();
    let mut previous = String::new();
    loop {
        if let Ok(command) = fs::read_to_string("/run/prek-control/emit")
            && command != previous
        {
            if command == "disconnect" {
                source.take();
            } else if command == "recreate-held" {
                let (mut fresh, node) = create_source();
                fresh.emit(&[evdev::InputEvent::new(1, 30, 1)]).unwrap();
                fs::write("/run/prek-control/source", node.display().to_string()).unwrap();
                source = Some(fresh);
            } else if let Some(source) = source.as_mut() {
                let value = i32::from(command.starts_with("press"));
                source
                    .emit(&[evdev::InputEvent::new(1, 30, value)])
                    .unwrap();
            }
            fs::write("/run/prek-control/emitted", &command).unwrap();
            previous = command;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "CLI agent launched inside the actual compositor login session"]
fn broker_cli_session_fixture() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::{Command, Stdio};
    assert_eq!(std::env::var("PREK_BROKER_CLI_FIXTURE").as_deref(), Ok("1"));
    let binary = std::env::var_os("PREK_BROKER_APP").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !Path::new("/run/prek-control/control.sock").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(25));
    }
    let witness = "/run/user/1000/prek-broker-agent.jsonl";
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(witness)
        .unwrap();
    let errors = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open("/run/user/1000/prek-broker-agent.stderr")
        .unwrap();
    let mut command = Command::new(&binary);
    if std::env::var("PREK_LUA_EVENTS").as_deref() == Ok("1") {
        command.arg("session").arg("--lua");
    } else {
        command.arg("session");
    }
    let mut agent = command
        .args([
            "--socket",
            "/run/prek-control/control.sock",
            "--json",
            "--observations",
            "--context",
        ])
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let observed = fs::read_to_string(witness).unwrap();
        if observed.contains("\"registered\":true") && observed.contains("\"subscribed\":true") {
            break;
        }
        assert!(
            agent.try_wait().unwrap().is_none(),
            "session command failed before registration"
        );
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(25));
    }
    let inspected = Command::new(&binary)
        .args([
            "status",
            "--socket",
            "/run/prek-control/control.sock",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        inspected.status.success(),
        "status: {}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(status["mode"], "observe");
    assert_eq!(status["state"], "desktop");
    assert!(status["revision"].as_u64().unwrap() > 0);
    fs::write("/run/user/1000/prek-broker-ready", agent.id().to_string()).unwrap();
    // The VM stops the broker while this real CLI connection remains active.
    // A disconnected broker must not leave an agent acting on stale authority.
    let mut inspected = String::new();
    let mut lock_client = None;
    let mut restarted = false;
    let mut morf_unlock_requested = false;
    loop {
        if let Some(status) = agent.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        if let Ok(request) = fs::read_to_string("/run/user/1000/prek-broker-inspect")
            && request != inspected
        {
            let status = Command::new(&binary)
                .args([
                    "status",
                    "--socket",
                    "/run/prek-control/control.sock",
                    "--json",
                ])
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "{}",
                String::from_utf8_lossy(&status.stderr)
            );
            fs::write("/run/user/1000/prek-broker-status.json", &status.stdout).unwrap();
            inspected = request;
        }
        if lock_client.is_none() && Path::new("/run/user/1000/prek-lock-start").exists() {
            lock_client = Some(if let Some(morf) = std::env::var_os("PREK_LOCK_MORF") {
                let log = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open("/run/user/1000/prek-lock-native.log")
                    .unwrap();
                Command::new(morf)
                    .arg(std::env::var_os("PREK_LOCK_MORF_CONFIG").unwrap())
                    .env("PREK_LOCK_PROVIDER", "1")
                    .stdout(Stdio::from(log.try_clone().unwrap()))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap()
            } else {
                Command::new(std::env::var_os("PREK_LOCK_PYTHON").unwrap())
                    .arg(std::env::var_os("PREK_LOCK_SCRIPT").unwrap())
                    .env("PREK_COMPOSITOR_VM", "1")
                    .spawn()
                    .unwrap()
            });
            if std::env::var_os("PREK_LOCK_MORF").is_some() {
                fs::write(
                    "/run/user/1000/prek-lock-acknowledged",
                    "native provider starting",
                )
                .unwrap();
            }
        }
        if !morf_unlock_requested
            && Path::new("/run/user/1000/prek-lock-unlock").exists()
            && let Some(morf) = std::env::var_os("PREK_LOCK_MORF")
        {
            let reply = Command::new(morf)
                .args(["--lock", "ipc", "call", "fixture_unlock"])
                .output()
                .unwrap();
            fs::write(
                "/run/user/1000/prek-lock-unlock-response.log",
                format!(
                    "status: {}\nstdout: {}\nstderr: {}",
                    reply.status,
                    String::from_utf8_lossy(&reply.stdout),
                    String::from_utf8_lossy(&reply.stderr)
                ),
            )
            .unwrap();
            assert!(reply.status.success());
            let status = lock_client.as_mut().unwrap().wait().unwrap();
            assert!(status.success());
            fs::write(
                "/run/user/1000/prek-lock-completed",
                "native unlock completed",
            )
            .unwrap();
            morf_unlock_requested = true;
        }
        if !restarted && Path::new("/run/user/1000/prek-broker-restart").exists() {
            agent.kill().unwrap();
            agent.wait().unwrap();
            // Allow the broker to observe the old endpoint's closure before
            // requesting its one-agent seat ownership again.
            thread::sleep(Duration::from_millis(300));
            let output = fs::OpenOptions::new().append(true).open(witness).unwrap();
            let errors = fs::OpenOptions::new()
                .append(true)
                .open("/run/user/1000/prek-broker-agent.stderr")
                .unwrap();
            agent = Command::new(&binary)
                .args([
                    "session",
                    "--socket",
                    "/run/prek-control/control.sock",
                    "--json",
                    "--observations",
                ])
                .stdout(Stdio::from(output))
                .stderr(Stdio::from(errors))
                .spawn()
                .unwrap();
            fs::write(
                "/run/user/1000/prek-broker-restarted",
                agent.id().to_string(),
            )
            .unwrap();
            restarted = true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if let Some(mut lock_client) = lock_client
        && !morf_unlock_requested
    {
        assert!(lock_client.wait().unwrap().success());
    }
    fs::write("/run/user/1000/prek-broker-agent-stopped", "stopped").unwrap();
}
