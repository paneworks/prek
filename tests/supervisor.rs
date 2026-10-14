//! Process gates with no input device access and no administrator requirement.
#![cfg(target_os = "linux")]

use nix::libc;
use prek::supervisor::{Heartbeat, Reason, Supervisor, TerminationSignals};
use std::{
    fs, io,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

fn command(case: &str) -> Command {
    let executable = std::env::current_exe().unwrap();
    let mut command = if let Some(runner) = std::env::var_os("PREK_TEST_RUNNER") {
        let mut command = Command::new(runner);
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command
        .env("PREK_PROCESS_TEST", case)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

fn worker_fixture() {
    let case = std::env::var("PREK_PROCESS_TEST").expect("not a standalone fixture");
    if case == "observer-fixture" {
        observer_fixture();
        return;
    }
    if case == "learn-fixture" {
        learn_fixture();
        return;
    }
    if case == "signal-supervisor" {
        let signals = TerminationSignals::install().unwrap();
        assert!(
            matches!(TerminationSignals::install(), Err(error) if error.kind() == io::ErrorKind::AlreadyExists)
        );
        let mut worker = Supervisor::spawn(command("healthy")).unwrap();
        wait_for_health(&mut worker);
        publish_worker(worker.worker_pid());
        let report = worker.wait_with_stop(|| signals.requested()).unwrap();
        assert_eq!(report.reason(), Reason::Requested);
        assert!(report.status().success());
        drop(signals);
        let fresh = TerminationSignals::install().unwrap();
        assert!(
            !fresh.requested(),
            "old cancellation leaked into a new handler guard"
        );
        return;
    }
    if case == "supervisor" {
        let worker = Supervisor::spawn(command("healthy")).unwrap();
        publish_worker(worker.worker_pid());
        // A deliberately stalled parent: the outer test kills it. No input fd
        // is opened here, and the child must die even without Drop running.
        thread::sleep(Duration::from_secs(20));
        drop(worker);
        return;
    }
    let mut health = Heartbeat::connect().unwrap();
    assert_eq!(
        Heartbeat::connect().unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    if case == "exit" {
        std::process::exit(7);
    }
    if case == "ignore-term" {
        // SAFETY: test-only SIG_IGN disposition, no callback into Rust.
        assert_ne!(
            unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) },
            libc::SIG_ERR
        );
    }
    health.progress().unwrap();
    if case == "disconnect" {
        drop(health);
        thread::sleep(Duration::from_secs(20));
        return;
    }
    if matches!(case.as_str(), "stall" | "ignore-term") {
        thread::sleep(Duration::from_secs(20));
        return;
    }
    if case == "malformed" {
        // SAFETY: own inherited test channel and initialized short packet.
        assert_eq!(
            unsafe {
                libc::send(
                    health.as_fd().as_raw_fd(),
                    b"bad".as_ptr().cast(),
                    3,
                    libc::MSG_NOSIGNAL,
                )
            },
            3
        );
        thread::sleep(Duration::from_secs(20));
        return;
    }
    if let Some(mode) = case.strip_prefix("packet-") {
        let mut packet = [0u8; 20];
        packet[..4].copy_from_slice(b"PKH1");
        let sequence: u64 = match mode {
            "duplicate" => 1,
            "gap" => 3,
            _ => 2,
        };
        packet[4..12].copy_from_slice(&sequence.to_be_bytes());
        let current = health.deadline().unwrap() - prek::supervisor::HEARTBEAT_INTERVAL;
        let time = match mode {
            "stale" => current.saturating_sub(Duration::from_secs(2)),
            "future" => current + Duration::from_secs(2),
            _ => current,
        };
        packet[12..].copy_from_slice(&(time.as_nanos() as u64).to_be_bytes());
        // SAFETY: owned test channel, fixed initialized protocol packet.
        assert_eq!(
            unsafe {
                libc::send(
                    health.as_fd().as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            20
        );
        thread::sleep(Duration::from_secs(20));
        return;
    }
    assert_eq!(case, "healthy");
    loop {
        match health.progress() {
            Ok(()) => thread::sleep(Duration::from_millis(10)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return,
            Err(error) => panic!("health failed: {error}"),
        }
    }
}

/// Disposable-VM source for the real observer loop. It emits no events and is
/// deliberately separate from Prek's virtual output identities. No physical
/// device is opened, and ordinary subprocess gates never enter this branch.
fn observer_fixture() {
    use evdev::{AttributeSet, KeyCode, uinput::VirtualDevice};
    use std::io::Write;
    assert_eq!(std::env::var("PREK_ISOLATED_ROUTING").as_deref(), Ok("1"));
    // SAFETY: read-only identity query; fixture creation is VM/root-only.
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let keys = AttributeSet::from_iter([KeyCode::KEY_A]);
    let mut source = VirtualDevice::builder()
        .unwrap()
        .name("Prek observer fixture")
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
    let path = std::env::var_os("PREK_PROCESS_WITNESS").unwrap();
    let mut witness = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    writeln!(witness, "{}", node.display()).unwrap();
    witness.sync_all().unwrap();
    // The VM owns this fixture's service lifecycle. Do not inject keystrokes,
    // including on its console, while testing idle health and shutdown.
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn learn_fixture() {
    use evdev::{
        AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode, uinput::VirtualDevice,
    };
    use std::io::Write;
    assert_eq!(std::env::var("PREK_ISOLATED_ROUTING").as_deref(), Ok("1"));
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let mut source = VirtualDevice::builder()
        .unwrap()
        .name("Prek learn fixture")
        .with_keys(&AttributeSet::from_iter([KeyCode::BTN_LEFT]))
        .unwrap()
        .with_relative_axes(&AttributeSet::from_iter([
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
        ]))
        .unwrap()
        .build()
        .unwrap();
    let node = source
        .enumerate_dev_nodes_blocking()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let path = std::env::var_os("PREK_PROCESS_WITNESS").unwrap();
    let mut witness = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    writeln!(witness, "{}", node.display()).unwrap();
    witness.sync_all().unwrap();
    let mut emitted = false;
    loop {
        if !emitted && std::path::Path::new("/run/prek-learn-fixture/start").exists() {
            source
                .emit(&[InputEvent::new(EventType::KEY.0, KeyCode::BTN_LEFT.0, 1)])
                .unwrap();
            thread::sleep(Duration::from_millis(40));
            source
                .emit(&[
                    InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, 50),
                    InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, 100),
                ])
                .unwrap();
            thread::sleep(Duration::from_millis(40));
            source
                .emit(&[
                    InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, 50),
                    InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, -100),
                    InputEvent::new(EventType::KEY.0, KeyCode::BTN_LEFT.0, 0),
                ])
                .unwrap();
            emitted = true;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn publish_worker(pid: u32) {
    use std::io::Write;
    let path = std::env::var_os("PREK_PROCESS_WITNESS").unwrap();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    writeln!(file, "{pid}").unwrap();
    file.sync_all().unwrap();
}

fn wait_for_health(worker: &mut Supervisor) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while worker.heartbeats() == 0 {
        assert!(
            worker.check().unwrap().is_none(),
            "worker died before reporting progress"
        );
        assert!(Instant::now() < deadline, "worker did not become healthy");
        thread::sleep(Duration::from_millis(10));
    }
}

fn healthy_worker_remains_live_and_acknowledges_orderly_stop() {
    let mut worker = Supervisor::spawn(command("healthy")).unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1400) {
        assert!(worker.check().unwrap().is_none());
        thread::sleep(Duration::from_millis(20));
    }
    assert!(worker.heartbeats() >= 4);
    worker.request_stop().unwrap();
    let report = worker.wait().unwrap();
    assert_eq!(report.reason(), Reason::Requested);
    assert!(report.status().success());
    assert!(!report.forced());
    assert_eq!(worker.check().unwrap(), Some(report));
}

fn stalled_forwarding_loop_is_terminated_within_two_seconds() {
    let mut worker = Supervisor::spawn(command("stall")).unwrap();
    let started = Instant::now();
    let report = worker.wait().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(report.reason(), Reason::Stalled);
    assert_eq!(report.status().signal(), Some(libc::SIGTERM));
    assert_eq!(report.heartbeats(), 1);
    assert!(!report.forced());
}

fn unresponsive_worker_is_force_killed_after_termination_grace() {
    let mut worker = Supervisor::spawn(command("ignore-term")).unwrap();
    let started = Instant::now();
    let report = worker.wait().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1200));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(report.reason(), Reason::Stalled);
    assert_eq!(report.status().signal(), Some(libc::SIGKILL));
    assert!(report.forced());
}

fn ordinary_worker_failure_preserves_exit_status() {
    let mut worker = Supervisor::spawn(command("exit")).unwrap();
    let report = worker.wait().unwrap();
    assert_eq!(report.reason(), Reason::Exited);
    assert_eq!(report.status().code(), Some(7));
    assert_eq!(report.heartbeats(), 0);
    assert!(report.belongs_to(&worker));
    let mut replacement = Supervisor::spawn(command("exit")).unwrap();
    assert!(!report.belongs_to(&replacement));
    let newer = replacement.wait().unwrap();
    assert!(newer.belongs_to(&replacement));
    assert!(!newer.belongs_to(&worker));
}

fn closed_health_channel_does_not_acknowledge_a_live_worker_release() {
    let mut worker = Supervisor::spawn(command("disconnect")).unwrap();
    let report = worker.wait().unwrap();
    assert_eq!(report.reason(), Reason::Disconnected);
    assert_eq!(report.status().signal(), Some(libc::SIGTERM));
}

fn ignored_stop_is_force_killed_and_drop_reaps_a_live_worker() {
    let mut worker = Supervisor::spawn(command("ignore-term")).unwrap();
    wait_for_health(&mut worker);
    let started = Instant::now();
    worker.request_stop().unwrap();
    let report = worker.wait().unwrap();
    assert_eq!(report.reason(), Reason::Requested);
    assert!(report.forced());
    assert_eq!(report.status().signal(), Some(libc::SIGKILL));
    assert!(started.elapsed() < Duration::from_secs(2));

    let mut worker = Supervisor::spawn(command("healthy")).unwrap();
    wait_for_health(&mut worker);
    let fd = pidfd(worker.worker_pid() as libc::pid_t);
    drop(worker);
    assert_terminated(&fd);
}

fn spawn_failure_creates_no_worker() {
    let command = Command::new("/this/prek/test/executable/does/not/exist");
    assert_eq!(
        Supervisor::spawn(command).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

fn malformed_health_is_not_treated_as_forwarding_progress() {
    let mut worker = Supervisor::spawn(command("malformed")).unwrap();
    let report = worker.wait().unwrap();
    assert_eq!(report.reason(), Reason::Protocol);
    assert_eq!(report.status().signal(), Some(libc::SIGTERM));
}

fn stale_future_duplicate_and_skipped_health_cannot_extend_worker_lifetime() {
    for case in [
        "packet-stale",
        "packet-future",
        "packet-duplicate",
        "packet-gap",
    ] {
        let mut worker = Supervisor::spawn(command(case)).unwrap();
        let report = worker.wait().unwrap();
        assert_eq!(report.reason(), Reason::Protocol, "{case}");
        assert_eq!(report.status().signal(), Some(libc::SIGTERM), "{case}");
        assert_eq!(
            report.heartbeats(),
            1,
            "invalid progress advanced the health counter"
        );
    }
}

struct Parent(Child);
impl Drop for Parent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Witness(PathBuf);
impl Witness {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/process-tests");
        fs::create_dir_all(&directory).unwrap();
        Self(directory.join(format!(
            "{}-{}.pid",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Witness {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn supervisor_death_kills_its_worker_without_requiring_cleanup_code() {
    let witness = Witness::new();
    let mut command = command("supervisor");
    command.env("PREK_PROCESS_WITNESS", &witness.0);
    let mut parent = Parent(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid: libc::pid_t = loop {
        if let Ok(text) = fs::read_to_string(&witness.0)
            && let Ok(pid) = text.trim().parse()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "parent did not publish its child identity"
        );
        assert!(
            parent.0.try_wait().unwrap().is_none(),
            "parent failed before creating worker"
        );
        thread::sleep(Duration::from_millis(10));
    };
    // SAFETY: pidfd_open pins the exact worker while its known parent is still
    // alive, avoiding PID-reuse guesses when verifying its death afterward.
    let fd = pidfd(pid);
    let mut event = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd; no descriptors are transferred.
    assert_eq!(
        unsafe { libc::poll(&mut event, 1, 0) },
        0,
        "worker was not live before parent death"
    );
    parent.0.kill().unwrap();
    parent.0.wait().unwrap();
    assert_terminated(&fd);
}

fn pidfd(pid: libc::pid_t) -> OwnedFd {
    // SAFETY: kernel pins this exact known live child; no reused PID can satisfy
    // a later termination check.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(raw >= 0, "process gate needs Linux pidfd support");
    unsafe { OwnedFd::from_raw_fd(raw as i32) }
}

fn assert_terminated(fd: &OwnedFd) {
    let mut event = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a single initialized pollfd for the pinned child.
    assert_eq!(
        unsafe { libc::poll(&mut event, 1, 2000) },
        1,
        "worker survived supervisor death"
    );
    assert_ne!(event.revents & libc::POLLIN, 0);
}

fn foreground_signals_request_orderly_worker_shutdown() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let witness = Witness::new();
        let mut launch = command("signal-supervisor");
        launch.env("PREK_PROCESS_WITNESS", &witness.0);
        let mut parent = Parent(launch.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Ok(text) = fs::read_to_string(&witness.0)
                && let Ok(pid) = text.trim().parse()
            {
                break pid;
            }
            assert!(parent.0.try_wait().unwrap().is_none());
            assert!(Instant::now() < deadline, "supervisor never became ready");
            thread::sleep(Duration::from_millis(10));
        };
        let fd = pidfd(pid);
        // SAFETY: this owned, unreaped process is the fixture supervisor.
        assert_eq!(
            unsafe { libc::kill(parent.0.id() as libc::pid_t, signal) },
            0
        );
        assert!(
            parent.0.wait().unwrap().success(),
            "signal {signal} did not stop gracefully"
        );
        assert_terminated(&fd);
    }
}

fn main() {
    if std::env::var_os("PREK_PROCESS_TEST").is_some() {
        worker_fixture();
        return;
    }
    // A separate executable gives fixtures a genuine exec-thread lifecycle;
    // no helper is installed with the application or opens any input device.
    let cases: &[(&str, fn())] = &[
        (
            "healthy loop and orderly stop",
            healthy_worker_remains_live_and_acknowledges_orderly_stop,
        ),
        (
            "stalled forwarding",
            stalled_forwarding_loop_is_terminated_within_two_seconds,
        ),
        (
            "forced termination",
            unresponsive_worker_is_force_killed_after_termination_grace,
        ),
        (
            "worker exit status",
            ordinary_worker_failure_preserves_exit_status,
        ),
        ("spawn failure", spawn_failure_creates_no_worker),
        (
            "closed health channel",
            closed_health_channel_does_not_acknowledge_a_live_worker_release,
        ),
        (
            "ignored stop and owner drop",
            ignored_stop_is_force_killed_and_drop_reaps_a_live_worker,
        ),
        (
            "foreground termination signals",
            foreground_signals_request_orderly_worker_shutdown,
        ),
        (
            "malformed health",
            malformed_health_is_not_treated_as_forwarding_progress,
        ),
        (
            "invalid health ordering and timestamps",
            stale_future_duplicate_and_skipped_health_cannot_extend_worker_lifetime,
        ),
        (
            "supervisor death",
            supervisor_death_kills_its_worker_without_requiring_cleanup_code,
        ),
    ];
    for (name, run) in cases {
        run();
        println!("process gate: {name} ... ok");
    }
    println!("{} process gates passed", cases.len());
}
