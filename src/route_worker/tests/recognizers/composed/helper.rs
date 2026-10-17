//! Supervised callbacks with model readiness and complete compiled worker plans.
use super::*;
use crate::{
    context_transfer::View,
    lua::{Definition as LuaDefinition, LogRecord, session::Session},
    rules::{Event, Evidence},
};
use std::{os::unix::fs::MetadataExt, path::Path};

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
            "composed worker helper did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "requires a matching executable: oslo make test-composed-worker-helper or the Nix native-composed-worker-helper check"]
fn supervised_composed_plans_survive_real_private_configuration_reconstruction() {
    let executable = std::env::var_os("PREK_TEST_LUA_BINARY").expect("matching Prek executable");
    let source = r#"
local p=require('prek');local calls=0
p.events.on('input',function()error('ordinary input leaked')end)
local function recognize(ticket)
 if ticket.cause=='cancel' then
  assert(ticket.reason=='completed');p.log.info('worker-chain-cleanup');return nil
 end
 calls=calls+1
 assert(ticket.recognizer==({'first','last'})[calls])
 assert(ticket.input.source.device=='18014398509481991')
 local info=p.devices.get(ticket.input.source)
 assert(info and info.device.name=='Prek test bridge model')
 return {phase='discrete',data={step=ticket.recognizer,from='actual-composed-worker'}}
end
local first=p.recognizers.define('first',{selectors={'keyboard'},inputs={'key'},reply_within='250ms'},recognize)
local last=p.recognizers.define('last',{selectors={'keyboard'},inputs={'key'},reply_within='250ms'},recognize)
return p.config.new{mode='route',policy={allow_input_events=true,allow_runtime_rules=true,allowed_devices={'keyboard'}},
 timing={join_window='1ms',decision_window='300ms'},devices={keyboard={identity='bridge-model'}},
 rules={{id='chain',devices={'keyboard'},contexts={'desktop'},scope='everywhere',delivery='reserve',
 trigger=p.triggers.sequence{steps={first,last},timeout='1s'},action=p.actions.event{name='bridge-match'}}}}
"#;
    let definition = LuaDefinition::compile(source, "composed-worker-helper.lua").unwrap();
    let native = definition.validate().unwrap();
    let mut subscription = owner();
    subscription.context.as_mut().unwrap().binding.uid = fs::metadata("/proc/self").unwrap().uid();
    let mut fixture = Fixture::new(native.clone(), subscription);
    let mut current = device();
    for generation in 1..=2 {
        if generation == 2 {
            let messages = replace(&mut fixture, &native, generation);
            assert!(
                messages.iter().any(|m| matches!(
                    m,
                    Message::Configuration {
                        generation: 2,
                        error: None
                    }
                )),
                "{messages:?}"
            );
            fixture.owner.fence.configuration = generation;
            fixture.owner.context.as_mut().unwrap().snapshot.captured_at = monotonic().unwrap();
            fixture.subscribe(fixture.owner.clone(), generation);
            current = fresh_source(&mut fixture, current.source.generation + 1);
        }
        assert_eq!(definition.validate().unwrap(), fixture.stream.config);
        let mut session = Session::spawn(
            Path::new(&executable),
            fixture.owner.fence,
            "seat0",
            definition.clone(),
        )
        .unwrap();
        assert!(settle(&mut session).is_empty());
        session
            .update_context(View {
                fence: fixture.owner.fence,
                sampled_at: monotonic().unwrap(),
                state: State::Desktop,
                context: fixture.owner.context.clone(),
                morf: vec![],
                morf_rendered: vec![],
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
        assert!(settle(&mut session).is_empty());
        let mut leaves = BTreeSet::new();
        let mut cleanups = 0;
        for (index, expected) in ["first", "last"].into_iter().enumerate() {
            if index != 0 {
                key(&mut fixture, &current.source, 2, false);
            }
            key(&mut fixture, &current.source, index as u64 * 2 + 1, true);
            let ticket = one(&mut fixture, expected);
            assert_eq!(ticket.fence.configuration, generation);
            assert!(leaves.insert(ticket.candidate));
            session.request_recognition(ticket.clone()).unwrap();
            assert!(settle(&mut session).is_empty());
            let mut replies = session.take_recognitions();
            assert_eq!(replies.len(), 1);
            let (id, proposal) = replies.pop().unwrap();
            assert_eq!(id, ticket.id);
            fixture.send(Command::Recognition {
                seat: "seat0".into(),
                fence: fixture.owner.fence,
                proposal: proposal.unwrap(),
            });
            assert_eq!(fixture.stream.pending.len(), 1);
            assert!(fixture.drain().is_empty());
            fixture.tick();
            let messages = fixture.drain();
            assert!(messages.iter().any(|m| matches!(m,Message::RecognitionResult { ticket: id, error: None, .. } if *id==ticket.id)), "{messages:?}");
            let dispatched = dispatches(&messages);
            if index == 0 {
                assert!(dispatched.is_empty());
            } else {
                assert!(
                    matches!(&dispatched[..], [Event::Matched { id, config_generation, evidence: Evidence::Sequence { steps }, .. }]
                    if !leaves.contains(id) && *config_generation==generation && steps.len()==2
                    && matches!(&steps[0],Evidence::Custom {data,..} if data["step"]=="first" && data["from"]=="actual-composed-worker")
                    && matches!(&steps[1],Evidence::Custom {data,..} if data["step"]=="last" && data["from"]=="actual-composed-worker")),
                    "{messages:?}"
                );
            }
            for message in messages {
                if let Message::Outcome {
                    report: Report::RecognitionCancelled { cancellation, .. },
                } = message
                {
                    assert_eq!(cancellation.candidate, ticket.candidate);
                    session.cancel_recognition(cancellation.clone()).unwrap();
                    assert!(
                        matches!(&settle(&mut session)[..],[log] if log.message=="worker-chain-cleanup")
                    );
                    assert_eq!(session.take_recognition_cancellations(), [cancellation]);
                    cleanups += 1;
                }
            }
        }
        assert_eq!(cleanups, 2);
        key(&mut fixture, &current.source, 4, false);
        fixture.tick();
        assert!(dispatches(&fixture.drain()).is_empty());
        assert!(
            fixture
                .stream
                .seats
                .get_mut("seat0")
                .unwrap()
                .routing
                .due_after_input(&current.source, monotonic().unwrap())
                .unwrap()
                .is_none()
        );
        session.stop().unwrap();
        settle(&mut session);
    }
}
