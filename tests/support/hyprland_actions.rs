//! Exact stock IPC with explicit model session grants, no physical desktop.
use super::*;
use prek::{
    actions::{Executor, HyprlandAdapter},
    config::{Action, WindowAction},
    context::{Binding, SessionKind},
    context_transfer::View,
    hyprland::ContextSampler,
    input::Source,
    ipc::{Channel, monotonic},
    protocol::{Capability, Completion, Epoch, Fence, Mode, Request},
    rules::{Event, Evidence, Stage},
    transport::Consumer,
    worker::ProvidedContext,
};
use std::collections::BTreeSet;

struct Fixture {
    server: Server,
    executor: Executor,
    view: View,
    behavior: Arc<AtomicU64>,
    dispatches: Arc<AtomicU64>,
}
#[test]
fn public_window_factory_uses_only_the_broker_instance_seat_process_and_fence() {
    let fixture = Fixture::new(Mode::Route);
    let mut executor = Executor::new(
        fixture.view.fence,
        "seat0".into(),
        Mode::Route,
        BTreeSet::from([Capability::HyprlandActions]),
    )
    .unwrap();
    assert!(
        executor
            .sync_hyprland_context(&fixture.server.directory, "other", fixture.view.clone())
            .is_err()
    );
    let mut wrong = fixture.view.clone();
    wrong.context.as_mut().unwrap().binding.seat = "seat1".into();
    assert!(
        executor
            .sync_hyprland_context(&fixture.server.directory, "test", wrong)
            .is_err()
    );
    let mut wrong = fixture.view.clone();
    wrong.context.as_mut().unwrap().binding.compositor_start += 1;
    assert!(
        executor
            .sync_hyprland_context(&fixture.server.directory, "test", wrong)
            .is_err()
    );
    let mut wrong = fixture.view.clone();
    wrong.fence.configuration += 1;
    assert!(
        executor
            .sync_hyprland_context(&fixture.server.directory, "test", wrong)
            .is_err()
    );
    executor
        .sync_hyprland_context(&fixture.server.directory, "test", fixture.view.clone())
        .unwrap();
    executor
        .offer(
            fixture.view.fence,
            fixture.event(1, WindowAction::Close),
            monotonic().unwrap(),
        )
        .unwrap();
    let now = monotonic().unwrap();
    executor
        .grant(
            fixture.view.fence,
            1,
            1,
            now + Duration::from_millis(250),
            now,
        )
        .unwrap();
    executor
        .execute_ready_with_lua(monotonic().unwrap(), None)
        .unwrap();
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 1);
}
impl Fixture {
    fn new(mode: Mode) -> Self {
        Self::with_version(mode, "0.54.3")
    }
    fn with_version(mode: Mode, version: &'static str) -> Self {
        Self::with_provider(mode, version, "hyprlang")
    }
    fn with_provider(mode: Mode, version: &'static str, provider: &'static str) -> Self {
        let behavior = Arc::new(AtomicU64::new(0));
        let workspace = Arc::new(AtomicU64::new(3));
        let closed = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(AtomicU64::new(0));
        let (behavior_copy, workspace_copy, closed_copy, dispatch_copy) = (
            behavior.clone(),
            workspace.clone(),
            closed.clone(),
            dispatches.clone(),
        );
        let server = Server::with_version(
            move |request| {
                let behavior = behavior_copy.load(Ordering::Relaxed);
                let workspace = workspace_copy.load(Ordering::Relaxed);
                let window = serde_json::json!({"address":"0x1234","stableId":if behavior==3 {"def"}else{"abc"},
                "mapped":true,"hidden":false,"monitor":1,"workspace":{"id":workspace},"at":[0,0],"size":[300,400]});
                match request {
                b"j/status" => match behavior {
                    6 => serde_json::json!({"configProvider":if provider=="lua" {"hyprlang"} else {"lua"}}).to_string(),
                    7 => "{}".into(),
                    8 => serde_json::json!({"configProvider":false}).to_string(),
                    9 => "invalid json".into(),
                    10 => {thread::sleep(Duration::from_millis(300)); "{}".into()},
                    _ => serde_json::json!({"configProvider":provider}).to_string(),
                },
                b"j/locked" => serde_json::json!({"locked":behavior==4}).to_string(),
                b"j/layers" => "{}".into(),
                b"j/activewindow" => if closed_copy.load(Ordering::Relaxed) || workspace==3 {"{}".into()} else {window.to_string()},
                b"j/clients" => if closed_copy.load(Ordering::Relaxed) {"[]".into()} else {serde_json::json!([window]).to_string()},
                b"j/monitors" => serde_json::json!([{"id":1,"name":"panel","disabled":false,
                    "width":if behavior==5{1200}else{1080},"height":1920,"scale":2.0,"x":0,"y":0,"transform":0,
                    "activeWorkspace":{"id":workspace},"dpmsStatus":true}]).to_string(),
                b"/dispatch closewindow address:0x1234"|b"/dispatch movetoworkspace 2,address:0x1234"|b"/dispatch movetoworkspace 4,address:0x1234" => {
                    assert_eq!(provider,"hyprlang");
                    dispatch_copy.fetch_add(1,Ordering::Relaxed);
                    if behavior!=1 {
                        match request {
                            b"/dispatch closewindow address:0x1234"=>closed_copy.store(true,Ordering::Relaxed),
                            b"/dispatch movetoworkspace 2,address:0x1234"=>workspace_copy.store(2,Ordering::Relaxed),
                            _=>workspace_copy.store(4,Ordering::Relaxed),
                        }
                    }
                    if behavior==2 {thread::sleep(Duration::from_millis(300));}
                    "ok".into()
                },
                request if request.starts_with(b"/dispatch (function() ") => {
                    assert_eq!(provider,"lua");
                    let text=std::str::from_utf8(request).unwrap();
                    assert!(text.contains("w.stable_id==0xabc"));
                    assert!(text.contains("hl.get_window(\"address:0x1234\")"));
                    assert!(text.contains("w.workspace.id==3"));
                    assert!(text.contains("not m.active_special_workspace"));
                    assert!(text.contains("m.scale==2.0 and m.transform==0"));
                    assert!(text.contains("p.x==0.0 and p.y==0.0 and s.x==300.0 and s.y==400.0"));
                    dispatch_copy.fetch_add(1,Ordering::Relaxed);
                    if behavior!=1 {
                        if text.contains("hl.dsp.window.close({window=w})") {
                            closed_copy.store(true,Ordering::Relaxed);
                        } else {
                            assert!(text.contains("follow=true"));
                            let workspace=if text.contains("workspace=\"2\"") {2} else {assert!(text.contains("workspace=\"4\""));4};
                            workspace_copy.store(workspace,Ordering::Relaxed);
                        }
                    }
                    if behavior==2 {thread::sleep(Duration::from_millis(300));}
                    "ok".into()
                },
                _=>panic!("unexpected request: {}",String::from_utf8_lossy(request)),
            }
            },
            Duration::ZERO,
            version,
        );
        let client = server.client();
        let Consumer::Hyprland { pid, start, .. } = client.consumer() else {
            panic!()
        };
        let snapshot = ContextSampler::default()
            .sample(&client, 1, SessionKind::User)
            .unwrap();
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        let view = View {
            morf: Vec::new(),
            morf_rendered: Vec::new(),
            fence,
            sampled_at: snapshot.captured_at,
            state: snapshot.state,
            error: None,
            context: Some(Box::new(ProvidedContext {
                layouts: None,
                binding: Binding {
                    seat: "seat0".into(),
                    session: "model".into(),
                    uid: uid(),
                    compositor_pid: pid,
                    compositor_start: start,
                    kind: SessionKind::User,
                },
                snapshot,
            })),
        };
        let adapter = HyprlandAdapter::new(client, view.clone()).unwrap();
        let mut executor = Executor::new(
            fence,
            "seat0".into(),
            mode,
            BTreeSet::from([Capability::HyprlandActions]),
        )
        .unwrap();
        executor.set_hyprland_adapter(adapter).unwrap();
        Self {
            server,
            executor,
            view,
            behavior,
            dispatches,
        }
    }
    fn event(&self, id: u64, action: WindowAction) -> Box<Event> {
        Box::new(Event::Matched {
            version: 1,
            rule: "window".into(),
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
            action: Action::Hyprland { action },
            evidence: Evidence::Key { down: true },
            window: Some("0x1234:abc".into()),
            output: Some("panel".into()),
            workspace: Some(3),
        })
    }
    fn offer(&mut self, action: WindowAction) {
        self.executor
            .offer(self.view.fence, self.event(1, action), monotonic().unwrap())
            .unwrap();
    }
    fn execute(&mut self) -> Vec<Request> {
        self.executor
            .execute_ready_with_lua(monotonic().unwrap(), None)
            .unwrap();
        let (mut channel, mut broker) = Channel::pair().unwrap();
        self.executor.flush(&mut channel).unwrap();
        let mut requests = Vec::new();
        while let Some(packet) = broker.receive::<Request>().unwrap() {
            requests.push(packet.message);
        }
        requests
    }
    fn grant(&mut self) {
        let now = monotonic().unwrap();
        self.executor
            .grant(self.view.fence, 1, 1, now + Duration::from_millis(250), now)
            .unwrap();
    }
    fn completion(&mut self) -> Completion {
        self.execute()
            .into_iter()
            .find_map(|request| match request {
                Request::Complete { result, .. } => Some(result),
                _ => None,
            })
            .unwrap()
    }
}

#[test]
fn frozen_window_close_and_adjacent_moves_need_a_separate_grant_and_do_not_retry() {
    for (version, provider) in [
        ("0.54.3", "hyprlang"),
        ("0.56.2", "hyprlang"),
        ("0.56.2", "lua"),
    ] {
        for action in [
            WindowAction::Close,
            WindowAction::MoveLeftAndFollow,
            WindowAction::MoveRightAndFollow,
        ] {
            let mut fixture = Fixture::with_provider(Mode::Route, version, provider);
            fixture.offer(action);
            assert!(
                !fixture
                    .execute()
                    .iter()
                    .any(|request| matches!(request, Request::Complete { .. }))
            );
            assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
            fixture.grant();
            assert_eq!(fixture.completion(), Completion::Applied);
            assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 1);
            assert!(
                fixture
                    .executor
                    .offer(
                        fixture.view.fence,
                        fixture.event(1, action),
                        monotonic().unwrap()
                    )
                    .is_err()
            );
        }
    }
}

#[test]
fn changed_unknown_or_stalled_config_provider_refuses_window_mutation() {
    for provider in ["hyprlang", "lua"] {
        for behavior in 6..=10 {
            let mut fixture = Fixture::with_provider(Mode::Route, "0.56.2", provider);
            fixture.offer(WindowAction::Close);
            fixture.grant();
            fixture.behavior.store(behavior, Ordering::Relaxed);
            assert_eq!(fixture.completion(), Completion::Unavailable);
            assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
        }
    }
}

#[test]
fn lua_window_effects_keep_lifetime_lock_geometry_and_single_attempt_guards() {
    for behavior in 1..=5 {
        let mut fixture = Fixture::with_provider(Mode::Route, "0.56.2", "lua");
        fixture.offer(WindowAction::Close);
        fixture.grant();
        fixture.behavior.store(behavior, Ordering::Relaxed);
        assert_eq!(
            fixture.completion(),
            if behavior <= 2 {
                Completion::Uncertain
            } else {
                Completion::Unavailable
            }
        );
        assert_eq!(
            fixture.dispatches.load(Ordering::Relaxed),
            u64::from(behavior <= 2)
        );
    }
}

#[test]
fn window_initialization_refuses_unknown_provider_before_context_binding() {
    let fixture = Fixture::new(Mode::Route);
    for reply in [
        r#"{"configProvider":"other"}"#,
        "{}",
        r#"{"configProvider":null}"#,
        "invalid",
    ] {
        let server = Server::with_version(
            move |request| {
                assert_eq!(request, b"j/status");
                reply.into()
            },
            Duration::ZERO,
            "0.56.2",
        );
        let error = HyprlandAdapter::new(server.client(), fixture.view.clone())
            .err()
            .unwrap();
        assert!(!error.to_string().contains("desktop context"));
    }
}

#[test]
fn window_actions_refuse_missing_stale_or_injected_targets_and_unsupported_operations() {
    let mut fixture = Fixture::new(Mode::Route);
    for target in [
        None,
        Some("active"),
        Some("0x1234:def"),
        Some("0x1234:abc;exit"),
    ] {
        let mut event = fixture.event(1, WindowAction::Close);
        if let Event::Matched { window, .. } = event.as_mut() {
            *window = target.map(str::to_owned);
        }
        assert!(
            fixture
                .executor
                .offer(fixture.view.fence, event, monotonic().unwrap())
                .is_err()
        );
    }
    for action in [
        WindowAction::ReorderUp,
        WindowAction::ReorderDown,
        WindowAction::ScrollStack,
    ] {
        assert!(
            fixture
                .executor
                .offer(
                    fixture.view.fence,
                    fixture.event(1, action),
                    monotonic().unwrap()
                )
                .is_err()
        );
    }
    for target in [None, Some("other")] {
        let mut event = fixture.event(1, WindowAction::Close);
        if let Event::Matched { output, .. } = event.as_mut() {
            *output = target.map(str::to_owned);
        }
        assert!(
            fixture
                .executor
                .offer(fixture.view.fence, event, monotonic().unwrap())
                .is_err()
        );
    }
    for target in [None, Some(2), Some(-1)] {
        let mut event = fixture.event(1, WindowAction::Close);
        if let Event::Matched { workspace, .. } = event.as_mut() {
            *workspace = target;
        }
        assert!(
            fixture
                .executor
                .offer(fixture.view.fence, event, monotonic().unwrap())
                .is_err()
        );
    }
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
}

#[test]
fn changed_window_lifetime_lock_or_geometry_prevents_the_mutation() {
    for behavior in [3, 4, 5] {
        let mut fixture = Fixture::new(Mode::Route);
        fixture.offer(WindowAction::Close);
        fixture.grant();
        fixture.behavior.store(behavior, Ordering::Relaxed);
        assert_eq!(fixture.completion(), Completion::Unavailable);
        assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
        if behavior == 4 {
            fixture.behavior.store(0, Ordering::Relaxed);
            assert!(
                !fixture
                    .executor
                    .ready_capabilities(monotonic().unwrap())
                    .contains(&Capability::HyprlandActions)
            );
        }
    }
}

#[test]
fn unconfirmed_or_lost_window_acknowledgements_are_uncertain_with_one_mutation() {
    for behavior in [1, 2] {
        let mut fixture = Fixture::new(Mode::Route);
        fixture.offer(WindowAction::Close);
        fixture.grant();
        fixture.behavior.store(behavior, Ordering::Relaxed);
        let started = Instant::now();
        assert_eq!(fixture.completion(), Completion::Uncertain);
        assert!(started.elapsed() < Duration::from_millis(300));
        assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn window_adapter_binding_mode_generation_and_context_freshness_are_enforced() {
    let mut fixture = Fixture::new(Mode::Observe);
    assert!(
        fixture
            .executor
            .offer(
                fixture.view.fence,
                fixture.event(1, WindowAction::Close),
                monotonic().unwrap()
            )
            .is_err()
    );
    assert!(
        fixture
            .executor
            .ready_capabilities(monotonic().unwrap())
            .is_empty()
    );
    let mut fixture = Fixture::new(Mode::Route);
    let duplicate = HyprlandAdapter::new(fixture.server.client(), fixture.view.clone()).unwrap();
    assert!(fixture.executor.set_hyprland_adapter(duplicate).is_err());
    let mut wrong = fixture.view.clone();
    wrong.context.as_mut().unwrap().binding.compositor_start += 1;
    assert!(HyprlandAdapter::new(fixture.server.client(), wrong).is_err());
    let mut overlay = fixture.view.clone();
    overlay.context.as_mut().unwrap().snapshot.secure_overlay = true;
    assert!(HyprlandAdapter::new(fixture.server.client(), overlay).is_err());
    fixture.offer(WindowAction::Close);
    fixture.grant();
    std::thread::sleep(Duration::from_millis(260));
    assert_eq!(fixture.completion(), Completion::Cancelled);
    assert!(
        !fixture
            .executor
            .ready_capabilities(monotonic().unwrap())
            .contains(&Capability::HyprlandActions)
    );
    let mut fence = fixture.view.fence;
    fence.configuration += 1;
    fixture.executor.transition(fence).unwrap();
    assert!(
        !fixture
            .executor
            .ready_capabilities(monotonic().unwrap())
            .contains(&Capability::HyprlandActions)
    );
}

#[test]
fn unsupported_stock_versions_and_changed_context_bindings_cannot_install_or_recover() {
    let mut fixture = Fixture::new(Mode::Route);
    for version in ["0.54.4", "0.55.4"] {
        let server = Server::with_version(
            |_| panic!("unsupported version must refuse before queries"),
            Duration::ZERO,
            version,
        );
        let error = HyprlandAdapter::new(server.client(), fixture.view.clone())
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
    let other = Server::new("{}", Duration::ZERO);
    assert!(HyprlandAdapter::new(other.client(), fixture.view.clone()).is_err());
    let mut wrong = fixture.view.clone();
    wrong.fence.session += 1;
    assert!(fixture.executor.update_hyprland_context(wrong).is_err());
    assert!(
        !fixture
            .executor
            .ready_capabilities(monotonic().unwrap())
            .contains(&Capability::HyprlandActions)
    );
    assert!(
        fixture
            .executor
            .update_hyprland_context(fixture.view.clone())
            .is_err()
    );
}

#[test]
fn workspace_boundaries_and_multiple_outputs_refuse_moves_without_mutation() {
    for (workspace, action) in [
        (1, WindowAction::MoveLeftAndFollow),
        (i64::MAX, WindowAction::MoveLeftAndFollow),
        (i64::from(i32::MAX), WindowAction::MoveRightAndFollow),
    ] {
        let mut fixture = Fixture::new(Mode::Route);
        let context = fixture.view.context.as_mut().unwrap();
        context.snapshot.windows[0].workspace = workspace;
        context.snapshot.outputs[0].active_workspace = Some(workspace);
        fixture
            .executor
            .update_hyprland_context(fixture.view.clone())
            .unwrap();
        let mut event = fixture.event(1, action);
        if let Event::Matched {
            workspace: target, ..
        } = event.as_mut()
        {
            *target = Some(workspace);
        }
        fixture
            .executor
            .offer(fixture.view.fence, event, monotonic().unwrap())
            .unwrap();
        fixture.grant();
        assert_eq!(fixture.completion(), Completion::Unavailable);
        assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
    }
    let mut fixture = Fixture::new(Mode::Route);
    let snapshot = &mut fixture.view.context.as_mut().unwrap().snapshot;
    let mut other = snapshot.outputs[0].clone();
    other.geometry.name = "other".into();
    other.geometry.origin.x = 600.0;
    snapshot.outputs.push(other);
    fixture
        .executor
        .update_hyprland_context(fixture.view.clone())
        .unwrap();
    fixture.offer(WindowAction::MoveRightAndFollow);
    fixture.grant();
    assert_eq!(fixture.completion(), Completion::Unavailable);
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
}

#[test]
fn complete_lua_plan_is_checked_before_any_observation_or_window_effect() {
    use prek::lua::{Definition, session::Session};
    for actions in [
        "p.actions.event{name='must-not-emit'},p.actions.hyprland{action='scroll_stack'}",
        "p.actions.hyprland{action='close'},p.actions.hyprland{action='move_right_and_follow'}",
    ] {
        let mut fixture = Fixture::new(Mode::Route);
        let source = format!(
            "local p=require('prek');local action=p.actions.define('window',{{requires={{'hyprland_actions','observations'}}}},function()return {{{actions}}}end);return {{version=1,rules={{{{id='window',trigger=p.triggers.key{{key='KEY_A'}},action=action}}}}}}"
        );
        let definition = Definition::compile(&source, "preflight-window.lua").unwrap();
        let action = definition.validate().unwrap().rules[0].action.clone();
        let mut session = Session::spawn(
            Path::new(env!("CARGO_BIN_EXE_prek")),
            fixture.view.fence,
            "seat0",
            definition,
        )
        .unwrap();
        let until = monotonic().unwrap() + Duration::from_secs(3);
        while !session.finished() {
            assert!(session.step(monotonic().unwrap()).unwrap().is_empty());
            assert!(monotonic().unwrap() < until);
            thread::sleep(Duration::from_millis(1));
        }
        fixture.executor = Executor::new(
            fixture.view.fence,
            "seat0".into(),
            Mode::Route,
            BTreeSet::from([
                Capability::HyprlandActions,
                Capability::LuaActions,
                Capability::Observations,
            ]),
        )
        .unwrap();
        fixture
            .executor
            .set_hyprland_adapter(
                HyprlandAdapter::new(fixture.server.client(), fixture.view.clone()).unwrap(),
            )
            .unwrap();
        fixture.executor.lua_ready(true);
        let mut event = fixture.event(1, WindowAction::Close);
        if let Event::Matched { action: target, .. } = event.as_mut() {
            *target = action;
        }
        fixture
            .executor
            .offer(fixture.view.fence, event, monotonic().unwrap())
            .unwrap();
        fixture.grant();
        let (mut channel, mut broker) = Channel::pair().unwrap();
        let until = monotonic().unwrap() + Duration::from_secs(3);
        let result = loop {
            fixture
                .executor
                .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
                .unwrap();
            session.step(monotonic().unwrap()).unwrap();
            for (id, ordinal, result) in session.take_actions() {
                fixture
                    .executor
                    .resolve_action(id, ordinal, result, monotonic().unwrap())
                    .unwrap();
            }
            fixture.executor.flush(&mut channel).unwrap();
            let mut completed = None;
            while let Some(packet) = broker.receive::<Request>().unwrap() {
                if let Request::Complete { result, .. } = packet.message {
                    completed = Some(result);
                }
            }
            if let Some(result) = completed {
                break result;
            }
            assert!(monotonic().unwrap() < until);
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(result, Completion::Unavailable);
        assert!(fixture.executor.take_events().is_empty());
        assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
    }
}
