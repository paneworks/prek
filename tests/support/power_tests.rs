//! Exact IPC and executor tests with explicit model grants, no real display.
use super::*;
use prek::{
    actions::{Executor, PowerAdapter},
    config::{Action, PowerAction},
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
    _server: Server,
    executor: Executor,
    view: View,
    behavior: Arc<AtomicU64>,
    dispatches: Arc<AtomicU64>,
}
impl Fixture {
    fn new(mode: Mode) -> Self {
        Self::with_version(mode, "0.55.4")
    }
    fn with_version(mode: Mode, version: &'static str) -> Self {
        Self::with_provider(mode, version, "hyprlang")
    }
    fn with_provider(mode: Mode, version: &'static str, provider: &'static str) -> Self {
        let behavior = Arc::new(AtomicU64::new(0));
        let state = Arc::new(AtomicBool::new(true));
        let dispatches = Arc::new(AtomicU64::new(0));
        let (behavior_copy, state_copy, dispatch_copy) =
            (behavior.clone(), state.clone(), dispatches.clone());
        let server = Server::with_version(
            move |request| {
                let behavior = behavior_copy.load(Ordering::Relaxed);
                match request {
                b"j/status"=>if behavior==7 {serde_json::json!({"configProvider":if provider=="lua" {"hyprlang"} else {"lua"}}).to_string()}
                    else {serde_json::json!({"configProvider":provider}).to_string()},
                b"j/locked"=> {
                    if behavior==6 {thread::sleep(Duration::from_millis(100));}
                    serde_json::json!({"locked":behavior==4}).to_string()
                },
                b"j/clients"=>"[]".into(), b"j/layers"|b"j/activewindow"=>"{}".into(),
                b"j/animations"=>"[[],[]]".into(),
                b"j/getoption animations:enabled"=>"{\"option\":\"animations:enabled\",\"int\":1,\"set\":false}".into(),
                b"j/monitors"=>serde_json::json!([{"id":1,"name":"panel","disabled":false,
                    "width":if behavior==3{1200}else{1080},"height":1920,"scale":2.0,"x":0,"y":0,"transform":0,
                    "activeWorkspace":{"id":1},"dpmsStatus":state_copy.load(Ordering::Relaxed)}]).to_string(),
                b"/dispatch dpms off panel"|b"/dispatch dpms on panel"=> {
                    assert_eq!(provider,"hyprlang");
                    dispatch_copy.fetch_add(1,Ordering::Relaxed);
                    if behavior!=1 {state_copy.store(request==b"/dispatch dpms on panel",Ordering::Relaxed);}
                    if behavior==2 {thread::sleep(Duration::from_millis(300));}
                    "ok".into()
                }
                request if request.starts_with(b"/dispatch (function() ") => {
                    assert_eq!(provider,"lua");
                    let text=std::str::from_utf8(request).unwrap();
                    assert!(text.contains("hl.get_monitor(\"panel\")"));
                    assert!(text.contains("m.name==\"panel\""));
                    assert!(text.contains("monitor=m"));
                    dispatch_copy.fetch_add(1,Ordering::Relaxed);
                    if behavior!=1 {state_copy.store(text.contains("action=\"on\""),Ordering::Relaxed);}
                    if behavior==2 {thread::sleep(Duration::from_millis(300));}
                    "ok".into()
                }
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
        let adapter = PowerAdapter::new(client, view.clone()).unwrap();
        let mut executor = Executor::new(
            fence,
            "seat0".into(),
            mode,
            BTreeSet::from([Capability::PowerActions]),
        )
        .unwrap();
        executor.set_power_adapter(adapter).unwrap();
        Self {
            _server: server,
            executor,
            view,
            behavior,
            dispatches,
        }
    }
    fn event(&self, id: u64) -> Box<Event> {
        Box::new(Event::Matched {
            version: 1,
            rule: "power".into(),
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
            action: Action::Power {
                action: PowerAction::Blank,
            },
            evidence: Evidence::Key { down: true },
            window: None,
            output: Some("panel".into()),
            workspace: None,
        })
    }
    fn offer(&mut self, id: u64) {
        self.executor
            .offer(self.view.fence, self.event(id), monotonic().unwrap())
            .unwrap();
    }
    fn granted(&mut self, id: u64) {
        self.executor
            .grant(
                self.view.fence,
                id,
                1,
                self.view.sampled_at + Duration::from_millis(250),
                monotonic().unwrap(),
            )
            .unwrap();
    }
    fn execute(&mut self) -> Vec<Request> {
        self.executor.execute_ready(monotonic().unwrap()).unwrap();
        let (mut left, mut right) = Channel::pair().unwrap();
        self.executor.flush(&mut left).unwrap();
        let mut requests = Vec::new();
        while let Some(packet) = right.receive::<Request>().unwrap() {
            requests.push(packet.message);
        }
        requests
    }
    fn completion(&mut self) -> Completion {
        self.execute()
            .into_iter()
            .find_map(|request| {
                if let Request::Complete { result, .. } = request {
                    Some(result)
                } else {
                    None
                }
            })
            .unwrap()
    }
}
#[test]
fn frozen_provider_is_checked_before_each_0562_power_effect() {
    for provider in ["hyprlang", "lua"] {
        for changed in [false, true] {
            let mut fixture = Fixture::with_provider(Mode::Route, "0.56.2", provider);
            fixture.offer(1);
            fixture.granted(1);
            if changed {
                fixture.behavior.store(7, Ordering::Relaxed);
            }
            assert_eq!(
                fixture.completion(),
                if changed {
                    Completion::Unavailable
                } else {
                    Completion::Applied
                }
            );
            assert_eq!(
                fixture.dispatches.load(Ordering::Relaxed),
                u64::from(!changed)
            );
        }
    }
}

#[test]
fn lua_power_effects_keep_lock_geometry_and_single_attempt_guards() {
    for behavior in 1..=4 {
        let mut fixture = Fixture::with_provider(Mode::Route, "0.56.2", "lua");
        fixture.offer(1);
        fixture.granted(1);
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
fn power_initialization_refuses_an_unknown_provider_without_dispatching() {
    let fixture = Fixture::new(Mode::Route);
    let server = Server::with_version(
        |request| {
            assert_eq!(request, b"j/status");
            serde_json::json!({"configProvider":"other"}).to_string()
        },
        Duration::ZERO,
        "0.56.2",
    );
    assert!(PowerAdapter::new(server.client(), fixture.view.clone()).is_err());
}

#[test]
fn power_requires_a_route_offer_and_explicit_grant_and_never_retries_a_duplicate() {
    let mut observed = Fixture::new(Mode::Observe);
    assert!(
        observed
            .executor
            .offer(observed.view.fence, observed.event(1), monotonic().unwrap())
            .is_err()
    );
    assert_eq!(observed.dispatches.load(Ordering::Relaxed), 0);
    assert!(observed.execute().iter().any(|request|matches!(request,Request::Availability{capabilities,..} if capabilities.is_empty())));
    let mut fixture = Fixture::new(Mode::Route);
    fixture.offer(1);
    assert!(
        !fixture
            .execute()
            .iter()
            .any(|request| matches!(request, Request::Complete { .. }))
    );
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
    fixture.granted(1);
    assert_eq!(fixture.completion(), Completion::Applied);
    assert!(
        fixture
            .executor
            .offer(fixture.view.fence, fixture.event(1), monotonic().unwrap())
            .is_err()
    );
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 1);
}
#[test]
fn power_rejects_missing_or_unsafe_outputs_and_unimplemented_lock_requests() {
    let mut fixture = Fixture::new(Mode::Route);
    for output in [
        None,
        Some("other".to_owned()),
        Some("panel;dispatch exit".to_owned()),
    ] {
        let mut event = fixture.event(1);
        if let Event::Matched { output: target, .. } = event.as_mut() {
            *target = output;
        }
        assert!(
            fixture
                .executor
                .offer(fixture.view.fence, event, monotonic().unwrap())
                .is_err()
        );
    }
    let mut event = fixture.event(1);
    if let Event::Matched { action, .. } = event.as_mut() {
        *action = Action::Power {
            action: PowerAction::LockAndBlank,
        };
    }
    assert!(
        fixture
            .executor
            .offer(fixture.view.fence, event, monotonic().unwrap())
            .is_err()
    );
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
}
#[test]
fn changed_geometry_or_lock_prevents_the_effect_and_lock_inhibition_latches() {
    for behavior in [3, 4] {
        let mut fixture = Fixture::new(Mode::Route);
        fixture.offer(1);
        fixture.granted(1);
        fixture.behavior.store(behavior, Ordering::Relaxed);
        assert_eq!(fixture.completion(), Completion::Unavailable);
        assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
        if behavior == 4 {
            fixture.behavior.store(0, Ordering::Relaxed);
            assert!(
                fixture
                    .executor
                    .offer(fixture.view.fence, fixture.event(2), monotonic().unwrap())
                    .is_err()
            );
        }
    }
}
#[test]
fn replaced_output_generation_or_invalid_context_retires_old_power_work() {
    let mut fixture = Fixture::new(Mode::Route);
    let replacement = PowerAdapter::new(fixture._server.client(), fixture.view.clone()).unwrap();
    assert!(fixture.executor.set_power_adapter(replacement).is_err());
    fixture.offer(1);
    fixture.granted(1);
    let mut changed = fixture.view.clone();
    changed.context.as_mut().unwrap().snapshot.outputs[0].geometry_generation += 1;
    fixture.executor.update_power_context(changed).unwrap();
    assert_eq!(fixture.completion(), Completion::Unavailable);
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
    let mut invalid = fixture.view.clone();
    invalid.fence.configuration += 1;
    assert!(fixture.executor.update_power_context(invalid).is_err());
    assert!(
        fixture
            .executor
            .offer(fixture.view.fence, fixture.event(2), monotonic().unwrap())
            .is_err()
    );
}
#[test]
fn dispatch_ack_without_verified_state_and_lost_ack_are_uncertain_without_retry() {
    for behavior in [1, 2] {
        let mut fixture = Fixture::new(Mode::Route);
        fixture.offer(1);
        fixture.granted(1);
        fixture.behavior.store(behavior, Ordering::Relaxed);
        let started = Instant::now();
        assert_eq!(fixture.completion(), Completion::Uncertain);
        assert!(started.elapsed() < Duration::from_millis(280));
        assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 1);
        fixture.execute();
        assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 1);
    }
}
#[test]
fn expired_context_withdraws_availability_and_expired_grants_do_not_dispatch() {
    let mut fixture = Fixture::new(Mode::Route);
    fixture.offer(1);
    let now = monotonic().unwrap();
    fixture
        .executor
        .grant(
            fixture.view.fence,
            1,
            1,
            now + Duration::from_millis(1),
            now,
        )
        .unwrap();
    thread::sleep(Duration::from_millis(3));
    assert_eq!(fixture.completion(), Completion::Cancelled);
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
    let mut stale = fixture.view.clone();
    stale.sampled_at = Duration::ZERO;
    stale.context.as_mut().unwrap().snapshot.captured_at = Duration::ZERO;
    fixture.executor.update_power_context(stale).unwrap();
    assert!(fixture.execute().iter().any(|request|matches!(request,Request::Availability{capabilities,..} if !capabilities.contains(&Capability::PowerActions))));
}

#[test]
fn preflight_uses_remaining_grant_time_and_generation_change_retires_the_adapter() {
    let mut fixture = Fixture::new(Mode::Route);
    fixture.offer(1);
    fixture.behavior.store(6, Ordering::Relaxed);
    let now = monotonic().unwrap();
    fixture
        .executor
        .grant(
            fixture.view.fence,
            1,
            1,
            now + Duration::from_millis(20),
            now,
        )
        .unwrap();
    let started = Instant::now();
    assert_eq!(fixture.completion(), Completion::Unavailable);
    assert!(started.elapsed() < Duration::from_millis(120));
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
    let mut fixture = Fixture::new(Mode::Route);
    fixture.offer(1);
    fixture.granted(1);
    fixture
        .executor
        .transition(Fence {
            configuration: 2,
            ..fixture.view.fence
        })
        .unwrap();
    assert!(
        !fixture
            .execute()
            .iter()
            .any(|request| matches!(request, Request::Complete { .. }))
    );
    assert!(
        fixture
            .executor
            .update_power_context(fixture.view.clone())
            .is_err()
    );
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 0);
}
#[test]
fn public_adapter_factory_uses_the_broker_binding_and_rejects_another_instance_or_seat() {
    let fixture = Fixture::new(Mode::Route);
    let mut executor = Executor::new(
        fixture.view.fence,
        "seat0".into(),
        Mode::Route,
        BTreeSet::from([Capability::PowerActions]),
    )
    .unwrap();
    assert!(
        executor
            .sync_power_context(&fixture._server.directory, "other", fixture.view.clone())
            .is_err()
    );
    let mut wrong = fixture.view.clone();
    wrong.context.as_mut().unwrap().binding.seat = "seat1".into();
    assert!(
        executor
            .sync_power_context(&fixture._server.directory, "test", wrong)
            .is_err()
    );
    let mut wrong = fixture.view.clone();
    wrong.context.as_mut().unwrap().binding.compositor_start += 1;
    assert!(
        executor
            .sync_power_context(&fixture._server.directory, "test", wrong)
            .is_err()
    );
    executor
        .sync_power_context(&fixture._server.directory, "test", fixture.view.clone())
        .unwrap();
    executor
        .offer(fixture.view.fence, fixture.event(1), monotonic().unwrap())
        .unwrap();
    executor
        .grant(
            fixture.view.fence,
            1,
            1,
            fixture.view.sampled_at + Duration::from_millis(250),
            monotonic().unwrap(),
        )
        .unwrap();
    executor.execute_ready(monotonic().unwrap()).unwrap();
    assert_eq!(fixture.dispatches.load(Ordering::Relaxed), 1);
}
