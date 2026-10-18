use super::*;
use crate::{
    config::{Action, Context, Delivery, Key, Scope, Selector, Trigger},
    context::{Binding, SessionKind, Snapshot, State},
    input::{EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT},
    protocol::{Capability, Epoch},
    supervisor::{Supervisor, WorkerIdentity},
    transport::{Consumer, ConsumerEvidence},
    worker::ProvidedContext,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::Instant,
};

mod control;
use crate::kernel_fixture as kernel;
#[cfg(test)]
mod models;
mod parent;
mod worker;

pub(super) const CASE: &str = "managed::selective_key_worker";
pub(super) const INVENTORY: &str = "PREK_MANAGED_KEY_WORKER_INVENTORY_V1";
pub(super) const RESULT: &str = "PREK_MANAGED_KEY_WORKER_RESULT=";
pub(super) const PHASE: &str = "PREK_MANAGED_KEY_WORKER_PHASE=";
pub(super) const DRAIN: &str = "PREK_MANAGED_KEY_WORKER_DRAIN=";
pub(super) const SOFTWARE: &str = "PREK_MANAGED_KEY_WORKER_SOFTWARE=";
const WAIT: Duration = Duration::from_secs(5);

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    path: PathBuf,
    descriptor: Descriptor,
}
impl Endpoint {
    fn validate(&self) -> io::Result<()> {
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if self.path.parent() != Some(Path::new("/dev/input"))
            || !name.strip_prefix("event").is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
            || !self.descriptor.is_isolated()
            || !self.descriptor.marker.starts_with("prek/test/")
        {
            return Err(invalid(
                "managed fixture endpoint is not explicitly isolated",
            ));
        }
        self.descriptor.validate().map_err(invalid)
    }
    fn receiver(&self) -> io::Result<kernel::Receiver> {
        self.receiver_as(&self.descriptor.source)
    }
    fn receiver_as(&self, source: &Source) -> io::Result<kernel::Receiver> {
        self.validate()?;
        let node = Path::new("/sys/class/input")
            .join(self.path.file_name().unwrap())
            .join("device");
        if !fs::canonicalize(&node)?.starts_with("/sys/devices/virtual/input")
            || fs::read_to_string(node.join("phys"))?.trim_end() != self.descriptor.marker
        {
            return Err(invalid(
                "managed fixture endpoint is not its owned virtual node",
            ));
        }
        Ok(kernel::Receiver::from_test_node(
            &self.path,
            source,
            &self.descriptor.marker,
        ))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "fixture", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Setup {
        devices: Vec<Device>,
        inputs: Vec<Endpoint>,
        consumer: Consumer,
        fence: Fence,
    },
    Arm {},
    Step {
        id: u64,
        frames: usize,
    },
    Finish {},
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "fixture", rename_all = "snake_case", deny_unknown_fields)]
enum Notice {
    Prepared {
        worker: WorkerIdentity,
        outputs: Vec<Endpoint>,
    },
    Armed {},
    Stepped {
        id: u64,
    },
    Closed {},
}

fn configuration(devices: &[Device]) -> io::Result<Config> {
    if devices.len() != 2
        || devices[0].source == devices[1].source
        || devices[0].identity == devices[1].identity
    {
        return Err(invalid("managed fixture needs two distinct sources"));
    }
    let mut config = Config::preset("desktop").map_err(invalid)?;
    config.mode = Mode::Route;
    config.devices = devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            (
                format!("source{index}"),
                Selector {
                    identity: Some(device.identity.clone()),
                    ..Default::default()
                },
            )
        })
        .collect();
    config.policy.allowed_devices = config.devices.keys().cloned().collect();
    config.rules.truncate(1);
    let rule = &mut config.rules[0];
    rule.id = "shift".into();
    rule.enabled = true;
    rule.devices = vec!["source0".into()];
    rule.contexts = vec![Context::Desktop];
    rule.scope = Scope::Everywhere;
    rule.output = None;
    rule.screen = None;
    rule.keyboard_visible = None;
    rule.require_keys.clear();
    rule.keys_throughout = false;
    rule.layer = None;
    rule.when_mode = None;
    rule.delivery = Delivery::Buffer;
    rule.trigger = Trigger::Key { key: Key(30) };
    rule.action = Action::Remap { key: Key(42) };
    config.validate().map_err(invalid)?;
    Ok(config)
}

fn subscription(
    fence: Fence,
    consumer: &Consumer,
    revision: u64,
    synthetic: bool,
) -> io::Result<Subscription> {
    let Consumer::IsolatedReceiver { pid, start } = consumer else {
        return Err(invalid("managed fixture requires its isolated receiver"));
    };
    let mut capabilities = BTreeSet::from([Capability::Context, Capability::Observations]);
    if synthetic {
        capabilities.insert(Capability::SyntheticInput);
    }
    Ok(Subscription {
        seat: "seat0".into(),
        fence,
        consumer: Some(consumer.clone()),
        capabilities,
        context: Some(Box::new(ProvidedContext {
            layouts: None,
            binding: Binding {
                seat: "seat0".into(),
                session: "isolated-managed".into(),
                uid: 0,
                compositor_pid: *pid,
                compositor_start: *start,
                kind: SessionKind::User,
            },
            snapshot: Snapshot {
                generation: fence.session,
                revision,
                captured_at: monotonic()?,
                state: State::Desktop,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
        })),
    })
}

pub fn entry() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [arg] if arg == "--inventory" => {
            println!("{INVENTORY}\n{}", serde_json::to_string(&[CASE])?);
            Ok(())
        }
        [] => {
            println!("managed native kernel gate skipped; use the user-launched fixed root runner");
            Ok(())
        }
        [arg] if arg == "--run" || arg == "--worker" => {
            kernel::authorized();
            if arg == "--worker" {
                worker::run()
            } else {
                parent::run()
            }
        }
        _ => Err(invalid(
            "managed native fixture accepts --inventory or the fixed root invocation",
        )),
    }
}
