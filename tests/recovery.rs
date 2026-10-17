//! Kernel/process recovery gates. Ordinary cargo test skips this executable's
//! device cases; the user-launched fixed probe supplies private run bindings.
#![cfg(target_os = "linux")]

#[path = "support/kernel.rs"]
mod kernel_fixture;
#[path = "support/recovery_control.rs"]
mod recovery_control;
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
use recovery_control::{Directory, Process};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

nix::ioctl_read_buf!(get_slots, b'E', 0x0a, libc::c_int);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    source: PathBuf,
    marker: String,
    device: Device,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    output: PathBuf,
    marker: String,
    worker: WorkerIdentity,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Finished {
    removed: usize,
    reason: String,
    forced: bool,
    success: bool,
}

fn directory() -> Directory {
    Directory::from_env().expect("fixed probe must bind a trusted private control directory")
}
fn write_json(name: &str, value: &impl Serialize) {
    directory().write(name, value).unwrap();
}
fn read_json<T: for<'a> Deserialize<'a>>(directory: &Directory, name: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(value) = directory.read(name).unwrap() {
            return value;
        }
        assert!(Instant::now() < deadline, "fixture did not publish {name}");
        thread::sleep(Duration::from_millis(10));
    }
}
fn command(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env("PREK_RECOVERY_ROLE", role);
    if role == "parent" {
        let parent = std::process::id();
        // SAFETY: only async-signal-safe parent-death binding before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() as u32 != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
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
    let setup: Setup = read_json(&directory(), "setup");
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

fn open_node(path: &Path, marker: &str) -> RawDevice {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .unwrap();
    let raw = RawDevice::from_fd(file.into()).unwrap();
    assert_eq!(raw.physical_path(), Some(marker));
    assert!(marker.starts_with("prek/test/"));
    raw
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
fn gate(base: &Directory, touch: bool, failure: &str, index: usize) {
    let registry = authorized();
    let name = format!("case-{index}");
    let control = base.child(&name).unwrap();
    let identity = control.identity().unwrap();
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
    control.write("setup", &setup).unwrap();
    let mut launch = command("parent");
    launch.env("PREK_RECOVERY_DIRECTORY", control.path());
    launch.env("PREK_RECOVERY_IDENTITY", identity.encode());
    let mut parent = Parent(launch.spawn().unwrap());
    let parent_process = Process::pin(parent.0.id(), None).unwrap();
    let prepared: Prepared = read_json(&control, "prepared");
    let birth = serde_json::to_value(prepared.worker).unwrap()["birth"]
        .as_u64()
        .unwrap();
    let worker_process = Process::pin(prepared.worker.pid(), Some(birth)).unwrap();
    let mut probe = open_node(&source_path, &setup.marker);
    assert_eq!(
        probe.grab().unwrap_err().raw_os_error(),
        Some(libc::EBUSY),
        "worker did not own the source grab"
    );
    assert_eq!(
        registry.recover_orphan(prepared.worker).unwrap(),
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
    assert!(read_json::<bool>(&control, "held"));
    let output = open_node(&prepared.output, &prepared.marker);
    assert!(held(&output, touch));
    match failure {
        "orderly" => parent_process.signal(libc::SIGTERM).unwrap(),
        "worker" => worker_process.signal(libc::SIGKILL).unwrap(),
        "parent" => parent_process.signal(libc::SIGKILL).unwrap(),
        "stall" => worker_process.signal(libc::SIGSTOP).unwrap(),
        _ => unreachable!(),
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    gone(&prepared.output, deadline);
    // Unlinking the event node can precede completion of handler teardown.
    // Require the retained descriptor to become unusable within the same
    // recovery deadline; a missing pathname alone proves too little.
    loop {
        match output.get_key_state() {
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::ENODEV));
                break;
            }
            Ok(_) => {
                assert!(
                    Instant::now() < deadline,
                    "removed device still accepts state queries"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    let status = parent.0.wait().unwrap();
    if failure == "parent" {
        assert!(!status.success());
        // Device destruction occurs during exit, before pidfd exit readiness.
        // A missing node alone must never acknowledge the worker's teardown.
        let removed = loop {
            let removed = registry.recover_orphan(prepared.worker).unwrap();
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
        let finished: Finished = read_json(&control, "finished");
        if failure == "orderly" {
            assert!(read_json::<bool>(&control, "neutral"));
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
    drop(probe);
    drop(source);
    for marker in [&setup.marker, &prepared.marker] {
        assert_eq!(
            prek::kernel::import_metadata(marker, &mut Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
    control.cleanup().unwrap();
    base.remove_child(&name, identity).unwrap();
    println!(
        "kernel recovery: {} / {failure} ... ok",
        if touch { "touch" } else { "keyboard" }
    );
}
fn main() {
    let cases: Vec<_> = [false, true]
        .into_iter()
        .flat_map(|touch| {
            ["orderly", "worker", "parent", "stall"]
                .into_iter()
                .map(move |failure| (touch, failure))
        })
        .collect();
    let names: Vec<_> = cases
        .iter()
        .map(|(touch, failure)| {
            format!(
                "recovery::{}::{failure}",
                if *touch { "touch" } else { "keyboard" }
            )
        })
        .collect();
    if std::env::args().nth(1).as_deref() == Some("--inventory") {
        println!("PREK_KERNEL_RECOVERY_INVENTORY_V1");
        println!("{}", serde_json::to_string(&names).unwrap());
        return;
    }
    if std::env::var("PREK_ISOLATED_ROUTING").as_deref() != Ok("1") {
        println!("kernel recovery gates require the user-launched fixed isolated runner; skipped");
        return;
    }
    authorized();
    match std::env::var("PREK_RECOVERY_ROLE").as_deref() {
        Ok("parent") => parent(),
        Ok("worker") => worker(),
        Err(_) => {
            let base = directory();
            for (index, (touch, failure)) in cases.into_iter().enumerate() {
                gate(&base, touch, failure, index);
            }
            println!(
                "PREK_KERNEL_RECOVERY_RESULT={}",
                serde_json::json!({"version":1,"passed":8,"tests":names})
            );
        }
        _ => panic!("invalid recovery fixture role"),
    }
}
