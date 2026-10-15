//! Opt-in real native handoff on the user-owned nested compositor. No input
//! devices, synthetic kernel sources or grabs are part of this probe.
use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn root() -> io::Result<()> {
    // SAFETY: actual process credentials, not an environment assertion.
    if unsafe { nix::libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "native handoff probe requires root",
        ));
    }
    Ok(())
}
fn value(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"))
}
fn number(name: &str) -> u32 {
    value(name)
        .parse()
        .unwrap_or_else(|_| panic!("invalid {name}"))
}
fn no_input_fds() {
    for entry in fs::read_dir("/proc/self/fd").unwrap() {
        let Ok(path) = fs::read_link(entry.unwrap().path()) else {
            continue;
        };
        let path = path.to_string_lossy();
        assert!(
            !path.starts_with("/dev/input/") && path != "/dev/uinput",
            "probe inherited input fd: {path}"
        );
    }
}
#[test]
fn probe_requires_actual_root_credentials() {
    // This check never contacts a provider. The ignored live case supplies
    // actual logind/native acceptance only when explicitly requested.
    if unsafe { nix::libc::geteuid() } != 0 {
        assert_eq!(root().unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}

#[test]
#[ignore = "explicit root probe of an owned local Cage/Morf session; no evdev"]
fn owned_native_handoff() {
    root().unwrap();
    assert_eq!(value("PREK_NATIVE_HANDOFF"), "1");
    no_input_fds();
    let runtime = PathBuf::from(value("PREK_NATIVE_RUNTIME"));
    assert_eq!(runtime.file_name().unwrap(), "r");
    let scratch = runtime.parent().unwrap();
    assert_eq!(scratch.parent().unwrap(), Path::new("/tmp"));
    assert!(
        scratch
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pc-")
    );
    let uid = number("PREK_NATIVE_UID");
    assert!(uid > 0);
    let expected_compositor = number("PREK_NATIVE_COMPOSITOR_PID");
    let expected_producer = number("PREK_NATIVE_PRODUCER_PID");
    let output = value("PREK_NATIVE_OUTPUT");
    let logind = Logind::open(Path::new(&value("PREK_NATIVE_LIBRARY"))).unwrap();
    let compositor = hyprland::Client::open(
        &runtime,
        &value("PREK_NATIVE_INSTANCE"),
        uid,
        Path::new(&value("PREK_NATIVE_COMPOSITOR")),
    )
    .unwrap();
    let mut clients = ShellClient::discover_all(
        &runtime,
        uid,
        Path::new(&value("PREK_NATIVE_MORF")),
        &logind,
        &compositor,
    )
    .unwrap();
    assert_eq!(clients.len(), 1, "probe requires one owned native shell");
    let client = clients.pop().unwrap();
    assert_eq!(client.pid, expected_producer);
    let binding = client.last_state().unwrap();
    assert_eq!(binding.compositor_pid, expected_compositor);
    assert_eq!(binding.output, output);
    let seat = client.registration.binding().unwrap().seat.clone();
    // The only supervised child is this immutable test executable's listing
    // mode. It owns no device fd and supplies genuine reaping evidence.
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--list")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (mut worker, channel) = Supervisor::spawn_controlled(command).unwrap();
    let identity = worker.worker_identity();
    let mut ledger = Ledger::new(identity, channel.peer()).unwrap();
    let scope = fence(1);
    ledger.watch(scope, &seat).unwrap();
    let mut session =
        Session::prepare(scope, seat.clone(), identity, client, compositor, &logind).unwrap();
    session
        .start(&logind, crate::ipc::monotonic().unwrap())
        .unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        session
            .poll(&logind, crate::ipc::monotonic().unwrap())
            .unwrap();
        if session.phase() == Phase::Quiesced {
            break;
        }
        assert!(
            Instant::now() < until,
            "native UI did not quiesce: {:?}",
            session.phase()
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(session.activate(&logind).unwrap());
    assert_eq!(session.phase(), Phase::Owned);
    // Exercise the real root queue and asynchronous native receipt. Hiding an
    // already hidden keyboard is confined to this owned shell. This does not
    // establish broker action-grant or rendered-animation acceptance.
    let request = crate::morf::controls::Request {
        version: 1,
        output: output.clone(),
        epoch: format!("{:016x}{:016x}", scope.broker.0[0], scope.broker.0[1]),
        gesture: format!("{:032x}", 42),
        ordinal: 1,
        action: crate::config::MorfAction::KeyboardHide,
        phase: crate::morf::controls::Phase::Discrete,
        dx: 0.,
        dy: 0.,
        vx: 0.,
        vy: 0.,
    };
    let now = crate::ipc::monotonic().unwrap();
    let deadline = (now + Duration::from_millis(200)).min(session.admission_until(now).unwrap());
    let ticket = session.submit_control(&request, deadline, &logind).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        session
            .poll(&logind, crate::ipc::monotonic().unwrap())
            .unwrap();
        if let Some(outcome) = session.take_control(&ticket) {
            assert_eq!(outcome, crate::morf::dispatch::Outcome::Dispatched);
            break;
        }
        assert!(
            Instant::now() < until,
            "native action receipt did not complete"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let report = loop {
        if let Some(report) = worker.check().unwrap() {
            break report;
        }
        assert!(Instant::now() < until, "probe worker was not reaped");
        thread::sleep(Duration::from_millis(5));
    };
    ledger.exited(&report).unwrap();
    let proof = ledger.take(scope, &seat).unwrap();
    assert!(proof.worker_exited() && proof.sources().is_empty());
    session
        .release(&proof, fence(2), &logind, crate::ipc::monotonic().unwrap())
        .unwrap();
    assert_eq!(session.phase(), Phase::ReleaseQueued);
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        session
            .poll(&logind, crate::ipc::monotonic().unwrap())
            .unwrap();
        if session.phase() == Phase::Restored {
            break;
        }
        assert!(
            Instant::now() < until,
            "native UI did not restore: {:?}",
            session.phase()
        );
        thread::sleep(Duration::from_millis(20));
    }
    no_input_fds();
    println!(
        "\nPREK_NATIVE_ROOT_RECEIPT={}",
        serde_json::json!({
            "status":"passed","uid":0,"native_ui_handoff_tested":true,
            "actual_logind_registration":true,"compositor_pid":expected_compositor,
            "native_action_receipts_tested":true,"native_action_rendering_tested":false,
            "producer_pid":expected_producer,"output":output,"worker_reaped":true,
            "input_devices_opened":0,"physical_takeover_tested":false,
            "broker_grant_tested":false,"restored":true
        })
    );
}
