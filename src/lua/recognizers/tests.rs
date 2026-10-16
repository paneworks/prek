#![cfg(target_os = "linux")]
use super::*;
use crate::{
    config::Millis,
    input::{Change, KeyState, Source},
    input_events::Event,
    protocol::{Capability, Epoch, Fence},
    recognition::{Candidate, Cause, Gate, InputKind},
};

fn fence() -> Fence {
    Fence {
        broker: Epoch([1, u64::MAX]),
        configuration: 1,
        session: 1,
    }
}
fn declaration() -> recognition::Definition {
    recognition::Definition {
        name: "hold".into(),
        selectors: BTreeSet::from(["keys".into()]),
        inputs: BTreeSet::from([InputKind::Key]),
        reply_within: Millis(250),
        continuous: false,
    }
}
fn compile(body: &str) -> Definition {
    let mut definition = Definition::compile(&format!(r#"
local p=require('prek'); local calls=0
p.events.on('input', function(ticket) calls=calls+1; {body} end)
p.events.on('ready',function()p.log.info('ordinary')end)
return p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keys'}}}},devices={{keys={{keys={{'KEY_A'}}}}}}}}
"#), "recognizer.lua").unwrap();
    definition.bind_recognizer(1, declaration()).unwrap();
    definition
}
fn candidate(at: Duration) -> Candidate {
    Candidate {
        id: u64::MAX,
        revision: u64::MAX,
        rule: "hold-rule".into(),
        source: Source {
            device: u64::MAX,
            generation: u64::MAX,
            seat: "seat0".into(),
        },
        ownership: None,
        winner: None,
        expires: at + Duration::from_secs(2),
    }
}
fn input(at: Duration, sequence: u64) -> Event {
    Event::Frame {
        source: candidate(at).source,
        at,
        sequence,
        selectors: vec!["keys".into()],
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
fn authorize(runtime: &mut Runtime, at: Duration) {
    let view = serde_json::from_value(serde_json::json!({
        "fence":fence(),"sampled_at":at,"state":"desktop","error":null,
        "context":{"binding":{"seat":"seat0","session":"model","uid":1000,"compositor_pid":123,"compositor_start":456,"kind":"user"},
        "snapshot":{"generation":1,"revision":1,"captured_at":at,"state":"desktop","secure_overlay":false,"regions":[],"windows":[],"outputs":[]}}
    })).unwrap();
    runtime.set_context(view, at).unwrap();
    runtime
        .set_capabilities(
            CapabilityView {
                fence: fence(),
                sampled_at: at,
                negotiated: BTreeSet::from([
                    Capability::InputEvents,
                    Capability::LuaEvents,
                    Capability::Context,
                    Capability::Observations,
                ]),
                ready: BTreeSet::new(),
            },
            at,
        )
        .unwrap();
    runtime.set_clock(at).unwrap();
}

#[test]
fn approved_callback_returns_a_bounded_proposal_and_does_not_receive_ordinary_input() {
    let definition = compile(
        r#"
assert(ticket.candidate=='18446744073709551615' and ticket.revision=='18446744073709551615')
assert(ticket.fence.broker[2]=='18446744073709551615' and ticket.input.source.device=='18446744073709551615')
assert(ticket.input.sequence=='18446744073709551615' and #ticket.input.changes==1)
ticket.id='forged';ticket.candidate='forged';ticket.fence.session='forged'
p.log.info('recognize',{calls=calls})
return {phase='discrete', data={calls=calls}}
"#,
    );
    let encoded = serde_json::to_vec(&definition).unwrap();
    let snapshot: Definition = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(snapshot, definition);
    let mut runtime = snapshot.instantiate().unwrap();
    let at = Duration::from_secs(1);
    authorize(&mut runtime, at);
    assert!(
        runtime
            .dispatch(EventKind::Input, &serde_json::json!({}))
            .unwrap()
            .is_empty()
    );
    let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
    let current = candidate(at);
    let ticket = gate
        .offer("hold", &current, &input(at, u64::MAX), at)
        .unwrap();
    let result = runtime.recognize(&ticket).unwrap();
    assert_eq!(result.logs[0].fields["calls"], 1);
    let proposal = result.proposal.unwrap();
    assert_eq!(proposal.fence, ticket.fence);
    assert_eq!(proposal.ticket, ticket.id);
    assert_eq!(
        gate.validate(proposal, &current, at + Duration::from_millis(10))
            .unwrap()
            .unwrap()
            .data["calls"],
        1
    );
    assert!(
        runtime
            .recognize(&ticket)
            .unwrap_err()
            .0
            .contains("repeated")
    );
    assert!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .is_err()
    );
}

#[test]
fn binding_metadata_requires_approved_input_and_selector_policy_and_preserves_source_parity() {
    let original = compile("return nil");
    let mut bad = original.clone();
    bad.recognizers[0].subscription = 2;
    assert!(bad.validate().is_err());
    let mut bad = original.clone();
    bad.recognizers[0]
        .definition
        .selectors
        .insert("other".into());
    assert!(bad.validate().is_err());
    let mut bad = original.clone();
    bad.recognizers.push(bad.recognizers[0].clone());
    assert!(bad.validate().is_err());
    let mut bad = original.clone();
    bad.registrations[0].active = false;
    assert!(bad.validate().is_err());
    let mut bad = original.clone();
    bad.source = bad.source.replace("'input'", "'matched'");
    assert!(bad.instantiate().is_err());
    let mut bad = original;
    assert!(bad.bind_recognizer(1, declaration()).is_err());
    assert_eq!(bad.recognizers.len(), 1);
}

#[test]
fn timer_callback_observes_time_without_repeating_a_key_transition() {
    let definition = compile(
        r#"
if ticket.cause=='frame' then assert(#ticket.input.changes==1); return nil end
assert(ticket.cause=='timer' and #ticket.input.changes==0 and calls==2)
assert(ticket.requested_at.secs==2 and ticket.input.at.secs==1)
return {phase='discrete',data={calls=calls}}
"#,
    );
    let mut runtime = definition.instantiate().unwrap();
    let at = Duration::from_secs(1);
    authorize(&mut runtime, at);
    let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
    let current = candidate(at);
    let first = gate.offer("hold", &current, &input(at, 1), at).unwrap();
    let proposal = runtime.recognize(&first).unwrap().proposal.unwrap();
    assert!(gate.validate(proposal, &current, at).unwrap().is_none());
    let at = at + Duration::from_secs(1);
    authorize(&mut runtime, at);
    let timer = gate.offer_timer("hold", &current, at).unwrap();
    assert_eq!(timer.cause, Cause::Timer);
    let proposal = runtime.recognize(&timer).unwrap().proposal.unwrap();
    let matched = gate.validate(proposal, &current, at).unwrap().unwrap();
    assert_eq!(matched.data["calls"], 2);
    assert_eq!(matched.observed_at, at);
}

#[test]
fn missing_stale_or_foreign_context_never_enters_the_callback() {
    for condition in ["missing", "expired", "locked", "seat", "capability"] {
        let mut runtime = compile("error('must not enter')").instantiate().unwrap();
        let at = Duration::from_secs(1);
        if condition != "missing" {
            authorize(&mut runtime, at);
        }
        if condition == "expired" {
            runtime.set_clock(at + Duration::from_secs(1)).unwrap();
        }
        if condition == "locked" {
            runtime.host.borrow_mut().context.as_mut().unwrap().state =
                crate::context::State::Locked;
        }
        if condition == "seat" {
            runtime
                .host
                .borrow_mut()
                .context
                .as_mut()
                .unwrap()
                .context
                .as_mut()
                .unwrap()
                .binding
                .seat = "other".into();
        }
        if condition == "capability" {
            runtime.clear_capabilities();
        }
        let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
        let ticket = gate
            .offer("hold", &candidate(at), &input(at, 1), at)
            .unwrap();
        // Even without a clock, the missing-context case must not run Lua.
        if condition == "missing" {
            runtime.set_clock(at).unwrap();
        }
        assert!(
            runtime.recognize(&ticket).unwrap().proposal.is_none(),
            "{condition}"
        );
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "ordinary"
        );
    }
}

#[test]
fn malformed_forged_cyclic_yielding_or_overbudget_callbacks_retire_without_logs_or_retries() {
    for body in [
        "return {phase='discrete',ticket='2'}",
        "return {phase='discrete',action={kind='command'}}",
        "return {phase='discrete',data=string.rep('x',4097)}",
        "local v={};v.self=v;return {phase='discrete',data=v}",
        "local v={};for i=1,16 do v={v,v,v,v} end;return {phase='discrete',data=v}",
        "local v={};for i=1,16 do v={v,v,v,v} end;p.log.info('aliased',v)",
        "return {},{}",
        "coroutine.yield({phase='discrete'})",
        "while true do end",
    ] {
        let mut runtime = compile(&format!("p.log.info('discarded'); {body}"))
            .instantiate()
            .unwrap();
        let at = Duration::from_secs(1);
        authorize(&mut runtime, at);
        let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
        let ticket = gate
            .offer("hold", &candidate(at), &input(at, 1), at)
            .unwrap();
        assert!(runtime.recognize(&ticket).is_err(), "{body}");
        assert!(runtime.host.borrow().logs.is_empty());
        assert!(runtime.recognize(&ticket).is_err());
    }
}

#[test]
fn full_width_ticket_identity_and_expiry_during_callback_remain_native_owned() {
    let mut runtime = compile(
        "assert(ticket.id=='18446744073709551615');ticket.id='1';return {phase='discrete'}",
    )
    .instantiate()
    .unwrap();
    let at = Duration::from_secs(1);
    authorize(&mut runtime, at);
    let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
    let mut ticket = gate
        .offer("hold", &candidate(at), &input(at, 1), at)
        .unwrap();
    // Explicitly modeled delivery identity; Gate never accepts this altered ID.
    ticket.id = u64::MAX;
    assert_eq!(
        runtime.recognize(&ticket).unwrap().proposal.unwrap().ticket,
        u64::MAX
    );

    let mut runtime =
        compile("p.log.info('discarded');__test_advance_clock();return {phase='discrete'}")
            .instantiate()
            .unwrap();
    let clock = runtime.host.clone();
    runtime.lua.enter(|ctx| {
        ctx.globals()
            .set(
                ctx,
                "__test_advance_clock",
                Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                    // Model time passing during execution without a timing-sensitive
                    // sleep that can expire before entry on a busy host.
                    let mut host = clock.borrow_mut();
                    host.now = Some(at + Duration::from_millis(200));
                    host.context_clock = Some(Instant::now());
                    stack.replace(ctx, ());
                    Ok(CallbackReturn::Return)
                }),
            )
            .unwrap();
    });
    authorize(&mut runtime, at);
    ticket.id = 1;
    ticket.deadline = at + Duration::from_millis(150);
    let reason = runtime.recognize(&ticket).unwrap_err().0;
    assert!(reason.contains("deadline expired"), "{reason}");
    assert!(runtime.host.borrow().logs.is_empty());
    assert!(runtime.recognize(&ticket).is_err());
}

#[test]
fn source_cleanup_uses_only_prior_identity_even_after_context_and_capabilities_expire() {
    let mut runtime = compile(
        r#"
if ticket.cause=='cancel' then
 assert(calls==2 and ticket.candidate=='18446744073709551615')
 assert(ticket.source.generation=='18446744073709551615' and ticket.through=='1')
 assert(ticket.input==nil and ticket.id==nil and ticket.reason=='source_lost')
 assert(p.context.state()=='unknown' and p.capabilities.current()==nil)
 assert(held[ticket.candidate]);held[ticket.candidate]=nil
 p.log.info('cleanup');return nil
end
held=held or {};held[ticket.candidate]=true;return nil
"#,
    )
    .instantiate()
    .unwrap();
    let at = Duration::from_secs(1);
    authorize(&mut runtime, at);
    let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
    let current = candidate(at);
    let ticket = gate.offer("hold", &current, &input(at, 1), at).unwrap();
    runtime.recognize(&ticket).unwrap();
    assert_eq!(runtime.recognition_candidates.len(), 1);
    runtime.clear_capabilities();
    runtime.set_clock(at + Duration::from_secs(1)).unwrap();
    gate.retire_source(&current.source);
    let cancellation = gate.take_cancellations().pop().unwrap();
    assert_eq!(
        runtime.cancel_recognition(&cancellation).unwrap()[0].message,
        "cleanup"
    );
    assert!(runtime.recognition_candidates.is_empty());
    assert!(
        runtime
            .cancel_recognition(&cancellation)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "ordinary"
    );
}

#[test]
fn unknown_cleanup_does_not_invoke_lua_and_stale_or_retargeted_cleanup_retires_the_vm() {
    for mismatch in ["fence", "revision", "source", "rule", "through", "unknown"] {
        let mut runtime =
            compile("if ticket.cause=='cancel' then error('must not enter') end; return nil")
                .instantiate()
                .unwrap();
        let at = Duration::from_secs(1);
        authorize(&mut runtime, at);
        let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
        let current = candidate(at);
        let first = gate.offer("hold", &current, &input(at, 1), at).unwrap();
        runtime.recognize(&first).unwrap();
        let last = gate.offer("hold", &current, &input(at, 2), at).unwrap();
        runtime.recognize(&last).unwrap();
        let mut cancellation =
            recognition::Cancellation::from_ticket(&last, recognition::Retirement::Cancelled);
        match mismatch {
            "fence" => cancellation.fence.session += 1,
            "revision" => cancellation.revision -= 1,
            "source" => cancellation.source.generation -= 1,
            "rule" => cancellation.rule = "other".into(),
            "through" => cancellation.through = first.id,
            "unknown" => cancellation.candidate -= 1,
            _ => unreachable!(),
        }
        if mismatch == "unknown" {
            assert!(
                runtime
                    .cancel_recognition(&cancellation)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(runtime.recognition_candidates.len(), 1);
        } else {
            assert!(
                runtime.cancel_recognition(&cancellation).is_err(),
                "{mismatch}"
            );
            assert!(runtime.failed);
            assert!(runtime.recognition_candidates.is_empty());
        }
    }
}

#[test]
fn cleanup_cannot_yield_propose_or_survive_callback_failure() {
    for cleanup in [
        "return {phase='discrete'}",
        "return nil,nil",
        "coroutine.yield()",
        "while true do end",
        "error('cleanup failed')",
    ] {
        let mut runtime = compile(&format!(
            "if ticket.cause=='cancel' then p.log.info('discarded'); {cleanup} end;return nil"
        ))
        .instantiate()
        .unwrap();
        let at = Duration::from_secs(1);
        authorize(&mut runtime, at);
        let mut gate = Gate::new(fence(), vec![declaration()]).unwrap();
        let ticket = gate
            .offer("hold", &candidate(at), &input(at, 1), at)
            .unwrap();
        runtime.recognize(&ticket).unwrap();
        let cancellation =
            recognition::Cancellation::from_ticket(&ticket, recognition::Retirement::ContextLost);
        assert!(
            runtime.cancel_recognition(&cancellation).is_err(),
            "{cleanup}"
        );
        assert!(runtime.host.borrow().logs.is_empty());
        assert!(runtime.cancel_recognition(&cancellation).is_err());
    }
}
