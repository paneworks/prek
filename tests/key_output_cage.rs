#![cfg(target_os = "linux")]
use prek::{
    input::{EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source},
    ipc::monotonic,
    keys::{
        Owner, SourceKey,
        output::{Claim, Output, Progress, Sink},
    },
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[path = "key_output_cage/worker.rs"]
mod worker;

struct Keyboard {
    child: Child,
    source: Source,
    path: PathBuf,
    trace: fs::File,
    pending: Option<Frame>,
    blocked: bool,
}
impl Keyboard {
    fn open(output: &Path, socket: &Path, device: u64) -> Self {
        let path = output.join(format!("key-output-{device}.keys"));
        fs::write(&path, "").unwrap();
        let child = Command::new(std::env::var_os("PREK_CAGE_KEYBOARD").unwrap())
            .args([
                socket.as_os_str(),
                path.as_os_str(),
                "--persistent".as_ref(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(output.join(format!("key-output-{device}.log"))).unwrap())
            .spawn()
            .unwrap();
        let mut keyboard = Self {
            child,
            source: source(device),
            path,
            trace: fs::File::create(output.join(format!("key-output-{device}.jsonl"))).unwrap(),
            pending: None,
            blocked: false,
        };
        keyboard.ack(b"ready\n").unwrap();
        keyboard
    }
    fn ack(&mut self, expected: &[u8]) -> io::Result<()> {
        let until = Instant::now() + Duration::from_secs(3);
        let stdout = self.child.stdout.as_mut().unwrap();
        for &wanted in expected {
            loop {
                if Instant::now() >= until {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "keyboard acknowledgment timeout",
                    ));
                }
                let mut fd = nix::libc::pollfd {
                    fd: stdout.as_raw_fd(),
                    events: nix::libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll receives one initialized descriptor owned by this child.
                let ready = unsafe { nix::libc::poll(&mut fd, 1, 10) };
                if ready < 0 {
                    return Err(io::Error::last_os_error());
                }
                if ready > 0 {
                    break;
                }
            }
            let mut byte = [0];
            stdout.read_exact(&mut byte)?;
            if byte[0] != wanted {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "wrong keyboard acknowledgment",
                ));
            }
        }
        Ok(())
    }
    fn send(&mut self, sequence: u64, edges: &[(u16, i32)]) {
        let events = edges
            .iter()
            .map(|&(code, value)| RawEvent {
                kind: EV_KEY,
                code,
                value,
            })
            .chain([RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }])
            .collect();
        self.enqueue(&Frame {
            source: self.source.clone(),
            sequence,
            at: monotonic().unwrap(),
            events,
        })
        .unwrap();
        while self.flush().unwrap() == Progress::Pending {}
    }
}
impl Sink for Keyboard {
    fn source(&self) -> &Source {
        &self.source
    }
    fn supports_key(&self, code: u16) -> bool {
        (1..768).contains(&code)
    }
    fn enqueue(&mut self, frame: &Frame) -> io::Result<()> {
        assert!(self.pending.is_none());
        assert_eq!(frame.source, self.source);
        assert!(
            frame.events.iter().all(|event| event.kind == EV_SYN
                || event.kind == EV_KEY && matches!(event.value, 0 | 1))
        );
        serde_json::to_writer(&mut self.trace, &json!({"event":"enqueue", "frame":frame}))?;
        writeln!(self.trace)?;
        self.pending = Some(frame.clone());
        self.blocked = true;
        Ok(())
    }
    fn flush(&mut self) -> io::Result<Progress> {
        if self.blocked {
            self.blocked = false;
            return Ok(Progress::Pending);
        }
        let frame = self.pending.take().unwrap();
        let keys = frame
            .events
            .iter()
            .filter(|event| event.kind == EV_KEY)
            .map(|event| format!("{} {}\n", event.code, event.value))
            .collect::<String>();
        fs::write(&self.path, keys)?;
        self.child.stdin.as_mut().unwrap().write_all(b"\n")?;
        self.child.stdin.as_mut().unwrap().flush()?;
        self.ack(b"done\n")?;
        serde_json::to_writer(
            &mut self.trace,
            &json!({"event":"producer_roundtrip", "sequence":frame.sequence}),
        )?;
        writeln!(self.trace)?;
        Ok(Progress::Complete)
    }
    fn abort(&mut self) -> io::Result<Vec<RawEvent>> {
        assert!(self.pending.take().is_some());
        writeln!(self.trace, "{{\"event\":\"abort_unwritten\"}}")?;
        Ok(Vec::new())
    }
}
impl Drop for Keyboard {
    fn drop(&mut self) {
        self.child.stdin.take();
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            if Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn source(device: u64) -> Source {
    Source {
        device,
        generation: 1,
        seat: "seat0".into(),
    }
}
fn remap(serial: u64, device: u64, code: u16) -> Claim {
    Claim {
        owner: Owner::Physical(SourceKey {
            source: source(device),
            code,
        }),
        origin: Some(source(device)),
        configuration: 1,
        serial,
    }
}
fn chord(id: u64, device: u64) -> Claim {
    Claim {
        owner: Owner::Synthetic {
            seat: "seat0".into(),
            generation: 1,
            id,
        },
        origin: Some(source(device)),
        configuration: 1,
        serial: id,
    }
}
fn drain(output: &mut Output<Keyboard>) {
    for _ in 0..16 {
        if output.flush().unwrap() == Progress::Complete {
            return;
        }
    }
    panic!("owned key output never drained");
}
fn phase(output: &Path, index: usize, text: &str, keys: &[u16]) {
    phase_service(output, index, text, keys, || {});
}
fn phase_service(output: &Path, index: usize, text: &str, keys: &[u16], mut service: impl FnMut()) {
    let after = monotonic().unwrap().as_nanos();
    let until = Instant::now() + Duration::from_secs(3);
    let receipt = loop {
        service();
        let mut bytes = String::new();
        fs::File::open(output.join("receipts.jsonl"))
            .unwrap()
            .take(4 * 1024 * 1024 + 1)
            .read_to_string(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= 4 * 1024 * 1024);
        let current = bytes
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|row| {
                row["event"] == "sync"
                    && row["requested_ns"]
                        .as_u64()
                        .is_some_and(|at| u128::from(at) > after)
            });
        if let Some(row) = current {
            break row;
        }
        assert!(
            Instant::now() < until,
            "missing post-producer application sync"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(receipt["text"], text);
    assert_eq!(receipt["keys"], json!(keys));
    assert_eq!(receipt["contacts"], json!([]));
    assert_eq!(
        receipt["modifiers"],
        json!({"depressed":u32::from(keys.iter().any(|code| matches!(code, 42 | 54))),"latched":0,"locked":0,"group":0})
    );
    fs::write(
        output.join(format!("key-output-phase-{index}.json")),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    fs::write(output.join(format!("key-output-ready-{index}")), b"ready").unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while !output.join(format!("key-output-captured-{index}")).exists() {
        service();
        assert!(
            Instant::now() < until,
            "key-output capture was not acknowledged"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "owned local Cage/nested Hyprland runner only"]
fn local_cage_generated_key_output() {
    assert_eq!(std::env::var("PREK_LOCAL_CAGE").as_deref(), Ok("1"));
    let output = PathBuf::from(std::env::var_os("PREK_CAGE_OUTPUT").unwrap());
    let report: Value =
        serde_json::from_slice(&fs::read(output.join("nested.json")).unwrap()).unwrap();
    let socket = Path::new(report["socket"].as_str().unwrap());
    assert_eq!(
        socket,
        Path::new(&std::env::var_os("PREK_CAGE_KEY_SOCKET").unwrap())
    );
    assert!(
        socket.is_absolute()
            && socket.parent() == Some(Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()))
    );
    let mut physical = Keyboard::open(&output, socket, 900);
    let mut generated = Output::new(Keyboard::open(&output, socket, 100)).unwrap();
    physical.send(1, &[(42, 1)]);
    phase(&output, 0, "bA", &[42]);
    let a = remap(1, 1, 30);
    let b = remap(2, 2, 31);
    generated
        .hold(a.clone(), &[42], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    generated
        .hold(b.clone(), &[42], monotonic().unwrap())
        .unwrap();
    generated
        .tap(chord(3, 3), &[42, 30], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    phase(&output, 1, "bAA", &[42]);
    generated.release(&a, monotonic().unwrap()).unwrap();
    generated.release(&b, monotonic().unwrap()).unwrap();
    drain(&mut generated);
    assert!(!generated.down(42));
    physical.send(2, &[(30, 1), (30, 0)]);
    phase(&output, 2, "bAAA", &[42]);
    physical.send(3, &[(42, 0)]);
    phase(&output, 3, "bAAA", &[]);
    generated
        .tap(chord(4, 3), &[30], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    generated
        .tap(chord(5, 3), &[42, 30], monotonic().unwrap())
        .unwrap();
    assert_eq!(generated.flush().unwrap(), Progress::Pending);
    generated
        .retire_source(&source(3), monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    phase(&output, 4, "bAAAa", &[]);
    generated
        .hold(remap(6, 1, 30), &[42], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    generated
        .hold(remap(7, 2, 31), &[42], monotonic().unwrap())
        .unwrap();
    generated
        .retire_source(&source(1), monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    phase(&output, 5, "bAAAa", &[42]);
    generated
        .tap(chord(8, 2), &[42, 30], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    generated
        .retire_source(&source(2), monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    phase(&output, 6, "bAAAaA", &[]);
    let sibling = chord(9, 4);
    generated
        .hold(sibling, &[42], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    let task = chord(10, 4);
    generated
        .tap(task.clone(), &[42, 30], monotonic().unwrap())
        .unwrap();
    assert_eq!(generated.flush().unwrap(), Progress::Pending);
    generated.cancel(&task, monotonic().unwrap()).unwrap();
    drain(&mut generated);
    assert!(generated.down(42));
    phase(&output, 7, "bAAAaA", &[42]);
    generated.cancel_all(monotonic().unwrap()).unwrap();
    drain(&mut generated);
    assert!(!generated.retiring());
    phase(&output, 8, "bAAAaA", &[]);
    let old = remap(11, 4, 30);
    generated
        .hold(old.clone(), &[42], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    generated.cancel(&old, monotonic().unwrap()).unwrap();
    drain(&mut generated);
    generated
        .hold(remap(12, 4, 30), &[42], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    assert_eq!(
        generated
            .release(&old, monotonic().unwrap())
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        generated
            .repeat(&old, monotonic().unwrap())
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        generated
            .cancel(&old, monotonic().unwrap())
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    physical.send(4, &[(30, 1), (30, 0)]);
    phase(&output, 9, "bAAAaAA", &[42]);
    generated.cancel_all(monotonic().unwrap()).unwrap();
    drain(&mut generated);
    generated
        .tap(chord(13, 4), &[30], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    phase(&output, 10, "bAAAaAAa", &[]);
    for (id, producer) in [(14, 7), (15, 8)] {
        generated
            .hold(
                Claim {
                    owner: Owner::Software {
                        seat: "seat0".into(),
                        producer: [producer, 1],
                        generation: 4,
                        id,
                    },
                    origin: None,
                    configuration: 4,
                    serial: id,
                },
                &[42],
                monotonic().unwrap(),
            )
            .unwrap();
        drain(&mut generated);
    }
    physical.send(5, &[(30, 1), (30, 0)]);
    phase(&output, 11, "bAAAaAAaA", &[42]);
    generated
        .retire_producer([7, 1], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    physical.send(6, &[(30, 1), (30, 0)]);
    phase(&output, 12, "bAAAaAAaAA", &[42]);
    physical.send(7, &[(42, 1)]);
    generated
        .retire_producer([8, 1], monotonic().unwrap())
        .unwrap();
    drain(&mut generated);
    physical.send(8, &[(30, 1), (30, 0)]);
    phase(&output, 13, "bAAAaAAaAAA", &[42]);
    physical.send(9, &[(42, 0), (30, 1), (30, 0)]);
    generated.retire(monotonic().unwrap()).unwrap();
    drain(&mut generated);
    assert!(generated.settled());
    assert!(generated.into_sink().is_ok());
    drop(physical);
    phase(&output, 14, "bAAAaAAaAAAa", &[]);
    fs::write(output.join("key-output-runtime-result.json"), serde_json::to_vec(&json!({"status":"passed","phases":15,
        "source":"two_owned_wayland_virtual_keyboards","production_route_tested":false,"kernel_input_tested":false,
        "unmodified_stock_acceptance":false,"test_backpressure":true,"software_owner_tested":true,
        "software_authorization_tested":false,"software_repeat_rendering_tested":false})).unwrap()).unwrap();
}
