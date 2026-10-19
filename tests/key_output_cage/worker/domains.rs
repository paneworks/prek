use super::*;
use prek::{
    context_transfer::View,
    input::EV_REL,
    input_events::{DeviceInfo, ResetReason},
    lua::{Definition, session::Session},
    recognition::{Metadata, Phase},
    worker::ProvidedContext,
};

fn write_due(
    routing: &mut Routing,
    source: &Source,
    sink: &mut Keyboard,
    written: &mut Vec<Value>,
) {
    while let Some(scheduled) = routing.due(source, monotonic().unwrap()).unwrap().cloned() {
        let sent = monotonic().unwrap();
        sink.enqueue(&Frame {
            source: sink.source.clone(),
            ..scheduled.frame.clone()
        })
        .unwrap();
        while sink.flush().unwrap() == Progress::Pending {}
        routing
            .delivered(source, scheduled.ticket, monotonic().unwrap())
            .unwrap();
        written.push(json!({"frame":scheduled.frame, "due":scheduled.due, "sent":sent}));
    }
}

pub(super) fn exercise(output: &Path, sink: &mut Keyboard) {
    let definition = Definition::compile(r#"
local p=require('prek')
local trigger=p.recognizers.define('rendered_relative',{
 selectors={'relative'},inputs={'relative'},reply_within='250ms',
},function(ticket)
 if ticket.cause=='cancel' then p.log.info('relative-cleanup');return nil end
 assert(ticket.cause=='frame' and #ticket.input.changes==2 and #ticket.input.contacts==0)
 if ticket.input.changes[1].value==5 then
  assert(ticket.input.changes[1].code==0 and ticket.input.changes[2].code==1 and ticket.input.changes[2].value==3)
  return {phase='cancel',data={x=5,y=3}}
 end
 assert(ticket.input.changes[1].code==0 and ticket.input.changes[1].value==47)
 assert(ticket.input.changes[2].code==1 and ticket.input.changes[2].value==31)
 return {phase='discrete',data={x=47,y=31}}
end)
return p.config.new{mode='route',
 policy={allow_input_events=true,allowed_devices={'relative'}},
 timing={join_window='1ms',decision_window='200ms'},
 devices={relative={keys={'KEY_LEFT'}}},
 rules={p.rules.define{id='rendered-relative',devices={'relative'},contexts={'desktop'},
  scope='everywhere',delivery='buffer',trigger=trigger,action=p.actions.event{name='relative-consumed'}}},
}
"#, "cage-relative.lua").unwrap();
    let config = definition.validate().unwrap();
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let binary =
        std::env::var_os("PREK_TEST_LUA_BINARY").expect("explicit current Lua helper binary");
    let mut session = Session::spawn(Path::new(&binary), fence, "seat0", definition).unwrap();
    assert!(custom::settle(&mut session).is_empty());
    let at = monotonic().unwrap();
    let provided = ProvidedContext {
        binding: Binding {
            seat: "seat0".into(),
            session: "owned-relative-protocol-model".into(),
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
    assert!(custom::settle(&mut session).is_empty());
    let device = Device {
        source: source(601),
        identity: "domain-protocol-model".into(),
        name: "Independent-domain source model".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![105]), (EV_REL, vec![0, 1])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    session
        .recognizer_metadata(Metadata {
            source: device.source.clone(),
            at,
            selectors: vec!["relative".into()],
            reason: ResetReason::Attached,
            metadata: Some(Box::new(DeviceInfo {
                device: device.clone(),
                selectors: vec!["relative".into()],
                mapping: None,
            })),
        })
        .unwrap();
    assert!(custom::settle(&mut session).is_empty());
    let mut routing = Routing::with_input_projection(config, fence).unwrap();
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
    routing.actions_available(
        1,
        BTreeSet::from([Capability::LuaEvents, Capability::Observations]),
        at,
    );
    let make = |sequence, events: &[(u16, u16, i32)]| Frame {
        source: device.source.clone(),
        sequence,
        at: monotonic().unwrap(),
        events: events
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .chain([RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }])
            .collect(),
    };
    let first = make(
        1,
        &[
            (EV_REL, 0, 47),
            (EV_KEY, 105, 1),
            (EV_REL, 1, 31),
            (EV_KEY, 105, 0),
        ],
    );
    routing
        .push(Record::Frame {
            frame: first.clone(),
        })
        .unwrap();
    let mut written = Vec::new();
    write_due(&mut routing, &device.source, sink, &mut written);
    assert_eq!(written.len(), 1);
    let ticket = routing
        .recognition_requests(monotonic().unwrap())
        .unwrap()
        .pop()
        .unwrap();
    session.request_recognition(ticket.clone()).unwrap();
    assert!(custom::settle(&mut session).is_empty());
    let mut proposals = session.take_recognitions();
    assert_eq!(proposals.len(), 1);
    let (id, proposal) = proposals.pop().unwrap();
    assert_eq!(id, ticket.id);
    let proposal = proposal.unwrap();
    assert_eq!(proposal.phase, Phase::Discrete);
    assert_eq!(proposal.data, json!({"x":47,"y":31}));
    let admitted_at = monotonic().unwrap();
    routing
        .recognition_response(proposal.clone(), admitted_at)
        .unwrap();
    let dispatches = routing.take_scoped_dispatches(admitted_at);
    assert_eq!(dispatches.len(), 1);
    let dispatch = serde_json::to_value(&dispatches[0].event).unwrap();
    write_due(&mut routing, &device.source, sink, &mut written);
    assert_eq!(written.len(), 1);
    let cancellations = routing.take_recognition_cancellations();
    assert_eq!(cancellations.len(), 1);
    session
        .cancel_recognition(cancellations.into_iter().next().unwrap())
        .unwrap();
    let mut logs = custom::settle(&mut session);
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "relative-cleanup");
    let second = make(2, &[(EV_REL, 0, 5), (EV_REL, 1, 3)]);
    routing
        .push(Record::Frame {
            frame: second.clone(),
        })
        .unwrap();
    let ordinary_ticket = routing
        .recognition_requests(monotonic().unwrap())
        .unwrap()
        .pop()
        .unwrap();
    session
        .request_recognition(ordinary_ticket.clone())
        .unwrap();
    assert!(custom::settle(&mut session).is_empty());
    let mut proposals = session.take_recognitions();
    assert_eq!(proposals.len(), 1);
    let (id, ordinary_proposal) = proposals.pop().unwrap();
    assert_eq!(id, ordinary_ticket.id);
    let ordinary_proposal = ordinary_proposal.unwrap();
    assert_eq!(ordinary_proposal.phase, Phase::Cancel);
    assert_eq!(ordinary_proposal.data, json!({"x":5,"y":3}));
    let ordinary_admitted_at = monotonic().unwrap();
    routing
        .recognition_response(ordinary_proposal.clone(), ordinary_admitted_at)
        .unwrap();
    assert!(
        routing
            .take_scoped_dispatches(ordinary_admitted_at)
            .is_empty()
    );
    let cancellations = routing.take_recognition_cancellations();
    assert_eq!(cancellations.len(), 1);
    session
        .cancel_recognition(cancellations.into_iter().next().unwrap())
        .unwrap();
    logs.extend(custom::settle(&mut session));
    assert_eq!(logs.len(), 2);
    assert!(logs.iter().all(|log| log.message == "relative-cleanup"));
    let until = Instant::now() + Duration::from_millis(200);
    while written.len() < 2 {
        write_due(&mut routing, &device.source, sink, &mut written);
        assert!(
            Instant::now() < until,
            "ordinary relative positive control did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(written.len(), 2);
    session.stop().unwrap();
    fs::write(output.join("domain-journal-runtime.json"), serde_json::to_vec(&json!({
        "status":"passed", "scope":"owned_protocol_configured_domains", "source_and_readiness_modeled":true,
        "native_gesture_arbitration_tested":true, "physical_takeover_tested":false,
        "kernel_input_opened":false, "mt_rendering_tested":false, "switch_ownership_tested":false,
        "inputs":[first,second], "delivered":written,
        "configured":{"ticket":ticket,"proposal":proposal,"dispatch":dispatch,"admitted_at":admitted_at,
            "ordinary_ticket":ordinary_ticket,"ordinary_proposal":ordinary_proposal,"ordinary_admitted_at":ordinary_admitted_at,
            "ordinary_dispatches":0,
            "supervised_lua":true,"helper_stopped":true,"cleanup_count":logs.len()},
    })).unwrap()).unwrap();
}
