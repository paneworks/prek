//! Local GUI acceptance: actual compositor queries and Luna helper callbacks,
//! with synthetic source/readiness authority explicitly confined to this test.
#![cfg(target_os = "linux")]
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
    let snapshot = loop {
        match sampler.sample(client, 1, SessionKind::User) {
            Ok(snapshot) => break snapshot,
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
            fence,
            sampled_at: now,
            state: snapshot.state,
            context: Some(Box::new(ProvidedContext {
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
    let input_control = std::env::var("PREK_CAGE_INPUT_TEST").as_deref() == Ok("1");
    let source = r#"
local p=require('prek')
local layer_control=__LAYER_CONTROL__
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
  p.log.info('local-context',{revision=snapshot.revision,outputs=#snapshot.outputs,uid=binding.uid})
end)
p.events.on('matched',function(event)
  assert(p.context.snapshot() and p.context.state()=='desktop')
  if event.event.rule=='local-a' then
    p.log.info('local-matched',{rule=event.event.rule,stage=event.event.stage})
  elseif event.event.rule=='local-b' and event.event.stage=='end' and not transaction then
    transaction=p.rules.transaction()
    if layer_control then transaction:deactivate_layer('local-layer') else transaction:disable('local-a') end
    transaction:commit()
  end
end)
p.events.on('rules_changed',function(event)
  assert(event.accepted and event.revision=='2' and p.rules.get('local-a').enabled==layer_control)
  assert(p.layers.get('local-layer').activation==(layer_control and 'inactive' or 'active'))
  assert(transaction:status().state=='accepted' and p.rules.revision()=='2')
  p.log.info('local-rules-changed',{accepted=event.accepted,revision=event.revision,state=transaction:status().state})
  p.log.info('local-layer-state',{activation=p.layers.get('local-layer').activation,rule_enabled=p.rules.get('local-a').enabled})
end)
return p.config.new {
  mode=input_control and 'observe' or 'route',policy={allowed_devices={'keyboard'},allow_runtime_rules=true,allow_input_events=input_control},
  devices={keyboard={name='Prek local keyboard'}},
  layers={p.layers.define{id='local-layer',key='KEY_LEFTMETA',activation='active'}},
  rules={p.rules.define {id='local-a',layer='local-layer',priority=20,devices={'keyboard'},contexts={'desktop'},
    delivery='reserve',trigger=p.triggers.key {key='KEY_A'},
    action=p.actions.event {name='local-a-claimed'}},
    p.rules.define {id='local-b',devices={'keyboard'},contexts={'desktop'},delivery='observe',
      trigger=p.triggers.key {key='KEY_B'},action=p.actions.event {name='local-rule-disable'}}}
}
"#.replace("__LAYER_CONTROL__", if layer_control { "true" } else { "false" }).replace("__INPUT_CONTROL__",if input_control {"true"}else{"false"});
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
    println!(
        "Verified real Hyprland context and Luna transaction receipts, with model-routed A consumption before the edit and A/B forwarding afterward"
    );
}
