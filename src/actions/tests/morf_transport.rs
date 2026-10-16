//! Agent policy/IPC models. These are not actual root/native acceptance.
use super::*;
use crate::config::{EffectKind, MorfAction};

fn view() -> crate::context_transfer::View {
    let at = monotonic().unwrap();
    let mut view = crate::context_transfer::tests::view(at);
    view.fence = fence();
    let context = view.context.as_mut().unwrap();
    context.binding.uid = unsafe { libc::geteuid() };
    context.snapshot.generation = fence().session;
    view.morf = vec![crate::morf::ShellState {
        version: 1,
        role: "shell".into(),
        epoch: "a".repeat(32),
        revision: 2,
        phase: crate::morf::ShellPhase::Configured,
        compositor_pid: context.binding.compositor_pid,
        compositor_start: context.binding.compositor_start,
        compositor_uid: context.binding.uid,
        output: context.snapshot.outputs[0].geometry.name.clone(),
    }];
    view
}
fn model_agent() -> Executor {
    let mut agent = Executor::new(
        fence(),
        "seat0".into(),
        Mode::Route,
        BTreeSet::from([
            Capability::MorfActions,
            Capability::Context,
            Capability::Commands,
            Capability::LuaActions,
        ]),
    )
    .unwrap();
    agent.sync_morf_context(view()).unwrap();
    agent
}
fn offered(id: u64, action: Action) -> Box<Event> {
    let output = view().context.unwrap().snapshot.outputs[0]
        .geometry
        .name
        .clone();
    Box::new(Event::Matched {
        version: 1,
        rule: "native".into(),
        id,
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
        output: Some(output),
        workspace: None,
    })
}
fn prepare(agent: &mut Executor, id: u64, action: Action) {
    agent
        .offer(fence(), offered(id, action), monotonic().unwrap())
        .unwrap();
    agent
        .grant(
            fence(),
            id,
            1,
            monotonic().unwrap() + Duration::from_millis(200),
            monotonic().unwrap(),
        )
        .unwrap();
}
fn action() -> Action {
    Action::Morf {
        action: MorfAction::KeyboardHide,
    }
}
fn completion(agent: &Executor, id: u64) -> Option<Completion> {
    agent.outgoing.iter().find_map(|request| match request {
        Request::Complete {
            id: found, result, ..
        } if *found == id => Some(*result),
        _ => None,
    })
}

#[test]
fn typed_submission_awaits_a_correlated_root_outcome_and_never_sends_targets() {
    let mut agent = model_agent();
    agent
        .offer(fence(), offered(1, action()), monotonic().unwrap())
        .unwrap();
    agent.execute_ready(monotonic().unwrap()).unwrap();
    assert!(
        !agent
            .outgoing
            .iter()
            .any(|request| matches!(request, Request::MorfAction { .. }))
    );
    agent
        .grant(
            fence(),
            1,
            1,
            monotonic().unwrap() + Duration::from_millis(200),
            monotonic().unwrap(),
        )
        .unwrap();
    agent.execute_ready(monotonic().unwrap()).unwrap();
    assert_eq!(completion(&agent, 1), None);
    let request = agent
        .outgoing
        .iter()
        .find(|request| matches!(request, Request::MorfAction { .. }))
        .unwrap();
    let json = serde_json::to_value(request).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 5);
    assert!(
        json.get("request").is_none()
            && json.get("output").is_none()
            && json.get("expires").is_none()
    );
    let mut injected = json.clone();
    injected["output"] = serde_json::json!("another-output");
    assert!(serde_json::from_value::<Request>(injected).is_err());
    let mut unknown = json.clone();
    unknown["action"] = serde_json::json!("run_shell");
    assert!(serde_json::from_value::<Request>(unknown).is_err());
    let (mut producer, mut consumer) = Channel::pair().unwrap();
    producer
        .send(
            &Reply::MorfResult {
                fence: fence(),
                id: 1,
                ordinal: 1,
                result: Completion::Applied,
            },
            monotonic().unwrap(),
        )
        .unwrap();
    let Reply::MorfResult {
        fence,
        id,
        ordinal,
        result,
    } = consumer.receive::<Reply>().unwrap().unwrap().message
    else {
        unreachable!()
    };
    agent.complete_morf(fence, id, ordinal, result).unwrap();
    assert_eq!(completion(&agent, 1), Some(Completion::Applied));
    agent.complete_morf(fence, id, ordinal, result).unwrap();
    assert_eq!(
        agent
            .outgoing
            .iter()
            .filter(|request| matches!(request, Request::Complete { .. }))
            .count(),
        1
    );
    assert!(agent.complete_morf(fence, 99, 1, result).is_err());
}

#[test]
fn cancellation_and_timeout_preserve_uncertainty_and_send_cleanup_once() {
    let mut agent = model_agent();
    prepare(&mut agent, 1, action());
    prepare(&mut agent, 2, action());
    agent.execute_ready(monotonic().unwrap()).unwrap();
    agent.cancel_action(fence(), 1, 1).unwrap();
    agent.cancel_action(fence(), 1, 1).unwrap();
    assert_eq!(
        agent
            .outgoing
            .iter()
            .filter(|request| matches!(request, Request::MorfCancel { id: 1, .. }))
            .count(),
        1
    );
    agent
        .complete_morf(fence(), 1, 1, Completion::Applied)
        .unwrap();
    assert_eq!(completion(&agent, 1), Some(Completion::Uncertain));
    agent.native.get_mut(&(2, 1)).unwrap().until = Duration::ZERO;
    agent.reap().unwrap();
    assert_eq!(completion(&agent, 2), Some(Completion::Uncertain));
    agent
        .complete_morf(fence(), 2, 1, Completion::Applied)
        .unwrap();
    agent.reap().unwrap();
    assert_eq!(
        agent
            .outgoing
            .iter()
            .filter(|request| matches!(request, Request::MorfCancel { id: 2, .. }))
            .count(),
        1
    );
}

#[test]
fn stale_targets_and_generation_replacement_cannot_issue_or_complete_new_effects() {
    let mut agent = model_agent();
    let mut missing = view();
    missing.state = crate::context::State::Unknown;
    missing.context = None;
    missing.morf.clear();
    agent.sync_morf_context(missing).unwrap();
    assert!(
        !agent
            .ready_capabilities(monotonic().unwrap())
            .contains(&Capability::MorfActions)
    );
    agent.sync_morf_context(view()).unwrap();
    assert!(
        agent
            .ready_capabilities(monotonic().unwrap())
            .contains(&Capability::MorfActions)
    );
    prepare(&mut agent, 1, action());
    let mut changed = view();
    changed.morf[0].epoch = "b".repeat(32);
    agent.sync_morf_context(changed).unwrap();
    agent.execute_ready(monotonic().unwrap()).unwrap();
    assert_eq!(completion(&agent, 1), Some(Completion::Cancelled));
    assert!(agent.native.is_empty());
    let mut agent = model_agent();
    prepare(&mut agent, 1, action());
    agent.execute_ready(monotonic().unwrap()).unwrap();
    agent
        .transition(Fence {
            session: 2,
            ..fence()
        })
        .unwrap();
    assert!(agent.groups.is_empty() && agent.native.is_empty());
    agent
        .complete_morf(fence(), 1, 1, Completion::Applied)
        .unwrap();
    assert!(agent.outgoing.is_empty());
}

#[test]
fn composed_named_plans_join_native_receipts_with_owned_command_children() {
    let mut agent = model_agent();
    let named = Action::Lua {
        handler: "mixed".into(),
        requires: BTreeSet::from([EffectKind::MorfActions, EffectKind::Commands]),
    };
    prepare(&mut agent, 1, named);
    agent.pending.get_mut(&(1, 1)).unwrap().resolving = true;
    agent
        .resolve_action(
            1,
            1,
            Some(crate::lua::ActionResult {
                actions: vec![
                    action(),
                    Action::Command {
                        argv: vec![
                            std::env::current_exe()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                            "--help".into(),
                        ],
                    },
                ],
                logs: vec![],
            }),
            monotonic().unwrap(),
        )
        .unwrap();
    agent.execute_ready(monotonic().unwrap()).unwrap();
    agent
        .complete_morf(fence(), 1, 1, Completion::Applied)
        .unwrap();
    assert_eq!(completion(&agent, 1), None);
    finish(&mut agent);
    assert_eq!(completion(&agent, 1), Some(Completion::Applied));
}
