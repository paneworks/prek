//! Source-bound callbacks through the real child and a private native plan.
use super::*;
use crate::{
    context_transfer::View,
    ipc::monotonic,
    lua::{Definition as LuaDefinition, LogRecord, session::Session},
};

fn settle(session: &mut Session) -> Vec<LogRecord> {
    let deadline = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    loop {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        if session.finished() {
            return logs;
        }
        assert!(
            monotonic().unwrap() < deadline,
            "held-touch helper did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn millis() -> u64 {
    monotonic().unwrap().as_millis().try_into().unwrap()
}

#[test]
#[ignore = "requires a matching executable: oslo make test-composition-helper or the Nix native-composition-helper check"]
fn supervised_retained_touch_handoffs_use_one_native_coordinator() {
    let executable = std::env::var_os("PREK_TEST_LUA_BINARY").expect("matching Prek executable");
    let definition = LuaDefinition::compile(r#"
local p=require('prek');local calls=0
p.events.on('input',function()error('ordinary input reached a custom callback')end)
p.recognizers.define('direct',{selectors={'keyboard'},inputs={'key'},reply_within='250ms'},
 function()error('unrelated callback was activated')end)
local function recognize(ticket)
 if ticket.cause=='cancel' then
  assert(ticket.reason=='completed');p.log.info('touch-cleanup');return nil
 end
 calls=calls+1
 assert(ticket.cause=='timer' and ticket.input.sequence=='1')
 assert(#ticket.input.changes==0 and #ticket.input.contacts==2)
 assert(#ticket.input.contacts_started==0 and #ticket.input.contacts_ended==0 and #ticket.input.ended_contacts==0)
 assert(not ticket.input.contacts[1].fresh and not ticket.input.contacts[2].fresh)
 assert(ticket.recognizer==({'first','last'})[calls])
 return {phase='discrete',data={step=ticket.recognizer,from='actual-held-touch-helper'}}
end
p.recognizers.define('first',{selectors={'keyboard'},inputs={'touch'},reply_within='250ms'},recognize)
p.recognizers.define('last',{selectors={'keyboard'},inputs={'touch'},reply_within='250ms'},recognize)
return p.config.new{policy={allow_input_events=true,allowed_devices={'keyboard'}},
 devices={keyboard={keys={'KEY_A'}}}}
"#, "native-held-touch-helper.lua").unwrap();
    let mut engine = model_steps(
        Trigger::TouchHold {
            fingers: 2,
            after: Millis(20),
        },
        Trigger::Sequence {
            steps: vec![
                touch_custom("first"),
                touch_custom("last"),
                motion::swipe(true),
            ],
            timeout: Millis(1000),
        },
        1,
        true,
    );
    let custom = engine.custom.as_ref().unwrap();
    assert_eq!(
        definition.validate().unwrap().recognizer_definitions(),
        custom.approved
    );
    let fence = custom.fence;
    let mut session = Session::spawn(
        std::path::Path::new(&executable),
        fence,
        "seat0",
        definition,
    )
    .unwrap();
    assert!(settle(&mut session).is_empty());
    let started = millis();
    let current = snapshot(1, 2, started);
    // SAFETY: read-only effective credential query.
    let uid = unsafe { nix::libc::geteuid() };
    let binding = Binding {
        seat: "seat0".into(),
        session: "coordinator-model".into(),
        uid,
        compositor_pid: 123,
        compositor_start: 456,
        kind: SessionKind::User,
    };
    engine.authority = Authority::default();
    engine
        .authority
        .adopt(binding.clone(), fence.session)
        .unwrap();
    engine
        .authority
        .update(current.clone(), ms(started))
        .unwrap();
    session
        .update_context(View {
            fence,
            sampled_at: ms(started),
            state: State::Desktop,
            context: Some(Box::new(crate::worker::ProvidedContext {
                layouts: None,
                binding,
                snapshot: current,
            })),
            morf: Vec::new(),
            morf_rendered: Vec::new(),
            error: None,
        })
        .unwrap();
    session
        .update_capabilities(
            BTreeSet::from([
                crate::protocol::Capability::CustomRecognizers,
                crate::protocol::Capability::LuaEvents,
                crate::protocol::Capability::Context,
                crate::protocol::Capability::Observations,
            ]),
            BTreeSet::new(),
        )
        .unwrap();
    assert!(settle(&mut session).is_empty());
    touch_start_at(&mut engine, 200, started, 1);
    let outer = engine.sequences[0].pending["seat0"].id;
    std::thread::sleep(Duration::from_millis(21));
    tick(&mut engine, millis());
    let mut candidate_ids = BTreeSet::new();
    let mut ticket_ids = BTreeSet::new();
    for expected in ["first", "last"] {
        let tickets = engine.recognition_requests(monotonic().unwrap()).unwrap();
        let [ticket] = &tickets[..] else {
            panic!("{tickets:?}; {:?}", engine.events)
        };
        assert_eq!(ticket.recognizer, expected);
        assert!(candidate_ids.insert(ticket.candidate));
        assert!(ticket_ids.insert(ticket.id));
        assert_eq!(ticket.cause, crate::recognition::Cause::Timer);
        assert_eq!(ticket.input.at(), ms(started));
        assert!(ticket.requested_at >= ms(started + 20));
        session.request_recognition(ticket.clone()).unwrap();
        assert!(settle(&mut session).is_empty());
        let mut replies = session.take_recognitions();
        assert_eq!(replies.len(), 1);
        let (id, proposal) = replies.pop().unwrap();
        assert_eq!(id, ticket.id);
        engine
            .recognition_response_after_input(
                proposal.expect("supervised native step proposal"),
                engine.input_at(),
                monotonic().unwrap(),
            )
            .unwrap();
        assert!(
            engine.sequences[0]
                .steps
                .iter()
                .all(|step| step.custom.is_none())
        );
        let cleanup = engine.take_recognition_cancellations();
        let [cleanup] = &cleanup[..] else {
            panic!("{cleanup:?}")
        };
        assert_eq!(cleanup.candidate, ticket.candidate);
        assert_eq!(cleanup.reason, Retirement::Completed);
        session.cancel_recognition(cleanup.clone()).unwrap();
        let logs = settle(&mut session);
        assert!(matches!(&logs[..], [entry] if entry.message=="touch-cleanup"));
        assert_eq!(
            session.take_recognition_cancellations().as_slice(),
            std::slice::from_ref(cleanup)
        );
    }
    assert_eq!(engine.input_watermark, 1);
    assert!(matches(&engine.events).is_empty());
    assert_eq!(engine.sequences[0].pending["seat0"].step, 3);
    for (sequence, x, end) in [(2, 400, false), (3, 500, false), (4, 500, true)] {
        std::thread::sleep(Duration::from_millis(2));
        shape_touch(
            &mut engine,
            millis(),
            sequence,
            &[(0, x, 500), (1, x + 100, 500)],
            false,
            end,
        );
    }
    let matched = matches(&engine.events);
    assert_eq!(matched.len(), 3, "{:?}", engine.events);
    for (event, (ordinal, stage)) in
        matched
            .iter()
            .zip([(1, Stage::Begin), (2, Stage::Update), (3, Stage::End)])
    {
        assert!(matches!(event,Event::Matched {
            id,ordinal:actual,stage:actual_stage,evidence:Evidence::Sequence{steps},..
        } if *id==outer && *actual==ordinal && *actual_stage==stage && steps.len()==4
            && matches!(&steps[1],Evidence::Custom{data,..} if data["from"]=="actual-held-touch-helper")
            && matches!(&steps[2],Evidence::Custom{data,..} if data["step"]=="last")
            && matches!(&steps[3],Evidence::Touch{gesture:TouchGesture::Pan{..},..})));
    }
    assert_eq!(engine.input_watermark, 4);
    assert!(engine.sequences[0].pending.is_empty());
    session.stop().unwrap();
}
