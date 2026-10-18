use super::*;
use crate::{
    config::{Action, Key},
    input::{EV_KEY, RawEvent},
    keys::worker::tests::{fence, frame, ms, push, routing, rule, source},
    protocol::Epoch,
};
use std::{cell::RefCell, collections::BTreeMap, os::unix::net::UnixStream, rc::Rc};

mod coordinator;
mod lifecycle;

#[derive(Default)]
struct State {
    ready: bool,
    blocked: bool,
    prefix: usize,
    observed: usize,
    activations: usize,
    dropped: usize,
    frames: Vec<Frame>,
    events: Vec<RawEvent>,
    pending: Option<(Frame, usize)>,
}
struct ModelSink {
    source: Source,
    state: Rc<RefCell<State>>,
    fd: UnixStream,
}
impl AsFd for ModelSink {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
impl Sink for ModelSink {
    fn source(&self) -> &Source {
        &self.source
    }
    fn supports_key(&self, code: u16) -> bool {
        (1..=0xff).contains(&code)
    }
    fn enqueue(&mut self, frame: &Frame) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        assert!(state.pending.is_none());
        assert_eq!(frame.source, self.source);
        state.frames.push(frame.clone());
        state.pending = Some((frame.clone(), 0));
        Ok(())
    }
    fn flush(&mut self) -> io::Result<Progress> {
        let mut state = self.state.borrow_mut();
        let (frame, offset) = state.pending.take().unwrap();
        if state.blocked {
            let end = (offset + state.prefix).min(frame.events.len());
            state.events.extend_from_slice(&frame.events[offset..end]);
            state.pending = Some((frame, end));
            Ok(Progress::Pending)
        } else {
            state.events.extend_from_slice(&frame.events[offset..]);
            Ok(Progress::Complete)
        }
    }
    fn abort(&mut self) -> io::Result<Vec<RawEvent>> {
        let (frame, offset) = self.state.borrow_mut().pending.take().unwrap();
        Ok(frame.events[..offset].to_vec())
    }
}
impl Drop for ModelSink {
    fn drop(&mut self) {
        self.state.borrow_mut().dropped += 1;
    }
}
struct ModelPreparation {
    descriptor: Descriptor,
    output: Option<Output<ModelSink>>,
    state: Rc<RefCell<State>>,
}
impl Preparation for ModelPreparation {
    type Sink = ModelSink;
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    fn deadline(&self) -> Duration {
        ms(5000)
    }
    fn observe(&mut self, _evidence: &ConsumerEvidence, _at: Duration) -> io::Result<()> {
        self.state.borrow_mut().observed += 1;
        Ok(())
    }
    fn activate(&mut self, _at: Duration) -> io::Result<Option<Output<ModelSink>>> {
        let mut state = self.state.borrow_mut();
        state.activations += 1;
        Ok(if state.ready {
            self.output.take()
        } else {
            None
        })
    }
}
fn model() -> (Slot<ModelPreparation>, Rc<RefCell<State>>) {
    let mut source = source(0);
    source.generation = 17;
    let descriptor = Descriptor::generated_keyboard(source.clone(), BTreeMap::new())
        .unwrap()
        .for_isolated()
        .unwrap()
        .bind_publication(Epoch([3, 4]))
        .unwrap();
    let state = Rc::new(RefCell::new(State::default()));
    let (fd, _peer) = UnixStream::pair().unwrap();
    let output = Output::new(ModelSink {
        source,
        state: state.clone(),
        fd,
    })
    .unwrap();
    let prepared = ModelPreparation {
        descriptor,
        output: Some(output),
        state: state.clone(),
    };
    (
        Slot::new(
            fence(),
            Consumer::IsolatedReceiver { pid: 1, start: 1 },
            prepared,
        )
        .unwrap(),
        state,
    )
}
fn evidence(slot: &Slot<ModelPreparation>, at: u64) -> ConsumerEvidence {
    ConsumerEvidence {
        consumer: slot.consumer.clone(),
        marker: slot.descriptor.marker.clone(),
        name: slot.descriptor.name.clone(),
        class: slot.descriptor.class,
        seat: slot.source().seat.clone(),
        settings: slot.descriptor.consumer_settings.clone(),
        at: ms(at),
    }
}
fn observe(slot: &mut Slot<ModelPreparation>, at: u64) {
    let evidence = evidence(slot, at);
    let source = slot.source().clone();
    let marker = slot.descriptor.marker.clone();
    assert!(
        slot.observe(fence(), &source, &marker, &evidence, ms(at))
            .unwrap()
    );
}
fn activate(slot: &mut Slot<ModelPreparation>, state: &Rc<RefCell<State>>, at: u64) {
    observe(slot, at);
    state.borrow_mut().ready = true;
    slot.advance(ms(at)).unwrap();
    assert!(slot.available(ms(at)));
}
fn ack(routing: &mut Routing, at: u64) {
    while let Some(scheduled) = routing.due(&source(1), ms(at)).unwrap() {
        let ticket = scheduled.ticket;
        routing.delivered(&source(1), ticket, ms(at)).unwrap();
    }
}
fn submit(
    slot: &mut Slot<ModelPreparation>,
    routing: &mut Routing,
    sequence: u64,
    at: u64,
    value: i32,
) {
    for work in push(routing, 1, sequence, at, value) {
        slot.enqueue(work, ms(at)).unwrap();
    }
    ack(routing, at);
}
fn drain(slot: &mut Slot<ModelPreparation>, routing: &mut Routing, at: u64) {
    for _ in 0..32 {
        slot.poll(routing, ms(at)).unwrap();
        if let Phase::Active(actor) | Phase::Closing(actor) = &slot.phase
            && actor.settled()
        {
            return;
        }
        if slot.closed() {
            return;
        }
    }
    panic!("slot did not settle");
}
fn keys(state: &Rc<RefCell<State>>) -> Vec<(u16, i32)> {
    state
        .borrow()
        .events
        .iter()
        .filter(|event| event.kind == EV_KEY)
        .map(|event| (event.code, event.value))
        .collect()
}

#[test]
fn preparation_requires_an_exact_live_consumer_before_execution() {
    let (mut slot, state) = model();
    slot.advance(ms(0)).unwrap();
    assert!(!slot.available(ms(0)));
    assert!(slot.poll_fd().is_none());
    assert!(state.borrow().frames.is_empty());
    observe(&mut slot, 10);
    assert!(!slot.available(ms(10)));
    state.borrow_mut().ready = true;
    slot.advance(ms(10)).unwrap();
    assert!(slot.available(ms(10)));
    assert_eq!(state.borrow().observed, 1);
    assert!(state.borrow().frames.is_empty());
}

#[test]
fn wrong_fence_source_or_publication_cannot_update_the_slot() {
    let (mut slot, state) = model();
    let evidence = evidence(&slot, 0);
    let source = slot.source().clone();
    let marker = slot.descriptor.marker.clone();
    let mut wrong = fence();
    wrong.session += 1;
    assert!(
        !slot
            .observe(wrong, &source, &marker, &evidence, ms(0))
            .unwrap()
    );
    let mut wrong_source = source.clone();
    wrong_source.generation += 1;
    assert!(
        !slot
            .observe(fence(), &wrong_source, &marker, &evidence, ms(0))
            .unwrap()
    );
    assert!(
        !slot
            .observe(fence(), &source, &format!("{marker}0"), &evidence, ms(0))
            .unwrap()
    );
    assert_eq!(state.borrow().observed, 0);
    assert!(slot.evidence.is_none());
}

#[test]
fn every_consumer_identity_field_and_sample_time_is_revalidated_after_activation() {
    for field in 0..8 {
        let (mut slot, state) = model();
        activate(&mut slot, &state, 0);
        let mut wrong = evidence(&slot, 10);
        match field {
            0 => wrong.consumer = Consumer::IsolatedReceiver { pid: 2, start: 1 },
            1 => wrong.marker.push('0'),
            2 => wrong.name.push('x'),
            3 => wrong.class = crate::transport::Class::Touchscreen,
            4 => wrong.seat = "seat1".into(),
            5 => {
                wrong.settings.insert("layout".into(), "other".into());
            }
            6 => wrong.at = ms(11),
            _ => wrong.at = ms(0),
        }
        let source = slot.source().clone();
        let marker = slot.descriptor.marker.clone();
        let at = if field == 7 { ms(251) } else { ms(10) };
        assert!(
            slot.observe(fence(), &source, &marker, &wrong, at).is_err(),
            "field{field}"
        );
        assert!(!slot.available(at));
        assert!(slot.retiring());
    }
}

#[test]
fn a_slot_requires_its_published_generated_role_and_matching_consumer_isolation() {
    for field in 0..3 {
        let (mut slot, state) = model();
        let Phase::Preparing(mut prepared) = std::mem::replace(&mut slot.phase, Phase::Closed)
        else {
            unreachable!()
        };
        let mut consumer = slot.consumer.clone();
        match field {
            0 => {
                prepared.descriptor =
                    Descriptor::generated_keyboard(slot.source().clone(), BTreeMap::new())
                        .unwrap()
                        .for_isolated()
                        .unwrap()
            }
            1 => {
                consumer = Consumer::Hyprland {
                    pid: 2,
                    start: 1,
                    instance: "model".into(),
                }
            }
            _ => {
                prepared.descriptor = Descriptor::mirror(
                    &crate::transport::tests::touch(),
                    crate::transport::KernelIdentity {
                        bus: 3,
                        vendor: 1,
                        product: 2,
                        version: 1,
                    },
                    None,
                    &BTreeMap::new(),
                    BTreeMap::new(),
                )
                .unwrap()
            }
        }
        assert!(Slot::new(fence(), consumer, *prepared).is_err());
        assert_eq!(state.borrow().dropped, 1);
        assert!(state.borrow().frames.is_empty());
    }
}

#[test]
fn actual_key_work_uses_the_managed_output_and_exposes_its_write_fd() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing =
        crate::keys::worker::tests::projected(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    slot.poll(&mut routing, ms(10)).unwrap();
    assert!(slot.poll_fd().is_some());
    assert!(slot.blocks_raw(&source(1), 2));
    drain(&mut slot, &mut routing, 11);
    assert_eq!(keys(&state), [(42, 1)]);
    assert!(slot.poll_fd().is_none());
    let pulse = frame(1, 2, 20, 30, 2);
    assert!(push(&mut routing, 1, 2, 20, 2).is_empty());
    ack(&mut routing, 20);
    slot.admit(&routing, vec![], Some(&pulse), ms(20)).unwrap();
    drain(&mut slot, &mut routing, 20);
    submit(&mut slot, &mut routing, 3, 30, 0);
    drain(&mut slot, &mut routing, 30);
    assert_eq!(keys(&state), [(42, 1), (42, 2), (42, 0)]);
}

#[test]
fn refreshes_continue_after_activation_and_old_samples_do_not_renew_the_lease() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    observe(&mut slot, 200);
    assert!(slot.available(ms(450)));
    assert!(!slot.available(ms(451)));
    let old = evidence(&slot, 199);
    let source = slot.source().clone();
    let marker = slot.descriptor.marker.clone();
    assert!(
        slot.observe(fence(), &source, &marker, &old, ms(210))
            .is_err()
    );
    assert!(!slot.available(ms(210)));
    assert!(slot.retiring());
}

#[test]
fn offer_retry_is_bounded_and_stops_after_retirement() {
    let (mut slot, _) = model();
    assert!(slot.offer(ms(0)).is_some());
    assert!(slot.offer(ms(99)).is_none());
    assert!(slot.offer(ms(100)).is_some());
    slot.retire(ms(101)).unwrap();
    assert!(slot.offer(ms(300)).is_none());
    assert!(slot.closed());
}
