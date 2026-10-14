//! Local GUI acceptance: actual compositor queries and Luna helper callbacks,
//! with synthetic source/readiness authority explicitly confined to this test.
#![cfg(target_os = "linux")]
use prek::{
    context::{Binding, SessionKind},
    context_transfer::View,
    hyprland::{Client, ContextSampler},
    input::{Device, EV_KEY, Frame, RawEvent, Source},
    ipc::monotonic,
    lua::{Definition, EventKind, session::Session},
    protocol::{Capability, Epoch, Fence},
    replay::Record,
    routing::Routing,
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
    worker::ProvidedContext,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    time::Duration,
};

fn settle(session: &mut Session, output: &mut impl Write) {
    let until = monotonic().unwrap() + Duration::from_secs(3);
    loop {
        for log in session.step(monotonic().unwrap()).unwrap() {
            serde_json::to_writer(&mut *output, &log).unwrap();
            writeln!(output).unwrap();
        }
        if session.finished() {
            break;
        }
        assert!(monotonic().unwrap() < until, "Lua helper did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "owned local Cage/nested Hyprland runner only"]
fn local_cage_lua_routing_and_context() {
    assert_eq!(std::env::var("PREK_LOCAL_CAGE").as_deref(), Ok("1"));
    let output = PathBuf::from(std::env::var_os("PREK_CAGE_OUTPUT").unwrap());
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("nested.json")).unwrap()).unwrap();
    let socket = Path::new(report["socket"].as_str().unwrap());
    let runtime = socket.parent().unwrap();
    let pid = u32::try_from(report["hyprland_pid"].as_u64().unwrap()).unwrap();
    let executable = fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    let instance = fs::read_dir(runtime.join("hypr"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            fs::metadata(entry.path().join(".socket2.sock"))
                .is_ok_and(|metadata| metadata.file_type().is_socket())
        })
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    // SAFETY: read-only query of this test's owning UID.
    let uid = unsafe { nix::libc::geteuid() };
    let client = Client::open(runtime, &instance, uid, &executable).unwrap();
    let Consumer::Hyprland { pid, start, .. } = client.consumer() else {
        panic!("wrong consumer")
    };
    let binding = Binding {
        seat: "seat0".into(),
        session: "local-cage-model".into(),
        uid,
        compositor_pid: pid,
        compositor_start: start,
        kind: SessionKind::User,
    };
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('context_changed',function(event)
  local snapshot=p.context.snapshot(); local binding=p.context.binding()
  assert(event.available and snapshot and binding and #snapshot.outputs>0)
  assert(snapshot.generation==event.fence.session and binding.seat=='seat0')
  p.log.info('local-context',{revision=snapshot.revision,outputs=#snapshot.outputs,uid=binding.uid})
end)
p.events.on('matched',function(event)
  assert(p.context.snapshot() and p.context.state()=='desktop')
  p.log.info('local-matched',{rule=event.event.rule,stage=event.event.stage})
end)
return p.config.new {
  mode='route',policy={allowed_devices={'keyboard'}},
  devices={keyboard={name='Prek local keyboard'}},
  rules={p.rules.define {id='local-a',priority=20,devices={'keyboard'},contexts={'desktop'},
    delivery='reserve',trigger=p.triggers.key {key='KEY_A'},
    action=p.actions.event {name='local-a-claimed'}}}
}
"#,
        "local-cage.lua",
    )
    .unwrap();
    let mut routing = Routing::new(toml::from_str(&definition.model).unwrap()).unwrap();
    routing
        .authority_mut()
        .bind(binding.clone(), true, false)
        .unwrap();
    let app = PathBuf::from(std::env::var_os("PREK_LOCAL_CAGE_APP").unwrap());
    let mut session = Session::spawn(&app, fence, "seat0", definition).unwrap();
    let mut logs = fs::File::create(output.join("prek-runtime.jsonl")).unwrap();
    settle(&mut session, &mut logs);
    let device = Device {
        identity: "test:local-cage".into(),
        source: Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        name: "Prek local keyboard".into(),
        path: String::new(),
        physical: "local-cage-model".into(),
        unique: String::new(),
        vendor: 0x1209,
        product: 0xbeef,
        properties: vec![],
        axes: BTreeMap::new(),
        capabilities: BTreeMap::from([(0, vec![0]), (EV_KEY, vec![30, 48])]),
        virtual_device: false,
    };
    let descriptor = Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: device.vendor,
            product: device.product,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    // Model readiness: no kernel device exists and no production grab is made.
    let consumer = Consumer::IsolatedReceiver { pid, start };
    let now = monotonic().unwrap();
    let mut gate = Readiness::new(descriptor.clone(), consumer.clone(), now).unwrap();
    gate.metadata(&descriptor, now).unwrap();
    gate.consumer(
        &ConsumerEvidence {
            consumer,
            marker: descriptor.marker,
            name: descriptor.name,
            class: descriptor.class,
            seat: "seat0".into(),
            settings: BTreeMap::new(),
            at: now,
        },
        now,
    )
    .unwrap();
    gate.neutral(true, now).unwrap();
    routing
        .attach(device.clone(), &gate.finish(now).unwrap(), None, now)
        .unwrap();
    let mut sampler = ContextSampler::default();
    let mut forwarded = Vec::new();
    let mut dispatches = Vec::new();
    for (sequence, (code, value)) in [(30, 1), (30, 0), (48, 1), (48, 0)].into_iter().enumerate() {
        let snapshot = sampler.sample(&client, 1, SessionKind::User).unwrap();
        assert!(!snapshot.outputs.is_empty());
        let now = monotonic().unwrap();
        routing
            .authority_mut()
            .update(snapshot.clone(), now)
            .unwrap();
        routing.actions_available(1, BTreeSet::from([Capability::Observations]), now);
        session
            .update_context(View {
                fence,
                sampled_at: now,
                state: snapshot.state,
                context: Some(Box::new(ProvidedContext {
                    binding: binding.clone(),
                    snapshot,
                })),
                error: None,
            })
            .unwrap();
        settle(&mut session, &mut logs);
        let now = monotonic().unwrap();
        routing
            .push(Record::Frame {
                frame: Frame {
                    source: device.source.clone(),
                    at: now,
                    sequence: sequence as u64 + 1,
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code,
                            value,
                        },
                        RawEvent {
                            kind: 0,
                            code: 0,
                            value: 0,
                        },
                    ],
                },
            })
            .unwrap();
        while let Some(scheduled) = routing
            .due(&device.source, monotonic().unwrap())
            .unwrap()
            .cloned()
        {
            forwarded.extend(
                scheduled
                    .frame
                    .events
                    .iter()
                    .filter(|event| event.kind == EV_KEY)
                    .cloned(),
            );
            routing
                .delivered(&device.source, scheduled.ticket, monotonic().unwrap())
                .unwrap();
        }
        for event in routing.take_dispatches(monotonic().unwrap()) {
            session
                .notify(
                    EventKind::Matched,
                    serde_json::json!({"fence":fence,"event":event}),
                )
                .unwrap();
            dispatches.push(event);
        }
        settle(&mut session, &mut logs);
    }
    assert_eq!(
        forwarded
            .iter()
            .map(|event| (event.code, event.value))
            .collect::<Vec<_>>(),
        vec![(48, 1), (48, 0)]
    );
    assert!(!dispatches.is_empty());
    fs::write(
        output.join("routed-keys.json"),
        serde_json::to_vec(&forwarded).unwrap(),
    )
    .unwrap();
    fs::write(
        output.join("dispatches.json"),
        serde_json::to_vec(&dispatches).unwrap(),
    )
    .unwrap();
    session.stop().unwrap();
    settle(&mut session, &mut logs);
    println!(
        "Verified selected Hyprland IPC, complete context in the real Luna helper, and model-routed A consumption/B delivery"
    );
}
