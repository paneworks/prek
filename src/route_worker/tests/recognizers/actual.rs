//! The sole worker's private protocol and the actual supervised Lua executable.
use super::*;
use crate::{
    context_transfer::View,
    lua::{Definition, LogRecord, session::Session},
    rules::{Event, Evidence, Stage},
};
use std::path::Path;

fn settle(session: &mut Session) -> Vec<LogRecord> {
    let deadline = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = vec![];
    loop {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        if session.finished() {
            return logs;
        }
        assert!(
            monotonic().unwrap() < deadline,
            "supervised routing callback did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "requires a matching executable: oslo make test-routing-helper or the Nix native-routing-helper check"]
fn supervised_callbacks_cross_the_sole_worker_protocol_with_native_ownership() {
    let executable = std::env::var_os("PREK_TEST_LUA_BINARY").expect("matching Prek executable");
    let definition = Definition::compile(r#"
local p=require('prek');local calls=0
p.events.on('input',function()error('ordinary input leaked into custom-only callback')end)
local trigger=p.recognizers.define('actual_bridge',{
 selectors={'keyboard'},inputs={'key'},continuous=true,reply_within='250ms',
},function(ticket)
 if ticket.cause=='cancel' then
  assert(ticket.input==nil and ticket.reason=='completed' and calls==3)
  p.log.info('actual-bridge-cleanup');return nil
 end
 calls=calls+1
 assert(ticket.input.source.device=='18014398509481991')
 assert(ticket.input.source.generation=='18014398509481987')
 local info=p.devices.get(ticket.input.source)
 assert(info and info.device.name=='Prek test bridge model')
 local phases={'begin','update','end'}
 return {phase=phases[calls],data={calls=calls,from='actual-helper'}}
end)
return p.config.new{mode='route',
 policy={allow_input_events=true,allowed_devices={'keyboard'}},
 timing={join_window='1ms',decision_window='200ms'},
 devices={keyboard={identity='bridge-model'}},
 rules={p.rules.define{id='actual-bridge',devices={'keyboard'},contexts={'desktop'},
  scope='everywhere',delivery='reserve',trigger=trigger,action=p.actions.event{name='actual-bridge'}}},
}
"#, "actual-routing-bridge.lua").unwrap();
    let config = definition.validate().unwrap();
    let mut session =
        Session::spawn(Path::new(&executable), owner().fence, "seat0", definition).unwrap();
    settle(&mut session);
    let mut subscription = owner();
    // SAFETY: read-only effective credential query.
    subscription.context.as_mut().unwrap().binding.uid = unsafe { nix::libc::geteuid() };
    let mut fixture = Fixture::new(config, subscription);
    session
        .update_context(View {
            fence: fixture.owner.fence,
            sampled_at: monotonic().unwrap(),
            state: State::Desktop,
            context: fixture.owner.context.clone(),
            morf: Vec::new(),
            morf_rendered: Vec::new(),
            error: None,
        })
        .unwrap();
    session
        .update_capabilities(fixture.owner.capabilities.clone(), BTreeSet::new())
        .unwrap();
    settle(&mut session);
    for message in std::mem::take(&mut fixture.initial) {
        if let Message::Outcome {
            report: Report::RecognizerMetadata { metadata, .. },
        } = message
        {
            session.recognizer_metadata(*metadata).unwrap();
        }
    }
    settle(&mut session);
    let mut cleanups = 0;
    for (index, (down, stage)) in [
        (true, Stage::Begin),
        (true, Stage::Update),
        (false, Stage::End),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 {
            fixture
                .stream
                .push(Record::Frame {
                    frame: Frame {
                        source: device().source,
                        at: monotonic().unwrap(),
                        sequence: 2,
                        events: vec![
                            RawEvent {
                                kind: EV_KEY,
                                code: 30,
                                value: 2,
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
        } else {
            fixture.key(index as u64 + 1, down);
        }
        fixture.tick();
        let mut tickets: Vec<_> = fixture
            .drain()
            .into_iter()
            .filter_map(|message| match message {
                Message::Outcome {
                    report: Report::Recognition { ticket, .. },
                } => Some(*ticket),
                _ => None,
            })
            .collect();
        assert_eq!(tickets.len(), 1);
        let ticket = tickets.pop().unwrap();
        session.request_recognition(ticket.clone()).unwrap();
        assert!(settle(&mut session).is_empty());
        let mut replies = session.take_recognitions();
        assert_eq!(replies.len(), 1);
        let proposal = replies.pop().unwrap().1.expect("real helper proposal");
        fixture.send(Command::Recognition {
            seat: "seat0".into(),
            fence: fixture.owner.fence,
            proposal,
        });
        assert_eq!(fixture.stream.pending.len(), 1);
        assert!(fixture.drain().is_empty());
        std::thread::sleep(Duration::from_millis(2));
        fixture.tick();
        let messages = fixture.drain();
        assert!(
            matches!(messages[0], Message::RecognitionResult { ticket: id, error: None, .. } if id == ticket.id)
        );
        let dispatches: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                Message::Outcome {
                    report: Report::Dispatch { event, .. },
                } => Some(event.as_ref()),
                _ => None,
            })
            .collect();
        assert!(
            matches!(&dispatches[..], [Event::Matched {stage: actual, ordinal,
            evidence: Evidence::Custom { data, .. }, .. }]
            if *actual == stage && *ordinal == index as u64 + 1 && data["from"] == "actual-helper" && data["calls"] == index + 1)
        );
        assert!(
            fixture
                .stream
                .seats
                .get_mut("seat0")
                .unwrap()
                .routing
                .due_after_input(&device().source, monotonic().unwrap())
                .unwrap()
                .is_none()
        );
        for message in messages {
            if let Message::Outcome {
                report: Report::RecognitionCancelled { cancellation, .. },
            } = message
            {
                cleanups += 1;
                assert_eq!(index, 2);
                session.cancel_recognition(cancellation.clone()).unwrap();
                let logs = settle(&mut session);
                assert_eq!(logs.len(), 1);
                assert_eq!(logs[0].message, "actual-bridge-cleanup");
                assert_eq!(session.take_recognition_cancellations(), [cancellation]);
            }
        }
    }
    assert_eq!(cleanups, 1);
    assert!(
        fixture
            .stream
            .seats
            .get_mut("seat0")
            .unwrap()
            .routing
            .take_recoveries()
            .is_empty()
    );
    session.stop().unwrap();
    settle(&mut session);
}
