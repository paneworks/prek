#![cfg(target_os = "linux")]
//! Actual helper and worker IPC with model seat authority; no kernel devices
//! or logind registration are created by this fixture.
use prek::{
    configuration::Command,
    ipc::{Channel, monotonic},
    lua::{Definition, EventKind, LogRecord, RuleProposal, RuleReceipt, session::Session},
    protocol::{Epoch, Fence},
    worker::{Message, Stream, Subscription, Subscriptions},
};
use std::{path::Path, time::Duration};

fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    }
}
const SOURCE: &str = r#"
local p=require('prek')
local tx
p.events.on('ready',function()
 tx=p.rules.transaction(); tx:disable('a'); tx:commit()
end)
p.events.on('rules_changed',function(event)
 assert(not p.rules.get('a').enabled and p.rules.revision()=='2')
 assert(tx:status().state==(event.accepted and 'accepted' or 'rejected'))
 p.log.info('receipt',{accepted=event.accepted,revision=event.revision,state=tx:status().state})
end)
p.events.on('reload',function()
 tx=p.rules.transaction(); tx:enable('a'); tx:commit()
end)
p.events.on('stop',function()
 assert(tx:status().state=='rejected')
 assert(not pcall(function()p.rules.transaction()end))
 p.log.info('stop-retired')
end)
return p.config.new{rules={p.rules.define{id='a',delivery='reserve',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='a'}}}}
"#;
fn spawn() -> (Session, Definition) {
    let definition = Definition::compile(SOURCE, "live-rules.lua").unwrap();
    let session = Session::spawn_with_rule_transactions(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(),
        "seat0",
        definition.clone(),
    )
    .unwrap();
    (session, definition)
}
fn proposal(session: &mut Session) -> RuleProposal {
    let expires = monotonic().unwrap() + Duration::from_secs(5);
    loop {
        session.step(monotonic().unwrap()).unwrap();
        let mut proposals = session.take_rule_proposals();
        if !proposals.is_empty() {
            assert_eq!(proposals.len(), 1);
            return proposals.remove(0);
        }
        assert!(monotonic().unwrap() < expires, "helper proposal timeout");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn drain(session: &mut Session) -> Vec<LogRecord> {
    let expires = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    loop {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        if session.finished() {
            return logs;
        }
        assert!(monotonic().unwrap() < expires, "helper receipt timeout");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn apply(parent: &mut Channel, worker: &mut Stream, proposal: RuleProposal) -> RuleReceipt {
    assert!(
        parent
            .send(
                &Command::Rules {
                    seat: "seat0".into(),
                    fence: fence(),
                    id: proposal.id,
                    transaction: proposal.transaction
                },
                monotonic().unwrap()
            )
            .unwrap()
    );
    worker.service().unwrap();
    worker.flush().unwrap();
    loop {
        match parent.receive::<Message>().unwrap().unwrap().message {
            Message::RulesChanged {
                seat,
                fence: owner,
                id,
                revision,
                error,
            } => {
                assert_eq!(seat, "seat0");
                assert_eq!(owner, fence());
                return RuleReceipt {
                    id,
                    revision,
                    error,
                };
            }
            Message::Outcome { .. } => {}
            other => panic!("unexpected worker message {other:?}"),
        }
    }
}

#[test]
fn unused_source_recognizer_is_installed_by_real_helper_and_native_worker_receipt() {
    use prek::{
        input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source},
        protocol::Capability,
        replay::Record,
        rules::{Event, Evidence},
        worker::Report,
    };
    use std::collections::{BTreeMap, BTreeSet};
    let definition = Definition::compile(r#"
local p=require('prek')
local trigger=p.recognizers.define('dynamic',{selectors={'keyboard'},inputs={'key'},reply_within='250ms'},function(ticket)
 if ticket.cause=='cancel' then return nil end
 local info=p.devices.get(ticket.input.source)
 assert(info and info.device.name=='approved dynamic keyboard')
 assert(ticket.revision=='2' and ticket.input.source.device=='18446744073709551615')
 return {phase='discrete',data={source=info.device.source.device}}
end)
p.events.on('input',function()error('ordinary input is not enabled')end)
p.events.on('ready',function()
 local tx=p.rules.transaction()
 tx:replace(p.rules.define{id='dynamic-rule',devices={'keyboard'},contexts={'desktop'},trigger=trigger,action=p.actions.event{name='dynamic'}})
 tx:commit()
end)
p.events.on('rules_changed',function(event)
 assert(event.accepted and p.rules.revision()=='2' and #p.rules.list()==1)
 assert(p.config.current().config.recognizers[1].name=='dynamic')
 p.log.info('dynamic-installed')
end)
return p.config.new{policy={allow_input_events=true,allow_runtime_rules=true,allowed_devices={'keyboard'}},devices={keyboard={keys={'KEY_A'}}}}
"#, "dynamic-recognizer.lua").unwrap();
    let config = definition.validate().unwrap();
    assert!(config.rules.is_empty());
    assert_eq!(config.recognizers.len(), 1);
    let mut session = Session::spawn_with_rule_transactions(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(),
        "seat0",
        definition,
    )
    .unwrap();
    let proposal = proposal(&mut session);
    let now = monotonic().unwrap();
    let context = serde_json::from_value::<prek::context_transfer::View>(serde_json::json!({
        "fence":fence(),"sampled_at":now,"state":"desktop","error":null,
        "context":{"binding":{"seat":"seat0","session":"model","uid":1000,"compositor_pid":123,"compositor_start":456,"kind":"user"},
        "snapshot":{"generation":1,"revision":1,"captured_at":now,"state":"desktop","outputs":[]}}
    })).unwrap();
    let capabilities = BTreeSet::from([
        Capability::Context,
        Capability::Observations,
        Capability::LuaEvents,
        Capability::CustomRecognizers,
        Capability::RuntimeRules,
    ]);
    let (mut parent, child) = Channel::pair().unwrap();
    let mut worker = Stream::new(config, child).unwrap();
    let source = Source {
        device: u64::MAX,
        generation: u64::MAX,
        seat: "seat0".into(),
    };
    worker.push(Record::Header { version: 1 }).unwrap();
    worker
        .push(Record::Device {
            device: Device {
                source: source.clone(),
                identity: "dynamic".into(),
                name: "approved dynamic keyboard".into(),
                path: "fixture".into(),
                physical: String::new(),
                unique: String::new(),
                vendor: 1,
                product: 1,
                properties: vec![],
                axes: BTreeMap::new(),
                capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
                virtual_device: false,
            },
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    parent
        .send(
            &Command::Subscriptions {
                update: Subscriptions {
                    revision: 1,
                    seats: vec![Subscription {
                        seat: "seat0".into(),
                        fence: fence(),
                        context: context.context.clone(),
                        capabilities: capabilities.clone(),
                        consumer: None,
                    }],
                },
            },
            monotonic().unwrap(),
        )
        .unwrap();
    worker.service().unwrap();
    worker.flush().unwrap();
    let mut metadata = Vec::new();
    while let Some(packet) = parent.receive::<Message>().unwrap() {
        match packet.message {
            Message::Outcome {
                report: Report::RecognizerMetadata { metadata: info, .. },
            } => metadata.push(*info),
            Message::Outcome {
                report: Report::Input { .. },
            } => panic!("ordinary input was not negotiated"),
            _ => {}
        }
    }
    assert_eq!(metadata.len(), 1);
    let receipt = apply(&mut parent, &mut worker, proposal);
    assert_eq!(receipt.revision, 2);
    assert!(receipt.error.is_none());
    session.acknowledge_rules(fence(), receipt).unwrap();
    session.update_context(context).unwrap();
    session
        .update_capabilities(capabilities, BTreeSet::new())
        .unwrap();
    session
        .recognizer_metadata(metadata.pop().unwrap())
        .unwrap();
    assert_eq!(drain(&mut session)[0].message, "dynamic-installed");
    let at = monotonic().unwrap();
    worker
        .push(Record::Frame {
            frame: Frame {
                source,
                at,
                sequence: 1,
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
        })
        .unwrap();
    worker
        .push(Record::Tick {
            at: monotonic().unwrap(),
        })
        .unwrap();
    worker.flush().unwrap();
    let mut ticket = None;
    while let Some(packet) = parent.receive::<Message>().unwrap() {
        if let Message::Outcome {
            report: Report::Recognition { ticket: issued, .. },
        } = packet.message
        {
            ticket = Some(*issued);
        }
    }
    let ticket = ticket.unwrap();
    assert_eq!(ticket.revision, 2);
    assert_eq!(ticket.rule, "dynamic-rule");
    session.request_recognition(ticket).unwrap();
    assert!(drain(&mut session).is_empty());
    let response = session.take_recognitions().pop().unwrap().1.unwrap();
    parent
        .send(
            &Command::Recognition {
                seat: "seat0".into(),
                fence: fence(),
                proposal: response,
            },
            monotonic().unwrap(),
        )
        .unwrap();
    worker.service().unwrap();
    worker
        .push(Record::Tick {
            at: monotonic().unwrap(),
        })
        .unwrap();
    worker.flush().unwrap();
    let mut matched = false;
    while let Some(packet) = parent.receive::<Message>().unwrap() {
        if let Message::Outcome {
            report: Report::Observation { event, .. },
        } = packet.message
        {
            matched |= matches!(*event, Event::Matched { evidence: Evidence::Custom { data, .. }, .. } if data["source"].as_str() == Some("18446744073709551615"));
        }
    }
    assert!(matched);
    session.stop().unwrap();
    assert!(drain(&mut session).is_empty());
}
#[test]
fn real_helper_layer_receipts_change_recognition_only_in_the_owning_seat() {
    exercise_seat_activation(
        r#"
local p=require('prek')
local tx
local phase=0
p.events.on('ready',function()
 tx=p.layers.transaction();tx:activate_layer('touch');tx:commit()
 assert(p.layers.get('touch').activation=='physical')
end)
p.events.on('rules_changed',function(event)
 assert(event.accepted and tx:status().state=='accepted')
 phase=phase+1
 local expected=({'active','inactive','physical'})[phase]
 assert(p.layers.get('touch').activation==expected and p.layers.revision()==event.revision)
 p.log.info('layer-receipt',{activation=expected,revision=event.revision})
end)
p.events.on('reload',function()
 tx=p.layers.transaction()
 if phase==1 then tx:deactivate_layer('touch') else tx:reset_layer('touch') end
 tx:commit()
end)
return p.config.new{
 policy={allowed_seats={'seat0','seat1'},allowed_devices={'keyboard'}},
 devices={keyboard={name='owned model keyboard'}},
 layers={p.layers.define{id='touch',key='KEY_LEFTMETA'}},
 rules={p.rules.define{id='a',layer='touch',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='a'}}}}
"#,
        ["active", "inactive", "physical"],
    );
}

#[test]
fn real_helper_mode_receipts_change_recognition_only_in_the_owning_seat() {
    exercise_seat_activation(
        r#"
local p=require('prek')
local tx
local phase=0
p.events.on('ready',function()
 tx=p.modes.transaction();tx:select_mode('navigate');tx:commit()
 assert(p.modes.current().id=='normal')
end)
p.events.on('rules_changed',function(event)
 assert(event.accepted and tx:status().state=='accepted')
 phase=phase+1
 local current=p.modes.current()
 local expected=({'navigate','normal','none'})[phase]
 assert((current and current.id or 'none')==expected and p.modes.revision()==event.revision)
 p.log.info('mode-receipt',{activation=expected,revision=event.revision})
end)
p.events.on('reload',function()
 tx=p.modes.transaction()
 if phase==1 then tx:select_mode('normal') else tx:reset_mode() end
 tx:commit()
end)
return p.config.new{
 policy={allowed_seats={'seat0','seat1'},allowed_devices={'keyboard'}},
 devices={keyboard={name='owned model keyboard'}},
 modes={p.modes.define{id='normal',active=true},p.modes.define{id='navigate'}},
 rules={p.rules.define{id='a',when_mode='navigate',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='a'}}}}
"#,
        ["navigate", "normal", "none"],
    );
}

fn exercise_seat_activation(source: &str, activations: [&str; 3]) {
    use prek::{
        input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source},
        replay::Record,
        rules::Event,
        worker::Report,
    };
    use std::collections::BTreeMap;
    let definition = Definition::compile(source, "live-activation.lua").unwrap();
    let mut session = Session::spawn_with_rule_transactions(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(),
        "seat0",
        definition.clone(),
    )
    .unwrap();
    let (mut parent, child) = Channel::pair().unwrap();
    let mut worker = Stream::new(definition.validate().unwrap(), child).unwrap();
    worker.push(Record::Header { version: 1 }).unwrap();
    worker.flush().unwrap();
    parent.receive::<Message>().unwrap().unwrap();
    parent
        .send(
            &Command::Subscriptions {
                update: Subscriptions {
                    revision: 1,
                    seats: ["seat0", "seat1"]
                        .iter()
                        .map(|seat| Subscription {
                            seat: (*seat).into(),
                            fence: fence(),
                            context: None,
                            capabilities: Default::default(),
                            consumer: None,
                        })
                        .collect(),
                },
            },
            monotonic().unwrap(),
        )
        .unwrap();
    worker.service().unwrap();
    worker.flush().unwrap();
    while parent.receive::<Message>().unwrap().is_some() {}
    let sources: Vec<_> = ["seat0", "seat1"]
        .iter()
        .enumerate()
        .map(|(index, seat)| Source {
            device: index as u64 + 1,
            generation: 1,
            seat: (*seat).into(),
        })
        .collect();
    for source in &sources {
        worker
            .push(Record::Device {
                device: Device {
                    identity: source.seat.clone(),
                    source: source.clone(),
                    name: "owned model keyboard".into(),
                    path: "/not-a-kernel-device".into(),
                    physical: String::new(),
                    unique: String::new(),
                    vendor: 0,
                    product: 0,
                    properties: vec![],
                    capabilities: BTreeMap::from([(EV_KEY, vec![30, 125])]),
                    axes: BTreeMap::new(),
                    virtual_device: false,
                },
                keys: Default::default(),
                contacts: vec![],
            })
            .unwrap();
    }
    let mut sequence = 0;
    for (index, activation) in activations.into_iter().enumerate() {
        if index > 0 {
            session
                .notify(EventKind::Reload, serde_json::json!({}))
                .unwrap();
        }
        let proposal = proposal(&mut session);
        let receipt = apply(&mut parent, &mut worker, proposal);
        assert_eq!(receipt.revision, index as u64 + 2);
        assert!(receipt.error.is_none());
        session.acknowledge_rules(fence(), receipt).unwrap();
        assert_eq!(
            drain(&mut session)[0].fields,
            serde_json::json!({"activation":activation,"revision":(index+2).to_string()})
        );
        for source in &sources {
            for value in [1, 0] {
                sequence += 1;
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
                                    code: SYN_REPORT,
                                    value: 0,
                                },
                            ],
                        },
                    })
                    .unwrap();
            }
        }
        worker.flush().unwrap();
        let mut seats = Vec::new();
        while let Some(packet) = parent.receive::<Message>().unwrap() {
            if let Message::Outcome {
                report:
                    Report::Observation {
                        subscription,
                        event,
                    },
            } = packet.message
                && matches!(*event, Event::Matched { .. })
            {
                seats.push(subscription.seat);
            }
        }
        assert_eq!(
            seats,
            if matches!(activation, "active" | "navigate") {
                vec!["seat0", "seat0"]
            } else {
                vec![]
            }
        );
    }
    session.stop().unwrap();
    drain(&mut session);
}

#[test]
fn real_helper_and_worker_exchange_correlated_acceptance_and_stale_rejection() {
    let (mut session, definition) = spawn();
    let first = proposal(&mut session);
    let (mut parent, child) = Channel::pair().unwrap();
    let mut worker = Stream::new(definition.validate().unwrap(), child).unwrap();
    worker.flush().unwrap();
    parent.receive::<Message>().unwrap().unwrap();
    assert!(
        parent
            .send(
                &Command::Subscriptions {
                    update: Subscriptions {
                        revision: 1,
                        seats: vec![Subscription {
                            seat: "seat0".into(),
                            fence: fence(),
                            context: None,
                            capabilities: Default::default(),
                            consumer: None
                        }]
                    }
                },
                monotonic().unwrap()
            )
            .unwrap()
    );
    worker.service().unwrap();
    worker.flush().unwrap();
    while parent.receive::<Message>().unwrap().is_some() {}
    let receipt = apply(&mut parent, &mut worker, first);
    assert_eq!(receipt.revision, 2);
    assert!(receipt.error.is_none());
    let mut wrong = fence();
    wrong.session += 1;
    assert!(session.acknowledge_rules(wrong, receipt.clone()).is_err());
    let mut bad = receipt.clone();
    bad.id += 1;
    assert!(session.acknowledge_rules(fence(), bad).is_err());
    let mut bad = receipt.clone();
    bad.revision += 1;
    assert!(session.acknowledge_rules(fence(), bad).is_err());
    session.acknowledge_rules(fence(), receipt.clone()).unwrap();
    assert!(session.acknowledge_rules(fence(), receipt).is_err());
    assert_eq!(
        drain(&mut session)[0].fields,
        serde_json::json!({"accepted":true,"revision":"2","state":"accepted"})
    );
    session
        .notify(EventKind::Reload, serde_json::json!({}))
        .unwrap();
    let mut second = proposal(&mut session);
    assert_eq!(second.id, 2);
    assert_eq!(second.transaction.revision, 2);
    second.transaction.revision = 1;
    let receipt = apply(&mut parent, &mut worker, second);
    assert_eq!(receipt.revision, 2);
    assert!(receipt.error.is_some());
    session.acknowledge_rules(fence(), receipt).unwrap();
    assert_eq!(
        drain(&mut session)[0].fields,
        serde_json::json!({"accepted":false,"revision":"2","state":"rejected"})
    );
}
#[test]
fn stopping_retires_an_unacknowledged_rule_proposal() {
    let (mut session, _) = spawn();
    let proposal = proposal(&mut session);
    session.stop().unwrap();
    assert!(
        session
            .acknowledge_rules(
                fence(),
                RuleReceipt {
                    id: proposal.id,
                    revision: 2,
                    error: None
                }
            )
            .is_err()
    );
    assert_eq!(drain(&mut session)[0].message, "stop-retired");
}

#[test]
fn pending_rule_receipts_expire_instead_of_accepting_late_activation() {
    let (mut session, _) = spawn();
    let proposal = proposal(&mut session);
    std::thread::sleep(Duration::from_millis(260));
    assert!(
        session
            .acknowledge_rules(
                fence(),
                RuleReceipt {
                    id: proposal.id,
                    revision: 2,
                    error: None
                }
            )
            .is_err()
    );
    assert_eq!(
        session.step(monotonic().unwrap()).unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
}

#[test]
fn ordinary_sessions_offer_owned_rule_queries_without_mutation_authority() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 assert(p.rules.revision()=='1' and #p.rules.list()==0 and p.rules.get('missing')==nil)
 assert(not pcall(function()p.rules.transaction()end))
 p.log.info('read-only-rules')
end)
return {version=1}
"#,
        "read-only-rules.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(),
        "seat0",
        definition,
    )
    .unwrap();
    assert_eq!(drain(&mut session)[0].message, "read-only-rules");
    assert!(session.take_rule_proposals().is_empty());
}
