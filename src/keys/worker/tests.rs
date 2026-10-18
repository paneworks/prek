use super::*;
use crate::{
    config::{Config, Context, Delivery, Key, Rule, Scope, Selector, Trigger},
    context::{Binding as ContextBinding, SessionKind, Snapshot, State as ContextState},
    input::{Device, EV_SYN, RawEvent, SYN_REPORT},
    protocol::{Capability, Epoch},
    replay::Record,
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
};
use std::{cell::RefCell, rc::Rc};

mod bounds;
mod coordinator;
mod independence;
mod lifecycle;
mod producers;
mod scope;

enum Step {
    Prefix(usize),
    Complete,
    Error(io::ErrorKind),
}
#[derive(Default)]
struct Writes {
    frames: Vec<Frame>,
    events: Vec<RawEvent>,
    steps: VecDeque<Step>,
    pending: Option<(Frame, usize)>,
}
struct TestSink {
    source: Source,
    writes: Rc<RefCell<Writes>>,
}
impl Sink for TestSink {
    fn source(&self) -> &Source {
        &self.source
    }
    fn supports_key(&self, code: u16) -> bool {
        (1..768).contains(&code)
    }
    fn enqueue(&mut self, frame: &Frame) -> io::Result<()> {
        let mut writes = self.writes.borrow_mut();
        assert!(writes.pending.is_none());
        assert_eq!(frame.source, self.source);
        writes.frames.push(frame.clone());
        writes.pending = Some((frame.clone(), 0));
        Ok(())
    }
    fn flush(&mut self) -> io::Result<Progress> {
        let mut writes = self.writes.borrow_mut();
        let (frame, offset) = writes.pending.take().unwrap();
        let step = writes.steps.pop_front().unwrap_or(Step::Complete);
        let end = match step {
            Step::Prefix(n) => (offset + n).min(frame.events.len()),
            Step::Complete => frame.events.len(),
            Step::Error(kind) => {
                writes.pending = Some((frame, offset));
                return Err(kind.into());
            }
        };
        writes.events.extend_from_slice(&frame.events[offset..end]);
        if matches!(step, Step::Complete) {
            Ok(Progress::Complete)
        } else {
            writes.pending = Some((frame, end));
            Ok(Progress::Pending)
        }
    }
    fn abort(&mut self) -> io::Result<Vec<RawEvent>> {
        let (frame, offset) = self.writes.borrow_mut().pending.take().unwrap();
        Ok(frame.events[..offset].to_vec())
    }
}
pub(crate) fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
pub(crate) fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    }
}
pub(crate) fn source(id: u64) -> Source {
    Source {
        device: id,
        generation: 1,
        seat: "seat0".into(),
    }
}
fn device(id: u64) -> Device {
    Device {
        source: source(id),
        identity: format!("worker-keys-{id}"),
        name: "Model keyboard".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![30, 31])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    }
}
fn ready(device: &Device) -> crate::transport::Ready {
    let descriptor = Descriptor::mirror(
        device,
        KernelIdentity {
            bus: 3,
            vendor: device.vendor,
            product: device.product,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    let consumer = Consumer::IsolatedReceiver { pid: 1, start: 1 };
    let mut readiness = Readiness::new(descriptor.clone(), consumer.clone(), ms(0)).unwrap();
    readiness.metadata(&descriptor, ms(0)).unwrap();
    readiness
        .consumer(
            &ConsumerEvidence {
                consumer,
                marker: descriptor.marker,
                name: descriptor.name,
                class: descriptor.class,
                seat: device.source.seat.clone(),
                settings: BTreeMap::new(),
                at: ms(0),
            },
            ms(0),
        )
        .unwrap();
    readiness.neutral(true, ms(0)).unwrap();
    readiness.finish(ms(0)).unwrap()
}
pub(crate) fn rule(id: &str, action: Action) -> Rule {
    Rule {
        id: id.into(),
        enabled: true,
        priority: 0,
        devices: vec!["keys".into()],
        seats: vec![],
        contexts: vec![Context::Desktop],
        scope: Scope::Everywhere,
        output: None,
        screen: None,
        keyboard_visible: None,
        require_keys: vec![],
        keys_throughout: false,
        layer: None,
        when_mode: None,
        delivery: Delivery::Observe,
        trigger: Trigger::Key { key: Key(30) },
        action,
    }
}
fn config(rules: Vec<Rule>) -> Config {
    let mut config = Config::preset("minimal").unwrap();
    config.mode = crate::config::Mode::Route;
    config.devices.insert(
        "keys".into(),
        Selector {
            keys: vec![Key(30)],
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("keys".into());
    config.rules = rules;
    config
}
pub(crate) fn context(routing: &mut Routing, at: u64, revision: u64, state: ContextState) {
    if at == 0 {
        routing
            .authority_mut()
            .bind(
                ContextBinding {
                    seat: "seat0".into(),
                    session: "worker-model".into(),
                    uid: 1000,
                    compositor_pid: 42,
                    compositor_start: 1,
                    kind: SessionKind::User,
                },
                true,
                false,
            )
            .unwrap();
    }
    routing
        .authority_mut()
        .update(
            Snapshot {
                generation: 1,
                revision,
                captured_at: ms(at),
                state,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
            ms(at),
        )
        .unwrap();
}
pub(crate) fn routing(rules: Vec<Rule>) -> Routing {
    with_fence(rules, Some(fence()))
}
pub(crate) fn with_fence(rules: Vec<Rule>, fence: Option<Fence>) -> Routing {
    with_order(rules, fence, false)
}
pub(crate) fn projected(rules: Vec<Rule>) -> Routing {
    with_order(rules, Some(fence()), true)
}
fn with_order(rules: Vec<Rule>, fence: Option<Fence>, ordered: bool) -> Routing {
    let mut routing = if let Some(fence) = fence {
        if ordered {
            Routing::with_key_projection(config(rules), fence)
        } else {
            Routing::with_recognition(config(rules), fence)
        }
    } else {
        Routing::new(config(rules))
    }
    .unwrap();
    context(&mut routing, 0, 1, ContextState::Desktop);
    for id in [1, 2] {
        let device = device(id);
        routing
            .attach(device.clone(), &ready(&device), None, ms(0))
            .unwrap();
    }
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(0));
    routing
}
fn model() -> (Seat<TestSink>, Rc<RefCell<Writes>>) {
    let writes = Rc::new(RefCell::new(Writes::default()));
    let output = Output::new(TestSink {
        source: source(100),
        writes: writes.clone(),
    })
    .unwrap();
    (Seat::new(fence(), output).unwrap(), writes)
}
pub(crate) fn frame(id: u64, sequence: u64, at: u64, code: u16, value: i32) -> Frame {
    Frame {
        source: source(id),
        sequence,
        at: ms(at),
        events: vec![
            RawEvent {
                kind: EV_KEY,
                code,
                value,
            },
            RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ],
    }
}
pub(crate) fn push(
    routing: &mut Routing,
    id: u64,
    sequence: u64,
    at: u64,
    value: i32,
) -> Vec<KeyWork> {
    routing
        .push(Record::Frame {
            frame: frame(id, sequence, at, 30, value),
        })
        .unwrap();
    routing
        .take_scoped_dispatches(ms(at))
        .into_iter()
        .map(|dispatch| dispatch.into_key_work().ok().expect("canonical key work"))
        .collect()
}
fn ack(routing: &mut Routing, id: u64, at: u64) {
    while let Some(scheduled) = routing.due(&source(id), ms(at)).unwrap() {
        let ticket = scheduled.ticket;
        routing.delivered(&source(id), ticket, ms(at)).unwrap();
    }
}
fn submit(seat: &mut Seat<TestSink>, work: Vec<KeyWork>, at: u64) -> Vec<Remap> {
    work.into_iter()
        .filter_map(|work| match seat.enqueue(work, ms(at)).unwrap() {
            Submission::Remap(handle) => Some(handle),
            _ => None,
        })
        .collect()
}
fn drain(seat: &mut Seat<TestSink>, routing: &Routing, at: u64) {
    for _ in 0..32 {
        if seat.poll(routing, ms(at)).unwrap() == Progress::Complete {
            assert!(seat.settled());
            return;
        }
    }
    panic!("key worker did not settle");
}
fn key_events(writes: &Rc<RefCell<Writes>>) -> Vec<(u16, i32)> {
    writes
        .borrow()
        .events
        .iter()
        .filter(|event| event.kind == EV_KEY)
        .map(|event| (event.code, event.value))
        .collect()
}
fn cancelled(work: &KeyWork) -> Event {
    let (gesture, _) = identity(work.event()).unwrap();
    Event::Cancelled {
        version: 1,
        at: ms(0),
        source: gesture.source,
        rule: gesture.rule,
        id: gesture.id,
        reason: "model cancellation".into(),
    }
}

#[test]
fn normal_release_queued_behind_a_pending_press_preserves_the_tap() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    assert_eq!(seat.poll(&routing, ms(10)).unwrap(), Progress::Pending);
    writes.borrow_mut().steps.push_back(Step::Prefix(0));
    submit(&mut seat, push(&mut routing, 1, 2, 20, 0), 20);
    ack(&mut routing, 1, 20);
    assert_eq!(seat.poll(&routing, ms(20)).unwrap(), Progress::Pending);
    drain(&mut seat, &routing, 21);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(!seat.down(42));
    assert!(seat.bindings.is_empty());
    assert_eq!(seat.bytes, 0);
}

#[test]
fn explicit_cancel_discards_an_unwritten_press_instead_of_emitting_a_tap() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    let cancellation = cancelled(&work);
    seat.enqueue(work, ms(10)).unwrap();
    ack(&mut routing, 1, 10);
    seat.poll(&routing, ms(10)).unwrap();
    seat.cancel(fence(), &cancellation, ms(11)).unwrap();
    drain(&mut seat, &routing, 11);
    assert!(key_events(&writes).is_empty());
    assert_eq!(seat.bytes, 0);
    assert!(seat.bindings.is_empty());
}
