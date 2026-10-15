#![cfg(target_os = "linux")]
//! Real helper/worker IPC; authority and device metadata are explicit models.
use prek::{
    configuration::Command,
    context::{Binding, SessionKind, Snapshot, State},
    input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, Source},
    ipc::{Channel, monotonic},
    lua::{Definition, EventKind, LogRecord, session::Session},
    protocol::{Capability, Epoch, Fence},
    replay::Record,
    worker::{Message, ProvidedContext, Report, Stream, Subscription, Subscriptions},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

fn drain(session: &mut Session) -> Vec<LogRecord> {
    let until = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    loop {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        if session.finished() {
            return logs;
        }
        assert!(monotonic().unwrap() < until, "input helper timeout");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn real_worker_and_helper_deliver_only_normalized_own_seat_input_and_held_state_resets() {
    let definition=Definition::compile(r#"
local p=require('prek')
local held=false
p.events.on('input',function(payload)
 local e=payload.input
 assert(e.source.device=='9223372036854775808' and e.source.generation=='18446744073709551615')
 assert(e.source.seat=='seat0' and e.raw==nil and e.events==nil and e.selectors[1]=='keyboard')
 local info=p.devices.get(e.source)
 assert(info and info.device.name=='owned input model' and info.device.capabilities[1][1]==30)
 assert(p.devices.revision()=='1' and #p.devices.list()==1)
 assert(p.devices.position(e.source,0,0)==nil)
 assert(p.seats.current().binding.seat=='seat0')
 if e.kind=='reset' then
   assert(e.reason=='attached' and e.keys[1]==30)
   held=true;p.log.info('input-reset')
 else
   assert(type(e.sequence)=='string' and #e.changes==1 and e.changes[1].code==30)
   if e.changes[1].state=='release' then assert(held);held=false;p.log.info('input-release')
   else assert(not held);held=true;p.log.info('input-press') end
 end
 e.source.seat='mutated';e.selectors[1]='mutated'
end)
return p.config.new{policy={allow_input_events=true,allowed_devices={'keyboard'}},devices={keyboard={keys={'KEY_A'}}}}
"#,"input-worker.lua").unwrap();
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence,
        "seat0",
        definition.clone(),
    )
    .unwrap();
    drain(&mut session);
    let (mut parent, child) = Channel::pair().unwrap();
    let mut worker = Stream::new(definition.validate().unwrap(), child).unwrap();
    worker.push(Record::Header { version: 1 }).unwrap();
    let source = Source {
        device: 1u64 << 63,
        generation: u64::MAX,
        seat: "seat0".into(),
    };
    let device = Device {
        source: source.clone(),
        identity: "owned-input-model".into(),
        name: "owned input model".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        axes: BTreeMap::new(),
        capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
        virtual_device: false,
    };
    worker
        .push(Record::Device {
            device,
            keys: BTreeSet::from([30]),
            contacts: vec![],
        })
        .unwrap();
    worker.flush().unwrap();
    parent.receive::<Message>().unwrap().unwrap();
    let now = monotonic().unwrap();
    let subscription = Subscription {
        seat: "seat0".into(),
        fence,
        consumer: None,
        capabilities: BTreeSet::from([
            Capability::InputEvents,
            Capability::LuaEvents,
            Capability::Observations,
            Capability::Context,
        ]),
        context: Some(Box::new(ProvidedContext {
            binding: Binding {
                seat: "seat0".into(),
                session: "model-session".into(),
                // SAFETY: read-only UID query for this helper's owning user.
                uid: unsafe { nix::libc::geteuid() },
                compositor_pid: 42,
                compositor_start: 1,
                kind: SessionKind::User,
            },
            snapshot: Snapshot {
                generation: 1,
                revision: 1,
                captured_at: now,
                state: State::Desktop,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
        })),
    };
    session
        .update_context(prek::context_transfer::View {
            morf: Vec::new(),
            fence,
            sampled_at: now,
            state: State::Desktop,
            context: subscription.context.clone(),
            error: None,
        })
        .unwrap();
    drain(&mut session);
    parent
        .send(
            &Command::Subscriptions {
                update: Subscriptions {
                    revision: 1,
                    seats: vec![subscription],
                },
            },
            now,
        )
        .unwrap();
    worker.service().unwrap();
    worker.flush().unwrap();
    let mut logs = Vec::new();
    let forward = |parent: &mut Channel, session: &mut Session, logs: &mut Vec<LogRecord>| {
        while let Some(packet) = parent.receive::<Message>().unwrap() {
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
                logs.extend(drain(session));
            }
        }
    };
    forward(&mut parent, &mut session, &mut logs);
    for (sequence, value) in [(1, 0), (2, 1)] {
        worker
            .push(Record::Frame {
                frame: Frame {
                    source: source.clone(),
                    at: monotonic().unwrap(),
                    sequence,
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code: 30,
                            value,
                        },
                        RawEvent {
                            kind: EV_SYN,
                            code: 0,
                            value: 0,
                        },
                    ],
                },
            })
            .unwrap();
        worker.flush().unwrap();
        forward(&mut parent, &mut session, &mut logs);
    }
    assert_eq!(
        logs.iter()
            .map(|log| log.message.as_str())
            .collect::<Vec<_>>(),
        vec!["input-reset", "input-release", "input-press"]
    );
    session.stop().unwrap();
    drain(&mut session);
}

#[test]
fn input_contact_and_sequence_identities_remain_exact_in_the_real_helper() {
    use prek::input::{Change, Contact, ContactId, KeyState};
    let definition = Definition::compile(
        r#"
local p=require('prek')
local subscription
subscription=p.events.on('input',function(payload)
 local e=payload.input
 assert(e.sequence=='18446744073709551615' and e.source.device=='18446744073709551615')
 assert(e.contacts[1].id.generation=='18446744073709551615')
 assert(e.contacts_started[1].generation=='18446744073709551615')
 assert(e.contacts_ended[1].generation=='18446744073709551615')
 assert(e.ended_contacts[1].id.generation=='18446744073709551615')
 assert(e.contacts[1].id.tracking==7 and e.contacts[1].x==123 and e.changes[1].code==30)
 p.log.info('input-exact');subscription:dispose()
end)
return {version=1}
"#,
        "input-exact.lua",
    )
    .unwrap();
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence,
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session);
    let id = ContactId {
        slot: 0,
        tracking: 7,
        generation: u64::MAX,
    };
    let contact = Contact {
        id,
        x: 123,
        y: 456,
        pressure: Some(10),
        fresh: true,
    };
    let input = prek::input_events::Event::Frame {
        source: Source {
            device: u64::MAX,
            generation: u64::MAX,
            seat: "seat0".into(),
        },
        at: monotonic().unwrap(),
        sequence: u64::MAX,
        selectors: vec!["touch".into()],
        changes: vec![Change::Key {
            code: 30,
            state: KeyState::Press,
        }],
        contacts: vec![contact.clone()],
        contacts_started: vec![id],
        contacts_ended: vec![id],
        ended_contacts: vec![contact],
    };
    let payload = serde_json::json!({"fence":fence,"input":input});
    session.notify(EventKind::Input, payload.clone()).unwrap();
    assert_eq!(drain(&mut session)[0].message, "input-exact");
    session.notify(EventKind::Input, payload).unwrap();
    assert!(drain(&mut session).is_empty());
    session.stop().unwrap();
    drain(&mut session);
}
