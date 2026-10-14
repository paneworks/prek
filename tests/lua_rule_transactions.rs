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
