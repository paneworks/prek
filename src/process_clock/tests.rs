use super::*;
use std::{
    collections::VecDeque,
    process::{Command, Stdio},
};

struct NativeSocket {
    root: std::path::PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl NativeSocket {
    fn new() -> Self {
        use std::{
            fs,
            io::{Read, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
        };
        let root = std::env::temp_dir().join(format!("prek-clock-socket-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(root.join("hypr/clock")).unwrap();
        for directory in [root.join("hypr"), root.join("hypr/clock")] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let listener = UnixListener::bind(root.join("hypr/clock/.socket.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut bytes = [0; 1024];
                        let count = stream.read(&mut bytes).unwrap_or(0);
                        if count == 0 {
                            continue;
                        }
                        seen.fetch_add(1, Ordering::Relaxed);
                        let reply = if &bytes[..count] == b"j/version" {
                            "{\"version\":\"0.56.2\"}"
                        } else {
                            "{\"keyboards\":[],\"mice\":[],\"touch\":[],\"switches\":[],\"tablets\":[]}"
                        };
                        let _ = stream.write_all(reply.as_bytes());
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("native test listener: {error}"),
                }
            }
        });
        Self {
            root,
            stop,
            requests,
            thread: Some(thread),
        }
    }
    fn client(&self) -> crate::hyprland::Client {
        // Read-only effective process UID.
        let uid = unsafe { nix::libc::geteuid() };
        crate::hyprland::Client::open(
            &self.root,
            "clock",
            uid,
            &std::path::PathBuf::from(format!("/proc/{}/exe", std::process::id())),
        )
        .unwrap()
    }
}
impl Drop for NativeSocket {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn ns(value: u64) -> Duration {
    Duration::from_nanos(value)
}
fn sample_values(epoch: &Mutex<Epoch>, values: [u64; 3]) -> io::Result<Duration> {
    let mut values = VecDeque::from(values.map(ns));
    let mut index = 0;
    let result = sample(epoch, |clock| {
        assert_eq!(
            clock,
            if index == 1 {
                ClockId::CLOCK_BOOTTIME
            } else {
                ClockId::CLOCK_MONOTONIC
            }
        );
        index += 1;
        Ok(values.pop_front().unwrap())
    });
    if result.is_ok() {
        assert_eq!(index, 3);
    }
    result
}

#[test]
fn brackets_preserve_monotonic_time_across_scheduling_jitter_and_negative_offsets() {
    for offset in [-100_i128, 0, 100] {
        let mut epoch = Epoch::new();
        for (before, after) in [(200, 210), (210, 500), (501, 502), (502, 502)] {
            let boot = (i128::from(before) + offset) as u64;
            assert_eq!(
                epoch.observe(ns(before), ns(boot), ns(after)).unwrap(),
                ns(after)
            );
        }
        assert_eq!(epoch.offset, Some((offset, offset)));
    }
}

#[test]
fn confirmed_offset_steps_invalidate_the_epoch_permanently_even_if_the_old_offset_returns() {
    for offset in [0, 50, 1_000_000_000] {
        let epoch = Mutex::new(Epoch::new());
        sample_values(&epoch, [100, 100 + offset, 100]).unwrap();
        assert!(sample_values(&epoch, [101, 102 + offset, 101]).is_err());
        let mut called = false;
        let error = sample(&epoch, |_| {
            called = true;
            Ok(ns(102))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(!called);
    }
}

#[test]
fn offset_intervals_are_intersected_and_cannot_be_widened_by_a_delayed_sample() {
    let epoch = Mutex::new(Epoch::new());
    sample_values(&epoch, [100, 105, 110]).unwrap();
    sample_values(&epoch, [110, 110, 110]).unwrap();
    assert_eq!(epoch.lock().unwrap().offset, Some((0, 0)));
    sample_values(&epoch, [111, 112, 1_000]).unwrap();
    assert_eq!(epoch.lock().unwrap().offset, Some((0, 0)));
    assert!(sample_values(&epoch, [1_000, 1_001, 1_000]).is_err());
}

#[test]
fn ambiguous_samples_do_not_fabricate_a_suspend_but_cannot_restore_a_failed_epoch() {
    let epoch = Mutex::new(Epoch::new());
    sample_values(&epoch, [100, 105, 110]).unwrap();
    sample_values(&epoch, [110, 115, 120]).unwrap();
    assert!(!epoch.lock().unwrap().failed);
    assert!(sample_values(&epoch, [120, 130, 120]).is_err());
    assert!(sample_values(&epoch, [120, 120, 120]).is_err());
}

#[test]
fn backwards_and_incoherent_samples_or_query_errors_never_renew_authority() {
    for values in [[110, 111, 109], [99, 100, 100], [100, 99, 100]] {
        let epoch = Mutex::new(Epoch::new());
        sample_values(&epoch, [100, 100, 100]).unwrap();
        assert!(sample_values(&epoch, values).is_err());
        assert!(epoch.lock().unwrap().failed);
    }
    for fail_at in 0..3 {
        let epoch = Mutex::new(Epoch::new());
        let mut calls = 0;
        let error = sample(&epoch, |_| {
            let index = calls;
            calls += 1;
            if index == fail_at {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(ns(100))
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(epoch.lock().unwrap().failed);
        assert!(sample(&epoch, |_| panic!("failed clock was queried again")).is_err());
    }
}

#[test]
fn duration_extremes_and_equal_samples_do_not_overflow_or_invent_movement() {
    for (mono, boot) in [
        (Duration::ZERO, Duration::MAX),
        (Duration::MAX, Duration::ZERO),
    ] {
        let mut epoch = Epoch::new();
        assert_eq!(epoch.observe(mono, boot, mono).unwrap(), mono);
        assert_eq!(epoch.observe(mono, boot, mono).unwrap(), mono);
    }
}

#[test]
fn healthy_runtime_clocks_and_concurrent_samples_share_one_monotonic_epoch() {
    let before = monotonic().unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                let mut last = monotonic().unwrap();
                for _ in 0..100 {
                    let next = monotonic().unwrap();
                    assert!(next >= last);
                    last = next;
                }
                last
            })
        })
        .collect();
    for thread in threads {
        assert!(thread.join().unwrap() >= before);
    }
    assert!(crate::ipc::monotonic().unwrap() >= before);
    assert!(crate::linux::monotonic().unwrap() >= before);
}

#[test]
fn isolated_process_epoch_invalidation_fences_io_and_reaps_owned_children() {
    const CASE: &str = "process_clock::tests::isolated_process_epoch_invalidation_fences_io_and_reaps_owned_children";
    const MODE: &str = "PREK_TEST_CLOCK_EPOCH_CHILD";
    let child = |mode: &str| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", CASE, "--test-threads=1"])
            .env(MODE, mode);
        command
    };
    match std::env::var(MODE).as_deref() {
        Ok("launcher-worker") => {
            assert!(crate::ipc::monotonic().is_ok());
            return;
        }
        Ok("worker") => {
            std::thread::sleep(Duration::from_secs(20));
            return;
        }
        Ok("owner") => {}
        _ => {
            let output = child("owner").output().unwrap();
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
    }
    let mut worker_command = child("worker");
    worker_command.stdout(Stdio::null()).stderr(Stdio::null());
    let worker = crate::supervisor::Supervisor::spawn(worker_command).unwrap();
    let identity = worker.worker_identity();
    assert!(!identity.confirmed_dead().unwrap());
    let native = NativeSocket::new();
    let client = native.client();
    let (mut sender, mut receiver) = crate::ipc::Channel::pair().unwrap();
    let now = crate::ipc::monotonic().unwrap();
    assert!(sender.send(&true, now).unwrap());
    {
        let mut epoch = PROCESS.lock().unwrap();
        let before = epoch.last.unwrap() + ns(1);
        let boot = before.as_nanos() as i128 + epoch.offset.unwrap().1 + 1;
        let boot = Duration::new(
            (boot / 1_000_000_000).try_into().unwrap(),
            (boot % 1_000_000_000).try_into().unwrap(),
        );
        assert!(epoch.observe(before, boot, before).is_err());
    }
    assert!(crate::ipc::monotonic().is_err());
    assert!(crate::linux::monotonic().is_err());
    struct Sink(usize);
    impl crate::linux::InputSink for Sink {
        fn push(&mut self, _: crate::replay::Record) -> io::Result<()> {
            self.0 += 1;
            Ok(())
        }
        fn deadline(&self) -> Option<Duration> {
            None
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0 += 1;
            Ok(())
        }
    }
    let mut sink = Sink(0);
    let error = crate::linux::observe(&[], Some(Duration::ZERO), &mut sink).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    assert_eq!(sink.0, 0);
    assert!(sender.send(&true, now).is_err());
    assert!(receiver.receive::<bool>().is_err());
    assert!(client.inventory().is_err());
    assert_eq!(
        native.requests.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(worker.deadline().is_err());
    assert!(crate::supervisor::Supervisor::spawn(child("worker")).is_err());
    drop(worker);
    assert!(identity.confirmed_dead().unwrap());
    crate::session_launcher::run(|| child("launcher-worker")).unwrap();
    assert!(crate::ipc::monotonic().is_err());
}
