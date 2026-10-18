//! Actual supervised callbacks with model source/context and native raw scopes.
use super::*;
use crate::{
    context_transfer::View,
    ipc::monotonic,
    lua::{Definition as LuaDefinition, LogRecord, session::Session},
};
use std::os::unix::fs::MetadataExt;

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
            "routing chain helper did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn millis() -> u64 {
    monotonic().unwrap().as_millis().try_into().unwrap()
}

#[test]
#[ignore = "requires a matching executable: oslo make test-chain-helper or the Nix native-chain-helper check"]
fn supervised_custom_steps_use_complete_native_routing_scopes() {
    let executable = std::env::var_os("PREK_TEST_LUA_BINARY").expect("matching Prek executable");
    let definition = LuaDefinition::compile(r#"
local p=require('prek');local calls=0
p.events.on('input',function()error('ungranted ordinary input')end)
p.recognizers.define('custom',{selectors={'touch'},inputs={'touch'},reply_within='250ms'},
 function()error('inactive recognizer')end)
local function recognize(ticket)
 if ticket.cause=='cancel' then
  assert(ticket.reason=='completed');p.log.info('chain-cleanup');return nil
 end
 calls=calls+1
 assert(ticket.cause=='timer' and ticket.input.sequence=='1')
 assert(ticket.recognizer==({'first','last'})[calls])
 assert(#ticket.input.contacts==2 and #ticket.input.changes==0)
 assert(#ticket.input.contacts_started==0 and #ticket.input.contacts_ended==0)
 assert(not ticket.input.contacts[1].fresh and not ticket.input.contacts[2].fresh)
 return {phase='discrete',data={step=ticket.recognizer,from='actual-routing-chain-helper'}}
end
p.recognizers.define('first',{selectors={'touch'},inputs={'touch'},reply_within='250ms'},recognize)
p.recognizers.define('last',{selectors={'touch'},inputs={'touch'},reply_within='250ms'},recognize)
return p.config.new{policy={allow_input_events=true,allowed_devices={'touch'}},devices={touch={keys={'BTN_TOUCH'}}}}
"#, "native-routing-chain-helper.lua").unwrap();
    let (mut routing, first, _) = fixture(
        vec![
            Trigger::TouchHold {
                fingers: 2,
                after: Millis(20),
            },
            custom_step("first", false, true),
            custom_step("last", false, true),
        ],
        Delivery::Buffer,
        true,
    );
    assert_eq!(
        definition.validate().unwrap().recognizer_definitions(),
        routing.engine.config().recognizer_definitions()
    );
    let mut session = Session::spawn(
        std::path::Path::new(&executable),
        fence(),
        "seat0",
        definition,
    )
    .unwrap();
    assert!(settle(&mut session).is_empty());
    let started = millis();
    let mut snapshot = routing.authority_mut().fresh(ms(0)).unwrap().clone();
    snapshot.captured_at = ms(started);
    snapshot.revision += 1;
    let binding = Binding {
        seat: "seat0".into(),
        session: "routing-chain-model".into(),
        uid: std::fs::metadata("/proc/self").unwrap().uid(),
        compositor_pid: 123,
        compositor_start: 456,
        kind: SessionKind::User,
    };
    *routing.authority_mut() = Authority::default();
    routing
        .authority_mut()
        .adopt(binding.clone(), fence().session)
        .unwrap();
    routing
        .authority_mut()
        .update(snapshot.clone(), ms(started))
        .unwrap();
    routing.actions_available(
        fence().session,
        BTreeSet::from([Capability::Observations]),
        ms(started),
    );
    session
        .update_context(View {
            fence: fence(),
            sampled_at: ms(started),
            state: State::Desktop,
            context: Some(Box::new(crate::worker::ProvidedContext {
                layouts: None,
                binding,
                snapshot,
            })),
            morf: Vec::new(),
            morf_rendered: Vec::new(),
            error: None,
        })
        .unwrap();
    session
        .update_capabilities(
            BTreeSet::from([
                Capability::CustomRecognizers,
                Capability::LuaEvents,
                Capability::Context,
                Capability::Observations,
            ]),
            BTreeSet::new(),
        )
        .unwrap();
    assert!(settle(&mut session).is_empty());
    frame(
        &mut routing,
        1,
        started,
        &[
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 10),
            (EV_ABS, ABS_MT_POSITION_X, 1000),
            (EV_ABS, ABS_MT_POSITION_Y, 1000),
            (EV_ABS, ABS_MT_SLOT, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, 11),
            (EV_ABS, ABS_MT_POSITION_X, 1500),
            (EV_ABS, ABS_MT_POSITION_Y, 1000),
        ],
    );
    let parent = owner(&routing);
    let original_reports: Vec<_> = routing.engine.route_intents()[0]
        .tail_reports()
        .cloned()
        .collect();
    assert_eq!(original_reports.len(), 1);
    assert!(original_reports[0].complete());
    std::thread::sleep(Duration::from_millis(21));
    let mut leaves = BTreeSet::new();
    for expected in ["first", "last"] {
        let ticket = request(&mut routing, millis());
        assert_eq!(ticket.recognizer, expected);
        assert_ne!(ticket.candidate, parent);
        assert!(leaves.insert(ticket.candidate));
        let (leaf, _) = routing
            .engine
            .recognition_scope(ticket.candidate, monotonic().unwrap())
            .unwrap();
        assert!(leaf.tail_reports().next().is_none());
        assert_eq!(routing.custom_chains[&ticket.candidate].owner, parent);
        assert_eq!(
            ticket.deadline,
            ms(started + 300).min(ticket.requested_at + ms(250))
        );
        session.request_recognition(ticket.clone()).unwrap();
        assert!(settle(&mut session).is_empty());
        let mut replies = session.take_recognitions();
        assert_eq!(replies.len(), 1);
        let (id, proposal) = replies.pop().unwrap();
        assert_eq!(id, ticket.id);
        routing
            .recognition_response_after_input(
                proposal.unwrap(),
                routing.engine.input_at(),
                monotonic().unwrap(),
            )
            .unwrap();
        let cleanup = routing.take_recognition_cancellations();
        let [cleanup] = &cleanup[..] else {
            panic!("{cleanup:?}")
        };
        assert_eq!(cleanup.candidate, ticket.candidate);
        assert_eq!(cleanup.reason, Retirement::Completed);
        session.cancel_recognition(cleanup.clone()).unwrap();
        assert!(matches!(&settle(&mut session)[..], [entry] if entry.message=="chain-cleanup"));
        assert_eq!(
            session.take_recognition_cancellations().as_slice(),
            std::slice::from_ref(cleanup)
        );
    }
    assert_eq!(routing.dispatches.len(), 1);
    assert_eq!(
        routing.dispatches[0]
            .outcome
            .intent
            .tail_reports()
            .cloned()
            .collect::<Vec<_>>(),
        original_reports
    );
    let events = routing.take_dispatches(monotonic().unwrap());
    assert!(
        matches!(&events[..], [Event::Matched { id, evidence: Evidence::Sequence { steps }, .. }]
        if *id==parent && steps.len()==3
        && matches!(&steps[1], Evidence::Custom { data, .. } if data["from"]=="actual-routing-chain-helper")
        && matches!(&steps[2], Evidence::Custom { data, .. } if data["step"]=="last"))
    );
    assert!(routing.custom_chains.is_empty());
    assert_eq!(routing.engine.input_watermark(), 1);
    up(&mut routing, 2, millis(), 2);
    assert!(
        routing
            .due_after_input(&first.source, monotonic().unwrap())
            .unwrap()
            .is_none()
    );
    session.stop().unwrap();
}
