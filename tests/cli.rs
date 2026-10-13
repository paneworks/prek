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
struct Directory(PathBuf);
impl Directory {
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
