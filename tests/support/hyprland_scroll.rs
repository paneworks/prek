//! Continuous workspace scrolling through the authenticated adapter and executor.
use super::*;
use prek::{
    actions::{Executor, HyprlandAdapter},
    config::{Action, Direction, WindowAction},
    context::{Binding, SessionKind},
    context_transfer::View,
    gestures::{Phase, Point, Space, TouchGesture},
    hyprland::ContextSampler,
    input::Source,
    ipc::{Channel, monotonic},
    protocol::{Capability, Completion, Epoch, Fence, Mode, ReorderScope, Request},
    rules::{Event, Evidence, Stage},
    transport::Consumer,
    worker::ProvidedContext,
};
use std::{collections::BTreeSet, sync::Mutex};

/// Four half-height strips on an 800 px panel, scrolled down by `offset`.
struct Native {
    offset: f64,
    windows: usize,
    moves: Vec<f64>,
    refuse: bool,
    ignore: bool,
}
impl Native {
    fn top(&self, index: usize) -> i64 {
        (400. * index as f64 - self.offset) as i64
    }
    fn window(&self, index: usize) -> serde_json::Value {
        serde_json::json!({"address":format!("0x{:x}",index+1),
            "stableId":format!("{:x}",index+1),"mapped":true,"hidden":false,
            "monitor":0,"workspace":{"id":1},"at":[0,self.top(index)],"size":[1000,400]})
    }
    fn layouts(&self) -> String {
        let windows: Vec<_> = (0..self.windows)
            .map(|i| {
                serde_json::json!({
                    "id":format!("0x{:x}:{:x}",i+1,i+1),"rect":[0,self.top(i),1000,400],
                    "hidden":false,"floating":false,"grouped":false,"fullscreen":0,
                    "column":{"index":i,"row":0}
                })
            })
            .collect();
        let columns: Vec<_> = (0..self.windows)
            .map(|i| {
                serde_json::json!({"index":i,"width":0.5,"members":[format!("0x{:x}:{:x}",i+1,i+1)]})
            })
            .collect();
        serde_json::json!({"schema":1,"workspaces":[{"id":1,"output":"panel","monitor":0,
            "active":true,"special":false,"layout":"scrolling","windows":windows,"columns":columns}]})
        .to_string()
    }
    fn reply(&mut self, request: &[u8]) -> String {
        if let Some(reply) = super::layouts::profile_reply(request) {
            return reply;
        }
        match request {
            b"j/monitors" => serde_json::json!([{"id":0,"name":"panel","disabled":false,
                "width":1000,"height":800,"scale":1,"x":0,"y":0,"transform":0,
                "activeWorkspace":{"id":1},"dpmsStatus":true,"reserved":[0,0,0,0]}])
            .to_string(),
            b"j/clients" => serde_json::json!(
                (0..self.windows)
                    .map(|i| self.window(i))
                    .collect::<Vec<_>>()
            )
            .to_string(),
            b"j/activewindow" => self.window(0).to_string(),
            b"j/layers" => "{}".into(),
            b"j/animations" => "[[],[]]".into(),
            b"j/getoption animations:enabled" => {
                "{\"option\":\"animations:enabled\",\"int\":1,\"set\":false}".into()
            }
            b"j/locked" => "{\"locked\":false}".into(),
            b"j/status" => "{\"configProvider\":\"lua\"}".into(),
            request if request.starts_with(b"/repl ") => self.layouts(),
            request
                if request
                    .starts_with(b"/dispatch (function() local m=hl.get_active_monitor();") =>
            {
                let text = std::str::from_utf8(request).unwrap();
                assert!(
                    text.contains(
                        "assert(m and m.focused and m.name==\"\\112\\097\\110\\101\\108\""
                    )
                );
                assert!(text.contains("ws.tiled_layout=='scrolling'"));
                assert!(text.ends_with("\") end)()"));
                let first = format!(
                    "address='0x1',stable=0x1,rect={{0.0,{:?},1000.0,400.0}}",
                    self.top(0) as f64
                );
                let members = text.matches("address='0x").count();
                if self.refuse || !text.contains(&first) || members != 2 * self.windows {
                    return "error: Prek scroll geometry changed".into();
                }
                let distance: f64 = text
                    .split("hl.dsp.layout(\"move ")
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                self.moves.push(distance);
                if !self.ignore {
                    self.offset -= distance;
                }
                "ok".into()
            }
            _ => panic!(
                "unexpected scroll request: {}",
                String::from_utf8_lossy(request)
            ),
        }
    }
}
struct Fixture {
    _server: Server,
    native: Arc<Mutex<Native>>,
    executor: Executor,
    view: View,
}
impl Fixture {
    fn new() -> Self {
        let native = Arc::new(Mutex::new(Native {
            offset: 0.,
            windows: 4,
            moves: vec![],
            refuse: false,
            ignore: false,
        }));
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
        let (snapshot, layouts) = ContextSampler::default()
            .sample_with_layouts(&client, 1, SessionKind::User)
            .unwrap();
        assert!(layouts.as_ref().unwrap().profile.is_some());
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
            _server: server,
            native,
            executor,
            view,
        }
    }
    fn scope(&self) -> ReorderScope {
        self.view
            .context
            .as_ref()
            .unwrap()
            .scroll_scope("panel", 1)
            .unwrap()
    }
    fn event(&self, ordinal: u64, stage: Stage, total: f64) -> Box<Event> {
        let phase = match stage {
            Stage::Begin => Phase::Begin,
            Stage::End => Phase::End,
            _ => Phase::Update,
        };
        Box::new(Event::Matched {
            version: 1,
            rule: "G11_left".into(),
            id: 7,
            ordinal,
            source: Source {
                device: 1,
                generation: 1,
                seat: "seat0".into(),
            },
            at: monotonic().unwrap(),
            config_generation: 1,
            session_generation: 1,
            stage,
            action: Action::Hyprland {
                action: WindowAction::ScrollStack,
            },
            evidence: Evidence::Touch {
                space: Space::Logical,
                gesture: TouchGesture::Pan {
                    fingers: 1,
                    phase,
                    direction: Direction::Vertical,
                    total: Point { x: 0., y: total },
                    delta: Point { x: 0., y: 0. },
                    velocity: Point { x: 0., y: 0. },
                    centroid: Point { x: 4., y: 400. },
                    swipe: true,
                },
            },
            window: None,
            output: Some("panel".into()),
            workspace: Some(1),
        })
    }
    fn offer(&mut self, ordinal: u64, stage: Stage, total: f64, scope: Option<ReorderScope>) {
        let event = self.event(ordinal, stage, total);
        self.executor
            .offer_scoped(self.view.fence, event, scope, monotonic().unwrap())
            .unwrap();
    }
    fn step(&mut self, ordinal: u64, stage: Stage, total: f64) -> Completion {
        let scope = self.scope();
        self.offer(ordinal, stage, total, Some(scope.clone()));
        let now = monotonic().unwrap();
        self.executor
            .grant_scoped(
                self.view.fence,
                7,
                ordinal,
                now + Duration::from_millis(250),
                Some(scope),
                now,
            )
            .unwrap();
        self.completion()
    }
    fn completion(&mut self) -> Completion {
        self.executor.execute_ready(monotonic().unwrap()).unwrap();
        let (mut channel, mut broker) = Channel::pair().unwrap();
        self.executor.flush(&mut channel).unwrap();
        let mut result = None;
        while let Some(packet) = broker.receive::<Request>().unwrap() {
            if let Request::Complete {
                result: completed, ..
            } = packet.message
            {
                result = Some(completed);
            }
        }
        result.expect("missing scroll completion")
    }
    fn state(&self) -> (f64, Vec<f64>) {
        let native = self.native.lock().unwrap();
        (native.offset, native.moves.clone())
    }
}

#[test]
fn begin_update_and_end_move_the_native_camera_with_paced_exact_attempts() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.step(1, Stage::Begin, -20.), Completion::Applied);
    assert_eq!(fixture.state(), (20., vec![-20.]));
    assert_eq!(fixture.step(2, Stage::Update, -100.), Completion::Cancelled);
    assert_eq!(fixture.state().1.len(), 1);
    assert_eq!(fixture.step(3, Stage::End, -150.), Completion::Applied);
    assert_eq!(fixture.state(), (150., vec![-20., -130.]));
    assert_eq!(
        fixture.step(4, Stage::Update, -300.),
        Completion::Unavailable
    );
    assert_eq!(fixture.state().1.len(), 2);
}

#[test]
fn travel_beyond_the_content_stops_at_the_native_bound_and_reverses() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.step(1, Stage::Begin, -5000.), Completion::Applied);
    assert_eq!(fixture.state().0, 800.);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(fixture.step(2, Stage::Update, 5000.), Completion::Applied);
    assert_eq!(fixture.state(), (0., vec![-800., 800.]));
}

#[test]
fn a_refused_guard_or_missing_settlement_retires_the_gesture_without_retry() {
    for case in ["refuse", "ignore"] {
        let mut fixture = Fixture::new();
        {
            let mut native = fixture.native.lock().unwrap();
            native.refuse = case == "refuse";
            native.ignore = case == "ignore";
        }
        assert_eq!(
            fixture.step(1, Stage::Begin, -40.),
            Completion::Uncertain,
            "{case}"
        );
        let moves = fixture.state().1.len();
        assert_eq!(moves, usize::from(case == "ignore"), "{case}");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            fixture.step(2, Stage::End, -80.),
            Completion::Unavailable,
            "{case}"
        );
        assert_eq!(fixture.state().1.len(), moves, "{case}");
    }
}

#[test]
fn a_missing_or_foreign_scroll_proof_and_cancellation_never_move_the_camera() {
    let mut fixture = Fixture::new();
    fixture.offer(1, Stage::Begin, -40., None);
    assert_eq!(fixture.completion(), Completion::Unavailable);
    let mut fixture = Fixture::new();
    fixture.offer(
        1,
        Stage::Begin,
        -40.,
        Some(ReorderScope {
            fingerprint: [0; 32],
        }),
    );
    assert_eq!(fixture.completion(), Completion::Unavailable);
    let mut fixture = Fixture::new();
    assert_eq!(fixture.step(1, Stage::Begin, -40.), Completion::Applied);
    fixture
        .executor
        .cancel_action(fixture.view.fence, 7, 1)
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        fixture.step(2, Stage::Update, -90.),
        Completion::Unavailable
    );
    assert_eq!(fixture.state(), (40., vec![-40.]));
}

#[test]
fn a_window_added_mid_gesture_fails_the_native_guard_and_stops_progress() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.step(1, Stage::Begin, -40.), Completion::Applied);
    fixture.native.lock().unwrap().windows = 5;
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(fixture.step(2, Stage::Update, -90.), Completion::Uncertain);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(fixture.step(3, Stage::End, -120.), Completion::Unavailable);
    assert_eq!(fixture.state(), (40., vec![-40.]));
}
