use prek::{
    Button, Gesture,
    config::{Action, Config, WindowAction},
    input::{
        Change, Device, EV_KEY, EV_SYN, Frame, KeyState, NormalizedFrame, RawEvent, SYN_REPORT,
        Source,
    },
    replay::{Observation, Record, write_record},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn routed_replay_cli_checks_downstream_frames_without_opening_recorded_devices() {
    use prek::{
        context::{Binding, SessionKind, Snapshot, State},
        replay::routed::{Outcome as RoutedOutcome, Record as RoutedRecord},
        router::Scheduled,
        transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity},
    };
    let directory = Directory::new();
    let path = directory.0.join("routing.jsonl");
    let config = directory.0.join("routing.lua");
    fs::write(&config, "local p=require('prek');return p.config.new{mode='route',policy={allowed_devices={'keys'}},devices={keys={keys={'KEY_A'}}}}").unwrap();
    let source = Source {
        device: 7,
        generation: 1,
        seat: "seat0".into(),
    };
    let device = Device {
        source: source.clone(),
        identity: "offline-keys".into(),
        name: "Offline keys".into(),
        path: "/no/such/device".into(),
        physical: "offline".into(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let descriptor = Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 1,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    let consumer = ConsumerEvidence {
        consumer: Consumer::IsolatedReceiver { pid: 1, start: 1 },
        marker: descriptor.marker.clone(),
        name: descriptor.name.clone(),
        class: descriptor.class,
        seat: "seat0".into(),
        settings: BTreeMap::new(),
        at: Duration::ZERO,
    };
    let input = |input, now| RoutedRecord::Input {
        now: Duration::from_millis(now),
        input: Box::new(input),
    };
    let mut records = vec![
        RoutedRecord::Header { version: 1 },
        input(
            Record::Context {
                at: Duration::ZERO,
                binding: Binding {
                    seat: "seat0".into(),
                    session: "offline".into(),
                    uid: 1000,
                    compositor_pid: 42,
                    compositor_start: 1,
                    kind: SessionKind::User,
                },
                snapshot: Box::new(Snapshot {
                    generation: 1,
                    revision: 1,
                    captured_at: Duration::ZERO,
                    state: State::Desktop,
                    outputs: vec![],
                    windows: vec![],
                    regions: vec![],
                    secure_overlay: false,
                }),
            },
            0,
        ),
        input(
            Record::Device {
                device,
                keys: BTreeSet::new(),
                contacts: vec![],
            },
            0,
        ),
        RoutedRecord::Attach {
            at: Duration::ZERO,
            descriptor: Box::new(descriptor),
            consumer,
            feedback: false,
        },
    ];
    let mut expected = vec![];
    for (ticket, at, value) in [(1, 10, 1), (2, 20, 0)] {
        let at = Duration::from_millis(at);
        let frame = Frame {
            source: source.clone(),
            sequence: ticket,
            at,
            events: vec![
                RawEvent {
                    kind: EV_KEY,
                    code: 30,
                    value,
                },
                RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ],
        };
        records.extend([
            RoutedRecord::Input {
                now: at,
                input: Box::new(Record::Frame {
                    frame: frame.clone(),
                }),
            },
            RoutedRecord::Poll { at },
            RoutedRecord::Write {
                at,
                source: source.clone(),
                ticket,
            },
        ]);
        expected.extend([
            RoutedOutcome::Offered {
                at,
                scheduled: Box::new(Scheduled {
                    ticket,
                    due: at,
                    frame,
                }),
            },
            RoutedOutcome::Delivered {
                at,
                source: source.clone(),
                ticket,
            },
        ]);
    }
    records.push(RoutedRecord::Retire {
        at: Duration::from_millis(20),
        source: source.clone(),
    });
    expected.push(RoutedOutcome::Recovery {
        at: Duration::from_millis(20),
        source,
        neutral_events: vec![],
        destroy_virtual_and_release_grab: true,
    });
    records.push(RoutedRecord::Expected {
        outcomes: expected.clone(),
    });
    let write_trace = |records: &[RoutedRecord]| {
        fs::write(
            &path,
            records
                .iter()
                .map(|record| serde_json::to_string(record).unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap()
    };
    write_trace(&records);
    let args = [
        "replay",
        path.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--routing",
        "--check",
        "--json",
    ];
    for _ in 0..2 {
        let result = run(&args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Vec<RoutedOutcome>>(&result.stdout).unwrap(),
            expected
        );
    }
    assert!(
        !run(&["replay", path.to_str().unwrap(), "--routing"])
            .status
            .success()
    );
    assert!(
        !run(&[
            "replay",
            path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--check"
        ])
        .status
        .success()
    );
    if let RoutedRecord::Expected { outcomes } = records.last_mut().unwrap() {
        outcomes.clear();
    }
    write_trace(&records);
    assert!(!run(&args).status.success());
}

#[test]
fn composed_source_cli_replay_checks_the_public_example_and_stays_unfenced_in_run() {
    use prek::{
        context::{Binding, SessionKind, Snapshot, State},
        rules::{Event, Evidence, Stage},
    };
    let directory = Directory::new();
    let config = directory.0.join("composed.lua");
    let path = directory.0.join("composed.jsonl");
    fs::write(
        &config,
        include_str!("../examples/lua/composed_recognizers.lua"),
    )
    .unwrap();
    let checked = run(&["check", "--config", config.to_str().unwrap()]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let source = Source {
        device: 7,
        generation: 1,
        seat: "seat0".into(),
    };
    let expected = Event::Matched {
        version: 1,
        rule: "composed".into(),
        id: 2,
        ordinal: 1,
        source: source.clone(),
        at: Duration::from_millis(30),
        config_generation: 1,
        session_generation: 1,
        stage: Stage::Discrete,
        action: Action::Event {
            name: "composed".into(),
        },
        evidence: Evidence::Sequence {
            steps: ["first", "last"]
                .into_iter()
                .map(|name| Evidence::Custom {
                    recognizer: name.into(),
                    data: serde_json::json!({"step":name}),
                })
                .collect(),
        },
        window: None,
        output: None,
        workspace: None,
    };
    let Record::Device { mut device, .. } = serde_json::from_str::<Record>(
        String::from_utf8(recording())
            .unwrap()
            .lines()
            .nth(1)
            .unwrap(),
    )
    .unwrap() else {
        unreachable!()
    };
    device.capabilities.insert(EV_KEY, vec![30]);
    let mut records = vec![
        Record::Header { version: 1 },
        Record::Context {
            at: Duration::ZERO,
            binding: Binding {
                seat: "seat0".into(),
                session: "recorded".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 1,
                kind: SessionKind::User,
            },
            snapshot: Box::new(Snapshot {
                generation: 1,
                revision: 1,
                captured_at: Duration::ZERO,
                state: State::Desktop,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            }),
        },
        Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: vec![],
        },
    ];
    for (sequence, value) in [(1, 1), (2, 0), (3, 1)] {
        records.push(Record::Frame {
            frame: Frame {
                source: source.clone(),
                sequence,
                at: Duration::from_millis(sequence * 10),
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            },
        });
    }
    records.push(Record::ExpectedRules {
        events: vec![expected.clone()],
    });
    let mut bytes = vec![];
    for record in records {
        write_record(&mut bytes, &record).unwrap();
    }
    fs::write(&path, bytes).unwrap();
    for _ in 0..2 {
        let output = run(&[
            "replay",
            path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--check",
            "--json",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: Vec<Event> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(actual.as_slice(), std::slice::from_ref(&expected));
    }
    let unfenced = run(&["run", "--config", config.to_str().unwrap()]);
    assert!(!unfenced.status.success());
    assert!(String::from_utf8_lossy(&unfenced.stderr).contains("trusted asynchronous"));
}

#[test]
fn custom_cli_replay_checks_actual_source_callbacks_without_executing_command_outcomes() {
    use prek::{
        context::{Binding, SessionKind, Snapshot, State},
        rules::{Event, Evidence, Stage},
    };
    let directory = Directory::new();
    let config = directory.0.join("custom.lua");
    let path = directory.0.join("custom.jsonl");
    let marker = directory.0.join("must-not-exist");
    let command = format!("printf called > {}", marker.display());
    fs::write(&config,format!(r#"
local p=require('prek');local calls=0
p.events.on('input',function()error('ordinary callback ran')end)
local trigger=p.recognizers.define('offline',{{selectors={{'keys'}},inputs={{'key'}},reply_within='100ms',wake='10ms'}},function(ticket)
 if ticket.cause=='cancel' then assert(calls==2 and ticket.reason=='completed');return nil end
 calls=calls+1
 if calls==1 then return nil end
 assert(ticket.cause=='timer' and ticket.input.sequence=='1')
 return {{phase='discrete',data={{calls=calls}}}}
end)
return p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keys'}}}},
 devices={{keys={{keys={{'KEY_A'}}}}}},rules={{{{id='offline',devices={{'keys'}},contexts={{'desktop'}},
 scope='everywhere',delivery='observe',trigger=trigger,action=p.actions.command{{argv={{'sh','-c',[==[{command}]==]}}}}}}}}}}
"#)).unwrap();
    let source = Source {
        device: u64::MAX,
        generation: 1,
        seat: "seat0".into(),
    };
    let expected = Event::Matched {
        version: 1,
        rule: "offline".into(),
        id: 1,
        ordinal: 1,
        source: source.clone(),
        at: Duration::from_millis(20),
        config_generation: 1,
        session_generation: 1,
        stage: Stage::Discrete,
        action: Action::Command {
            argv: vec!["sh".into(), "-c".into(), command],
        },
        evidence: Evidence::Custom {
            recognizer: "offline".into(),
            data: serde_json::json!({"calls":2}),
        },
        window: None,
        output: None,
        workspace: None,
    };
    let mut records = vec![
        Record::Header { version: 1 },
        Record::Context {
            at: Duration::ZERO,
            binding: Binding {
                seat: "seat0".into(),
                session: "recorded".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 1,
                kind: SessionKind::User,
            },
            snapshot: Box::new(Snapshot {
                generation: 1,
                revision: 1,
                captured_at: Duration::ZERO,
                state: State::Desktop,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            }),
        },
    ];
    let Record::Device { mut device, .. } = serde_json::from_str::<Record>(
        String::from_utf8(recording())
            .unwrap()
            .lines()
            .nth(1)
            .unwrap(),
    )
    .unwrap() else {
        unreachable!()
    };
    device.source = source.clone();
    device.capabilities.insert(EV_KEY, vec![30]);
    records.push(Record::Device {
        device,
        keys: BTreeSet::new(),
        contacts: vec![],
    });
    records.push(Record::Frame {
        frame: Frame {
            source,
            sequence: 1,
            at: Duration::from_millis(10),
            events: vec![
                RawEvent {
                    kind: EV_KEY,
                    code: 30,
                    value: 1,
                },
                RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ],
        },
    });
    records.push(Record::Tick {
        at: Duration::from_millis(20),
    });
    records.push(Record::ExpectedRules {
        events: vec![expected.clone()],
    });
    let mut bytes = vec![];
    for record in records {
        write_record(&mut bytes, &record).unwrap();
    }
    fs::write(&path, bytes).unwrap();
    let mut previous = None;
    for _ in 0..2 {
        let output = run(&[
            "replay",
            path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--check",
            "--json",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: Vec<Event> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(actual.as_slice(), std::slice::from_ref(&expected));
        if let Some(previous) = &previous {
            assert_eq!(previous, &output.stdout);
        } else {
            previous = Some(output.stdout);
        }
        assert!(!marker.exists());
    }
    let bad = fs::read_to_string(&path)
        .unwrap()
        .replace("\"calls\":2", "\"calls\":3");
    fs::write(&path, bad).unwrap();
    assert!(
        !run(&[
            "replay",
            path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--check"
        ])
        .status
        .success()
    );
    assert!(!marker.exists());
}

#[test]
fn session_help_documents_explicit_desktop_opt_ins_without_enabling_routing() {
    let output = run(&["session", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--power") && help.contains("protected-policy DPMS"));
    assert!(help.contains("--hyprland") && help.contains("protected-policy targeted Hyprland"));
    assert!(help.contains("--recognizers") && help.contains("native validated"));
    assert!(help.contains("--reconnect") && !help.contains("--reconnect-child"));
    assert!(help.contains("--logind-library"));
    let unwatched = run(&["session", "--logind-library", "/owned/library"]);
    assert!(!unwatched.status.success());
    assert!(String::from_utf8_lossy(&unwatched.stderr).contains("--reconnect"));
    let bounded = run(&["session", "--reconnect", "--duration", "1"]);
    assert!(!bounded.status.success());
    assert!(String::from_utf8_lossy(&bounded.stderr).contains("cannot be used with"));
    let unbound = run(&["session", "--reconnect-child"]);
    assert!(!unbound.status.success());
    assert!(String::from_utf8_lossy(&unbound.stderr).contains("launcher pipe"));
    let denied = run(&["session", "--recognizers"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--lua"));
}

#[test]
fn reconnecting_cli_reexecutes_only_against_its_missing_owned_endpoint_and_stops() {
    let directory = Directory::new();
    let log = directory.0.join("session-stderr");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prek"));
    command
        .args([
            "session",
            "--reconnect",
            "--instance",
            "owned-missing-instance",
            "--socket",
        ])
        .arg(directory.0.join("missing socket"))
        .args([
            "--json",
            "--lua",
            "--input",
            "--recognizers",
            "--rule-mutations",
            "--observations",
            "--context",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(fs::File::create(&log).unwrap());
    if unsafe { nix::libc::geteuid() } == 0 {
        assert!(!command.status().unwrap().success());
        assert!(
            fs::read_to_string(log)
                .unwrap()
                .contains("unprivileged user")
        );
        return;
    }
    let mut child = command.spawn().unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        if fs::read_to_string(&log)
            .unwrap()
            .matches("reconnecting with fresh process authority")
            .count()
            >= 2
        {
            break;
        }
        if child.try_wait().unwrap().is_some() || std::time::Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "owned CLI reconnect failed: {}",
                fs::read_to_string(&log).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        unsafe { nix::libc::kill(child.id() as i32, nix::libc::SIGTERM) },
        0
    );
    let until = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("owned CLI ignored termination");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn broker_route_mode_refuses_before_runtime_or_device_access() {
    let directory = Directory::new();
    let runtime = directory.0.join("broker-runtime");
    let output = run(&[
        "broker",
        "--mode",
        "route",
        "--config",
        "/missing/config.toml",
        "--compositor-executable",
        "/missing/Hyprland",
        "--logind-library",
        "/missing/libsystemd.so.0",
        "--runtime-dir",
        runtime.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("broker routing is unavailable"));
    assert!(!runtime.exists());
}

#[test]
fn t33_configured_mouse_shape_cli_replay_checks_a_hand_written_outcome() {
    use prek::{
        config::{Key, Trigger},
        gestures::Space,
        paths::ShapeMatch,
        rules::{Event, Evidence, Stage},
    };
    let directory = Directory::new();
    let source = Source {
        device: 9,
        generation: 1,
        seat: "seat0".into(),
    };
    let device = Device {
        identity: "shape-mouse".into(),
        source: source.clone(),
        name: "fixture".into(),
        path: "fixture".into(),
        physical: "fixture".into(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        virtual_device: false,
        capabilities: BTreeMap::from([(EV_KEY, vec![0x110]), (2, vec![0, 1])]),
        axes: BTreeMap::new(),
    };
    let mut config = Config::preset("desktop").unwrap();
    config.shapes.truncate(1);
    config.devices.get_mut("keyboard").unwrap().keys = vec![Key(0x110)];
    let rule = &mut config.rules[0];
    rule.enabled = true;
    rule.require_keys = vec![Key(0x110)];
    rule.keys_throughout = true;
    rule.trigger = Trigger::Shape {
        name: "v".into(),
        fingers: None,
        button: None,
    };
    let expected = Event::Matched {
        version: 1,
        rule: rule.id.clone(),
        id: 1,
        ordinal: 1,
        source: source.clone(),
        at: Duration::from_millis(80),
        config_generation: 1,
        session_generation: 0,
        stage: Stage::Discrete,
        action: rule.action.clone(),
        evidence: Evidence::Shape {
            space: Space::Relative,
            fingers: None,
            shape: ShapeMatch {
                name: "v".into(),
                confidence: 1.0,
                margin: 1.0,
            },
        },
        window: None,
        output: None,
        workspace: None,
    };
    let config_path = directory.0.join("shape.toml");
    fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
    let mut bytes = Vec::new();
    write_record(&mut bytes, &Record::Header { version: 1 }).unwrap();
    write_record(
        &mut bytes,
        &Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: vec![],
        },
    )
    .unwrap();
    for (index, (at, input)) in [
        (10, vec![(EV_KEY, 0x110, 1)]),
        (50, vec![(2, 0, 50), (2, 1, 100)]),
        (80, vec![(2, 0, 50), (2, 1, -100), (EV_KEY, 0x110, 0)]),
    ]
    .into_iter()
    .enumerate()
    {
        let mut events: Vec<_> = input
            .into_iter()
            .map(|(kind, code, value)| RawEvent { kind, code, value })
            .collect();
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        write_record(
            &mut bytes,
            &Record::Frame {
                frame: Frame {
                    source: source.clone(),
                    at: Duration::from_millis(at),
                    sequence: index as u64 + 1,
                    events,
                },
            },
        )
        .unwrap();
    }
    write_record(
        &mut bytes,
        &Record::ExpectedRules {
            events: vec![expected],
        },
    )
    .unwrap();
    let path = directory.0.join("shape.jsonl");
    fs::write(&path, bytes).unwrap();
    let result = run(&[
        "replay",
        path.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--check",
        "--json",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let events: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(events[0]["evidence"]["shape"]["name"], "v");
    let learned = directory.0.join("learned.toml");
    let result = run(&[
        "learn-shape",
        "my-v",
        "--device",
        "fixture",
        "--mouse",
        "--from-recording",
        path.to_str().unwrap(),
        "--output",
        learned.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(summary["space"], "relative");
    assert_eq!(summary["enabled_rules"], 0);
    let exported = Config::parse(&fs::read_to_string(&learned).unwrap()).unwrap();
    assert!(exported.rules.is_empty() && exported.devices.is_empty());
    assert_eq!(exported.shapes[0].name, "my-v");
    assert_eq!(exported.shapes[0].points.len(), 64);
    let learned_lua = directory.0.join("learned.lua");
    let lua_result = run(&[
        "learn-shape",
        "my-v",
        "--device",
        "fixture",
        "--mouse",
        "--from-recording",
        path.to_str().unwrap(),
        "--output",
        learned_lua.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        lua_result.status.success(),
        "{}",
        String::from_utf8_lossy(&lua_result.stderr)
    );
    assert_eq!(
        Config::parse_named(&fs::read_to_string(&learned_lua).unwrap(), "learned.lua").unwrap(),
        exported
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&learned).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let original = fs::read(&learned).unwrap();
    let failed = run(&[
        "learn-shape",
        "my-v",
        "--device",
        "fixture",
        "--mouse",
        "--from-recording",
        path.to_str().unwrap(),
        "--output",
        learned.to_str().unwrap(),
    ]);
    assert!(!failed.status.success());
    assert_eq!(fs::read(&learned).unwrap(), original);

    // Reuse the exported points in an actual configured replay, without enabling
    // any rule in the template artifact itself.
    let mut config = config.clone();
    config.shapes = exported.shapes;
    config.rules[0].trigger = Trigger::Shape {
        name: "my-v".into(),
        fingers: None,
        button: None,
    };
    fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
    let result = run(&[
        "replay",
        path.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--json",
    ]);
    assert!(result.status.success());
    let events: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(events[0]["evidence"]["shape"]["name"], "my-v");
}
#[test]
fn t42_configured_sequence_cli_replay_matches_a_hand_written_outcome() {
    use prek::{
        config::{Key, Millis, Trigger},
        rules::{Event, Evidence, Stage},
    };
    let directory = Directory::new();
    let source = Source {
        device: 7,
        generation: 1,
        seat: "seat0".into(),
    };
    let device = Device {
        identity: "sequence-key".into(),
        source: source.clone(),
        name: "fixture".into(),
        path: "fixture".into(),
        physical: "fixture".into(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let mut config = Config::preset("desktop").unwrap();
    config.rules[0].enabled = true;
    config.devices.get_mut("keyboard").unwrap().keys = vec![Key(30)];
    config.rules[0].trigger = Trigger::Sequence {
        steps: vec![
            Trigger::ButtonTaps {
                key: Key(30),
                count: 2,
            },
            Trigger::ButtonHold {
                key: Key(30),
                after: Millis(600),
                repeat: None,
            },
        ],
        timeout: Millis(5000),
    };
    let expected = Event::Matched {
        version: 1,
        rule: config.rules[0].id.clone(),
        id: 1,
        ordinal: 1,
        source: source.clone(),
        at: Duration::from_millis(900),
        config_generation: 1,
        session_generation: 0,
        stage: Stage::Discrete,
        action: config.rules[0].action.clone(),
        evidence: Evidence::Sequence {
            steps: vec![
                Evidence::Taps { count: 2 },
                Evidence::Hold { repeat: false },
            ],
        },
        window: None,
        output: None,
        workspace: None,
    };
    let mut bytes = Vec::new();
    write_record(&mut bytes, &Record::Header { version: 1 }).unwrap();
    write_record(
        &mut bytes,
        &Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: vec![],
        },
    )
    .unwrap();
    for (index, (at, value)) in [(0, 1), (50, 0), (150, 1), (200, 0), (300, 1)]
        .into_iter()
        .enumerate()
    {
        write_record(
            &mut bytes,
            &Record::Frame {
                frame: Frame {
                    source: source.clone(),
                    at: Duration::from_millis(at),
                    sequence: index as u64 + 1,
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code: 30,
                            value,
                        },
                        RawEvent {
                            kind: EV_SYN,
                            code: SYN_REPORT,
                            value: 0,
                        },
                    ],
                },
            },
        )
        .unwrap();
    }
    write_record(
        &mut bytes,
        &Record::Tick {
            at: Duration::from_millis(900),
        },
    )
    .unwrap();
    write_record(
        &mut bytes,
        &Record::ExpectedRules {
            events: vec![expected],
        },
    )
    .unwrap();
    let config_path = directory.0.join("sequence.toml");
    let path = directory.0.join("sequence.jsonl");
    fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    let result = run(&[
        "replay",
        path.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--check",
        "--json",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let events: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(events[0]["evidence"]["steps"][0]["count"], 2);
}

struct Directory(PathBuf);

#[test]
fn t94_migration_preserves_complete_presets_input_and_existing_destinations() {
    let directory = Directory::new();
    for preset in ["minimal", "desktop", "phone"] {
        let config = Config::preset(preset).unwrap();
        let input = directory.0.join(format!("{preset}.toml"));
        let output = directory.0.join(format!("{preset}.lua"));
        let legacy = toml::to_string_pretty(&config).unwrap();
        fs::write(&input, &legacy).unwrap();
        let result = run(&[
            "migrate",
            "--config",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read_to_string(&input).unwrap(), legacy);
        let source = fs::read_to_string(&output).unwrap();
        assert_eq!(
            Config::parse_named(&source, "migrated.lua").unwrap(),
            config
        );
        assert!(
            run(&["check", "--config", output.to_str().unwrap()])
                .status
                .success()
        );
        assert!(
            !run(&[
                "migrate",
                "--config",
                input.to_str().unwrap(),
                "--output",
                output.to_str().unwrap()
            ])
            .status
            .success()
        );
        assert_eq!(fs::read_to_string(&output).unwrap(), source);
        fs::write(&input, "version=999\n").unwrap();
        assert!(
            !run(&[
                "migrate",
                "--config",
                input.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--force"
            ])
            .status
            .success()
        );
        assert_eq!(fs::read_to_string(&output).unwrap(), source);
        fs::write(&input, &legacy).unwrap();
        assert!(
            run(&[
                "migrate",
                "--config",
                input.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--force"
            ])
            .status
            .success()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let original = directory.0.join("minimal.toml");
        let alias = directory.0.join("input.lua");
        symlink(&original, &alias).unwrap();
        assert!(
            !run(&[
                "migrate",
                "--config",
                alias.to_str().unwrap(),
                "--output",
                alias.to_str().unwrap(),
                "--force"
            ])
            .status
            .success()
        );
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&original).unwrap(),
            toml::to_string_pretty(&Config::preset("minimal").unwrap()).unwrap()
        );
    }
}

#[test]
fn init_defaults_to_a_private_minimal_lua_config_in_the_working_directory() {
    let directory = Directory::new();
    let binary = env!("CARGO_BIN_EXE_prek");
    let mut command = if let Some(runner) = std::env::var_os("PREK_TEST_RUNNER") {
        let mut command = Command::new(runner);
        command.arg(binary);
        command
    } else {
        Command::new(binary)
    };
    let result = command
        .arg("init")
        .current_dir(&directory.0)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let source = fs::read_to_string(directory.0.join("config.lua")).unwrap();
    assert_eq!(
        Config::parse_named(&source, "config.lua").unwrap(),
        Config::preset("minimal").unwrap()
    );
    assert!(!directory.0.join("config.toml").exists());
}
#[test]
fn t84_lua_presets_validate_through_the_real_cli_without_a_display() {
    let directory = Directory::new();
    for preset in ["minimal", "desktop", "phone"] {
        let path = directory.0.join(format!("{preset}.lua"));
        let initialized = run(&[
            "init",
            "--preset",
            preset,
            "--output",
            path.to_str().unwrap(),
        ]);
        assert!(
            initialized.status.success(),
            "{}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("require(\"prek\")")
        );
        let checked = run(&["check", "--config", path.to_str().unwrap(), "--json"]);
        assert!(
            checked.status.success(),
            "{}",
            String::from_utf8_lossy(&checked.stderr)
        );
        let checked: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap();
        assert_eq!(checked["status"], "valid");
    }
}

#[test]
fn t85_installed_api_discovery_matches_the_configuration_manifest() {
    let result = run(&["api", "--json"]);
    assert!(result.status.success());
    let actual: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let documented: serde_json::Value =
        serde_json::from_str(include_str!("../docs/lua-api.json")).unwrap();
    assert_eq!(actual, documented);
    assert_eq!(actual["runtime_api"], "partial");
    let editor = run(&["api", "--lua"]);
    assert!(editor.status.success());
    assert_eq!(editor.stdout, include_bytes!("../docs/lua/prek.lua"));
    assert!(!run(&["api", "--lua", "--json"]).status.success());
    let denied = run(&["session", "--input"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--lua"));
}

#[cfg(target_os = "linux")]
#[test]
fn t84_lua_modules_compile_through_the_isolated_cli_and_preserve_snapshots() {
    use prek::lua::{Definition, EventKind, Sources};
    let directory = Directory::new();
    let entry = directory.0.join("config.lua");
    let module = directory.lua_module("settings", "local p=require('prek'); p.events.on('ready',function()p.log.info('approved-module')end); return p.config.preset('minimal')");
    fs::write(&entry, "return require('settings')").unwrap();
    let result = run(&["check", "--config", entry.to_str().unwrap(), "--json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let approved = Sources::capture(fs::read_to_string(&entry).unwrap(), &entry).unwrap();
    let definition = Definition::compile_sources(&approved).unwrap();
    fs::write(
        &module,
        "error('changed file must never be reread by the approved runtime')",
    )
    .unwrap();
    let mut runtime = definition.instantiate().unwrap();
    assert_eq!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "approved-module"
    );
    let result = run(&["check", "--config", entry.to_str().unwrap()]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("changed file"));
    fs::write(&module, "return require('other')").unwrap();
    directory.lua_module("other", "return require('settings')");
    let result = run(&["check", "--config", entry.to_str().unwrap()]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("module cycle"));
    let example =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/lua/modular/config.lua");
    let result = run(&["check", "--config", example.to_str().unwrap(), "--json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn t89_module_capture_refuses_unsafe_files_and_permission_expansion() {
    use prek::lua::Sources;
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = Directory::new();
    let entry = directory.0.join("config.lua");
    let tree = directory.0.join("modules");
    let module = directory.lua_module("settings", "return {version=1}");
    fs::write(&entry, "return require('settings')").unwrap();
    let capture = || Sources::capture(fs::read_to_string(&entry).unwrap(), &entry);
    assert!(capture().is_ok());
    fs::set_permissions(&module, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        capture().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    fs::set_permissions(&module, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&tree, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        capture().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    fs::set_permissions(&tree, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(&module).unwrap();
    symlink(&entry, &module).unwrap();
    assert!(capture().is_err());
    fs::remove_file(&module).unwrap();
    nix::unistd::mkfifo(
        &module,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    assert!(
        capture().is_err(),
        "special files must be refused before reading"
    );
    fs::remove_file(&module).unwrap();
    let ambiguous = directory.lua_module("bad.name", "return true");
    assert!(
        capture().is_err(),
        "module filename segments cannot alias nested paths"
    );
    fs::remove_file(&ambiguous).unwrap();
    fs::remove_dir(&tree).unwrap();
    symlink(&directory.0, &tree).unwrap();
    assert!(capture().is_err(), "the module root must not be a symlink");
}

#[test]
fn t88_isolated_lua_rejects_expansion_and_recovers_for_the_next_compile() {
    let directory = Directory::new();
    let path = directory.0.join("bounded.lua");
    // Reused Lua tables occupy little arena memory but expand when decoded
    // into owned Rust strings. Neither failure may terminate the CLI parent.
    for (rules, arguments, bytes) in [(32, 32, 4096), (1024, 256, 4096)] {
        let source = format!(
            r#"
local p = require("prek")
local config = p.config.preset("minimal")
local argv = {{"fixture"}}
local argument = string.rep("x", {bytes})
for i = 2, {arguments} do argv[i] = argument end
config.rules = {{}}
for i = 1, {rules} do
    config.rules[i] = {{id = "expanded-" .. i, enabled = true,
        trigger = p.triggers.key {{key = "KEY_A"}},
        action = p.actions.command {{argv = argv}}}}
end
return config
"#
        );
        fs::write(&path, source).unwrap();
        let rejected = run(&["check", "--config", path.to_str().unwrap()]);
        assert!(!rejected.status.success());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                rejected.status.signal(),
                None,
                "parent must survive helper failure"
            );
        }
        assert!(!rejected.stderr.is_empty());
        let error = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            error.contains("model exceeds")
                || error.contains("compiler failed")
                || error.contains("timed out")
                || error.contains("memory allocation")
                || (rules == 1024 && error.contains("time limit exceeded")),
            "{error}"
        );
    }
    fs::write(&path, "return require('prek').config.preset('minimal')").unwrap();
    let checked = run(&["check", "--config", path.to_str().unwrap(), "--json"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
}

impl Directory {
    #[cfg(target_os = "linux")]
    fn lua_module(&self, name: &str, source: &str) -> PathBuf {
        use std::{
            io::Write,
            os::unix::fs::{DirBuilderExt, OpenOptionsExt},
        };
        let directory = self.0.join("modules");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join(format!("{name}.lua"));
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        path
    }

    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-fixtures")
            .join(format!(
                "{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn run(args: &[&str]) -> Output {
    let binary = env!("CARGO_BIN_EXE_prek");
    let mut command = if let Some(runner) = std::env::var_os("PREK_TEST_RUNNER") {
        let mut command = Command::new(runner);
        command.arg(binary);
        command
    } else {
        Command::new(binary)
    };
    command
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
        .output()
        .unwrap()
}

#[test]
fn t01_help_requires_no_desktop_or_input_access() {
    let result = run(&["--help"]);
    assert!(result.status.success());
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains("replay"));
    assert!(text.contains("check"));
    assert!(text.contains("doctor"));
    assert!(text.contains("run"));
    assert!(!text.contains("observe-worker"));
}

#[cfg(target_os = "linux")]
#[test]
fn t59_unbound_worker_refuses_before_configuration_or_device_access() {
    let result = run(&[
        "observe-worker",
        "--config",
        "/this/prek/test/configuration/does/not/exist",
    ]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("worker health descriptor is missing")
    );
}

#[test]
fn t05_configured_cli_replay_checks_rules_without_device_access() {
    use prek::{
        config::{Key, Trigger},
        rules::{Event, Evidence, Stage},
    };
    let dir = Directory::new();
    let path = dir.0.join("rules.jsonl");
    let config_path = dir.0.join("rules.toml");
    let mut config = Config::preset("desktop").unwrap();
    config.rules[0].enabled = true;
    config.rules[0].trigger = Trigger::ButtonTaps {
        key: Key(30),
        count: 2,
    };
    fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
    let mut bytes = Vec::new();
    let mut source = None;
    for line in String::from_utf8(recording()).unwrap().lines() {
        let mut record: Record = serde_json::from_str(line).unwrap();
        if matches!(record, Record::Expected { .. }) {
            break;
        }
        if let Record::Device { device, .. } = &mut record {
            device.capabilities.insert(EV_KEY, vec![30]);
            source = Some(device.source.clone());
        }
        write_record(&mut bytes, &record).unwrap();
    }
    write_record(
        &mut bytes,
        &Record::ExpectedRules {
            events: vec![Event::Matched {
                version: 1,
                rule: "sample_double_tap".into(),
                id: 1,
                ordinal: 1,
                source: source.unwrap(),
                at: Duration::from_millis(500),
                config_generation: 1,
                session_generation: 0,
                stage: Stage::Discrete,
                action: Action::Event {
                    name: "volume_double_tap".into(),
                },
                evidence: Evidence::Taps { count: 2 },
                window: None,
                output: None,
                workspace: None,
            }],
        },
    )
    .unwrap();
    fs::write(&path, &bytes).unwrap();
    let args = [
        "replay",
        path.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--check",
        "--json",
    ];
    let result = run(&args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let events: Vec<Event> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(events.len(), 1);
    fs::write(
        &path,
        String::from_utf8(bytes)
            .unwrap()
            .replace("\"count\":2", "\"count\":3"),
    )
    .unwrap();
    let failed = run(&args);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("rule outcome mismatch"));
}

#[test]
fn t01_route_refuses_before_any_input_takeover() {
    let dir = Directory::new();
    let path = dir.0.join("minimal.toml");
    fs::write(
        &path,
        toml::to_string_pretty(&Config::preset("minimal").unwrap()).unwrap(),
    )
    .unwrap();
    let result = run(&[
        "run",
        "--config",
        path.to_str().unwrap(),
        "--mode",
        "route",
        "--json",
    ]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("routing is unavailable"));
}
#[test]
fn t04_doctor_json_reports_unavailable_routing_without_claiming_success() {
    let dir = Directory::new();
    let path = dir.0.join("config.toml");
    let name = path.to_str().unwrap();
    fs::write(
        &path,
        toml::to_string_pretty(&Config::preset("minimal").unwrap()).unwrap(),
    )
    .unwrap();
    let result = run(&["doctor", "--config", name, "--json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["ready"], true);
    fs::write(
        &path,
        toml::to_string_pretty(&Config::preset("phone").unwrap()).unwrap(),
    )
    .unwrap();
    let result = run(&["doctor", "--config", name, "--json"]);
    assert!(!result.status.success());
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["ready"], false);
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "routing_pending")
    );
}
#[test]
fn t03_presets_check_and_preserve_existing_files() {
    let dir = Directory::new();
    let path = dir.0.join("phone.toml");
    let name = path.to_str().unwrap();
    assert!(
        run(&["init", "--preset", "phone", "--output", name])
            .status
            .success()
    );
    let original = fs::read(&path).unwrap();
    assert!(
        !run(&["init", "--preset", "minimal", "--output", name])
            .status
            .success()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    let checked = run(&["check", "--config", name, "--json"]);
    assert!(checked.status.success());
    let result: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(result["status"], "valid");
    let config = Config::parse(std::str::from_utf8(&original).unwrap()).unwrap();
    assert!(config.rules.iter().any(|rule| matches!(
        rule.action,
        Action::Hyprland {
            action: WindowAction::MoveLeftAndFollow
        }
    )));
    assert!(
        run(&["init", "--preset", "minimal", "--output", name, "--force"])
            .status
            .success()
    );
    assert!(
        Config::parse(&fs::read_to_string(&path).unwrap())
            .unwrap()
            .rules
            .is_empty()
    );
}
#[cfg(unix)]
#[test]
fn t03_force_replaces_symlink_without_modifying_its_target() {
    let dir = Directory::new();
    let target = dir.0.join("unrelated");
    let link = dir.0.join("config");
    fs::write(&target, b"keep me").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(
        run(&[
            "init",
            "--preset",
            "minimal",
            "--output",
            link.to_str().unwrap(),
            "--force"
        ])
        .status
        .success()
    );
    assert_eq!(fs::read(&target).unwrap(), b"keep me");
    assert!(
        !fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
#[test]
fn t02_invalid_json_check_has_only_stderr_diagnostics() {
    let dir = Directory::new();
    let path = dir.0.join("bad.toml");
    fs::write(&path, "version=1\nunknown=true\n").unwrap();
    let result = run(&["check", "--config", path.to_str().unwrap(), "--json"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("unknown"));
    assert!(error.contains("bad.toml"));
}

fn recording() -> Vec<u8> {
    let source = Source {
        device: 7,
        generation: 1,
        seat: "seat0".into(),
    };
    let device = Device {
        identity: "test-key".into(),
        source: source.clone(),
        name: "fixture".into(),
        path: "fixture".into(),
        physical: "fixture".into(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::new(),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let mut bytes = Vec::new();
    write_record(&mut bytes, &Record::Header { version: 1 }).unwrap();
    write_record(
        &mut bytes,
        &Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: vec![],
        },
    )
    .unwrap();
    let mut expected = Vec::new();
    for (index, (at, value)) in [(0, 1), (50, 0), (150, 1), (200, 0)]
        .into_iter()
        .enumerate()
    {
        let frame = Frame {
            source: source.clone(),
            at: Duration::from_millis(at),
            sequence: index as u64 + 1,
            events: vec![
                RawEvent {
                    kind: EV_KEY,
                    code: 30,
                    value,
                },
                RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ],
        };
        write_record(
            &mut bytes,
            &Record::Frame {
                frame: frame.clone(),
            },
        )
        .unwrap();
        expected.push(Observation::Input {
            frame: NormalizedFrame {
                raw: frame,
                changes: vec![Change::Key {
                    code: 30,
                    state: if value == 1 {
                        KeyState::Press
                    } else {
                        KeyState::Release
                    },
                }],
                contacts: vec![],
                contacts_started: vec![],
                contacts_ended: vec![],
                ended_contacts: vec![],
            },
        });
    }
    write_record(
        &mut bytes,
        &Record::Tick {
            at: Duration::from_millis(500),
        },
    )
    .unwrap();
    expected.push(Observation::Button {
        at: Duration::from_millis(500),
        gesture: Gesture::Taps {
            button: Button {
                device: 7,
                code: 30,
            },
            count: 2,
        },
    });
    write_record(
        &mut bytes,
        &Record::Expected {
            observations: expected,
        },
    )
    .unwrap();
    bytes
}

#[test]
fn t05_cli_checks_manual_double_tap_expectation_offline() {
    let dir = Directory::new();
    let path = dir.0.join("tap.jsonl");
    fs::write(&path, recording()).unwrap();
    let result = run(&["replay", path.to_str().unwrap(), "--check", "--json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let observations: Vec<Observation> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(observations.len(), 5);
    assert!(matches!(
        observations.last(),
        Some(Observation::Button {
            gesture: Gesture::Taps { count: 2, .. },
            ..
        })
    ));
    let bad = String::from_utf8(recording())
        .unwrap()
        .replace("\"count\":2", "\"count\":3");
    fs::write(&path, bad).unwrap();
    assert!(
        !run(&["replay", path.to_str().unwrap(), "--check"])
            .status
            .success()
    );
}

#[test]
fn t04_device_inventory_is_json_without_a_display() {
    let result = run(&["devices", "--json"]);
    assert!(result.status.success());
    let entries: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(
        entries
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry.get("available").is_some() && entry.get("path").is_some())
    );
}

#[cfg(unix)]
#[test]
fn t03_generated_config_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Directory::new();
    let path = dir.0.join("minimal.toml");
    assert!(
        run(&[
            "init",
            "--preset",
            "minimal",
            "--output",
            path.to_str().unwrap()
        ])
        .status
        .success()
    );
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(target_os = "linux")]
#[test]
fn t04_already_closed_stdout_is_a_clean_exit_for_json_commands() {
    use std::process::Stdio;
    fn closed_output(args: &[&str]) -> Output {
        let (reader, writer) = nix::unistd::pipe().unwrap();
        drop(reader);
        Command::new(env!("CARGO_BIN_EXE_prek"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::piped())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .output()
            .unwrap()
    }
    let dir = Directory::new();
    let config = dir.0.join("config.toml");
    fs::write(
        &config,
        toml::to_string_pretty(&Config::preset("minimal").unwrap()).unwrap(),
    )
    .unwrap();
    let replay = dir.0.join("recording.jsonl");
    fs::write(&replay, recording()).unwrap();
    for args in [
        vec!["api", "--json"],
        vec!["api", "--lua"],
        vec!["devices", "--json"],
        vec!["doctor", "--config", config.to_str().unwrap(), "--json"],
        vec!["check", "--config", config.to_str().unwrap(), "--json"],
        vec!["replay", replay.to_str().unwrap(), "--json"],
    ] {
        let output = closed_output(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::write(&config, "this is not a valid configuration").unwrap();
    let invalid = closed_output(&["check", "--config", config.to_str().unwrap(), "--json"]);
    assert!(!invalid.status.success());
    assert!(!invalid.stderr.is_empty());
}
