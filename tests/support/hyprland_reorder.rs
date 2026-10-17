//! Frozen native scopes through the authenticated socket adapter and executor.
use super::*;
use prek::{
    actions::{Executor, HyprlandAdapter},
    config::{Action, WindowAction},
    context::{Binding, SessionKind},
    context_transfer::View,
    hyprland::ContextSampler,
    input::Source,
    ipc::{Channel, monotonic},
    protocol::{Capability, Completion, Epoch, Fence, Mode, ReorderScope, Request},
    rules::{Event, Evidence, Stage},
    transport::Consumer,
    worker::ProvidedContext,
};
use std::{collections::BTreeSet, sync::Mutex};

struct Native {
    order: [usize; 3],
    stable: [usize; 3],
    graph: bool,
    provider: bool,
    mutations: usize,
    lose_reply: bool,
    closed: bool,
}
impl Default for Native {
    fn default() -> Self {
        Self {
            order: [0, 1, 2],
            stable: [1, 2, 3],
            graph: true,
            provider: true,
            mutations: 0,
            lose_reply: false,
            closed: false,
        }
    }
}
impl Native {
    fn slot(&self, index: usize) -> usize {
        self.order.iter().position(|id| *id == index).unwrap()
    }
    fn id(&self, index: usize) -> String {
        format!("0x{:x}:{:x}", index + 1, self.stable[index])
    }
    fn window(&self, index: usize) -> serde_json::Value {
        serde_json::json!({"address":format!("0x{:x}",index+1),
            "stableId":format!("{:x}",self.stable[index]),"mapped":true,"hidden":false,
            "monitor":0,"workspace":{"id":1},"at":[0,20+self.slot(index)*100],"size":[100,80]})
    }
    fn reply(&mut self, request: &[u8]) -> String {
        match request {
            b"j/monitors" => serde_json::json!([{"id":0,"name":"panel","disabled":false,
                "width":1000,"height":800,"scale":1,"x":0,"y":0,"transform":0,
                "activeWorkspace":{"id":1},"dpmsStatus":true}])
            .to_string(),
            b"j/clients" => serde_json::json!(
                (0..3)
                    .filter(|i| !self.closed || *i != 1)
                    .map(|i| self.window(i))
                    .collect::<Vec<_>>()
            )
            .to_string(),
            b"j/activewindow" => self.window(2).to_string(),
            b"j/layers" => "{}".into(),
            b"j/locked" => "{\"locked\":false}".into(),
            b"j/status" => {
                serde_json::json!({"configProvider":if self.provider {"lua"}else{"hyprlang"}})
                    .to_string()
            }
            request if request.starts_with(b"/repl ") => {
                if !self.graph {
                    return "{}".into();
                }
                let windows: Vec<_> = (0..3)
                    .map(|i| {
                        serde_json::json!({
                            "id":self.id(i),"rect":[0,20+self.slot(i)*100,100,80],
                            "hidden":false,"floating":false,"grouped":false,"fullscreen":0,
                            "column":{"index":self.slot(i),"row":0}
                        })
                    })
                    .collect();
                let columns: Vec<_> = self
                    .order
                    .iter()
                    .enumerate()
                    .map(|(slot, i)| {
                        serde_json::json!({
                            "index":slot,"width":0.5,"members":[self.id(*i)]
                        })
                    })
                    .collect();
                serde_json::json!({"schema":1,"workspaces":[{"id":1,"output":"panel","monitor":0,
                    "active":true,"special":false,"layout":"scrolling","windows":windows,"columns":columns}]}).to_string()
            }
            request if request.starts_with(b"/dispatch (function() ") => {
                let text = std::str::from_utf8(request).unwrap();
                assert!(text.contains("hl.get_window(\"address:0x2\")"));
                if text.contains("hl.dsp.window.close({window=w})") {
                    self.closed = true;
                    self.mutations += 1;
                    return "ok".into();
                }
                assert!(text.contains("hl.dsp.window.swap({window=w,target=v})"));
                assert!(!text.contains("hl.dsp.window.focus"));
                assert!(text.ends_with("end)()"));
                let neighbor = if text.contains("local v=native['0x1']") {
                    0
                } else {
                    assert!(text.contains("local v=native['0x3']"));
                    2
                };
                let (source, neighbor) = (self.slot(1), self.slot(neighbor));
                self.order.swap(source, neighbor);
                self.mutations += 1;
                if self.lose_reply {
                    thread::sleep(Duration::from_millis(250));
                }
                "ok".into()
            }
            _ => panic!(
                "unexpected reorder request: {}",
                String::from_utf8_lossy(request)
            ),
        }
    }
}
struct Fixture {
    server: Server,
    native: Arc<Mutex<Native>>,
    sampler: ContextSampler,
    executor: Executor,
    view: View,
}
impl Fixture {
    fn new() -> Self {
        let native = Arc::new(Mutex::new(Native::default()));
        let state = native.clone();
        let server = Server::with_version(
            move |request| state.lock().unwrap().reply(request),
            Duration::ZERO,
            "0.56.2",
        );
        let client = server.client();
        let Consumer::Hyprland { pid, start, .. } = client.consumer() else {
            panic!()
        };
        let mut sampler = ContextSampler::default();
        let (snapshot, layouts) = sampler
            .sample_with_layouts(&client, 1, SessionKind::User)
            .unwrap();
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        let view = View {
            fence,
            sampled_at: snapshot.captured_at,
            state: snapshot.state,
            error: None,
            morf: vec![],
            morf_rendered: vec![],
            context: Some(Box::new(ProvidedContext {
                binding: Binding {
                    seat: "seat0".into(),
                    session: "model".into(),
                    uid: uid(),
                    compositor_pid: pid,
                    compositor_start: start,
                    kind: SessionKind::User,
                },
                snapshot,
                layouts,
            })),
        };
        let mut executor = Executor::new(
            fence,
            "seat0".into(),
            Mode::Route,
            BTreeSet::from([Capability::HyprlandActions]),
        )
        .unwrap();
        executor
            .set_hyprland_adapter(HyprlandAdapter::new(client, view.clone()).unwrap())
            .unwrap();
        Self {
            server,
            native,
            sampler,
            executor,
            view,
        }
    }
    fn scope(&self) -> ReorderScope {
        self.view
            .context
            .as_ref()
            .unwrap()
            .reorder_scope("0x2:2")
            .unwrap()
    }
    fn event(&self, action: WindowAction) -> Box<Event> {
        Box::new(Event::Matched {
            version: 1,
            rule: "reorder".into(),
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
            action: Action::Hyprland { action },
            evidence: Evidence::Key { down: true },
            window: Some("0x2:2".into()),
            output: Some("panel".into()),
            workspace: Some(1),
        })
    }
    fn offer(&mut self, scope: Option<ReorderScope>, action: WindowAction) {
        self.executor
            .offer_scoped(
                self.view.fence,
                self.event(action),
                scope,
                monotonic().unwrap(),
            )
            .unwrap();
    }
    fn grant(&mut self, scope: Option<ReorderScope>) {
        let now = monotonic().unwrap();
        self.executor
            .grant_scoped(
                self.view.fence,
                1,
                1,
                now + Duration::from_millis(250),
                scope,
                now,
            )
            .unwrap();
    }
    fn refresh(&mut self) {
        let (snapshot, layouts) = self
            .sampler
            .sample_with_layouts(&self.server.client(), 1, SessionKind::User)
            .unwrap();
        self.view.sampled_at = snapshot.captured_at;
        self.view.state = snapshot.state;
        let context = self.view.context.as_mut().unwrap();
        context.snapshot = snapshot;
        context.layouts = layouts;
        self.executor
            .update_hyprland_context(self.view.clone())
            .unwrap();
    }
    fn completion(&mut self) -> Completion {
        self.executor.execute_ready(monotonic().unwrap()).unwrap();
        let (mut channel, mut broker) = Channel::pair().unwrap();
        self.executor.flush(&mut channel).unwrap();
        while let Some(packet) = broker.receive::<Request>().unwrap() {
            if let Request::Complete { result, .. } = packet.message {
                return result;
            }
        }
        panic!("missing native action completion")
    }
}
#[test]
fn exact_claim_and_grant_swap_the_nonfocused_source_with_one_frozen_neighbor() {
    for action in [WindowAction::ReorderUp, WindowAction::ReorderDown] {
        let mut fixture = Fixture::new();
        let scope = fixture.scope();
        assert!(fixture.view.context.as_ref().unwrap().snapshot.windows[2].focused);
        fixture.offer(Some(scope.clone()), action);
        fixture.refresh();
        assert_eq!(fixture.scope(), scope);
        fixture.grant(Some(scope));
        assert_eq!(fixture.completion(), Completion::Applied);
        let state = fixture.native.lock().unwrap();
        assert_eq!(state.mutations, 1);
        assert_eq!(
            state.order,
            if action == WindowAction::ReorderUp {
                [1, 0, 2]
            } else {
                [0, 2, 1]
            }
        );
    }
}
#[test]
fn missing_or_different_claim_and_grant_scopes_never_mutate_the_native_graph() {
    for case in ["no-offer", "wrong-offer", "no-grant", "wrong-grant"] {
        let mut fixture = Fixture::new();
        let scope = fixture.scope();
        let wrong = ReorderScope {
            fingerprint: [0; 32],
        };
        let offered = match case {
            "no-offer" => None,
            "wrong-offer" => Some(wrong.clone()),
            _ => Some(scope.clone()),
        };
        fixture.offer(offered, WindowAction::ReorderUp);
        if case.ends_with("grant") {
            fixture.grant(if case == "no-grant" {
                None
            } else {
                Some(wrong)
            });
        }
        assert_eq!(fixture.completion(), Completion::Unavailable, "{case}");
        assert_eq!(fixture.native.lock().unwrap().mutations, 0);
    }
}
#[test]
fn native_loss_or_neighbor_replacement_cannot_retarget_or_resurrect_an_offer() {
    for case in [
        "graph",
        "graph-restored",
        "neighbor",
        "neighbor-restored",
        "preflight",
        "provider",
    ] {
        let mut fixture = Fixture::new();
        let scope = fixture.scope();
        fixture.offer(Some(scope.clone()), WindowAction::ReorderUp);
        {
            let mut state = fixture.native.lock().unwrap();
            match case {
                "graph" | "graph-restored" => state.graph = false,
                "provider" => state.provider = false,
                _ => state.stable[0] = 9,
            }
        }
        if case != "preflight" && case != "provider" {
            fixture.refresh();
        }
        if case.ends_with("restored") {
            {
                let mut state = fixture.native.lock().unwrap();
                state.graph = true;
                state.stable[0] = 1;
            }
            fixture.refresh();
            assert_ne!(fixture.scope(), scope);
        }
        fixture.grant(Some(scope));
        assert_eq!(fixture.completion(), Completion::Unavailable, "{case}");
        assert_eq!(fixture.native.lock().unwrap().mutations, 0, "{case}");
    }
}
#[test]
fn lost_native_reply_is_uncertain_and_duplicate_grants_do_not_retry() {
    let mut fixture = Fixture::new();
    let scope = fixture.scope();
    fixture.offer(Some(scope.clone()), WindowAction::ReorderUp);
    fixture.grant(Some(scope.clone()));
    fixture.native.lock().unwrap().lose_reply = true;
    assert_eq!(fixture.completion(), Completion::Uncertain);
    let now = monotonic().unwrap();
    assert!(
        fixture
            .executor
            .grant_scoped(
                fixture.view.fence,
                1,
                1,
                now + Duration::from_millis(250),
                Some(scope),
                now
            )
            .is_err()
    );
    fixture
        .executor
        .execute_ready(monotonic().unwrap())
        .unwrap();
    assert_eq!(fixture.native.lock().unwrap().mutations, 1);
}

#[test]
fn delayed_claim_cannot_cross_native_loss_restoration_or_a_freshness_gap() {
    for case in ["graph", "neighbor", "provider", "freshness"] {
        let mut fixture = Fixture::new();
        let old = fixture.scope();
        if case == "freshness" {
            thread::sleep(prek::context::SNAPSHOT_MAX_AGE + Duration::from_millis(2));
        } else {
            {
                let mut state = fixture.native.lock().unwrap();
                match case {
                    "graph" => state.graph = false,
                    "provider" => state.provider = false,
                    _ => state.stable[0] = 9,
                }
            }
            fixture.refresh();
            {
                let mut state = fixture.native.lock().unwrap();
                state.graph = true;
                state.provider = true;
                state.stable[0] = 1;
            }
        }
        fixture.refresh();
        assert_ne!(
            fixture.scope(),
            old,
            "{case} reused a retired sampled incarnation"
        );
        fixture.offer(Some(old), WindowAction::ReorderUp);
        assert_eq!(fixture.completion(), Completion::Unavailable, "{case}");
        assert_eq!(fixture.native.lock().unwrap().mutations, 0);
    }
}

#[test]
fn named_handlers_keep_close_but_validate_the_entire_plan_before_retired_reorder() {
    use prek::lua::{Definition, session::Session};
    for actions in [
        "p.actions.hyprland{action='close'}",
        "p.actions.hyprland{action='reorder_up'}",
        "p.actions.event{name='must-not-emit'},p.actions.hyprland{action='reorder_up'}",
    ] {
        let close = actions.contains("action='close'");
        let mut fixture = Fixture::new();
        let definition=Definition::compile(&format!(
            "local p=require('prek');local a=p.actions.define('window',{{requires={{'hyprland_actions','observations'}}}},function()return {{{actions}}}end);return {{version=1,rules={{{{id='reorder',trigger=p.triggers.key{{key='KEY_A'}},action=a}}}}}}"
        ),"retired-reorder.lua").unwrap();
        let action = definition.validate().unwrap().rules[0].action.clone();
        let mut helper = Session::spawn(
            Path::new(env!("CARGO_BIN_EXE_prek")),
            fixture.view.fence,
            "seat0",
            definition,
        )
        .unwrap();
        let until = monotonic().unwrap() + Duration::from_secs(3);
        while !helper.finished() {
            assert!(helper.step(monotonic().unwrap()).unwrap().is_empty());
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
        let scope = fixture.scope();
        let mut event = fixture.event(WindowAction::ReorderUp);
        if let Event::Matched {
            action: offered, ..
        } = event.as_mut()
        {
            *offered = action;
        }
        fixture
            .executor
            .offer_scoped(
                fixture.view.fence,
                event,
                Some(scope.clone()),
                monotonic().unwrap(),
            )
            .unwrap();
        fixture.native.lock().unwrap().graph = false;
        fixture.refresh();
        fixture.native.lock().unwrap().graph = true;
        fixture.refresh();
        fixture.grant(Some(scope));
        let (mut channel, mut broker) = Channel::pair().unwrap();
        let result = loop {
            fixture
                .executor
                .execute_ready_with_lua(monotonic().unwrap(), Some(&mut helper))
                .unwrap();
            helper.step(monotonic().unwrap()).unwrap();
            for (id, ordinal, result) in helper.take_actions() {
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
        assert_eq!(
            result,
            if close {
                Completion::Applied
            } else {
                Completion::Unavailable
            }
        );
        assert_eq!(fixture.native.lock().unwrap().mutations, usize::from(close));
        assert!(fixture.executor.take_events().is_empty());
    }
}
