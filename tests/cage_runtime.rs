//! Local GUI acceptance: actual compositor queries and Luna helper callbacks,
//! with synthetic source/readiness authority explicitly confined to this test.
#![cfg(target_os = "linux")]
#[path = "cage_runtime/rendering.rs"]
mod rendering;
use prek::{
    context::{Binding, SessionKind, Snapshot},
    context_transfer::View,
    hyprland::{Client, ContextSampler},
    input::{Device, EV_KEY, Frame, RawEvent, Source},
    ipc::{Channel, monotonic},
    lua::{Definition, EventKind, session::Session},
    protocol::{Capability, Epoch, Fence},
    replay::Record,
    routing::Routing,
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
    worker::{Message, ProvidedContext, Report, Stream, Subscription, Subscriptions},
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

fn attach(routing: &mut Routing, device: &Device, pid: u32, start: u64) {
    let descriptor = Descriptor::mirror(
        device,
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
}

fn context(
    routing: &mut Routing,
    session: &mut Session,
    logs: &mut impl Write,
    sampler: &mut ContextSampler,
    client: &Client,
    binding: &Binding,
    fence: Fence,
) -> Snapshot {
    // A compositor can miss one request budget while mapping/rendering the
    // receiver. Inject no next input until a new, independently bracketed
    // snapshot succeeds; never extend the production request/freshness limit
    // or reuse the prior sample. Invalid/security failures remain immediate.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let (snapshot, layouts) = loop {
        match sampler.sample_with_layouts(client, 1, SessionKind::User) {
            Ok((_, None)) if std::env::var("PREK_CAGE_CONFIG_PROVIDER").as_deref() == Ok("lua") => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "owned native context remained unavailable"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(context) => break context,
            Err(error)
                if error.kind() == std::io::ErrorKind::TimedOut
                    && std::time::Instant::now() < deadline =>
            {
                eprintln!("Retrying a fresh fixture context sample after timeout");
                settle(session, logs);
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fixture context sample failed: {error}"),
        }
    };
    assert!(!snapshot.outputs.is_empty());
    let now = monotonic().unwrap();
    routing
        .authority_mut()
        .update(snapshot.clone(), now)
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::Observations]), now);
    session
        .update_context(View {
            morf: Vec::new(),
            morf_rendered: Vec::new(),
            fence,
            sampled_at: now,
            state: snapshot.state,
            context: Some(Box::new(ProvidedContext {
                layouts,
                binding: binding.clone(),
                snapshot: snapshot.clone(),
            })),
            error: None,
        })
        .unwrap();
    settle(session, logs);
    snapshot
}

// Observation uses the real worker IPC and helper, with the same explicit
// synthetic device/seat authority as this fixture's routing model.
struct InputObserver {
    parent: Channel,
    worker: Stream,
    revision: u64,
    source: Option<Source>,
}
impl InputObserver {
    fn new(mut config: prek::config::Config) -> Self {
        config.mode = prek::config::Mode::Observe;
        let (parent, child) = Channel::pair().unwrap();
        let mut worker = Stream::new(config, child).unwrap();
        worker.push(Record::Header { version: 1 }).unwrap();
        Self {
            parent,
            worker,
            revision: 0,
            source: None,
        }
    }
    fn context(&mut self, snapshot: Snapshot, binding: &Binding, fence: Fence, device: &Device) {
        if self.source.is_none() {
            self.worker
                .push(Record::Device {
                    device: device.clone(),
                    keys: BTreeSet::new(),
                    contacts: vec![],
                })
                .unwrap();
            self.source = Some(device.source.clone());
        }
        self.revision += 1;
        self.parent
            .send(
                &prek::configuration::Command::Subscriptions {
                    update: Subscriptions {
                        revision: self.revision,
                        seats: vec![Subscription {
                            seat: binding.seat.clone(),
                            fence,
                            capabilities: BTreeSet::from([
                                Capability::InputEvents,
                                Capability::LuaEvents,
                                Capability::Context,
                                Capability::Observations,
                            ]),
                            consumer: None,
                            context: Some(Box::new(ProvidedContext {
                                layouts: None,
                                binding: binding.clone(),
                                snapshot,
                            })),
                        }],
                    },
                },
                monotonic().unwrap(),
            )
            .unwrap();
        self.worker.service().unwrap();
        if self.source.as_ref() != Some(&device.source) {
            self.worker
                .push(Record::Remove {
                    source: self.source.take().unwrap(),
                    at: monotonic().unwrap(),
                })
                .unwrap();
            self.worker
                .push(Record::Device {
                    device: device.clone(),
                    keys: BTreeSet::new(),
                    contacts: vec![],
                })
                .unwrap();
            self.source = Some(device.source.clone());
        }
    }
    fn frame(&mut self, frame: Frame, session: &mut Session, logs: &mut impl Write) {
        self.worker.push(Record::Frame { frame }).unwrap();
        self.worker.flush().unwrap();
        while let Some(packet) = self.parent.receive::<Message>().unwrap() {
            if let Message::Outcome {
                report:
                    Report::Input {
                        subscription,
                        input,
                    },
            } = packet.message
            {
                input.validate().unwrap();
                session
                    .notify(
                        EventKind::Input,
                        serde_json::json!({"fence":subscription.fence,"input":input}),
                    )
                    .unwrap();
                settle(session, logs);
            }
        }
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
    let startup = std::time::Instant::now() + Duration::from_secs(3);
    let client = loop {
        match Client::open(runtime, &instance, uid, &executable) {
            Ok(client) => break client,
            Err(error)
                if error.kind() == std::io::ErrorKind::TimedOut
                    && std::time::Instant::now() < startup =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!("owned compositor client did not become ready: {error}"),
        }
    };
    fs::write(
        output.join("hyprland-devices.json"),
        serde_json::to_vec(&client.inventory().unwrap()).unwrap(),
    )
    .unwrap();
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
    let layer_control = std::env::var("PREK_CAGE_LAYER_TEST").as_deref() == Ok("1");
    let mode_control = std::env::var("PREK_CAGE_MODE_TEST").as_deref() == Ok("1");
    let input_control = std::env::var("PREK_CAGE_INPUT_TEST").as_deref() == Ok("1");
    let source = r#"
local p=require('prek')
local layer_control=__LAYER_CONTROL__
local mode_control=__MODE_CONTROL__
local input_control=__INPUT_CONTROL__
local transaction
if input_control then p.events.on('input',function(payload)
  local e=payload.input
  assert(e.source.seat=='seat0' and e.raw==nil and e.events==nil)
  assert(e.source.generation=='1' or e.source.generation=='2')
  assert(e.selectors[1]=='keyboard')
  if e.reason=='removed' then assert(p.devices.get(e.source)==nil)
  else
    local info=p.devices.get(e.source)
    assert(info and info.device.name=='Prek local keyboard' and #p.devices.list()==1)
    assert(info.device.source.generation==e.source.generation)
    assert(p.devices.position(e.source,0,0)==nil)
  end
  assert(p.seats.current().binding.seat=='seat0' and p.devices.revision())
  if e.kind=='frame' then
    assert(#e.changes==1 and e.changes[1].kind=='key')
    p.log.info('local-input',{generation=e.source.generation,sequence=e.sequence,code=e.changes[1].code,state=e.changes[1].state})
  else p.log.info('local-input-reset',{generation=e.source.generation,reason=e.reason}) end
end) end
p.events.on('context_changed',function(event)
  local snapshot=p.context.snapshot(); local binding=p.context.binding()
  assert(event.available and snapshot and binding and #snapshot.outputs>0)
  assert(snapshot.generation==event.fence.session and binding.seat=='seat0')
  local outputs=p.outputs.list(); assert(outputs and #outputs==#snapshot.outputs)
  for _,view in ipairs(outputs) do
    assert(view.reference.revision==snapshot.revision and view.reference.fence.session==event.fence.session)
    assert(p.outputs.get(view.state.geometry.name).reference.geometry_generation==view.state.geometry_generation)
    local global=p.coordinates.convert(view.reference,{x=0,y=0},'local','global')
    assert(global and global.x==view.state.geometry.origin.x and global.y==view.state.geometry.origin.y)
    local local_point=p.coordinates.convert(view.reference,global,'global','local')
    assert(local_point and local_point.x==0 and local_point.y==0)
  end
  assert(#p.regions.list()==#snapshot.regions)
  local native=p.context.layouts()
  if native then
    assert(native.revision==snapshot.revision and native.generation==snapshot.generation)
    assert(native.fence.session==event.fence.session and #native.workspaces>0)
    local id=native.workspaces[1].windows[1].id
    native.workspaces[1].windows[1].id='forged'
    assert(p.context.layouts().workspaces[1].windows[1].id==id)
    p.log.info('local-native-layouts',{revision=snapshot.revision,windows=#native.workspaces[1].windows})
  end
  p.log.info('local-context',{revision=snapshot.revision,outputs=#snapshot.outputs,uid=binding.uid})
end)
p.events.on('matched',function(event)
  assert(p.context.snapshot() and p.context.state()=='desktop')
  if event.event.rule=='local-a' then
    p.log.info('local-matched',{rule=event.event.rule,stage=event.event.stage})
  elseif event.event.rule=='local-b' and event.event.stage=='end' and not transaction then
    transaction=p.rules.transaction()
    if mode_control then transaction:select_mode('ordinary')
    elseif layer_control then transaction:deactivate_layer('local-layer') else transaction:disable('local-a') end
    transaction:commit()
  end
end)
p.events.on('rules_changed',function(event)
  assert(event.accepted and event.revision=='2' and p.rules.get('local-a').enabled==(layer_control or mode_control))
  assert(p.layers.get('local-layer').activation==((layer_control and not mode_control) and 'inactive' or 'active'))
  assert(p.modes.current().id==(mode_control and 'ordinary' or 'capture'))
  assert(transaction:status().state=='accepted' and p.rules.revision()=='2')
  p.log.info('local-rules-changed',{accepted=event.accepted,revision=event.revision,state=transaction:status().state})
  p.log.info('local-layer-state',{activation=p.layers.get('local-layer').activation,rule_enabled=p.rules.get('local-a').enabled})
  p.log.info('local-mode-state',{selected=p.modes.current().id,revision=p.modes.revision(),rule_enabled=p.rules.get('local-a').enabled})
end)
return p.config.new {
  mode=input_control and 'observe' or 'route',policy={allowed_devices={'keyboard'},allow_runtime_rules=true,allow_input_events=input_control},
  devices={keyboard={name='Prek local keyboard'}},
  layers={p.layers.define{id='local-layer',key='KEY_LEFTMETA',activation='active'}},
  modes={p.modes.define{id='capture',active=true},p.modes.define{id='ordinary'}},
  rules={p.rules.define {id='local-a',layer='local-layer',when_mode='capture',priority=20,devices={'keyboard'},contexts={'desktop'},
    delivery='reserve',trigger=p.triggers.key {key='KEY_A'},
    action=p.actions.event {name='local-a-claimed'}},
    p.rules.define {id='local-b',devices={'keyboard'},contexts={'desktop'},delivery='observe',
      trigger=p.triggers.key {key='KEY_B'},action=p.actions.event {name='local-rule-disable'},
      on_begin=function(event)p.log.info('local-rule-hook',{rule=event.event.rule,stage=event.event.stage,ordinal=event.event.ordinal})end,
      on_end=function(event)p.log.info('local-rule-hook',{rule=event.event.rule,stage=event.event.stage,ordinal=event.event.ordinal})end}}
}
"#.replace("__LAYER_CONTROL__", if layer_control { "true" } else { "false" }).replace("__MODE_CONTROL__", if mode_control { "true" } else { "false" }).replace("__INPUT_CONTROL__",if input_control {"true"}else{"false"});
    let definition = Definition::compile(&source, "local-cage.lua").unwrap();
    let config = definition.validate().unwrap();
    let mut input_observer = input_control.then(|| InputObserver::new(config.clone()));
    let mut route_config = config;
    route_config.mode = prek::config::Mode::Route;
    let mut routing = Routing::new(route_config).unwrap();
    routing
        .authority_mut()
        .bind(binding.clone(), true, false)
        .unwrap();
    let app = PathBuf::from(std::env::var_os("PREK_LOCAL_CAGE_APP").unwrap());
    let mut session =
        Session::spawn_with_rule_transactions(&app, fence, "seat0", definition).unwrap();
    let mut logs = fs::File::create(output.join("prek-runtime.jsonl")).unwrap();
    settle(&mut session, &mut logs);
    let mut device = Device {
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
    attach(&mut routing, &device, pid, start);
    let mut sampler = ContextSampler::default();
    let mut forwarded = Vec::new();
    let mut dispatches = Vec::new();
    let mut deferred = None;
    for phase in 0..2 {
        if phase == 1 {
            assert_eq!(
                forwarded
                    .iter()
                    .map(|event: &RawEvent| (event.code, event.value))
                    .collect::<Vec<_>>(),
                vec![(48, 1), (48, 0)]
            );
            fs::write(
                output.join("routed-keys.json"),
                serde_json::to_vec(&forwarded).unwrap(),
            )
            .unwrap();
            fs::write(output.join("phase1-ready"), b"ready").unwrap();
            let until = monotonic().unwrap() + Duration::from_secs(3);
            let mut refreshed = monotonic().unwrap();
            while !output.join("phase1-delivered").exists() {
                settle(&mut session, &mut logs);
                if monotonic().unwrap().saturating_sub(refreshed) >= Duration::from_millis(50) {
                    let snapshot = context(
                        &mut routing,
                        &mut session,
                        &mut logs,
                        &mut sampler,
                        &client,
                        &binding,
                        fence,
                    );
                    if let Some(observer) = input_observer.as_mut() {
                        observer.context(snapshot, &binding, fence, &device);
                    }
                    refreshed = monotonic().unwrap();
                }
                assert!(
                    monotonic().unwrap() < until,
                    "application did not acknowledge phase one"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            context(
                &mut routing,
                &mut session,
                &mut logs,
                &mut sampler,
                &client,
                &binding,
                fence,
            );
            session
                .notify(
                    EventKind::Matched,
                    serde_json::json!({"fence":fence,"event":deferred.take().unwrap()}),
                )
                .unwrap();
            let until = monotonic().unwrap() + Duration::from_secs(3);
            let proposal = loop {
                for log in session.step(monotonic().unwrap()).unwrap() {
                    serde_json::to_writer(&mut logs, &log).unwrap();
                    writeln!(logs).unwrap();
                }
                let mut proposals = session.take_rule_proposals();
                if !proposals.is_empty() {
                    assert_eq!(proposals.len(), 1);
                    break proposals.remove(0);
                }
                assert!(
                    monotonic().unwrap() < until,
                    "Lua rule proposal did not arrive"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            assert_eq!(proposal.transaction.revision, 1);
            let revision = routing
                .transact_rules(&proposal.transaction, monotonic().unwrap())
                .unwrap();
            assert_eq!(revision, 2);
            let recoveries = routing.take_recoveries();
            assert_eq!(recoveries.len(), 1);
            assert!(
                recoveries[0].neutral_events.is_empty(),
                "phase one must finish neutral"
            );
            // Recreated transports use a new source incarnation; old handles
            // remain stale while file/session fences and gesture IDs persist.
            device.source.generation += 1;
            attach(&mut routing, &device, pid, start);
            session
                .acknowledge_rules(
                    fence,
                    prek::lua::RuleReceipt {
                        id: proposal.id,
                        revision,
                        error: None,
                    },
                )
                .unwrap();
            settle(&mut session, &mut logs);
            fs::write(output.join("rule-receipt.json"),serde_json::to_vec(&serde_json::json!({"id":proposal.id,"revision":revision,"configuration":fence.configuration,"session":fence.session})).unwrap()).unwrap();
        }
        let mut phase_keys = Vec::new();
        for (sequence, (code, value)) in
            [(30, 1), (30, 0), (48, 1), (48, 0)].into_iter().enumerate()
        {
            let snapshot = context(
                &mut routing,
                &mut session,
                &mut logs,
                &mut sampler,
                &client,
                &binding,
                fence,
            );
            if let Some(observer) = input_observer.as_mut() {
                observer.context(snapshot, &binding, fence, &device);
            }
            let frame = Frame {
                source: device.source.clone(),
                at: monotonic().unwrap(),
                sequence: (phase * 4 + sequence + 1) as u64,
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
            };
            routing
                .push(Record::Frame {
                    frame: frame.clone(),
                })
                .unwrap();
            if let Some(observer) = input_observer.as_mut() {
                observer.frame(frame, &mut session, &mut logs);
            }
            while let Some(scheduled) = routing
                .due(&device.source, monotonic().unwrap())
                .unwrap()
                .cloned()
            {
                let keys = scheduled
                    .frame
                    .events
                    .iter()
                    .filter(|event| event.kind == EV_KEY)
                    .cloned();
                phase_keys.extend(keys.clone());
                forwarded.extend(keys);
                routing
                    .delivered(&device.source, scheduled.ticket, monotonic().unwrap())
                    .unwrap();
            }
            for event in routing.take_dispatches(monotonic().unwrap()) {
                if phase == 0
                    && matches!(&event,prek::rules::Event::Matched {rule,stage:prek::rules::Stage::End,..} if rule=="local-b")
                {
                    deferred = Some(event.clone());
                } else {
                    session
                        .notify(
                            EventKind::Matched,
                            serde_json::json!({"fence":fence,"event":event}),
                        )
                        .unwrap();
                }
                dispatches.push(event);
            }
            settle(&mut session, &mut logs);
        }
        if phase == 1 {
            assert_eq!(
                phase_keys
                    .iter()
                    .map(|event| (event.code, event.value))
                    .collect::<Vec<_>>(),
                vec![(30, 1), (30, 0), (48, 1), (48, 0)]
            );
            fs::write(
                output.join("changed-keys.json"),
                serde_json::to_vec(&phase_keys).unwrap(),
            )
            .unwrap();
        }
    }
    assert_eq!(dispatches.len(), 6);
    fs::write(
        output.join("dispatches.json"),
        serde_json::to_vec(&dispatches).unwrap(),
    )
    .unwrap();
    fs::write(output.join("phase2-ready"), b"ready").unwrap();
    let until = monotonic().unwrap() + Duration::from_secs(3);
    while !output.join("phase2-delivered").exists() {
        settle(&mut session, &mut logs);
        assert!(
            monotonic().unwrap() < until,
            "application did not acknowledge phase two"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    session.stop().unwrap();
    settle(&mut session, &mut logs);
    if std::env::var("PREK_CAGE_POWER_TEST").as_deref() == Ok("1") {
        let checking = Client::open(runtime, &instance, uid, &executable).unwrap();
        power_acceptance(
            &checking,
            &binding,
            fence,
            &app,
            &output,
            (runtime, &instance),
        );
    }
    if std::env::var("PREK_CAGE_WINDOW_TEST").as_deref() == Ok("1") {
        let checking = Client::open(runtime, &instance, uid, &executable).unwrap();
        window_acceptance(&checking, &binding, fence, &app, &output, socket);
    }
    println!(
        "Verified real Hyprland context and Luna transaction receipts, with model-routed A consumption before the edit and A/B forwarding afterward"
    );
}

struct OwnedReceiver(std::process::Child);
impl Drop for OwnedReceiver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn focus_owned_window(id: &str, instance: &str) {
    let address = prek::config::hyprland_window_address(id).unwrap();
    let selector = format!("address:{address}");
    let args = match std::env::var("PREK_CAGE_CONFIG_PROVIDER").as_deref() {
        Ok("lua") => vec![
            "dispatch".to_owned(),
            format!("hl.dsp.focus({{window={selector:?}}})"),
        ],
        Ok("hyprlang") | Err(std::env::VarError::NotPresent) => {
            vec!["dispatch".to_owned(), "focuswindow".to_owned(), selector]
        }
        other => panic!("invalid owned compositor config provider: {other:?}"),
    };
    assert!(
        std::process::Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
            .args(args)
            .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

fn window_acceptance(
    checking: &Client,
    binding: &Binding,
    fence: Fence,
    app: &Path,
    output: &Path,
    socket: &Path,
) {
    use prek::{
        actions::Executor,
        config::WindowAction,
        protocol::{Completion, Mode, Request},
        rules::{Event, Evidence, Stage},
    };
    let mut sampler = ContextSampler::default();
    let mut spawn = |name: &str| {
        let previous = sampler.sample(checking, 1, SessionKind::User).unwrap();
        let child = OwnedReceiver(
            std::process::Command::new(std::env::var_os("PREK_CAGE_RECEIVER").unwrap())
                .arg(output.join(format!("{name}.jsonl")))
                .env("PREK_TEST_RECEIVER_LABEL", name)
                .env("WAYLAND_DISPLAY", socket)
                .env_remove("WAYLAND_SOCKET")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let until = monotonic().unwrap() + Duration::from_secs(3);
        loop {
            let snapshot = sampler.sample(checking, 1, SessionKind::User).unwrap();
            if let Some(window) = snapshot
                .windows
                .iter()
                .find(|window| !previous.windows.iter().any(|old| old.id == window.id))
            {
                return (child, window.id.clone());
            }
            assert!(monotonic().unwrap() < until, "owned receiver never mapped");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let (mut target_child, target_id) = spawn("hyprland-target");
    let (mut guard_child, guard_id) = spawn("hyprland-guard");
    let runtime = socket.parent().unwrap();
    let Consumer::Hyprland { instance, .. } = checking.consumer() else {
        panic!()
    };
    if std::env::var("PREK_CAGE_CONFIG_PROVIDER").as_deref() == Ok("lua") {
        layout_acceptance(checking, binding, fence, app, &instance, output);
    }
    let fractional_scale = if std::env::var("PREK_CAGE_CONFIG_PROVIDER").as_deref() == Ok("lua") {
        let before = sampler.sample(checking, 1, SessionKind::User).unwrap();
        assert_eq!(before.outputs.len(), 1);
        let geometry = &before.outputs[0].geometry;
        owned_monitor_scale(checking, &instance, &geometry.name, 1.6);
        Some((geometry.name.clone(), geometry.scale))
    } else {
        None
    };
    focus_owned_window(&target_id, &instance);
    let initial = sampler.sample(checking, 1, SessionKind::User).unwrap();
    let initial_window = initial
        .windows
        .iter()
        .find(|window| window.id == target_id)
        .unwrap();
    assert!(initial_window.focused);
    // The owned compositor disables animations, so stock goals are rendered.
    assert!(
        initial
            .windows
            .iter()
            .filter(|window| [&target_id, &guard_id].contains(&&window.id))
            .all(|window| window.visible),
        "still owned windows on the shown workspace were not proven visible"
    );
    if std::env::var("PREK_CAGE_CONFIG_PROVIDER").as_deref() == Ok("lua") {
        inherited_stillness(checking, &instance, &[&target_id, &guard_id], output);
    }
    let target_output = initial_window.output.clone();
    let source = r#"
local p=require('prek')
local output=__OUTPUT__
p.events.on('ready',function()
 assert(p.capabilities.has('hyprland_actions') and not p.capabilities.ready('hyprland_actions'))
end)
p.events.on('stop',function()assert(p.capabilities.current()==nil)end)
local handler=p.actions.define('window',{requires={'hyprland_actions'}},function(payload)
 if payload.event.stage~='begin' then return end
 assert(p.capabilities.ready('hyprland_actions') and p.capabilities.ready('lua_actions'))
 assert(payload.event.window~=nil)
 return p.actions.hyprland{action=payload.event.rule}
end)
return p.config.new{mode='route',policy={allowed_devices={'keyboard'},allow_hyprland_actions=true},devices={keyboard={keys={'KEY_A','KEY_B','KEY_C'}}},rules={
 p.rules.define{id='move_right_and_follow',devices={'keyboard'},contexts={'desktop'},output=output,trigger=p.triggers.key{key='KEY_A'},action=handler},
 p.rules.define{id='move_left_and_follow',devices={'keyboard'},contexts={'desktop'},output=output,trigger=p.triggers.key{key='KEY_B'},action=handler},
 p.rules.define{id='close',devices={'keyboard'},contexts={'desktop'},output=output,trigger=p.triggers.key{key='KEY_C'},action=handler},
}}
"#.replace("__OUTPUT__", &serde_json::to_string(&target_output).unwrap());
    let definition = Definition::compile(&source, "owned-windows.lua").unwrap();
    let config = definition.validate().unwrap();
    let negotiated = BTreeSet::from([
        Capability::Context,
        Capability::Observations,
        Capability::LuaEvents,
        Capability::LuaActions,
        Capability::HyprlandActions,
    ]);
    let mut session = Session::spawn(app, fence, "seat0", definition).unwrap();
    session
        .update_capabilities(negotiated.clone(), BTreeSet::new())
        .unwrap();
    let mut ignored = Vec::new();
    settle(&mut session, &mut ignored);
    let view = |snapshot: Snapshot| View {
        morf: Vec::new(),
        morf_rendered: Vec::new(),
        fence,
        sampled_at: snapshot.captured_at,
        state: snapshot.state,
        error: None,
        context: Some(Box::new(ProvidedContext {
            layouts: None,
            binding: binding.clone(),
            snapshot,
        })),
    };
    let first = sampler.sample(checking, 1, SessionKind::User).unwrap();
    let mut executor =
        Executor::new(fence, "seat0".into(), Mode::Route, negotiated.clone()).unwrap();
    executor
        .sync_hyprland_context(runtime, &instance, view(first))
        .unwrap();
    executor.lua_ready(true);
    let (mut channel, mut broker) = Channel::pair().unwrap();
    let mut receipts = Vec::new();
    for (index, action) in [
        WindowAction::MoveRightAndFollow,
        WindowAction::MoveLeftAndFollow,
        WindowAction::Close,
    ]
    .into_iter()
    .enumerate()
    {
        // A new gesture starts after the preceding workspace/layout animation
        // settles. Do this before freezing context and starting its short grant;
        // changing geometry after that boundary must still refuse the action.
        let until = monotonic().unwrap() + Duration::from_secs(3);
        let mut previous = None;
        let mut stable_since = monotonic().unwrap();
        let snapshot = loop {
            assert!(
                monotonic().unwrap() < until,
                "owned window geometry never settled"
            );
            let snapshot = match sampler.sample(checking, 1, SessionKind::User) {
                Ok(snapshot) => snapshot,
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    // A missing observation cannot contribute to stability.
                    // A new read-only sample gets its own normal IPC budget.
                    previous = None;
                    stable_since = monotonic().unwrap();
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("window readiness sample failed: {error}"),
            };
            let window = snapshot
                .windows
                .iter()
                .find(|window| window.id == target_id)
                .unwrap();
            let state = (
                window.clone(),
                snapshot
                    .outputs
                    .iter()
                    .find(|output| output.geometry.name == window.output)
                    .unwrap()
                    .clone(),
            );
            let now = monotonic().unwrap();
            if previous.as_ref() != Some(&state) {
                previous = Some(state);
                stable_since = now;
            }
            if now.saturating_sub(stable_since) >= Duration::from_millis(150) {
                break snapshot;
            }
            assert!(now < until, "owned window geometry never settled");
            std::thread::sleep(Duration::from_millis(10));
        };
        let window = snapshot
            .windows
            .iter()
            .find(|window| window.id == target_id)
            .unwrap()
            .clone();
        assert_eq!(config.rules[index].output.as_ref(), Some(&window.output));
        let output_scale = snapshot
            .outputs
            .iter()
            .find(|state| state.geometry.name == window.output)
            .unwrap()
            .geometry
            .scale;
        if fractional_scale.is_some() {
            assert_eq!((output_scale as f32).to_bits(), 1.6f32.to_bits());
        }
        let expires = snapshot.captured_at + Duration::from_millis(250);
        let fresh = view(snapshot);
        executor.update_hyprland_context(fresh.clone()).unwrap();
        session
            .update_capabilities(
                negotiated.clone(),
                executor.ready_capabilities(monotonic().unwrap()),
            )
            .unwrap();
        session.update_context(fresh).unwrap();
        settle(&mut session, &mut ignored);
        let id = index as u64 + 1;
        let event = Box::new(Event::Matched {
            version: 1,
            rule: config.rules[index].id.clone(),
            id,
            ordinal: 1,
            source: Source {
                device: 501,
                generation: 1,
                seat: "seat0".into(),
            },
            at: monotonic().unwrap(),
            config_generation: 1,
            session_generation: 1,
            stage: Stage::Begin,
            action: config.rules[index].action.clone(),
            evidence: Evidence::Key { down: true },
            window: Some(target_id.clone()),
            output: Some(window.output.clone()),
            workspace: Some(window.workspace),
        });
        executor.offer(fence, event, monotonic().unwrap()).unwrap();
        executor.flush(&mut channel).unwrap();
        let mut authorized = false;
        while let Some(packet) = broker.receive::<Request>().unwrap() {
            if matches!(packet.message,Request::Authorize{id:incoming,ordinal:1,..} if incoming==id)
            {
                authorized = true;
            }
        }
        assert!(
            authorized,
            "window action must request its explicit model grant"
        );
        if action == WindowAction::Close {
            // Change focus only through the owned nested compositor. The offer
            // already froze the target; closing active/focused would kill guard.
            focus_owned_window(&guard_id, &instance);
            let current = sampler.sample(checking, 1, SessionKind::User).unwrap();
            assert!(
                current
                    .windows
                    .iter()
                    .any(|window| window.id == guard_id && window.focused)
            );
        }
        executor
            .grant(fence, id, 1, expires, monotonic().unwrap())
            .unwrap();
        let until = monotonic().unwrap() + Duration::from_secs(3);
        let completion = loop {
            executor
                .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
                .unwrap();
            assert!(session.step(monotonic().unwrap()).unwrap().is_empty());
            for (id, ordinal, result) in session.take_actions() {
                executor
                    .resolve_action(id, ordinal, result, monotonic().unwrap())
                    .unwrap();
            }
            executor.flush(&mut channel).unwrap();
            let mut completed = None;
            while let Some(packet) = broker.receive::<Request>().unwrap() {
                if let Request::Complete {
                    id: incoming,
                    ordinal: 1,
                    result,
                    ..
                } = packet.message
                {
                    assert_eq!(incoming, id);
                    completed = Some(result);
                }
            }
            if let Some(completion) = completed {
                break completion;
            }
            assert!(monotonic().unwrap() < until, "window plan did not complete");
            std::thread::sleep(Duration::from_millis(1));
        };
        if completion != Completion::Applied {
            let sampled = sampler.sample(checking, 1, SessionKind::User);
            fs::write(output.join("hyprland-window-failure.json"),serde_json::to_vec(&serde_json::json!({
                "action":action,"completion":completion,"frozen_window":window,
                "snapshot":sampled.as_ref().ok(),"sample_error":sampled.as_ref().err().map(ToString::to_string)
            })).unwrap()).unwrap();
        }
        assert_eq!(
            completion,
            Completion::Applied,
            "window plan {action:?} failed"
        );
        let snapshot = sampler.sample(checking, 1, SessionKind::User).unwrap();
        let observed = snapshot
            .windows
            .iter()
            .find(|window| window.id == target_id);
        let after = match action {
            WindowAction::MoveRightAndFollow => Some(window.workspace + 1),
            WindowAction::MoveLeftAndFollow => Some(window.workspace - 1),
            _ => None,
        };
        if let Some(workspace) = after {
            let observed = observed.unwrap();
            assert_eq!(observed.workspace, workspace);
            assert!(observed.focused);
            assert_eq!(observed.output, window.output);
        } else {
            assert!(observed.is_none());
            let until = monotonic().unwrap() + Duration::from_secs(1);
            while target_child.0.try_wait().unwrap().is_none() {
                assert!(
                    monotonic().unwrap() < until,
                    "target did not acknowledge its close"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                fs::read_to_string(output.join("hyprland-target.jsonl"))
                    .unwrap()
                    .contains("\"event\":\"closed\"")
            );
        }
        assert!(guard_child.0.try_wait().unwrap().is_none());
        receipts.push(serde_json::json!({"action":config.rules[index].id,"completion":completion,"window":target_id,
            "workspace_before":window.workspace,"workspace_after":after,"target_closed":after.is_none(),
            "guard_alive":true,"lua_capabilities_checked":true,
            "native_fractional_scale_tested":fractional_scale.is_some(),"output_scale":output_scale}));
    }
    session.stop().unwrap();
    settle(&mut session, &mut ignored);
    if let Some((name, scale)) = fractional_scale {
        owned_monitor_scale(checking, &instance, &name, scale);
    }
    fs::write(
        output.join("hyprland-receipts.json"),
        serde_json::to_vec(&receipts).unwrap(),
    )
    .unwrap();
}

fn owned_monitor_scale(checking: &Client, instance: &str, name: &str, scale: f64) {
    let command = format!(
        "hl.monitor({{output={name:?},mode=\"1280x720@60\",position=\"auto\",scale={scale:?}}})"
    );
    let reply = std::process::Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
        .args(["eval", &command])
        .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
        .output()
        .unwrap();
    assert!(reply.status.success());
    assert_eq!(String::from_utf8(reply.stdout).unwrap().trim(), "ok");
    let until = monotonic().unwrap() + Duration::from_secs(3);
    let mut sampler = ContextSampler::default();
    loop {
        let snapshot = match sampler.sample(checking, 1, SessionKind::User) {
            Ok(snapshot) => snapshot,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                // Record which stock query is slow while the output reconfigures.
                for query in [
                    "monitors",
                    "clients",
                    "layers",
                    "activewindow",
                    "animations",
                    "getoption animations:enabled",
                    "locked",
                ] {
                    let started = std::time::Instant::now();
                    let status =
                        std::process::Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
                            .arg("-j")
                            .args(query.split(' '))
                            .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
                            .output()
                            .map(|reply| reply.status.success());
                    eprintln!("scale-timeout {query}: {:?} {status:?}", started.elapsed());
                }
                assert!(
                    monotonic().unwrap() < until,
                    "owned fractional scale context kept timing out"
                );
                continue;
            }
            Err(error) => panic!("{error}"),
        };
        if snapshot.outputs.iter().any(|state| {
            state.geometry.name == name
                && (state.geometry.scale as f32).to_bits() == (scale as f32).to_bits()
        }) {
            break;
        }
        assert!(
            monotonic().unwrap() < until,
            "owned fractional scale never settled"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn layout_acceptance(
    checking: &Client,
    binding: &Binding,
    fence: Fence,
    app: &Path,
    instance: &str,
    output: &Path,
) {
    let mut sampler = ContextSampler::default();
    let definition=Definition::compile(r#"
local p=require('prek')
p.events.on('context_changed',function(event)
 local native=p.context.layouts();local core=p.context.snapshot()
 assert(native and core and native.revision==core.revision and native.fence.session==event.fence.session)
 assert(not p.capabilities.has('hyprland_actions'))
 local ws=native.workspaces[1];local windows={}
 for _,window in ipairs(ws.windows) do windows[window.id]=window end
 for index,column in ipairs(ws.columns) do
  assert(column.index==index-1)
  for row,id in ipairs(column.members) do
   assert(windows[id] and windows[id].column.index==column.index and windows[id].column.row==row-1)
  end
 end
 if ws.columns[1] then
  local id=ws.columns[1].members[1];ws.columns[1].members[1]='forged'
  assert(p.context.layouts().workspaces[1].columns[1].members[1]==id)
 end
 p.log.info('bound-native-layout',{revision=native.revision,layout=ws.layout,columns=#ws.columns,windows=#ws.windows})
end)
return {version=1}
"#,"owned-bound-layout.lua").unwrap();
    let mut session = Session::spawn(app, fence, "seat0", definition).unwrap();
    let mut logs = fs::File::create(output.join("bound-layouts.jsonl")).unwrap();
    settle(&mut session, &mut logs);
    let mut sample = |sampler: &mut ContextSampler| {
        let until = monotonic().unwrap() + Duration::from_secs(3);
        let (snapshot, layouts) = loop {
            let (snapshot, layouts) = sampler
                .sample_with_layouts(checking, 1, SessionKind::User)
                .unwrap();
            if let Some(layouts) = layouts {
                break (snapshot, layouts);
            }
            assert!(
                monotonic().unwrap() < until,
                "owned matching native layout remained unavailable"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        session
            .update_context(View {
                fence,
                sampled_at: monotonic().unwrap(),
                state: snapshot.state,
                context: Some(Box::new(ProvidedContext {
                    binding: binding.clone(),
                    snapshot,
                    layouts: Some(layouts.clone()),
                })),
                error: None,
                morf: vec![],
                morf_rendered: vec![],
            })
            .unwrap();
        settle(&mut session, &mut logs);
        layouts
    };
    let original = sample(&mut sampler);
    let workspace = original
        .workspaces
        .iter()
        .find(|workspace| workspace.active && !workspace.special)
        .unwrap();
    assert_eq!(workspace.layout, "dwindle");
    assert!(workspace.windows.len() >= 3);
    let id = workspace.id;
    let identities: BTreeSet<_> = workspace
        .windows
        .iter()
        .map(|window| window.id.clone())
        .collect();
    let mut receipts = vec![original];
    for (layout, direction) in [
        ("scrolling", "down"),
        ("scrolling", "up"),
        ("dwindle", "right"),
    ] {
        let command = format!(
            "hl.config({{general={{layout={layout:?}}},scrolling={{direction={direction:?}}}}})"
        );
        let result = std::process::Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
            .args(["eval", &command])
            .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "ok");
        let until = monotonic().unwrap() + Duration::from_secs(3);
        let snapshot = loop {
            let snapshot = sample(&mut sampler);
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .unwrap();
            if workspace.layout == layout {
                assert_eq!(
                    workspace
                        .windows
                        .iter()
                        .map(|window| window.id.clone())
                        .collect::<BTreeSet<_>>(),
                    identities
                );
                if layout == "scrolling" {
                    assert_eq!(workspace.columns.len(), workspace.windows.len());
                    let first = workspace
                        .windows
                        .iter()
                        .find(|window| window.id == workspace.columns[0].members[0])
                        .unwrap();
                    let second = workspace
                        .windows
                        .iter()
                        .find(|window| window.id == workspace.columns[1].members[0])
                        .unwrap();
                    let delta = second.rect[1] - first.rect[1];
                    let settled = if direction == "down" {
                        delta > 1.
                    } else {
                        delta < -1.
                    };
                    if !settled {
                        assert!(
                            monotonic().unwrap() < until,
                            "native scrolling direction did not settle"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                }
                break snapshot;
            }
            assert!(
                monotonic().unwrap() < until,
                "owned native layout did not settle"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        if layout == "scrolling" && direction == "down" {
            scroll_profile_acceptance(checking, binding, fence, instance, id, output);
        }
        if layout == "scrolling" {
            reorder_acceptance(checking, binding, fence, app, instance, output, direction);
        }
        receipts.push(snapshot);
    }
    let columns = |snapshot: &prek::hyprland::layouts::Snapshot| {
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .unwrap()
            .columns
            .clone()
    };
    assert_eq!(columns(&receipts[1]), columns(&receipts[2]));
    session.stop().unwrap();
    settle(&mut session, &mut logs);
    fs::write(
        output.join("hyprland-layouts.json"),
        serde_json::to_vec(&serde_json::json!({
            "status":"passed","workspace":id,"snapshots":receipts
        }))
        .unwrap(),
    )
    .unwrap();
}

/// With animations on, disabling only the `windows` and `workspaces` parents
/// still proves goal geometry rendered; enabling `windows` withdraws it.
fn inherited_stillness(checking: &Client, instance: &str, ids: &[&String], output: &Path) {
    let eval = |command: &str| {
        let result = std::process::Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
            .args(["eval", command])
            .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "ok");
    };
    let mut sampler = ContextSampler::default();
    let mut visible = || -> Vec<bool> {
        sampler
            .sample(checking, 1, SessionKind::User)
            .unwrap()
            .windows
            .iter()
            .filter(|window| ids.contains(&&window.id))
            .map(|window| window.visible)
            .collect()
    };
    eval(
        "hl.animation({leaf=\"windows\",enabled=false}) \
         hl.animation({leaf=\"workspaces\",enabled=false}) \
         hl.config({animations={enabled=true}})",
    );
    let parents = visible();
    eval("hl.animation({leaf=\"windows\",enabled=true,speed=1,bezier=\"default\"})");
    let moving = visible();
    eval("hl.config({animations={enabled=false}})");
    let restored = visible();
    assert_eq!(parents, vec![true; ids.len()], "disabled parents");
    assert_eq!(moving, vec![false; ids.len()], "enabled windows node");
    assert_eq!(restored, vec![true; ids.len()], "animations off again");
    fs::write(
        output.join("inherited-stillness.json"),
        serde_json::to_vec(&serde_json::json!({
            "status":"passed","parents_disabled":parents,"windows_enabled":moving,"off":restored
        }))
        .unwrap(),
    )
    .unwrap();
}

/// The owned stock workspace is refused with its default gaps or borders,
/// then yields a viewport matching its windows once both are zero.
fn scroll_profile_acceptance(
    checking: &Client,
    binding: &Binding,
    fence: Fence,
    instance: &str,
    id: i64,
    output: &Path,
) {
    use prek::hyprland::scrolling::profile::{self, Refusal};
    let eval = |command: String| {
        let result = std::process::Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
            .args(["eval", &command])
            .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "ok");
    };
    let mut sampler = ContextSampler::default();
    let mut judge = || {
        let until = monotonic().unwrap() + Duration::from_secs(3);
        loop {
            if let (snapshot, Some(layouts)) = sampler
                .sample_with_layouts(checking, 1, SessionKind::User)
                .unwrap()
                && let Some(settings) = layouts.profile.clone()
            {
                assert_eq!(sampler.sample_scroll_profile(checking).unwrap(), settings);
                let workspace = layouts.workspaces.iter().find(|w| w.id == id).unwrap();
                let state = snapshot
                    .outputs
                    .iter()
                    .find(|o| workspace.output.as_ref() == Some(&o.geometry.name))
                    .unwrap();
                let viewport = profile::viewport(workspace, state, &settings);
                assert_eq!(
                    profile::available(&snapshot, &layouts, &state.geometry.name, id),
                    viewport
                );
                break (viewport, settings);
            }
            assert!(
                monotonic().unwrap() < until,
                "scroll profile was never attached"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let (initial, original) = judge();
    let expected = if original.gaps_in != [0; 4] || original.gaps_out != [0; 4] {
        Refusal::Gaps
    } else {
        Refusal::Border
    };
    assert_eq!(initial, Err(expected), "{original:?}");
    eval("hl.config({general={gaps_in=0,gaps_out=0,border_size=0}})".into());
    let until = monotonic().unwrap() + Duration::from_secs(3);
    let (viewport, flat) = loop {
        if let (Ok(viewport), settings) = judge() {
            break (viewport, settings);
        }
        assert!(
            monotonic().unwrap() < until,
            "zero-gap profile never validated"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let (low, high) = viewport.bounds();
    assert!(low <= viewport.offset() && viewport.offset() <= high);
    let executed = scroll_executor_acceptance(checking, binding, fence, instance, id);
    let gaps = |[top, right, bottom, left]: [i64; 4]| {
        format!("{{top={top},right={right},bottom={bottom},left={left}}}")
    };
    eval(format!(
        "hl.config({{general={{gaps_in={},gaps_out={},border_size={}}}}})",
        gaps(original.gaps_in),
        gaps(original.gaps_out),
        original.border
    ));
    let until = monotonic().unwrap() + Duration::from_secs(3);
    while judge().1 != original {
        assert!(
            monotonic().unwrap() < until,
            "profile settings were not restored"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let stock = |args: &[&str]| -> serde_json::Value {
        let result = std::process::Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
            .args(args)
            .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
            .output()
            .unwrap();
        assert!(result.status.success());
        serde_json::from_slice(&result.stdout).unwrap()
    };
    eval(
        "hl.curve(\"prekin\",{type=\"bezier\",points={{0.2,0.9},{0.3,1.0}}}) \
         hl.animation({leaf=\"layersIn\",enabled=true,speed=3,bezier=\"prekin\"})"
            .into(),
    );
    let animated = stock(&["-j", "animations"]);
    let global = stock(&["-j", "getoption", "animations:enabled"]);
    eval("hl.animation({leaf=\"layersIn\",enabled=false})".into());
    let layers_in = animated[0]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "layersIn")
        .unwrap();
    assert_eq!(
        (&layers_in["enabled"], &layers_in["bezier"]),
        (&serde_json::json!(true), &serde_json::json!("prekin"))
    );
    assert!(
        animated[1]
            .as_array()
            .unwrap()
            .iter()
            .any(|curve| curve["name"] == "prekin")
    );
    fs::write(
        output.join("layer-animations.json"),
        serde_json::to_vec(&serde_json::json!({"animations":animated,"option":global})).unwrap(),
    )
    .unwrap();
    fs::write(
        output.join("scroll-profile.json"),
        serde_json::to_vec(&serde_json::json!({
            "status":"passed","workspace":id,
            "initial_refusal":format!("{expected:?}"),
            "original":{"gaps_in":original.gaps_in,"gaps_out":original.gaps_out,"border":original.border},
            "flat":{"gaps_in":flat.gaps_in,"gaps_out":flat.gaps_out,"border":flat.border,
                "reserved":flat.reserved},
            "viewport":{"bounds":[low, high],"offset":viewport.offset()},
            "executor":executed
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Drives a continuous executor scroll over the flat owned workspace and
/// checks every settled native offset against the planner's destination.
fn scroll_executor_acceptance(
    checking: &Client,
    binding: &Binding,
    fence: Fence,
    instance: &str,
    id: i64,
) -> serde_json::Value {
    use prek::{
        actions::Executor,
        config::{Action, Direction, WindowAction},
        gestures::{Phase, Point, Space, TouchGesture},
        hyprland::scrolling::profile,
        protocol::{Completion, Mode, Request},
        rules::{Event, Evidence, Stage},
    };
    let mut sampler = ContextSampler::default();
    let mut sample = || {
        let until = monotonic().unwrap() + Duration::from_secs(3);
        loop {
            if let (snapshot, Some(layouts)) = sampler
                .sample_with_layouts(checking, 1, SessionKind::User)
                .unwrap()
                && layouts.profile.is_some()
            {
                return (snapshot, layouts);
            }
            assert!(
                monotonic().unwrap() < until,
                "owned scroll graph unavailable"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let view = |snapshot: Snapshot, layouts: prek::hyprland::layouts::Snapshot| View {
        fence,
        sampled_at: snapshot.captured_at,
        state: snapshot.state,
        context: Some(Box::new(ProvidedContext {
            binding: binding.clone(),
            snapshot,
            layouts: Some(layouts),
        })),
        error: None,
        morf: vec![],
        morf_rendered: vec![],
    };
    let (snapshot, layouts) = sample();
    let name = layouts
        .workspaces
        .iter()
        .find(|workspace| workspace.id == id)
        .and_then(|workspace| workspace.output.clone())
        .unwrap();
    let start = profile::available(&snapshot, &layouts, &name, id).unwrap();
    let (low, high) = start.bounds();
    let sign = if start.offset() - 200. >= low {
        1.
    } else {
        -1.
    };
    let mut executor = Executor::new(
        fence,
        "seat0".into(),
        Mode::Route,
        BTreeSet::from([Capability::HyprlandActions]),
    )
    .unwrap();
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    executor
        .sync_hyprland_context(&runtime, instance, view(snapshot, layouts))
        .unwrap();
    let (mut channel, mut broker) = Channel::pair().unwrap();
    let totals: Vec<f64> = (1..=10)
        .map(|step| 20. * f64::from(step))
        .chain((1..=5).map(|step| 200. - 20. * f64::from(step)))
        .map(|total| sign * total)
        .collect();
    let mut steps = Vec::new();
    let mut latencies = Vec::new();
    for (index, total) in totals.iter().copied().enumerate() {
        let ordinal = index as u64 + 1;
        let (stage, phase) = match index {
            0 => (Stage::Begin, Phase::Begin),
            _ if index + 1 == totals.len() => (Stage::End, Phase::End),
            _ => (Stage::Update, Phase::Update),
        };
        let (snapshot, layouts) = sample();
        let current = view(snapshot, layouts);
        let scope = current
            .context
            .as_ref()
            .unwrap()
            .scroll_scope(&name, id)
            .unwrap();
        executor.update_hyprland_context(current).unwrap();
        let now = monotonic().unwrap();
        let event = Box::new(Event::Matched {
            version: 1,
            rule: "G11_left".into(),
            id: 41,
            ordinal,
            source: Source {
                device: 702,
                generation: 1,
                seat: "seat0".into(),
            },
            at: now,
            config_generation: 1,
            session_generation: 1,
            stage,
            action: Action::Hyprland {
                action: WindowAction::ScrollStack,
            },
            evidence: Evidence::Touch {
                space: Space::Logical,
                gesture: TouchGesture::Pan {
                    fingers: 1,
                    phase,
                    direction: Direction::Vertical,
                    total: Point { x: 0., y: total },
                    delta: Point {
                        x: 0.,
                        y: 20. * sign,
                    },
                    velocity: Point { x: 0., y: 0. },
                    centroid: Point { x: 4., y: 300. },
                    swipe: true,
                },
            },
            window: None,
            output: Some(name.clone()),
            workspace: Some(id),
        });
        executor
            .offer_scoped(fence, event, Some(scope.clone()), now)
            .unwrap();
        executor
            .grant_scoped(
                fence,
                41,
                ordinal,
                now + Duration::from_millis(250),
                Some(scope),
                now,
            )
            .unwrap();
        let started = monotonic().unwrap();
        executor.execute_ready(started).unwrap();
        let elapsed = monotonic().unwrap() - started;
        executor.flush(&mut channel).unwrap();
        let mut completion = None;
        while let Some(packet) = broker.receive::<Request>().unwrap() {
            if let Request::Complete { result, .. } = packet.message {
                completion = Some(result);
            }
        }
        assert_eq!(completion, Some(Completion::Applied), "scroll step {index}");
        let (snapshot, layouts) = sample();
        let observed = profile::available(&snapshot, &layouts, &name, id)
            .unwrap()
            .offset();
        let expected = start.native((start.offset() - total).clamp(low, high));
        assert!(
            (observed - expected).abs() <= 2.,
            "scroll step {index}: observed {observed}, expected {expected}"
        );
        latencies.push(elapsed);
        steps.push(serde_json::json!({
            "ordinal":ordinal,"total":total,"expected":expected,"observed":observed,
            "execute_us":elapsed.as_micros()
        }));
        std::thread::sleep(Duration::from_millis(20));
    }
    latencies.sort();
    let percentile = |q: usize| latencies[(latencies.len() - 1) * q / 100].as_micros();
    serde_json::json!({
        "start":start.offset(),"bounds":[low, high],"steps":steps,
        "execute_us":{"p50":percentile(50),"p90":percentile(90),"max":percentile(100)}
    })
}

fn reorder_acceptance(
    checking: &Client,
    binding: &Binding,
    fence: Fence,
    app: &Path,
    instance: &str,
    output: &Path,
    direction: &str,
) {
    use prek::{
        actions::Executor,
        protocol::{Completion, Mode, Request},
        rules::{Event, Evidence, Stage},
    };
    let mut sampler = ContextSampler::default();
    let sample = |sampler: &mut ContextSampler| {
        let until = monotonic().unwrap() + Duration::from_secs(3);
        loop {
            let (snapshot, layouts) = sampler
                .sample_with_layouts(checking, 1, SessionKind::User)
                .unwrap();
            if let Some(layouts) = layouts {
                return (snapshot, layouts);
            }
            assert!(
                monotonic().unwrap() < until,
                "owned reorder graph unavailable"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let (_, initial) = sample(&mut sampler);
    let workspace = initial
        .workspaces
        .iter()
        .find(|ws| ws.active && !ws.special)
        .unwrap();
    assert_eq!(workspace.columns.len(), 3);
    let source = workspace.columns[1].members[0].clone();
    let guard = workspace.columns[if direction == "down" { 2 } else { 0 }].members[0].clone();
    focus_owned_window(&guard, instance);
    let source_lua = r#"
local p=require('prek')
local handler=p.actions.define('reorder',{requires={'hyprland_actions'}},function(payload)
 assert(payload.event.stage=='begin' and p.capabilities.ready('hyprland_actions'))
 return p.actions.hyprland{action=payload.event.rule}
end)
return p.config.new{mode='route',policy={allowed_devices={'keyboard'},allow_hyprland_actions=true},devices={keyboard={keys={'KEY_A','KEY_B'}}},rules={
 p.rules.define{id='reorder_up',devices={'keyboard'},trigger=p.triggers.key{key='KEY_A'},action=handler},
 p.rules.define{id='reorder_down',devices={'keyboard'},trigger=p.triggers.key{key='KEY_B'},action=handler},
}}
"#;
    let definition = Definition::compile(source_lua, "owned-native-reorder.lua").unwrap();
    let config = definition.validate().unwrap();
    let mut helper = Session::spawn(app, fence, "seat0", definition).unwrap();
    let capabilities = BTreeSet::from([
        Capability::Context,
        Capability::Observations,
        Capability::LuaEvents,
        Capability::LuaActions,
        Capability::HyprlandActions,
    ]);
    helper
        .update_capabilities(capabilities.clone(), BTreeSet::new())
        .unwrap();
    let mut logs = fs::File::create(output.join(format!("reorder-{direction}.jsonl"))).unwrap();
    settle(&mut helper, &mut logs);
    let view = |snapshot: Snapshot, layouts: prek::hyprland::layouts::Snapshot| View {
        fence,
        sampled_at: snapshot.captured_at,
        state: snapshot.state,
        context: Some(Box::new(ProvidedContext {
            binding: binding.clone(),
            snapshot,
            layouts: Some(layouts),
        })),
        error: None,
        morf: vec![],
        morf_rendered: vec![],
    };
    let (first, native) = sample(&mut sampler);
    let mut executor =
        Executor::new(fence, "seat0".into(), Mode::Route, capabilities.clone()).unwrap();
    let runtime = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    executor
        .sync_hyprland_context(&runtime, instance, view(first, native))
        .unwrap();
    executor.lua_ready(true);
    let (mut channel, mut broker) = Channel::pair().unwrap();
    let mut receipts = Vec::new();
    for index in 0..2 {
        let (snapshot, native) = sample(&mut sampler);
        let window = snapshot
            .windows
            .iter()
            .find(|window| window.id == source)
            .unwrap()
            .clone();
        let before = native
            .workspaces
            .iter()
            .find(|workspace| workspace.id == window.workspace)
            .unwrap()
            .columns
            .clone();
        let current = view(snapshot, native);
        let reorder_scope = current
            .context
            .as_ref()
            .unwrap()
            .reorder_scope(&source)
            .unwrap();
        executor.update_hyprland_context(current.clone()).unwrap();
        helper
            .update_capabilities(
                capabilities.clone(),
                executor.ready_capabilities(monotonic().unwrap()),
            )
            .unwrap();
        helper.update_context(current).unwrap();
        settle(&mut helper, &mut logs);
        let id = index as u64 + 1;
        let event = Box::new(Event::Matched {
            version: 1,
            rule: config.rules[index].id.clone(),
            id,
            ordinal: 1,
            source: Source {
                device: 701,
                generation: 1,
                seat: "seat0".into(),
            },
            at: monotonic().unwrap(),
            config_generation: 1,
            session_generation: 1,
            stage: Stage::Begin,
            action: config.rules[index].action.clone(),
            evidence: Evidence::Key { down: true },
            window: Some(source.clone()),
            output: Some(window.output.clone()),
            workspace: Some(window.workspace),
        });
        executor
            .offer_scoped(
                fence,
                event,
                Some(reorder_scope.clone()),
                monotonic().unwrap(),
            )
            .unwrap();
        executor.flush(&mut channel).unwrap();
        let mut authorized = false;
        while let Some(packet) = broker.receive::<Request>().unwrap() {
            if matches!(packet.message,Request::Authorize{id:incoming,ordinal:1,..} if incoming==id)
            {
                authorized = true;
            }
        }
        assert!(authorized, "reorder requires its own explicit model grant");
        let now = monotonic().unwrap();
        executor
            .grant_scoped(
                fence,
                id,
                1,
                now + Duration::from_millis(250),
                Some(reorder_scope.clone()),
                now,
            )
            .unwrap();
        let until = now + Duration::from_secs(3);
        let completion = loop {
            executor
                .execute_ready_with_lua(monotonic().unwrap(), Some(&mut helper))
                .unwrap();
            for log in helper.step(monotonic().unwrap()).unwrap() {
                serde_json::to_writer(&mut logs, &log).unwrap();
                writeln!(&mut logs).unwrap();
            }
            for (id, ordinal, result) in helper.take_actions() {
                executor
                    .resolve_action(id, ordinal, result, monotonic().unwrap())
                    .unwrap();
            }
            executor.flush(&mut channel).unwrap();
            let mut completed = None;
            while let Some(packet) = broker.receive::<Request>().unwrap() {
                if let Request::Complete {
                    id: incoming,
                    ordinal: 1,
                    result,
                    ..
                } = packet.message
                {
                    assert_eq!(incoming, id);
                    completed = Some(result);
                }
            }
            if let Some(result) = completed {
                break result;
            }
            assert!(
                monotonic().unwrap() < until,
                "reorder plan did not complete"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        let (after, native) = sample(&mut sampler);
        fs::write(output.join(format!("reorder-{direction}-{index}-debug.json")),serde_json::to_vec(&serde_json::json!({"completion":completion,"source":source,"before":before,"after":native})).unwrap()).unwrap();
        assert_eq!(
            completion,
            Completion::Applied,
            "native {direction} reorder"
        );
        let ws = native
            .workspaces
            .iter()
            .find(|workspace| workspace.id == window.workspace)
            .unwrap();
        let mut expected = before.clone();
        let position = expected
            .iter()
            .position(|column| column.members.contains(&source))
            .unwrap();
        let neighbor = if (index == 0) == (direction == "down") {
            position - 1
        } else {
            position + 1
        };
        let temporary = expected[position].members.clone();
        expected[position].members = expected[neighbor].members.clone();
        expected[neighbor].members = temporary;
        assert_eq!(ws.columns, expected);
        assert!(
            after
                .windows
                .iter()
                .any(|window| window.id == guard && window.mapped)
        );
        assert!(ws.windows.iter().any(|window| window.id == source));
        let capture = rendering::capture(
            &after,
            window.workspace,
            instance,
            output,
            &format!("reorder-{direction}-{index}"),
        );
        receipts.push(serde_json::json!({"completion":completion,"action":config.rules[index].id,"source":source,"guard":guard,"workspace":window.workspace,"columns_before":before,"columns_after":ws.columns,"capture":capture,"model_grant":true,"physical_takeover_tested":false}));
    }
    helper.stop().unwrap();
    settle(&mut helper, &mut logs);
    fs::write(
        output.join(format!("reorder-{direction}-receipts.json")),
        serde_json::to_vec(&receipts).unwrap(),
    )
    .unwrap();
}

fn power_acceptance(
    checking: &Client,
    binding: &Binding,
    fence: Fence,
    app: &Path,
    output: &Path,
    connection: (&Path, &str),
) {
    use prek::{
        actions::Executor,
        config::{Action, PowerAction, Screen},
        protocol::{Completion, Mode, Request},
        rules::{Event, Evidence, Stage},
    };
    let initial = ContextSampler::default()
        .sample(checking, 1, SessionKind::User)
        .unwrap();
    assert_eq!(initial.outputs.len(), 1);
    let target = &initial.outputs[0].geometry.name;
    assert!(prek::config::valid_power_output(target));
    let source=r#"
local p=require('prek')
local target=__OUTPUT__
p.events.on('ready',function()
 assert(p.capabilities.has('power_actions') and p.capabilities.has('lua_actions'))
 assert(not p.capabilities.ready('power_actions'))
end)
p.events.on('stop',function()assert(p.capabilities.current()==nil)end)
local handler=p.actions.define('screen',{requires={'power_actions'}},function(payload)
 assert(p.capabilities.ready('power_actions') and p.capabilities.ready('lua_actions'))
 assert(not p.capabilities.has('commands') and not p.capabilities.ready('commands'))
 return p.actions.power{action=payload.event.rule}
end)
return p.config.new{mode='route',policy={allowed_devices={'keyboard'},allow_power_actions=true},
devices={keyboard={keys={'KEY_A','KEY_B'}}},rules={
 p.rules.define{id='blank',devices={'keyboard'},output=target,trigger=p.triggers.key{key='KEY_A'},action=handler},
 p.rules.define{id='wake',devices={'keyboard'},output=target,trigger=p.triggers.key{key='KEY_B'},action=handler},
}}
"#.replace("__OUTPUT__",&serde_json::to_string(target).unwrap());
    let definition = Definition::compile(&source, "owned-power.lua").unwrap();
    let config = definition.validate().unwrap();
    let mut session = Session::spawn(app, fence, "seat0", definition).unwrap();
    let negotiated = BTreeSet::from([
        Capability::Context,
        Capability::Observations,
        Capability::LuaEvents,
        Capability::LuaActions,
        Capability::PowerActions,
    ]);
    session
        .update_capabilities(negotiated.clone(), BTreeSet::new())
        .unwrap();
    let mut ignored = Vec::new();
    settle(&mut session, &mut ignored);
    let mut sampler = ContextSampler::default();
    let first = sampler.sample(checking, 1, SessionKind::User).unwrap();
    assert_eq!(
        first.outputs.len(),
        1,
        "power fixture must own exactly one nested output"
    );
    let view = |snapshot: Snapshot| View {
        morf: Vec::new(),
        morf_rendered: Vec::new(),
        fence,
        sampled_at: snapshot.captured_at,
        state: snapshot.state,
        error: None,
        context: Some(Box::new(ProvidedContext {
            layouts: None,
            binding: binding.clone(),
            snapshot,
        })),
    };
    let mut executor = Executor::new(
        fence,
        "seat0".into(),
        Mode::Route,
        BTreeSet::from([Capability::LuaActions, Capability::PowerActions]),
    )
    .unwrap();
    executor
        .sync_power_context(connection.0, connection.1, view(first))
        .unwrap();
    executor.lua_ready(true);
    let (mut channel, mut broker) = Channel::pair().unwrap();
    let mut receipts = Vec::new();
    for (index, (power, screen)) in [
        (PowerAction::Blank, Screen::Off),
        (PowerAction::Wake, Screen::On),
    ]
    .into_iter()
    .enumerate()
    {
        let snapshot = sampler.sample(checking, 1, SessionKind::User).unwrap();
        let name = snapshot.outputs[0].geometry.name.clone();
        assert_eq!(config.rules[index].output.as_ref(), Some(&name));
        let expires = snapshot.captured_at + Duration::from_millis(250);
        let fresh = view(snapshot);
        executor.update_power_context(fresh.clone()).unwrap();
        session
            .update_capabilities(
                negotiated.clone(),
                executor.ready_capabilities(monotonic().unwrap()),
            )
            .unwrap();
        session.update_context(fresh).unwrap();
        settle(&mut session, &mut ignored);
        let id = index as u64 + 1;
        let event = Box::new(Event::Matched {
            version: 1,
            rule: config.rules[index].id.clone(),
            id,
            ordinal: 1,
            source: Source {
                device: 500,
                generation: 1,
                seat: "seat0".into(),
            },
            at: monotonic().unwrap(),
            config_generation: 1,
            session_generation: 1,
            stage: Stage::Discrete,
            action: config.rules[index].action.clone(),
            evidence: Evidence::Key { down: true },
            window: None,
            output: Some(name.clone()),
            workspace: None,
        });
        assert!(matches!(
            event.as_ref(),
            Event::Matched {
                action: Action::Lua { .. },
                ..
            }
        ));
        executor.offer(fence, event, monotonic().unwrap()).unwrap();
        executor.flush(&mut channel).unwrap();
        let mut authorized = false;
        while let Some(packet) = broker.receive::<Request>().unwrap() {
            if matches!(packet.message,Request::Authorize{id:incoming,ordinal:1,..} if incoming==id)
            {
                authorized = true;
            }
        }
        assert!(
            authorized,
            "effect must first request its explicit model broker grant"
        );
        executor
            .grant(fence, id, 1, expires, monotonic().unwrap())
            .unwrap();
        let deadline = monotonic().unwrap() + Duration::from_secs(3);
        let completion = loop {
            executor
                .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
                .unwrap();
            assert!(session.step(monotonic().unwrap()).unwrap().is_empty());
            for (id, ordinal, result) in session.take_actions() {
                executor
                    .resolve_action(id, ordinal, result, monotonic().unwrap())
                    .unwrap();
            }
            executor.flush(&mut channel).unwrap();
            let mut completed = None;
            while let Some(packet) = broker.receive::<Request>().unwrap() {
                if let Request::Complete {
                    id: incoming,
                    ordinal: 1,
                    result,
                    ..
                } = packet.message
                {
                    assert_eq!(incoming, id);
                    completed = Some(result);
                }
            }
            if let Some(completion) = completed {
                break completion;
            }
            assert!(
                monotonic().unwrap() < deadline,
                "power plan did not complete"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(
            completion,
            Completion::Applied,
            "DPMS plan {power:?} failed"
        );
        let observed = sampler.sample(checking, 1, SessionKind::User).unwrap();
        assert_eq!(observed.outputs[0].screen, screen);
        receipts.push(
            serde_json::json!({"id":id,"completion":completion,"screen":screen,"output":name,"lua_capabilities_checked":true}),
        );
    }
    session.stop().unwrap();
    settle(&mut session, &mut ignored);
    fs::write(
        output.join("power-receipts.json"),
        serde_json::to_vec(&receipts).unwrap(),
    )
    .unwrap();
}
