#![cfg(target_os = "linux")]
use prek::{
    ipc::monotonic,
    lua::{Definition, EventKind, LogRecord, session::Session},
    protocol::{Epoch, Fence},
};
use std::{path::Path, time::Duration};
fn fence(generation: u64) -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: generation,
        session: 1,
    }
}
fn drain(session: &mut Session) -> Result<Vec<LogRecord>, std::io::Error> {
    let end = monotonic()? + Duration::from_secs(5);
    let mut logs = Vec::new();
    loop {
        logs.extend(session.step(monotonic()?)?);
        if session.finished() {
            return Ok(logs);
        }
        assert!(monotonic()? < end, "Lua session never settled");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn context_view(at: Duration, windows: usize) -> prek::context_transfer::View {
    // SAFETY: read-only current process credential query.
    let uid = unsafe { nix::libc::geteuid() };
    let rect = serde_json::json!({"origin":{"x":0.1,"y":10.0},"size":{"x":100.0,"y":120.0}});
    let windows: Vec<_> = (0..windows).map(|i| serde_json::json!({
        "id":format!("window-{i}"),"output":"fixture-output","workspace":3,
        "rect":rect,"content":rect,"mapped":true,"visible":true,"focused":i==0,"z_order":null
    })).collect();
    serde_json::from_value(serde_json::json!({
        "fence":fence(1),"sampled_at":at,"state":"desktop","error":null,
        "context":{"binding":{"seat":"seat0","session":"fixture","uid":uid,
            "compositor_pid":123,"compositor_start":456,"kind":"user"},
            "snapshot":{"generation":1,"revision":7,"captured_at":at,"state":"desktop",
                "secure_overlay":false,"regions":[],"windows":windows,"outputs":[{
                    "geometry":{"name":"fixture-output","pixel_width":1000.0,"pixel_height":800.0,
                        "scale":1.0,"origin":{"x":0.0,"y":0.0},"transform":"normal"},
                    "geometry_generation":2,"active_workspace":3,"screen":"on","keyboard":null
                }]}}
    }))
    .unwrap()
}

fn recognizer_definition(body: &str) -> (Definition, prek::recognition::Definition) {
    use prek::{
        config::Millis,
        recognition::{Definition as Recognizer, InputKind},
    };
    use std::collections::BTreeSet;
    let mut definition = Definition::compile(&format!(r#"
local p=require('prek'); local calls=0
p.events.on('input',function(ticket) calls=calls+1; {body} end)
p.events.on('ready',function()p.log.info('ready')end)
return p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keyboard'}}}},devices={{keyboard={{keys={{'KEY_A'}}}}}}}}
"#), "recognizer-helper.lua").unwrap();
    let recognizer = Recognizer {
        name: "hold".into(),
        selectors: BTreeSet::from(["keyboard".into()]),
        inputs: BTreeSet::from([InputKind::Key]),
        reply_within: Millis(250),
        continuous: false,
    };
    definition.bind_recognizer(1, recognizer.clone()).unwrap();
    (definition, recognizer)
}

fn recognizer_candidate(at: Duration) -> prek::recognition::Candidate {
    prek::recognition::Candidate {
        id: u64::MAX,
        revision: u64::MAX,
        rule: "hold-rule".into(),
        source: prek::input::Source {
            device: u64::MAX,
            generation: u64::MAX,
            seat: "seat0".into(),
        },
        ownership: Some(prek::router::Ownership::Pending),
        winner: None,
        expires: at + Duration::from_secs(2),
    }
}

fn recognizer_frame(at: Duration, sequence: u64) -> prek::input_events::Event {
    use prek::input::{Change, KeyState};
    prek::input_events::Event::Frame {
        source: recognizer_candidate(at).source,
        at,
        sequence,
        selectors: vec!["keyboard".into()],
        changes: vec![Change::Key {
            code: 30,
            state: KeyState::Press,
        }],
        contacts: vec![],
        contacts_started: vec![],
        contacts_ended: vec![],
        ended_contacts: vec![],
    }
}

fn authorize_recognizer(session: &mut Session) {
    use prek::protocol::Capability;
    use std::collections::BTreeSet;
    session
        .update_context(context_view(monotonic().unwrap(), 0))
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
    drain(session).unwrap();
}

#[test]
fn agent_bridge_uses_real_helper_readiness_public_tickets_and_exact_cleanup() {
    use prek::{
        ipc::Channel,
        lua::session::RecognizerBridge,
        protocol::{Reply, Request},
        recognition::{Gate, Phase, Retirement},
    };
    // Model broker admission explicitly; both socket endpoints run as this
    // unprivileged test user. This is not root/logind acceptance.
    let (definition, recognizer) = recognizer_definition(
        r#"
if ticket.cause=='cancel' then p.log.info('bridge-cleanup');return nil end
p.log.info('bridge-recognize');return {phase='discrete',data={calls=calls}}
"#,
    );
    let mut runtime = Some(
        Session::spawn(
            Path::new(env!("CARGO_BIN_EXE_prek")),
            fence(1),
            "seat0",
            definition,
        )
        .unwrap(),
    );
    let (mut agent, mut broker) = Channel::pair().unwrap();
    let mut bridge = RecognizerBridge::default();
    bridge
        .flush(fence(1), true, runtime.as_ref(), &mut agent)
        .unwrap();
    assert!(matches!(
        broker.receive::<Request>().unwrap().unwrap().message,
        Request::RecognizerReady { ready: false, .. }
    ));
    drain(runtime.as_mut().unwrap()).unwrap();
    authorize_recognizer(runtime.as_mut().unwrap());
    bridge
        .flush(fence(1), false, runtime.as_ref(), &mut agent)
        .unwrap();
    assert!(
        broker.receive::<Request>().unwrap().is_none(),
        "helper readiness alone cannot bypass the input worker subscription"
    );
    bridge
        .flush(fence(1), true, runtime.as_ref(), &mut agent)
        .unwrap();
    let request = broker.receive::<Request>().unwrap().unwrap().message;
    request.validate().unwrap();
    assert!(matches!(
        request,
        Request::RecognizerReady { ready: true, .. }
    ));
    broker
        .send(
            &Reply::RecognizerReady {
                fence: fence(1),
                ready: true,
            },
            monotonic().unwrap(),
        )
        .unwrap();
    let Reply::RecognizerReady {
        fence: ready_fence,
        ready,
    } = agent.receive::<Reply>().unwrap().unwrap().message
    else {
        panic!()
    };
    bridge.acknowledge_ready(ready_fence, ready);
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    let ticket = gate
        .offer("hold", &current, &recognizer_frame(at, 1), at)
        .unwrap();
    broker
        .send(
            &Reply::Recognition {
                fence: fence(1),
                ticket: Box::new(ticket.clone()),
            },
            monotonic().unwrap(),
        )
        .unwrap();
    let reply = agent.receive::<Reply>().unwrap().unwrap().message;
    reply.validate().unwrap();
    let Reply::Recognition {
        fence,
        ticket: delivered,
    } = reply
    else {
        panic!()
    };
    assert!(
        bridge
            .recognize(fence, *delivered, "seat0", &mut runtime)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        drain(runtime.as_mut().unwrap()).unwrap()[0].message,
        "bridge-recognize"
    );
    assert!(bridge.collect(runtime.as_mut().unwrap()).unwrap());
    bridge
        .flush(fence, true, runtime.as_ref(), &mut agent)
        .unwrap();
    let request = broker.receive::<Request>().unwrap().unwrap().message;
    request.validate().unwrap();
    let Request::Recognize { proposal, .. } = request else {
        panic!()
    };
    let validated = gate
        .validate(proposal, &current, monotonic().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(validated.phase, Phase::Discrete);
    assert_eq!(validated.data["calls"], 1);
    gate.retire_with(current.id, Retirement::Completed);
    let cancellation = gate.take_cancellations().pop().unwrap();
    broker
        .send(
            &Reply::RecognitionCancelled {
                fence,
                cancellation: cancellation.clone(),
            },
            monotonic().unwrap(),
        )
        .unwrap();
    let reply = agent.receive::<Reply>().unwrap().unwrap().message;
    reply.validate().unwrap();
    let Reply::RecognitionCancelled {
        fence,
        cancellation: delivered,
    } = reply
    else {
        panic!()
    };
    assert!(
        bridge
            .cancel(fence, delivered, &mut runtime)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        drain(runtime.as_mut().unwrap()).unwrap()[0].message,
        "bridge-cleanup"
    );
    assert!(bridge.collect(runtime.as_mut().unwrap()).unwrap());
    bridge
        .flush(fence, true, runtime.as_ref(), &mut agent)
        .unwrap();
    let request = broker.receive::<Request>().unwrap().unwrap().message;
    request.validate().unwrap();
    assert_eq!(
        request,
        Request::RecognitionCancelled {
            fence,
            cancellation
        }
    );
    runtime = None;
    bridge
        .flush(fence, true, runtime.as_ref(), &mut agent)
        .unwrap();
    assert!(matches!(
        broker.receive::<Request>().unwrap().unwrap().message,
        Request::RecognizerReady { ready: false, .. }
    ));
    assert!(!bridge.wants_write(fence, true, runtime.as_ref()));
}

#[test]
fn agent_cancels_queued_proposals_and_preserves_cleanup_when_the_helper_fails() {
    use prek::{
        ipc::Channel,
        lua::session::RecognizerBridge,
        protocol::Request,
        recognition::{Gate, Retirement},
    };
    for fail_cleanup in [false, true] {
        let (definition, recognizer) = recognizer_definition(if fail_cleanup {
            "if ticket.cause=='cancel' then error('cleanup crash') end; return nil"
        } else {
            "if ticket.cause=='cancel' then p.log.info('cleanup') end; return nil"
        });
        let mut runtime = Some(
            Session::spawn(
                Path::new(env!("CARGO_BIN_EXE_prek")),
                fence(1),
                "seat0",
                definition,
            )
            .unwrap(),
        );
        drain(runtime.as_mut().unwrap()).unwrap();
        authorize_recognizer(runtime.as_mut().unwrap());
        let (mut agent, mut broker) = Channel::pair().unwrap();
        let mut bridge = RecognizerBridge::default();
        bridge
            .flush(fence(1), true, runtime.as_ref(), &mut agent)
            .unwrap();
        broker.receive::<Request>().unwrap().unwrap();
        bridge.acknowledge_ready(fence(1), true);
        let at = monotonic().unwrap();
        let current = recognizer_candidate(at);
        let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
        let ticket = gate
            .offer("hold", &current, &recognizer_frame(at, 1), at)
            .unwrap();
        assert!(
            bridge
                .recognize(fence(1), ticket, "seat0", &mut runtime)
                .unwrap()
                .is_none()
        );
        drain(runtime.as_mut().unwrap()).unwrap();
        assert!(bridge.collect(runtime.as_mut().unwrap()).unwrap());
        assert!(bridge.wants_write(fence(1), true, runtime.as_ref()));
        gate.retire_with(current.id, Retirement::SourceLost);
        let cancellation = gate.take_cancellations().pop().unwrap();
        assert!(
            bridge
                .cancel(fence(1), cancellation.clone(), &mut runtime)
                .unwrap()
                .is_none()
        );
        if fail_cleanup {
            assert!(drain(runtime.as_mut().unwrap()).is_err());
            runtime = None;
        } else {
            assert_eq!(
                drain(runtime.as_mut().unwrap()).unwrap()[0].message,
                "cleanup"
            );
            assert!(bridge.collect(runtime.as_mut().unwrap()).unwrap());
        }
        bridge
            .flush(fence(1), true, runtime.as_ref(), &mut agent)
            .unwrap();
        if fail_cleanup {
            assert!(matches!(
                broker.receive::<Request>().unwrap().unwrap().message,
                Request::RecognizerReady { ready: false, .. }
            ));
        }
        assert_eq!(
            broker.receive::<Request>().unwrap().unwrap().message,
            Request::RecognitionCancelled {
                fence: fence(1),
                cancellation
            }
        );
        assert!(
            broker.receive::<Request>().unwrap().is_none(),
            "cancelled proposal must never cross the public socket"
        );
        assert!(!bridge.wants_write(fence(1), true, runtime.as_ref()));
    }
}

#[test]
fn actual_helper_returns_only_fenced_proposals_for_frames_and_stationary_timer_wakes() {
    use prek::recognition::{Gate, Phase};
    let (definition, recognizer) = recognizer_definition(
        r#"
assert(ticket.candidate=='18446744073709551615' and ticket.revision=='18446744073709551615')
assert(ticket.input.source.device=='18446744073709551615' and ticket.input.source.generation=='18446744073709551615')
if ticket.cause=='frame' then assert(calls==1 and #ticket.input.changes==1);return nil end
assert(ticket.cause=='timer' and calls==2 and #ticket.input.changes==0)
local function seconds(t)return t.secs+t.nanos/1000000000 end
assert(seconds(ticket.requested_at)-seconds(ticket.input.at)>0.25)
p.log.info('hold',{calls=calls})
return {phase='discrete',data={calls=calls}}
"#,
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "ready");
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    let first = gate
        .offer("hold", &current, &recognizer_frame(at, 1), at)
        .unwrap();
    session.request_recognition(first.clone()).unwrap();
    assert!(session.request_recognition(first).is_err());
    assert!(drain(&mut session).unwrap().is_empty());
    let (id, pending) = session.take_recognitions().pop().unwrap();
    let pending = pending.unwrap();
    assert_eq!(id, pending.ticket);
    assert_eq!(pending.phase, Phase::Pending);
    assert!(
        gate.validate(pending, &current, monotonic().unwrap())
            .unwrap()
            .is_none()
    );
    std::thread::sleep(Duration::from_millis(275));
    authorize_recognizer(&mut session);
    let timer = gate
        .offer_timer("hold", &current, monotonic().unwrap())
        .unwrap();
    session.request_recognition(timer).unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "hold");
    let (_, proposal) = session.take_recognitions().pop().unwrap();
    let recognized = gate
        .validate(proposal.unwrap(), &current, monotonic().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(recognized.data["calls"], 2);
    // The real child returned recognition; Rust still has not claimed input.
    assert_eq!(
        recognized.candidate.ownership,
        Some(prek::router::Ownership::Pending)
    );
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn actual_helper_discards_unauthorized_requests_and_retires_forged_callback_results() {
    use prek::recognition::Gate;
    let (definition, recognizer) =
        recognizer_definition("p.log.info('discard');return {phase='discrete',target='forged'}");
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    let first = gate
        .offer("hold", &current, &recognizer_frame(at, 1), at)
        .unwrap();
    session.request_recognition(first).unwrap();
    assert!(drain(&mut session).unwrap().is_empty());
    assert!(session.take_recognitions().pop().unwrap().1.is_none());
    authorize_recognizer(&mut session);
    let now = monotonic().unwrap();
    let next = gate
        .offer("hold", &current, &recognizer_frame(now, 2), now)
        .unwrap();
    session.request_recognition(next).unwrap();
    assert!(drain(&mut session).is_err());
    assert!(session.take_recognitions().is_empty());
}

#[test]
fn recognizer_parent_queue_is_bounded_and_stop_retires_queued_work() {
    use prek::recognition::Gate;
    let (definition, recognizer) =
        recognizer_definition("error('queued callback must be cancelled by stop')");
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    let mut last = None;
    for sequence in 1..=64 {
        let ticket = gate
            .offer("hold", &current, &recognizer_frame(at, sequence), at)
            .unwrap();
        session.request_recognition(ticket.clone()).unwrap();
        last = Some(ticket);
    }
    let mut excess = last.unwrap();
    excess.id += 1;
    assert!(session.request_recognition(excess.clone()).is_err());
    session.stop().unwrap();
    assert!(session.request_recognition(excess).is_err());
    assert!(drain(&mut session).unwrap().is_empty());
    assert!(session.take_recognitions().is_empty());
}

#[test]
fn actual_helper_cleans_retired_source_state_without_fresh_desktop_authority() {
    use prek::{
        context::State,
        recognition::{Gate, Retirement},
    };
    let (definition, recognizer) = recognizer_definition(
        r#"
held=held or {}
if ticket.cause=='cancel' then
 assert(ticket.input==nil and ticket.id==nil and held[ticket.candidate])
 if ticket.candidate=='7' then
  assert(ticket.reason=='source_lost' and ticket.source.generation=='4')
  assert(p.context.state()=='unknown' and p.devices.list()==nil)
 else assert(ticket.candidate=='8' and ticket.reason=='completed') end
 held[ticket.candidate]=nil
 p.log.info('cleanup',{candidate=ticket.candidate,calls=calls});return nil
end
assert(not held[ticket.candidate]);assert(ticket.candidate=='7' or not held['7'])
held[ticket.candidate]=true;p.log.info('frame',{calls=calls});return nil
"#,
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let mut current = recognizer_candidate(at);
    current.id = 7;
    current.source.generation = 4;
    let mut frame = recognizer_frame(at, 1);
    if let prek::input_events::Event::Frame { source, .. } = &mut frame {
        *source = current.source.clone();
    }
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    let ticket = gate.offer("hold", &current, &frame, at).unwrap();
    session.request_recognition(ticket).unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].fields["calls"], 1);
    let proposal = session.take_recognitions().pop().unwrap().1.unwrap();
    assert!(
        gate.validate(proposal, &current, monotonic().unwrap())
            .unwrap()
            .is_none()
    );

    let mut withdrawn = context_view(monotonic().unwrap(), 0);
    withdrawn.state = State::Unknown;
    withdrawn.context = None;
    session.update_context(withdrawn).unwrap();
    drain(&mut session).unwrap();
    gate.retire_source(&current.source);
    let cancellation = gate.take_cancellations().pop().unwrap();
    let expected_cancellation = cancellation.clone();
    session.cancel_recognition(cancellation.clone()).unwrap();
    session.cancel_recognition(cancellation).unwrap();
    let logs = drain(&mut session).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "cleanup");
    assert_eq!(logs[0].fields["calls"], 2);
    assert_eq!(
        session.take_recognition_cancellations(),
        vec![expected_cancellation]
    );
    assert!(session.take_recognitions().is_empty());

    // This is modeled source/context recovery, not live broker admission.
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    current.id = 8;
    current.source.generation = 5;
    current.expires = at + Duration::from_secs(2);
    let mut frame = recognizer_frame(at, 1);
    if let prek::input_events::Event::Frame { source, .. } = &mut frame {
        *source = current.source.clone();
    }
    session
        .request_recognition(gate.offer("hold", &current, &frame, at).unwrap())
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].fields["calls"], 3);
    session.take_recognitions();
    gate.retire_with(current.id, Retirement::Completed);
    session
        .cancel_recognition(gate.take_cancellations().pop().unwrap())
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].fields["calls"], 4);
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn cancelling_unsent_recognition_never_enters_its_callback() {
    use prek::recognition::Gate;
    let (definition, recognizer) = recognizer_definition(
        "assert(calls==1 and ticket.candidate=='8');p.log.info('new');return nil",
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let mut current = recognizer_candidate(at);
    current.id = 7;
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    session
        .request_recognition(
            gate.offer("hold", &current, &recognizer_frame(at, 1), at)
                .unwrap(),
        )
        .unwrap();
    gate.retire(current.id);
    session
        .cancel_recognition(gate.take_cancellations().pop().unwrap())
        .unwrap();
    assert!(drain(&mut session).unwrap().is_empty());
    assert!(session.take_recognitions().is_empty());
    current.id = 8;
    let at = monotonic().unwrap();
    session
        .request_recognition(
            gate.offer("hold", &current, &recognizer_frame(at, 2), at)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "new");
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn cancelling_inflight_recognition_discards_its_result_and_logs_before_cleanup() {
    use prek::recognition::Gate;
    let (definition, recognizer) = recognizer_definition(
        r#"
if ticket.cause=='cancel' then assert(calls==2);p.log.info('cleanup');return nil end
assert(calls==1);p.log.info('discard-inflight');return {phase='discrete'}
"#,
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    session
        .request_recognition(
            gate.offer("hold", &current, &recognizer_frame(at, 1), at)
                .unwrap(),
        )
        .unwrap();
    // One step sends the request; no subsequent read has accepted its reply.
    assert!(session.step(monotonic().unwrap()).unwrap().is_empty());
    assert!(!session.finished());
    gate.retire(current.id);
    session
        .cancel_recognition(gate.take_cancellations().pop().unwrap())
        .unwrap();
    let logs = drain(&mut session).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "cleanup");
    assert!(session.take_recognitions().is_empty());
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn cancelling_completed_recognition_purges_results_still_owned_by_the_parent() {
    use prek::recognition::Gate;
    let (definition, recognizer) = recognizer_definition(
        "if ticket.cause=='cancel' then p.log.info('cleanup');return nil end;return {phase='discrete'}",
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    session
        .request_recognition(
            gate.offer("hold", &current, &recognizer_frame(at, 1), at)
                .unwrap(),
        )
        .unwrap();
    drain(&mut session).unwrap();
    // The result is queued in the parent and has not been handed to Rust yet.
    gate.retire(current.id);
    session
        .cancel_recognition(gate.take_cancellations().pop().unwrap())
        .unwrap();
    assert!(session.take_recognitions().is_empty());
    assert_eq!(drain(&mut session).unwrap()[0].message, "cleanup");
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn cleanup_has_reserved_capacity_and_runs_before_a_full_ordinary_event_queue() {
    use prek::recognition::Gate;
    let (definition, recognizer) = recognizer_definition(
        r#"
if ticket.cause=='cancel' then cleaned=true;p.log.info('cleanup');return nil end
cleaned=false
p.events.on('matched',function()assert(cleaned,'ordinary callback preceded cleanup')end)
return nil
"#,
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    session
        .request_recognition(
            gate.offer("hold", &current, &recognizer_frame(at, 1), at)
                .unwrap(),
        )
        .unwrap();
    drain(&mut session).unwrap();
    session.take_recognitions();
    for _ in 0..64 {
        session
            .notify(EventKind::Matched, serde_json::json!({}))
            .unwrap();
    }
    gate.retire(current.id);
    session
        .cancel_recognition(gate.take_cancellations().pop().unwrap())
        .unwrap();
    let logs = drain(&mut session).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "cleanup");
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn pending_cleanup_reserves_parent_candidate_slots_until_receipts_are_consumed() {
    use prek::recognition::Gate;
    let (definition, recognizer) = recognizer_definition(
        "assert(calls==1 and ticket.candidate=='65');p.log.info('new');return nil",
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let mut current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    for id in 1..=64 {
        current.id = id;
        session
            .request_recognition(
                gate.offer("hold", &current, &recognizer_frame(at, 1), at)
                    .unwrap(),
            )
            .unwrap();
    }
    gate.retire_source(&current.source);
    for cancellation in gate.take_cancellations() {
        session.cancel_recognition(cancellation).unwrap();
    }
    current.id = 65;
    let now = monotonic().unwrap();
    let deferred = gate
        .offer("hold", &current, &recognizer_frame(now, 1), now)
        .unwrap();
    assert!(
        session
            .request_recognition(deferred)
            .unwrap_err()
            .to_string()
            .contains("admitted")
    );
    assert!(drain(&mut session).unwrap().is_empty());
    assert!(session.take_recognitions().is_empty());
    authorize_recognizer(&mut session);
    let now = monotonic().unwrap();
    let fresh = gate
        .offer("hold", &current, &recognizer_frame(now, 2), now)
        .unwrap();
    assert!(
        session
            .request_recognition(fresh.clone())
            .unwrap_err()
            .to_string()
            .contains("admitted")
    );
    let acknowledged = session.take_recognition_cancellations();
    assert_eq!(acknowledged.len(), 64);
    assert_eq!(
        acknowledged
            .iter()
            .map(|cancel| cancel.candidate)
            .collect::<Vec<_>>(),
        (1..=64).collect::<Vec<_>>()
    );
    session.request_recognition(fresh).unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "new");
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn a_cleanup_proposal_retires_the_actual_helper_without_logs_or_effects() {
    use prek::recognition::Gate;
    let (definition, recognizer) = recognizer_definition(
        "if ticket.cause=='cancel' then p.log.info('discarded');return {phase='discrete'} end;return nil",
    );
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let current = recognizer_candidate(at);
    let mut gate = Gate::new(fence(1), vec![recognizer]).unwrap();
    session
        .request_recognition(
            gate.offer("hold", &current, &recognizer_frame(at, 1), at)
                .unwrap(),
        )
        .unwrap();
    drain(&mut session).unwrap();
    session.take_recognitions();
    gate.retire(current.id);
    session
        .cancel_recognition(gate.take_cancellations().pop().unwrap())
        .unwrap();
    assert!(drain(&mut session).is_err());
    assert!(session.take_recognitions().is_empty());
}

#[test]
fn private_input_worker_round_trips_a_stationary_hold_through_the_actual_luna_helper() {
    use prek::{
        config::Trigger,
        configuration::Command,
        input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source},
        ipc::Channel,
        protocol::Capability,
        recognition::{Cause, Retirement},
        rules::{Event, Evidence, Stage},
        worker::{Message, Report, Stream, Subscription, Subscriptions},
    };
    use std::collections::{BTreeMap, BTreeSet};
    fn messages(stream: &mut Stream, parent: &mut Channel) -> Vec<Message> {
        let mut messages = Vec::new();
        for _ in 0..4 {
            stream.flush().unwrap();
            while let Some(packet) = parent.receive::<Message>().unwrap() {
                messages.push(packet.message);
            }
        }
        messages
    }
    fn respond(
        stream: &mut Stream,
        parent: &mut Channel,
        proposal: prek::recognition::Proposal,
    ) -> Vec<Message> {
        let id = proposal.ticket;
        assert!(
            parent
                .send(
                    &Command::Recognition {
                        seat: "seat0".into(),
                        fence: fence(1),
                        proposal
                    },
                    monotonic().unwrap()
                )
                .unwrap()
        );
        stream.service().unwrap();
        stream
            .push(prek::replay::Record::Tick {
                at: monotonic().unwrap(),
            })
            .unwrap();
        let messages = messages(stream, parent);
        assert!(messages.iter().any(|message| matches!(message, Message::RecognitionResult { ticket, fence: actual, error: None, .. } if *ticket == id && *actual == fence(1))));
        messages
    }
    let mut definition = Definition::compile(
        r#"
local p=require('prek');local calls=0
local declaration={name='native_hold',selectors={'keyboard'},inputs={'key'},reply_within='250ms'}
p.events.on('input',function(ticket)
 calls=calls+1
 if ticket.cause=='cancel' then
  assert(ticket.input==nil and ticket.reason=='completed' and calls==3)
  p.log.info('native-cleanup');return nil
 end
 assert(ticket.input.source.device=='18446744073709551615')
 if ticket.cause=='frame' then assert(calls==1);return nil end
 assert(ticket.cause=='timer' and calls==2 and #ticket.input.changes==0)
 return {phase='discrete',data={calls=calls}}
end)
return p.config.new{
 policy={allow_input_events=true,allowed_devices={'keyboard'}},devices={keyboard={keys={'KEY_A'}}},
 rules={p.rules.define{id='native_hold_rule',contexts={'desktop'},
  trigger={kind='custom',recognizer=declaration,timeout='1s',wake='20ms'},
  action=p.actions.event{name='native_hold'}}},
}
"#,
        "native-recognizer-helper.lua",
    )
    .unwrap();
    let config = definition.validate().unwrap();
    let Trigger::Custom { recognizer, .. } = &config.rules[0].trigger else {
        unreachable!()
    };
    definition.bind_recognizer(1, recognizer.clone()).unwrap();
    let mut forged = definition.clone();
    forged.recognizers[0].definition.continuous = true;
    assert!(forged.validate().is_err());
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    authorize_recognizer(&mut session);
    let at = monotonic().unwrap();
    let (mut parent, child) = Channel::pair().unwrap();
    let mut stream = Stream::new(config, child).unwrap();
    // Explicit authority model; no broker, logind or compositor was verified.
    let provided = context_view(at, 0).context.unwrap();
    stream
        .push(prek::replay::Record::Header { version: 1 })
        .unwrap();
    let source = Source {
        device: u64::MAX,
        generation: u64::MAX,
        seat: "seat0".into(),
    };
    stream
        .push(prek::replay::Record::Device {
            device: Device {
                source: source.clone(),
                identity: "model keyboard".into(),
                name: "model keyboard".into(),
                path: "/dev/input/model".into(),
                physical: "prek/test/model".into(),
                unique: String::new(),
                vendor: 1,
                product: 1,
                properties: Vec::new(),
                capabilities: BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30])]),
                axes: BTreeMap::new(),
                virtual_device: false,
            },
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    assert!(
        parent
            .send(
                &Command::Subscriptions {
                    update: Subscriptions {
                        revision: 1,
                        seats: vec![Subscription {
                            seat: "seat0".into(),
                            fence: fence(1),
                            context: Some(provided),
                            consumer: None,
                            capabilities: BTreeSet::from([
                                Capability::CustomRecognizers,
                                Capability::LuaEvents,
                                Capability::Context,
                                Capability::Observations
                            ]),
                        }]
                    }
                },
                monotonic().unwrap()
            )
            .unwrap()
    );
    stream.service().unwrap();
    let initial = messages(&mut stream, &mut parent);
    assert!(initial.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::RecognizerReady { ready: true, .. }
        }
    )));
    let press_at = monotonic().unwrap();
    stream
        .push(prek::replay::Record::Frame {
            frame: Frame {
                source,
                at: press_at,
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
    stream
        .push(prek::replay::Record::Tick {
            at: monotonic().unwrap(),
        })
        .unwrap();
    let first = messages(&mut stream, &mut parent)
        .into_iter()
        .find_map(|message| match message {
            Message::Outcome {
                report: Report::Recognition { ticket, .. },
            } => Some(*ticket),
            _ => None,
        })
        .unwrap();
    assert_eq!(first.cause, Cause::Frame);
    session.request_recognition(first).unwrap();
    drain(&mut session).unwrap();
    let mut replies = respond(
        &mut stream,
        &mut parent,
        session.take_recognitions().pop().unwrap().1.unwrap(),
    );
    if !replies.iter().any(|message| {
        matches!(
            message,
            Message::Outcome {
                report: Report::Recognition { .. }
            }
        )
    }) {
        std::thread::sleep(Duration::from_millis(25));
        stream
            .push(prek::replay::Record::Tick {
                at: monotonic().unwrap(),
            })
            .unwrap();
        replies.extend(messages(&mut stream, &mut parent));
    }
    let timer = replies
        .into_iter()
        .find_map(|message| match message {
            Message::Outcome {
                report: Report::Recognition { ticket, .. },
            } => Some(*ticket),
            _ => None,
        })
        .unwrap();
    assert_eq!(timer.cause, Cause::Timer);
    assert_eq!(timer.input.at(), press_at);
    session.request_recognition(timer).unwrap();
    drain(&mut session).unwrap();
    let replies = respond(
        &mut stream,
        &mut parent,
        session.take_recognitions().pop().unwrap().1.unwrap(),
    );
    assert!(replies.iter().any(|message| matches!(message, Message::Outcome { report: Report::Observation { event, .. } } if matches!(event.as_ref(), Event::Matched { stage: Stage::Discrete, ordinal: 1, evidence: Evidence::Custom { data, .. }, .. } if data["calls"] == 2))));
    let cancellation = replies
        .into_iter()
        .find_map(|message| match message {
            Message::Outcome {
                report: Report::RecognitionCancelled { cancellation, .. },
            } => Some(cancellation),
            _ => None,
        })
        .unwrap();
    assert_eq!(cancellation.reason, Retirement::Completed);
    session.cancel_recognition(cancellation.clone()).unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "native-cleanup");
    assert_eq!(session.take_recognition_cancellations(), vec![cancellation]);
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn real_helper_receives_broker_stamped_status_with_independent_expiry_and_stop() {
    use prek::{context::State, lua::StatusView, protocol::Mode};
    use std::collections::BTreeSet;
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 local status=p.status.current()
 assert(status and status.revision=='18446744073709551615' and status.pending==63)
 assert(status.fence.configuration=='1' and status.state=='unknown')
 assert(status.context_error=='provider unavailable' and not p.capabilities.has('commands'))
 local config=p.config.current();local policy=p.policy.current()
 assert(config.revision=='1' and policy.revision=='1' and not policy.policy.allow_power_actions)
 config.config.policy.allow_power_actions=true
 assert(not p.policy.current().policy.allow_power_actions)
 p.log.info('status')
end)
p.events.on('matched',function()
 assert(p.status.current()==nil and p.capabilities.current()~=nil)
 p.log.info('expired')
end)
p.events.on('stop',function()
 assert(p.status.current()==nil and p.capabilities.current()==nil)
 p.log.info('cleared')
end)
return {version=1}
"#,
        "status-helper.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    for pending in 0..64 {
        session
            .update_status(StatusView {
                fence: fence(1),
                mode: Mode::Observe,
                state: State::Unknown,
                revision: u64::MAX,
                pending,
                last_reload_error: None,
                context_error: Some("provider unavailable".into()),
                sampled_at: monotonic().unwrap(),
            })
            .unwrap();
    }
    session
        .update_capabilities(BTreeSet::new(), BTreeSet::new())
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "status");
    std::thread::sleep(Duration::from_millis(260));
    session
        .update_capabilities(BTreeSet::new(), BTreeSet::new())
        .unwrap();
    session
        .notify(EventKind::Matched, serde_json::json!({}))
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "expired");
    session.stop().unwrap();
    assert!(
        session
            .update_status(StatusView {
                fence: fence(1),
                mode: Mode::Observe,
                state: State::Unknown,
                revision: 0,
                pending: 0,
                last_reload_error: None,
                context_error: None,
                sampled_at: monotonic().unwrap()
            })
            .is_err()
    );
    assert_eq!(drain(&mut session).unwrap()[0].message, "cleared");
}

#[test]
fn real_helper_runs_offline_replay_without_live_context_or_effect_capabilities() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 assert(p.context.snapshot()==nil and p.capabilities.current()==nil)
 local replay=p.replay.new()
 replay:push('{"record":"header","version":1}')
 replay:push('{"record":"tick","at":{"secs":0,"nanos":0}}')
 local result=replay:finish()
 assert(result.mode=='observations' and not result.checked and #result.outcomes==0)
 assert(result.json=='[]' and replay:status().state=='finished')
 local rules=p.replay.new(p.config.preset('minimal'))
 rules:push('{"record":"header","version":1}')
 assert(rules:finish().mode=='rules')
 p.log.info('offline-replayed')
end)
return {version=1}
"#,
        "offline-replay-helper.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "offline-replayed");
}

#[test]
fn real_helper_reports_negotiated_capabilities_readiness_expiry_and_stop() {
    use prek::protocol::Capability;
    use std::collections::BTreeSet;
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 local v=p.capabilities.current()
 assert(v.fence.configuration=='1' and v.fence.session=='1')
 assert(p.capabilities.has('commands') and p.capabilities.has('context'))
 assert(p.capabilities.ready('commands') and not p.capabilities.ready('context'))
 v.ready[1]='power_actions';assert(not p.capabilities.ready('power_actions'))
 p.log.info('negotiated')
end)
p.events.on('matched',function(event)
 if event.expired then
  assert(p.capabilities.current()==nil and not p.capabilities.has('commands'))
  assert(not p.capabilities.ready('commands'));p.log.info('expired')
 else
  assert(p.capabilities.has('commands') and not p.capabilities.ready('commands'))
  assert(#p.capabilities.current().ready==0);p.log.info('withdrawn')
 end
end)
p.events.on('stop',function()
 assert(p.capabilities.current()==nil and not p.capabilities.has('context'))
 assert(not p.capabilities.ready('commands'));p.log.info('cleared')
end)
return {version=1}
"#,
        "capability-helper.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    let negotiated = BTreeSet::from([Capability::Context, Capability::Commands]);
    session
        .update_capabilities(negotiated.clone(), BTreeSet::from([Capability::Commands]))
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "negotiated");
    session
        .update_capabilities(negotiated, BTreeSet::new())
        .unwrap();
    session
        .notify(EventKind::Matched, serde_json::json!({"expired":false}))
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "withdrawn");
    std::thread::sleep(Duration::from_millis(260));
    session
        .notify(EventKind::Matched, serde_json::json!({"expired":true}))
        .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "expired");
    session.stop().unwrap();
    assert!(
        session
            .update_capabilities(BTreeSet::new(), BTreeSet::new())
            .is_err()
    );
    assert_eq!(drain(&mut session).unwrap()[0].message, "cleared");
}

#[test]
fn real_helper_coalesces_unsent_readiness_without_changing_negotiated_flags() {
    use prek::protocol::Capability;
    use std::collections::BTreeSet;
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 assert(p.capabilities.has('commands') and not p.capabilities.has('power_actions'))
 assert(not p.capabilities.ready('commands'));p.log.info('latest')
end)
return {version=1}
"#,
        "capability-queue.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    let negotiated = BTreeSet::from([Capability::Commands]);
    for _ in 0..100 {
        session
            .update_capabilities(negotiated.clone(), negotiated.clone())
            .unwrap();
        session
            .update_capabilities(negotiated.clone(), BTreeSet::new())
            .unwrap();
    }
    assert!(
        session
            .update_capabilities(BTreeSet::from([Capability::PowerActions]), BTreeSet::new())
            .is_err()
    );
    assert!(
        session
            .update_capabilities(negotiated, BTreeSet::from([Capability::Context]))
            .is_err()
    );
    assert_eq!(drain(&mut session).unwrap()[0].message, "latest");
}

#[test]
fn real_helper_never_infers_capabilities_from_configuration_permissions() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 assert(p.capabilities.current()==nil)
 assert(not p.capabilities.has('power_actions') and not p.capabilities.ready('commands'))
 p.log.info('unavailable')
end)
return {version=1,policy={allow_power_actions=true}}
"#,
        "capability-none.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "unavailable");
}

#[test]
fn real_helper_preserves_maximum_identities_in_context_events_actions_and_scheduled_work() {
    use prek::{
        input::Source,
        rules::{Event, Evidence, Stage},
    };
    let definition = Definition::compile(r#"
local p=require('prek')
local function check(fence)
  assert(fence.broker[1]=='18446744073709551615' and fence.broker[2]=='9223372036854775808')
  assert(fence.configuration=='18446744073709551614' and fence.session=='18446744073709551615')
end
p.events.on('ready',function(event)
  check(event.fence);p.log.info('exact-ready')
  p.scheduler.after('1ms',function(wake)check(wake.context.fence);p.log.info('exact-timer')end)
  p.scheduler.spawn(function(wake)
    check(wake.context.fence);p.log.info('exact-task')
    local next=p.scheduler.sleep('1ms');check(next.context.fence);p.log.info('exact-resume')
  end)
end)
p.events.on('context_changed',function(event)
  check(event.fence)
  local snapshot=p.context.snapshot();local binding=p.context.binding()
  assert(snapshot.generation==event.fence.session and snapshot.revision==event.revision)
  assert(snapshot.revision=='18446744073709551615')
  assert(snapshot.outputs[1].geometry_generation=='18446744073709551614')
  assert(binding.compositor_start=='18446744073709551615')
  check(p.context.fence());p.log.info('exact-context',{revision=event.revision})
end)
local function matched(payload)
  check(payload.fence)
  local e=payload.event
  assert(e.id=='18446744073709551615' and e.ordinal=='18446744073709551614')
  assert(e.source.device=='9223372036854775808' and e.source.generation=='18446744073709551615')
  assert(e.config_generation==payload.fence.configuration and e.session_generation==payload.fence.session)
  assert(e.evidence.count==3);p.log.info('exact-match',{id=e.id})
end
p.events.on('matched',matched)
p.events.on('stop',function(event)check(event.fence);p.log.info('exact-stop')end)
local action=p.actions.define('exact',{requires={'observations'}},matched)
return p.config.new{rules={p.rules.define{id='exact',trigger=p.triggers.key{key='KEY_A'},action=action}}}
"#, "exact-helper.lua").unwrap();
    let action = definition.validate().unwrap().rules[0].action.clone();
    let fence = Fence {
        broker: Epoch([u64::MAX, 1 << 63]),
        configuration: u64::MAX - 1,
        session: u64::MAX,
    };
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence,
        "seat0",
        definition,
    )
    .unwrap();
    let deadline = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    while logs.len() < 4 {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        assert!(
            monotonic().unwrap() < deadline,
            "scheduled helper work did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(logs[0].message, "exact-ready");
    let mut messages = logs
        .iter()
        .map(|log| log.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        messages.iter().position(|message| *message == "exact-task")
            < messages
                .iter()
                .position(|message| *message == "exact-resume")
    );
    messages.sort_unstable();
    assert_eq!(
        messages,
        vec!["exact-ready", "exact-resume", "exact-task", "exact-timer"]
    );
    let mut view = context_view(monotonic().unwrap(), 1);
    view.fence = fence;
    let context = view.context.as_mut().unwrap();
    context.snapshot.generation = fence.session;
    context.snapshot.revision = u64::MAX;
    context.snapshot.outputs[0].geometry_generation = u64::MAX - 1;
    context.binding.compositor_start = u64::MAX;
    session.update_context(view).unwrap();
    let logs = drain(&mut session).unwrap();
    assert_eq!(
        logs[0].fields,
        serde_json::json!({"revision":u64::MAX.to_string()})
    );
    let event = Box::new(Event::Matched {
        version: 1,
        rule: "exact".into(),
        id: u64::MAX,
        ordinal: u64::MAX - 1,
        source: Source {
            device: 1 << 63,
            generation: u64::MAX,
            seat: "seat0".into(),
        },
        at: monotonic().unwrap(),
        config_generation: fence.configuration,
        session_generation: fence.session,
        stage: Stage::Discrete,
        action,
        evidence: Evidence::Taps { count: 3 },
        window: None,
        output: None,
        workspace: None,
    });
    session
        .notify(
            EventKind::Matched,
            serde_json::json!({"fence":fence,"event":event}),
        )
        .unwrap();
    assert_eq!(
        drain(&mut session).unwrap()[0].fields,
        serde_json::json!({"id":u64::MAX.to_string()})
    );
    session
        .request_action(event, monotonic().unwrap() + Duration::from_millis(250))
        .unwrap();
    drain(&mut session).unwrap();
    let actions = session.take_actions();
    assert_eq!((actions[0].0, actions[0].1), (u64::MAX, u64::MAX - 1));
    assert_eq!(
        actions[0].2.as_ref().unwrap().logs[0].fields,
        serde_json::json!({"id":u64::MAX.to_string()})
    );
    session.stop().unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "exact-stop");
}

#[test]
fn real_helper_receives_owned_context_and_expires_original_timestamps() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
local saved
p.events.on('context_changed',function(event)
  if event.available then
    local snapshot=p.context.snapshot(); local binding=p.context.binding()
    assert(snapshot.windows[1].rect.origin.x==0.1 and snapshot.outputs[1].active_workspace==3)
    local output=p.outputs.get('fixture-output')
    assert(#p.outputs.list()==1 and #p.regions.list()==0 and #p.regions.at({x=0,y=0})==0)
    assert(output and output.reference.revision==snapshot.revision)
    if saved then assert(p.coordinates.convert(saved,{x=0,y=0},'local','global')==nil) end
    saved=output.reference
    local position=p.coordinates.convert(output.reference,{x=0.5,y=0.5},'normalized','global')
    assert(position and position.x==500 and position.y==400)
    output.state.geometry.origin.x=123
    assert(p.coordinates.convert(output.reference,{x=0,y=0},'local','global').x==0)
    snapshot.windows[1].id='forged'; binding.uid=0
    assert(p.context.snapshot().windows[1].id=='window-0' and p.context.binding().uid~=0)
    p.log.info('context',{state=p.context.state(),revision=event.revision})
  else
    assert(p.context.snapshot()==nil and p.context.state()=='unknown')
    assert(p.outputs.list()==nil and p.regions.list()==nil)
    assert(p.coordinates.convert(saved,{x=0,y=0},'local','global')==nil)
    p.log.info('expired-context')
  end
end)
return {version=1}
"#,
        "context.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    let mut view = context_view(monotonic().unwrap(), 1);
    let mut wrong = view.clone();
    wrong.fence.configuration = 2;
    assert!(session.update_context(wrong).is_err());
    let mut wrong = view.clone();
    wrong.context.as_mut().unwrap().binding.seat = "seat1".into();
    assert!(session.update_context(wrong).is_err());
    let mut wrong = view.clone();
    wrong.context.as_mut().unwrap().binding.uid += 1;
    assert!(session.update_context(wrong).is_err());
    session.update_context(view.clone()).unwrap();
    assert!(!session.finished());
    let logs = drain(&mut session).unwrap();
    assert_eq!(
        logs[0].fields,
        serde_json::json!({"state":"desktop","revision":"7"})
    );
    view.sampled_at = monotonic().unwrap();
    let snapshot = &mut view.context.as_mut().unwrap().snapshot;
    snapshot.captured_at = view.sampled_at;
    snapshot.revision += 1;
    session.update_context(view.clone()).unwrap();
    assert_eq!(
        drain(&mut session).unwrap()[0].fields,
        serde_json::json!({"state":"desktop","revision":"8"})
    );
    view.sampled_at = Duration::ZERO;
    view.context.as_mut().unwrap().snapshot.captured_at = Duration::ZERO;
    session.update_context(view).unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "expired-context");
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn real_helper_receives_native_morf_bindings_and_retires_them_with_context() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('context_changed',function()
  local providers=p.morf.providers()
  if providers==nil then p.log.info('unavailable');return end
  if #providers==0 then p.log.info('empty');return end
  local provider=providers[1]
  assert(provider.phase=='configured' and provider.output=='fixture-output')
  assert(provider.compositor_start=='18446744073709551615')
  assert(provider.fence.session==p.context.fence().session)
  local reports=p.morf.regions()
  assert(#reports==1 and reports[1].output==provider.output)
  assert(reports[1].context.frame.regions[1].id=='drawer-keyboard')
  assert(reports[1].context.ui_generation=='2')
  p.log.info('native-binding',{epoch=provider.epoch,birth=provider.compositor_start})
end)
return {version=1}
"#,
        "native-morf.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    let mut view = context_view(monotonic().unwrap(), 0);
    let binding = &mut view.context.as_mut().unwrap().binding;
    binding.compositor_start = u64::MAX;
    view.morf.push(prek::morf::ShellState {
        version: 1,
        role: "shell".into(),
        epoch: "a".repeat(32),
        revision: 2,
        phase: prek::morf::ShellPhase::Configured,
        compositor_pid: binding.compositor_pid,
        compositor_start: binding.compositor_start,
        compositor_uid: binding.uid,
        output: "fixture-output".into(),
    });
    // This helper test transfers an explicit native-context model; actual
    // native GPU observations are exercised separately in the owned Cage run.
    view.morf_rendered
        .push(prek::morf::rendered::OutputContext {
            layout: None,
            output: "fixture-output".into(),
            context: serde_json::from_value(serde_json::json!({
                "version":1,"epoch":"a".repeat(32),"ui_generation":"2","revision":"7",
                "sampled_ns":view.sampled_at.as_nanos().to_string(),
                "coverage":"primary_surface_named_bounds",
                "frame":{"width":1000,"height":800,"scale_120":120,"regions":[{
                "node":"18446744073709551615","id":"drawer-keyboard","interactive":false,
                    "bounds":{"x":10.0,"y":600.0,"width":980.0,"height":200.0}
                }]}
            }))
            .unwrap(),
        });
    session.update_context(view.clone()).unwrap();
    assert_eq!(
        drain(&mut session).unwrap()[0].fields,
        serde_json::json!({"epoch":"a".repeat(32),"birth":u64::MAX.to_string()})
    );
    view.morf.clear();
    view.morf_rendered.clear();
    view.sampled_at = monotonic().unwrap();
    view.context.as_mut().unwrap().snapshot.captured_at = view.sampled_at;
    session.update_context(view.clone()).unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "empty");
    view.sampled_at = Duration::ZERO;
    view.context.as_mut().unwrap().snapshot.captured_at = Duration::ZERO;
    session.update_context(view).unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "unavailable");
    session.stop().unwrap();
    drain(&mut session).unwrap();
}

#[test]
fn real_helper_assembles_multiple_context_chunks_and_replaces_unsent_updates() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('context_changed',function(event)
  p.log.info('context-revision',{revision=event.revision})
end)
return {version=1}
"#,
        "context.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    let mut view = context_view(monotonic().unwrap(), 512);
    assert!(view.encode().unwrap().len() > prek::context_transfer::CHUNK_BYTES);
    session.update_context(view.clone()).unwrap();
    session.step(monotonic().unwrap()).unwrap(); // First chunk freezes this upload.
    view.context.as_mut().unwrap().snapshot.revision = 8;
    session.update_context(view.clone()).unwrap();
    view.context.as_mut().unwrap().snapshot.revision = 9;
    session.update_context(view).unwrap();
    let logs = drain(&mut session).unwrap();
    assert_eq!(
        logs.iter()
            .map(|log| log.fields["revision"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["7", "9"]
    );
}
#[test]
fn real_supervised_vm_preserves_closures_and_runs_stop_callbacks() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
local count=0
p.events.on('matched',function(event)
 count=count+1; p.log.info('match',{count=count,rule=event.event.rule})
end)
p.events.on('stop',function()p.log.info('stopped')end)
return {version=1}
"#,
        "session.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    assert!(drain(&mut session).unwrap().is_empty());
    for count in 1..=2 {
        session
            .notify(
                EventKind::Matched,
                serde_json::json!({"fence":fence(1),"event":{"rule":"fixture"}}),
            )
            .unwrap();
        let logs = drain(&mut session).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].fields,
            serde_json::json!({"count":count,"rule":"fixture"})
        );
    }
    session.stop().unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "stopped");
}

#[test]
fn approved_modules_reconstruct_in_the_real_helper_without_filesystem_sources() {
    use prek::lua::{Module, Sources};
    let mut sources = Sources::single(
        "local p=require('prek'); p.events.on('ready',require('counter')); return {version=1}"
            .into(),
        "/missing/approved/config.lua".into(),
    );
    sources.modules.insert("counter".into(), Module { source: "local p=require('prek'); local count=0; return function()count=count+1; p.log.info('module-callback',{count=count})end".into() });
    let definition = Definition::compile_sources(&sources).unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    let initial = drain(&mut session).unwrap();
    assert_eq!(initial[0].fields, serde_json::json!({"count":1}));
    for count in 2..=3 {
        session
            .notify(EventKind::Ready, serde_json::json!({"fence":fence(1)}))
            .unwrap();
        let logs = drain(&mut session).unwrap();
        assert_eq!(logs[0].message, "module-callback");
        assert_eq!(logs[0].fields, serde_json::json!({"count":count}));
    }
}

#[test]
fn named_handler_executes_exact_argv_only_after_a_current_model_grant() {
    use prek::{
        actions::Executor,
        config::Action,
        input::Source,
        ipc::Channel,
        protocol::{Capability, Completion, Mode, Request},
        rules::{Event, Evidence, Stage},
    };
    use std::{collections::BTreeSet, fs};
    let path = std::env::temp_dir().join(format!(
        "prek-lua-action-{}-$(literal);'quoted'",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let definition=Definition::compile(r#"
local p=require('prek')
local action=p.actions.define('touch',{requires={'commands'}},function(event)
 p.log.info('named-action',{rule=event.event.rule})
 return p.actions.command{argv={'touch','--',event.event.output}}
end)
return p.config.new{rules={p.rules.define{id='touch-rule',trigger=p.triggers.key{key='KEY_A'},action=action}}}
"#,"named-process.lua").unwrap();
    let action = definition.validate().unwrap().rules[0].action.clone();
    assert!(matches!(action, Action::Lua { .. }));
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    assert!(session.action_ready());
    let mut executor = Executor::new(
        fence(1),
        "seat0".into(),
        Mode::Route,
        BTreeSet::from([Capability::Commands, Capability::LuaActions]),
    )
    .unwrap();
    executor.lua_ready(true);
    let (mut agent_channel, mut model_broker) = Channel::pair().unwrap();
    let event = Box::new(Event::Matched {
        version: 1,
        rule: "touch-rule".into(),
        id: 1,
        ordinal: 1,
        source: Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        at: monotonic().unwrap(),
        config_generation: 1,
        session_generation: 1,
        stage: Stage::Discrete,
        action,
        evidence: Evidence::Key { down: true },
        window: None,
        output: Some(path.to_str().unwrap().into()),
        workspace: None,
    });
    executor
        .offer(fence(1), event.clone(), monotonic().unwrap())
        .unwrap();
    executor
        .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
        .unwrap();
    assert!(
        !path.exists(),
        "an offered action is not an execution grant"
    );
    executor
        .grant(
            fence(1),
            1,
            1,
            monotonic().unwrap() + Duration::from_millis(250),
            monotonic().unwrap(),
        )
        .unwrap();
    assert!(
        executor
            .offer(fence(1), event.clone(), monotonic().unwrap())
            .is_err()
    );
    executor
        .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
        .unwrap();
    // Progress coalescing must retain the byte charge for the queued action.
    // Otherwise sending it can underflow the shared queue counter.
    for _ in 0..2 {
        session
            .notify(
                EventKind::Update,
                serde_json::json!({
                    "fence": fence(1), "event": {"id": 1, "stage": "update", "source": "fixture"}
                }),
            )
            .unwrap();
    }
    // Cancel before the queued callback starts. Its command and log must stay
    // absent while the same supervised VM accepts a later gesture.
    executor.cancel_action(fence(1), 1, 1).unwrap();
    session.cancel_action(1, 1).unwrap();
    drain(&mut session).unwrap();
    for (id, ordinal, result) in session.take_actions() {
        assert!(
            executor
                .resolve_action(id, ordinal, result, monotonic().unwrap())
                .unwrap()
                .is_empty()
        );
    }
    assert!(!path.exists() && session.action_ready());
    let mut later = event;
    let Event::Matched { id, at, .. } = later.as_mut() else {
        unreachable!()
    };
    *id = 2;
    *at = monotonic().unwrap();
    executor
        .offer(fence(1), later, monotonic().unwrap())
        .unwrap();
    let now = monotonic().unwrap();
    executor
        .grant(fence(1), 2, 1, now + Duration::from_millis(250), now)
        .unwrap();
    executor
        .execute_ready_with_lua(now, Some(&mut session))
        .unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(3);
    let mut logs = Vec::new();
    let mut completions = Vec::new();
    while completions.len() < 2 {
        session.step(monotonic().unwrap()).unwrap();
        for (id, ordinal, result) in session.take_actions() {
            logs.extend(
                executor
                    .resolve_action(id, ordinal, result, monotonic().unwrap())
                    .unwrap(),
            );
        }
        executor
            .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
            .unwrap();
        executor.reap().unwrap();
        executor.flush(&mut agent_channel).unwrap();
        while let Some(packet) = model_broker.receive::<Request>().unwrap() {
            if let Request::Complete {
                fence,
                id,
                ordinal,
                result,
            } = packet.message
            {
                completions.push((fence, id, ordinal, result));
            }
        }
        assert!(monotonic().unwrap() < end, "named command never executed");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "named-action");
    assert!(path.exists());
    assert_eq!(
        completions,
        vec![
            (fence(1), 1, 1, Completion::Cancelled),
            (fence(1), 2, 1, Completion::Applied)
        ]
    );
    assert!(drain(&mut session).unwrap().is_empty());
    fs::remove_file(&path).unwrap();
}
#[test]
fn callback_failure_does_not_poison_a_replacement_generation() {
    let binary = Path::new(env!("CARGO_BIN_EXE_prek"));
    let bad=Definition::compile("local p=require('prek');p.events.on('matched',function()while true do end end);return {version=1}","failed.lua").unwrap();
    let mut failed = Session::spawn(binary, fence(1), "seat0", bad).unwrap();
    drain(&mut failed).unwrap();
    failed
        .notify(
            EventKind::Matched,
            serde_json::json!({"fence":fence(1),"event":{}}),
        )
        .unwrap();
    assert!(drain(&mut failed).is_err());
    drop(failed);
    let good=Definition::compile("local p=require('prek');p.events.on('ready',function()p.log.info('new generation')end);return {version=1}","new.lua").unwrap();
    let mut replacement = Session::spawn(binary, fence(2), "seat0", good).unwrap();
    assert_eq!(
        drain(&mut replacement).unwrap()[0].message,
        "new generation"
    );
}

#[test]
fn real_helper_delivers_rule_hooks_with_phase_filtering_disposal_and_generation_retirement() {
    let source = r#"
local p=require('prek');local count=0;local once
once=p.rules.on('watch','begin',function(payload)
 assert(payload.event.id=='18446744073709551615')
 once:dispose();p.log.info('once',{generation=payload.fence.configuration})
end)
for _,phase in ipairs({'discrete','begin','update','end','cancel'}) do
 p.rules.on('watch',phase,function(payload)
  count=count+1;p.log.info('hook',{phase=phase,count=count,generation=payload.fence.configuration})
 end)
end
return p.config.new{rules={
 p.rules.define{id='watch',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='watch'}},
 p.rules.define{id='other',trigger=p.triggers.key{key='KEY_B'},action=p.actions.event{name='other'}},
}}
"#;
    let definition = Definition::compile(source, "scoped-helper.lua").unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_prek"));
    let mut session = Session::spawn(binary, fence(1), "seat0", definition.clone()).unwrap();
    drain(&mut session).unwrap();
    let mut count = 0;
    for (phase, kind) in [
        ("begin", EventKind::Matched),
        ("begin", EventKind::Begin),
        ("update", EventKind::Matched),
        ("update", EventKind::Update),
        ("end", EventKind::Matched),
        ("end", EventKind::End),
        ("discrete", EventKind::Matched),
        ("cancel", EventKind::Matched),
        ("cancel", EventKind::Cancel),
    ] {
        session
            .notify(
                kind,
                serde_json::json!({"fence":fence(1),"event":{
                    "outcome":"matched","rule":"watch","stage":phase,"id":u64::MAX,"ordinal":1
                }}),
            )
            .unwrap();
        let logs = drain(&mut session).unwrap();
        if kind == EventKind::Matched && phase != "cancel" || kind == EventKind::Cancel {
            count += 1;
            let hook = logs.last().unwrap();
            assert_eq!(hook.message, "hook");
            assert_eq!(
                hook.fields,
                serde_json::json!({"phase":phase,"count":count,"generation":"1"})
            );
            assert_eq!(logs.len(), if count == 1 { 2 } else { 1 });
        } else {
            assert!(logs.is_empty());
        }
    }
    session
        .notify(
            EventKind::Matched,
            serde_json::json!({"fence":fence(1),"event":{
                "outcome":"matched","rule":"other","stage":"begin","id":u64::MAX,"ordinal":1
            }}),
        )
        .unwrap();
    assert!(drain(&mut session).unwrap().is_empty());
    session.stop().unwrap();
    drain(&mut session).unwrap();
    drop(session);
    let mut replacement = Session::spawn(binary, fence(2), "seat0", definition).unwrap();
    drain(&mut replacement).unwrap();
    replacement
        .notify(
            EventKind::Matched,
            serde_json::json!({"fence":fence(2),"event":{
                "outcome":"matched","rule":"watch","stage":"begin","id":u64::MAX,"ordinal":1
            }}),
        )
        .unwrap();
    let logs = drain(&mut replacement).unwrap();
    assert_eq!(logs[0].message, "once");
    assert_eq!(
        logs[1].fields,
        serde_json::json!({"phase":"begin","count":1,"generation":"2"})
    );
    replacement.stop().unwrap();
    drain(&mut replacement).unwrap();
}

#[test]
fn supervised_timers_run_during_idle_and_cancel_before_the_next_tick() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
local periodic
periodic=p.scheduler.every('10ms',function(event)
 p.log.info('periodic',{generation=event.context.fence.configuration})
 periodic:cancel()
end)
p.scheduler.after('20ms',function()p.log.info('after')end)
return {version=1}
"#,
        "timer-process.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    while logs.len() < 2 {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        logs.iter()
            .map(|log| log.message.as_str())
            .collect::<Vec<_>>(),
        vec!["periodic", "after"]
    );
    assert_eq!(logs[0].fields["generation"], "1");
    for _ in 0..40 {
        assert!(session.step(monotonic().unwrap()).unwrap().is_empty());
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn replacing_a_vm_retires_its_pending_timer_generation() {
    let binary = Path::new(env!("CARGO_BIN_EXE_prek"));
    let old=Definition::compile("local p=require('prek');p.scheduler.after('100ms',function()p.log.info('old generation')end);return {version=1}","old-timer.lua").unwrap();
    let mut retired = Session::spawn(binary, fence(1), "seat0", old).unwrap();
    drain(&mut retired).unwrap();
    drop(retired);
    let new=Definition::compile("local p=require('prek');p.scheduler.after('100ms',function(event)p.log.info('new generation',{generation=event.context.fence.configuration})end);return {version=1}","new-timer.lua").unwrap();
    let mut replacement = Session::spawn(binary, fence(2), "seat0", new).unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    loop {
        let logs = replacement.step(monotonic().unwrap()).unwrap();
        if !logs.is_empty() {
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].message, "new generation");
            assert_eq!(logs[0].fields["generation"], "2");
            break;
        }
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn supervised_tasks_sleep_cancel_and_keep_current_generation_context() {
    let definition = Definition::compile(r#"
local p=require('prek')
local cancelled=p.scheduler.spawn(function()error('cancelled task ran')end)
cancelled:cancel()
local sleeper=p.scheduler.spawn(function()p.scheduler.sleep('1s');error('cancelled sleeper resumed')end)
p.scheduler.spawn(function(wake)
 p.log.info('task-start',{generation=wake.context.fence.configuration})
 local next=p.scheduler.sleep('10ms')
 p.log.info('task-resumed',{generation=next.context.fence.configuration})
end)
p.scheduler.after('100ms',function()sleeper:cancel();p.log.info(sleeper:status())end)
return {version=1}
"#, "task-process.lua").unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    while logs.len() < 3 {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        assert!(monotonic().unwrap() < end, "task helper did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        logs.iter()
            .map(|log| log.message.as_str())
            .collect::<Vec<_>>(),
        vec!["task-start", "task-resumed", "cancelled"]
    );
    assert_eq!(logs[0].fields["generation"], "1");
    assert_eq!(logs[1].fields["generation"], "1");
}

#[test]
fn replacing_a_vm_retires_its_sleeping_task_generation() {
    let binary = Path::new(env!("CARGO_BIN_EXE_prek"));
    let old = Definition::compile("local p=require('prek');p.scheduler.spawn(function()p.log.info('old-start');p.scheduler.sleep('100ms');p.log.info('old-resumed')end);return {version=1}", "old-task.lua").unwrap();
    let mut retired = Session::spawn(binary, fence(1), "seat0", old).unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    loop {
        let logs = retired.step(monotonic().unwrap()).unwrap();
        if !logs.is_empty() {
            assert_eq!(logs[0].message, "old-start");
            break;
        }
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(retired);
    let new = Definition::compile("local p=require('prek');p.scheduler.spawn(function(wake)p.log.info('new-task',{generation=wake.context.fence.configuration})end);return {version=1}", "new-task.lua").unwrap();
    let mut replacement = Session::spawn(binary, fence(2), "seat0", new).unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    loop {
        let logs = replacement.step(monotonic().unwrap()).unwrap();
        if !logs.is_empty() {
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].message, "new-task");
            assert_eq!(logs[0].fields["generation"], "2");
            break;
        }
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
