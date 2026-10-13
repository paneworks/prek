//! Kernel/process recovery gates. Ordinary cargo test skips this executable's
//! device cases; run only in the disposable VM with PREK_ISOLATED_ROUTING=1.
#![cfg(target_os = "linux")]

#[path = "support/kernel.rs"]
mod kernel_fixture;
use evdev::{KeyCode, raw_stream::RawDevice};
use kernel_fixture::*;
use nix::libc;
use prek::{
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Device, EV_ABS,
        EV_KEY, Frame,
    },
    kernel::{Flush, Registry, Virtual},
    supervisor::{Heartbeat, Supervisor, TerminationSignals, WorkerIdentity},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

nix::ioctl_read_buf!(get_slots, b'E', 0x0a, libc::c_int);

#[derive(Serialize, Deserialize)]
struct Setup {
    source: PathBuf,
    marker: String,
    device: Device,
}
#[derive(Serialize, Deserialize)]
struct Prepared {
    output: PathBuf,
    marker: String,
    worker: WorkerIdentity,
}
#[derive(Serialize, Deserialize)]
struct Finished {
    removed: usize,
    reason: String,
    forced: bool,
    success: bool,
}

fn directory() -> PathBuf {
    std::env::var_os("PREK_RECOVERY_DIRECTORY").unwrap().into()
}
fn write_json(name: &str, value: &impl Serialize) {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory().join(name))
        .unwrap();
    serde_json::to_writer(&file, value).unwrap();
    file.sync_all().unwrap();
}
fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> T {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Ok(bytes) = fs::read(path)
            && let Ok(value) = serde_json::from_slice(&bytes)
        {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not publish {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn command(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env("PREK_RECOVERY_ROLE", role);
    command
}
fn parent() {
    let signals = TerminationSignals::install().unwrap();
    let mut supervisor = Supervisor::spawn(command("worker")).unwrap();
    let report = supervisor.wait_with_stop(|| signals.requested()).unwrap();
    assert!(report.belongs_to(&supervisor));
    let removed = Registry::system().unwrap().recover_worker(&report).unwrap();
    write_json(
        "finished",
        &Finished {
            removed,
            reason: format!("{:?}", report.reason()),
            forced: report.forced(),
            success: report.status().success(),
        },
    );
}
fn worker() {
    let mut health = Heartbeat::connect().unwrap();
    health.progress().unwrap();
    let setup: Setup = read_json(&directory().join("setup"));
    let registry = authorized();
    let mut output = Virtual::create(
        registry
            .prepare_isolated_for_worker(descriptor(&setup.device), &health)
            .unwrap(),
    )
    .unwrap();
    let mut target = Receiver::open(&output, &setup.device.source);
    health.progress().unwrap();
    let mut input = Receiver::from_test_node(&setup.source, &setup.device.source, &setup.marker);
    let mut router = route(setup.device.clone(), &output);
    write_json(
        "prepared",
        &Prepared {
            output: output.event_nodes().unwrap().remove(0),
            marker: output.marker().into(),
            worker: health.worker_identity(),
        },
    );
    let mut sequence = 0;
    let mut witnessed = false;
    loop {
        match health.progress() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                let recovery = router.recover().unwrap();
                output
                    .enqueue(&Frame {
                        source: recovery.source,
                        sequence: sequence + 1,
                        at: now(),
                        events: recovery.neutral_events,
                    })
                    .unwrap();
                let deadline = Instant::now() + Duration::from_millis(200);
                while output.flush().unwrap() != Flush::Complete {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                let neutral_frame = target
                    .next(Duration::from_millis(100))
                    .expect("neutral frame was not delivered");
                assert!(
                    neutral_frame
                        .events
                        .iter()
                        .any(|event| event.kind == EV_KEY && event.value == 0)
                );
                assert!(neutral(
                    &target.raw,
                    setup.device.touch_capacity().is_some()
                ));
                write_json("neutral", &true);
                drop(target);
                drop(output);
                input.raw.ungrab().unwrap();
                return;
            }
            Err(error) => panic!("forwarding health failed: {error}"),
        }
        if let Some(frame) = input.next(Duration::from_millis(50)) {
            sequence = frame.sequence;
            router.push(frame.clone(), None, now()).unwrap();
            assert!(deliver(&mut router, &mut output, now()));
            assert_eq!(
                target.next(Duration::from_millis(100)).unwrap().events,
                frame.events
            );
            if !witnessed {
                assert!(held(&target.raw, setup.device.touch_capacity().is_some()));
                write_json("held", &true);
                witnessed = true;
            }
        }
    }
}

fn held(raw: &RawDevice, touch: bool) -> bool {
    let keys = raw.get_key_state().unwrap();
    if !touch {
        return keys.contains(KeyCode::KEY_LEFTMETA) && keys.contains(KeyCode::KEY_A);
    }
    let mut slots = [0i32; 11];
    slots[0] = i32::from(ABS_MT_TRACKING_ID);
    // SAFETY: owned event fd; initialized axis selector and ten slot outputs.
    unsafe { get_slots(raw.as_raw_fd(), &mut slots) }.unwrap();
    keys.contains(KeyCode::BTN_TOUCH) && slots[1] == 42
}
fn neutral(raw: &RawDevice, touch: bool) -> bool {
    if raw.get_key_state().unwrap().iter().next().is_some() {
        return false;
    }
    if !touch {
        return true;
    }
    let mut slots = [0i32; 11];
    slots[0] = i32::from(ABS_MT_TRACKING_ID);
    // SAFETY: same bounded slot query as the held-state check.
    unsafe { get_slots(raw.as_raw_fd(), &mut slots) }.unwrap();
    slots[1..].iter().all(|id| *id < 0)
}

fn open_node(path: &Path) -> RawDevice {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .unwrap();
    RawDevice::from_fd(file.into()).unwrap()
}
fn signal(pid: u32, signal: i32) {
    // All PIDs come from this fixture's owned parent/child binding, never input
    // discovery or a production process. Files remain private to the VM root.
    assert_eq!(unsafe { libc::kill(pid as i32, signal) }, 0);
}
struct Parent(Child);
impl Drop for Parent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn gone(path: &Path, deadline: Instant) {
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "resource remained live: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn gate(touch: bool, failure: &str, index: usize) {
    let registry = authorized();
    let path = PathBuf::from(format!("/run/prek-recovery/{index}"));
    fs::create_dir_all(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    // The external harness owns only the synthetic source generator. The real
    // supervising parent is a separate exec, inheriting no input descriptors.
    let original = fixture(touch);
    let device = fixture(touch);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let source_path = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(path) = source.event_nodes().unwrap().first()
                && path.exists()
            {
                break path.clone();
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    };
    let setup = Setup {
        source: source_path.clone(),
        marker: source.marker().into(),
        device,
    };
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path.join("setup"))
        .unwrap();
    serde_json::to_writer(file, &setup).unwrap();
    let mut launch = command("parent");
    launch.env("PREK_RECOVERY_DIRECTORY", &path);
    let mut parent = Parent(launch.spawn().unwrap());
    let prepared: Prepared = read_json(&path.join("prepared"));
    let mut probe = open_node(&source_path);
    assert_eq!(
        probe.grab().unwrap_err().raw_os_error(),
        Some(libc::EBUSY),
        "worker did not own the source grab"
    );
    assert_eq!(
        registry.recover_orphans().unwrap(),
        0,
        "live output metadata was reaped"
    );
    for entry in fs::read_dir(format!("/proc/{}/fd", parent.0.id())).unwrap() {
        let target = fs::read_link(entry.unwrap().path()).unwrap();
        assert!(!target.starts_with("/dev/input") && target != Path::new("/dev/uinput"));
    }
    let pressed = if touch {
        vec![
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 42),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
        ]
    } else {
        vec![(EV_KEY, 125, 1), (EV_KEY, 30, 1)]
    };
    send(&mut source, 1, &pressed);
    assert!(read_json::<bool>(&path.join("held")));
    let output = open_node(&prepared.output);
    assert!(held(&output, touch));
    match failure {
        "orderly" => signal(parent.0.id(), libc::SIGTERM),
        "worker" => signal(prepared.worker.pid(), libc::SIGKILL),
        "parent" => signal(parent.0.id(), libc::SIGKILL),
        "stall" => signal(prepared.worker.pid(), libc::SIGSTOP),
        _ => unreachable!(),
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    gone(&prepared.output, deadline);
    assert_eq!(
        output.get_key_state().unwrap_err().raw_os_error(),
        Some(libc::ENODEV)
    );
    let status = parent.0.wait().unwrap();
    if failure == "parent" {
        assert!(!status.success());
        // Device destruction occurs during exit, before pidfd exit readiness.
        // A missing node alone must never acknowledge the worker's teardown.
        let removed = loop {
            let removed = registry.recover_orphans().unwrap();
            if removed != 0 {
                break removed;
            }
            assert!(
                Instant::now() < deadline,
                "worker death was not confirmed in time"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(removed, 1);
    } else {
        assert!(status.success());
        let finished: Finished = read_json(&path.join("finished"));
        if failure == "orderly" {
            assert!(read_json::<bool>(&path.join("neutral")));
            assert!(finished.success);
            assert_eq!(finished.reason, "Requested");
            assert_eq!(finished.removed, 0);
        } else {
            assert_eq!(finished.removed, 1);
            assert!(!finished.success);
            if failure == "stall" {
                assert!(finished.forced);
                assert_eq!(finished.reason, "Stalled");
            }
        }
    }
    probe
        .grab()
        .expect("failed worker retained its original grab");
    assert!(
        held(&probe, touch),
        "recovery invented releases on the physical source"
    );
    let released = if touch {
        vec![
            (EV_KEY, 0x14a, 0),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
        ]
    } else {
        vec![(EV_KEY, 30, 0), (EV_KEY, 125, 0)]
    };
    send(&mut source, 2, &released);
    assert!(neutral(&probe, touch));
    probe.ungrab().unwrap();
    drop(source);
    assert_eq!(fs::read_dir("/run/prek/devices").unwrap().count(), 0);
    println!(
        "kernel recovery: {} / {failure} ... ok",
        if touch { "touch" } else { "keyboard" }
    );
}
fn main() {
    if std::env::var("PREK_ISOLATED_ROUTING").as_deref() != Ok("1") {
        println!("kernel recovery gates require the explicitly isolated VM; skipped");
        return;
    }
    match std::env::var("PREK_RECOVERY_ROLE").as_deref() {
        Ok("parent") => parent(),
        Ok("worker") => worker(),
        Err(_) => {
            for (index, (touch, failure)) in [false, true]
                .into_iter()
                .flat_map(|touch| {
                    ["orderly", "worker", "parent", "stall"]
                        .into_iter()
                        .map(move |failure| (touch, failure))
                })
                .enumerate()
            {
                gate(touch, failure, index);
            }
            println!("8 isolated kernel recovery gates passed");
        }
        _ => panic!("invalid recovery fixture role"),
    }
}
