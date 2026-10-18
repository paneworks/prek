use super::*;
use prek::{
    context_transfer::View,
    input_events::{DeviceInfo, ResetReason},
    lua::{Definition, LogRecord, session::Session},
    recognition::{Metadata, Phase},
    worker::ProvidedContext,
};

fn settle(session: &mut Session) -> Vec<LogRecord> {
    let until = Instant::now() + Duration::from_secs(3);
    let mut logs = Vec::new();
    loop {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        if session.finished() {
            return logs;
        }
        assert!(Instant::now() < until, "custom Lua helper did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn feed(routing: &mut Routing, sequence: u64, events: &[(u16, i32)], inputs: &mut Vec<Frame>) {
    let frame = Frame {
        source: source(500),
        sequence,
        at: monotonic().unwrap(),
        events: events
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
            .collect(),
    };
    inputs.push(frame.clone());
    routing.push(Record::Frame { frame }).unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Case {
    Root,
    Activation,
    NativeFirst,
    CustomFirst,
}

pub(super) fn exercise(
    output: &Path,
    socket: &Path,
    index: usize,
    text: &str,
    case: Case,
) -> String {
    let event_local = case == Case::Activation;
    let native_first = case == Case::NativeFirst;
    let custom_first = case == Case::CustomFirst;
    let definition = Definition::compile(
        &r#"
local p=require('prek')
local trigger=p.recognizers.define('rendered_key',{
 selectors={'custom_keys'},inputs={'key'},reply_within='250ms',
},function(ticket)
 if ticket.cause=='cancel' then p.log.info('custom-key-cleanup');return nil end
 assert(ticket.cause=='frame')
 @changes@
 return {phase='discrete',data={trigger=202}}
end)
@sequence@
return p.config.new{mode='route',
 policy={allow_input_events=true,allowed_devices={'custom_keys'}},
 timing={join_window='1ms',decision_window='200ms'},
 devices={custom_keys={keys={'KEY_PROG3'}}},
 rules={p.rules.define{id='rendered-custom-key',devices={'custom_keys'},contexts={'desktop'},
  scope='everywhere',delivery='buffer',trigger=trigger,@activation@action=p.actions.keys{keys={'KEY_A'}}}},
}
"#.replace("@changes@", if event_local {
            "assert(#ticket.input.changes==3);assert(ticket.input.changes[1].code==42 and ticket.input.changes[1].state=='press');assert(ticket.input.changes[2].code==202 and ticket.input.changes[2].state=='press');assert(ticket.input.changes[3].code==42 and ticket.input.changes[3].state=='release')"
        } else { "assert(#ticket.input.changes==1 and ticket.input.changes[1].code==202)" })
        .replace("@activation@", if event_local { "require_keys={'KEY_LEFTSHIFT'}," } else { "" })
        .replace("@sequence@", match case {
            Case::NativeFirst => "trigger=p.triggers.sequence{steps={p.triggers.key{key='KEY_PROG3'},trigger},timeout='200ms'}",
            Case::CustomFirst => "trigger=p.triggers.sequence{steps={trigger,p.triggers.key{key='KEY_PROG4'}},timeout='200ms'}",
            _ => "",
        }),
        "cage-custom-key.lua",
    )
    .unwrap();
    let config = definition.validate().unwrap();
    let trigger = serde_json::to_value(&config.rules[0].trigger).unwrap();
    let activation = json!({
        "require_keys":config.rules[0].require_keys.iter().map(|key| key.0).collect::<Vec<_>>(),
        "keys_throughout":config.rules[0].keys_throughout,
    });
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let binary =
        std::env::var_os("PREK_TEST_LUA_BINARY").expect("explicit current Lua helper binary");
    let mut session = Session::spawn(Path::new(&binary), fence, "seat0", definition).unwrap();
    assert!(settle(&mut session).is_empty());
    let at = monotonic().unwrap();
    let provided = ProvidedContext {
        binding: Binding {
            seat: "seat0".into(),
            session: "owned-custom-protocol-model".into(),
            uid: 1000,
            compositor_pid: 42,
            compositor_start: 1,
            kind: SessionKind::User,
        },
        snapshot: Snapshot {
            generation: 1,
            revision: 1,
            captured_at: at,
            state: State::Desktop,
            outputs: vec![],
            windows: vec![],
            regions: vec![],
            secure_overlay: false,
        },
        layouts: None,
    };
    session
        .update_context(View {
            fence,
            sampled_at: at,
            state: State::Desktop,
            context: Some(Box::new(provided.clone())),
            error: None,
            morf: vec![],
            morf_rendered: vec![],
        })
        .unwrap();
    session
        .update_capabilities(
            BTreeSet::from([
                Capability::CustomRecognizers,
                Capability::LuaEvents,
                Capability::Observations,
                Capability::Context,
            ]),
            BTreeSet::new(),
        )
        .unwrap();
    assert!(settle(&mut session).is_empty());
    let device = Device {
        source: source(500),
        identity: "custom-key-protocol-model".into(),
        name: "Custom protocol fixture source".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![31, 42, 202, 203])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    session
        .recognizer_metadata(Metadata {
            source: device.source.clone(),
            at,
            selectors: vec!["custom_keys".into()],
            reason: ResetReason::Attached,
            metadata: Some(Box::new(DeviceInfo {
                device: device.clone(),
                selectors: vec!["custom_keys".into()],
                mapping: None,
            })),
        })
        .unwrap();
    assert!(settle(&mut session).is_empty());
    let mut routing = Routing::with_key_projection(config, fence).unwrap();
    routing
        .authority_mut()
        .bind(provided.binding, true, false)
        .unwrap();
    routing
        .authority_mut()
        .update(provided.snapshot, at)
        .unwrap();
    routing
        .attach(device.clone(), &ready(&device, at), None, at)
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), at);
    let mut raw = Keyboard::open(output, socket, 960);
    let mut generated = Seat::new(
        fence,
        Output::new(Keyboard::open(output, socket, 961)).unwrap(),
    )
    .unwrap();
    let mut inputs = Vec::new();
    feed(
        &mut routing,
        1,
        if event_local {
            &[(31, 1), (31, 0), (42, 1), (202, 1), (42, 0)]
        } else if native_first {
            &[(31, 1), (31, 0), (202, 1)]
        } else {
            &[(202, 1)]
        },
        &mut inputs,
    );
    let tickets = routing.recognition_requests(monotonic().unwrap()).unwrap();
    assert_eq!(tickets.len(), 1);
    let ticket = tickets.into_iter().next().unwrap();
    session.request_recognition(ticket.clone()).unwrap();
    assert!(settle(&mut session).is_empty());
    let replies = session.take_recognitions();
    assert_eq!(replies.len(), 1);
    let (id, proposal) = replies.into_iter().next().unwrap();
    assert_eq!(id, ticket.id);
    let proposal = proposal.unwrap();
    assert_eq!(proposal.phase, Phase::Discrete);
    assert_eq!(proposal.data, json!({"trigger":202}));
    let proposed_at = monotonic().unwrap();
    feed(
        &mut routing,
        2,
        if custom_first {
            &[(203, 1), (203, 0), (31, 1), (31, 0)]
        } else {
            &[(31, 1), (31, 0)]
        },
        &mut inputs,
    );
    let admitted_at = monotonic().unwrap();
    routing
        .recognition_response(proposal.clone(), admitted_at)
        .unwrap();
    let mut work = routing
        .take_scoped_dispatches(admitted_at)
        .into_iter()
        .map(|dispatch| dispatch.into_key_work().ok().unwrap())
        .collect::<Vec<_>>();
    let reply_dispatches = work.len();
    let work_admitted_at = if custom_first {
        assert!(work.is_empty());
        feed(&mut routing, 3, &[(203, 1), (203, 0)], &mut inputs);
        let at = monotonic().unwrap();
        work = routing
            .take_scoped_dispatches(at)
            .into_iter()
            .map(|dispatch| dispatch.into_key_work().ok().unwrap())
            .collect();
        at
    } else {
        admitted_at
    };
    assert_eq!(work.len(), 1);
    generated
        .admit(&routing, work, None, work_admitted_at)
        .unwrap();
    feed(
        &mut routing,
        if custom_first { 4 } else { 3 },
        &[(202, 0)],
        &mut inputs,
    );
    let mut ordinary = Vec::new();
    let until = Instant::now() + Duration::from_millis(200);
    loop {
        let at = monotonic().unwrap();
        routing.push(Record::Tick { at }).unwrap();
        generated.poll(&routing, at).unwrap();
        while let Some(scheduled) = routing.due(&device.source, at).unwrap().cloned() {
            let span = routing
                .input_span(&device.source, scheduled.ticket)
                .unwrap();
            if generated.blocks_ordered_span(&routing, &device.source, span) {
                break;
            }
            let edges = scheduled
                .frame
                .events
                .iter()
                .filter(|event| event.kind == EV_KEY)
                .map(|event| (event.code, event.value))
                .collect::<Vec<_>>();
            ordinary.extend_from_slice(&edges);
            raw.send(scheduled.frame.sequence, &edges);
            routing
                .delivered(&device.source, scheduled.ticket, at)
                .unwrap();
        }
        if generated.settled() {
            break;
        }
        assert!(
            Instant::now() < until,
            "custom protocol key action did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        ordinary,
        if event_local || native_first {
            vec![(31, 1), (31, 0), (31, 1), (31, 0)]
        } else if custom_first {
            vec![(203, 1), (203, 0), (31, 1), (31, 0)]
        } else {
            vec![(31, 1), (31, 0)]
        }
    );
    let cancellations = routing.take_recognition_cancellations();
    assert_eq!(cancellations.len(), 1);
    session
        .cancel_recognition(cancellations.into_iter().next().unwrap())
        .unwrap();
    let logs = settle(&mut session);
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "custom-key-cleanup");
    session.stop().unwrap();
    generated.retire(monotonic().unwrap()).unwrap();
    for _ in 0..8 {
        if generated.drain(monotonic().unwrap()).unwrap() == Progress::Complete {
            break;
        }
    }
    assert!(generated.settled());
    assert!(generated.into_output().unwrap().into_sink().is_ok());
    drop(raw);
    fs::write(output.join(match case {
        Case::Root => "custom-key-runtime.json",
        Case::Activation => "custom-key-activation-runtime.json",
        Case::NativeFirst => "custom-key-native-prefix-runtime.json",
        Case::CustomFirst => "custom-key-native-suffix-runtime.json",
    }), serde_json::to_vec(&json!({
        "status":"passed","scope":if native_first || custom_first { "owned_protocol_composed_key_only_custom" } else { "owned_protocol_root_key_only_custom" },"actual_lua_callback":true,
        "source_and_readiness_modeled":true,"physical_takeover_tested":false,"kernel_input_opened":false,
        "helper_stopped":true,"generated_neutral":true,"ordinary":ordinary,
        "activation":activation,
        "trigger":trigger,"work_admitted_at":work_admitted_at,"reply_dispatches":reply_dispatches,
        "inputs":inputs,"ticket":ticket,"proposal":proposal,"proposed_at":proposed_at,"admitted_at":admitted_at,
    })).unwrap()).unwrap();
    let text = format!(
        "{text}{}",
        if event_local || native_first {
            "ssa"
        } else {
            "sa"
        }
    );
    phase(output, index, &text, &[]);
    text
}
