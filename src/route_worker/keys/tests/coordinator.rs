use super::*;
use crate::{
    config::Delivery,
    input::{EV_SYN, SYN_REPORT},
    keys::worker::tests::projected,
};

fn packet(events: &[(u16, i32)]) -> Frame {
    let mut frame = frame(1, 1, 10, 30, 1);
    frame.events = events
        .iter()
        .map(|&(code, value)| RawEvent {
            kind: EV_KEY,
            code,
            value,
        })
        .collect();
    frame.events.push(RawEvent {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    });
    frame
}
fn work(routing: &mut Routing, frame: &Frame) -> Vec<KeyWork> {
    routing
        .push(crate::replay::Record::Frame {
            frame: frame.clone(),
        })
        .unwrap();
    routing
        .take_scoped_dispatches(ms(10))
        .into_iter()
        .map(|dispatch| dispatch.into_key_work().ok().unwrap())
        .collect()
}
fn pump(slot: &mut Slot<ModelPreparation>, routing: &mut Routing) {
    for _ in 0..8 {
        slot.poll(routing, ms(10)).unwrap();
    }
}
fn raw(slot: &Slot<ModelPreparation>, routing: &mut Routing) -> Vec<(u16, i32)> {
    let mut output = Vec::new();
    while let Some(scheduled) = routing.due(&source(1), ms(10)).unwrap().cloned() {
        if slot.blocks_input(
            routing,
            &source(1),
            scheduled.frame.sequence,
            routing.input_span(&source(1), scheduled.ticket),
        ) {
            break;
        }
        output.extend(
            scheduled
                .frame
                .events
                .iter()
                .filter(|event| event.kind == EV_KEY)
                .map(|event| (event.code, event.value)),
        );
        routing
            .delivered(&source(1), scheduled.ticket, ms(10))
            .unwrap();
    }
    output
}

#[test]
fn mixed_down_repeat_and_up_are_admitted_before_end_closes_the_binding() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut rule = rule("shift", Action::Remap { key: Key(42) });
    rule.delivery = Delivery::Buffer;
    let mut routing = projected(vec![rule]);
    let input = packet(&[(30, 1), (30, 2), (31, 1), (30, 0), (31, 0)]);
    let work = work(&mut routing, &input);
    assert_eq!(work.len(), 2);
    slot.admit(&routing, work, Some(&input), ms(10)).unwrap();
    pump(&mut slot, &mut routing);
    assert_eq!(keys(&state), [(42, 1), (42, 2)]);
    assert_eq!(raw(&slot, &mut routing), [(31, 1)]);
    pump(&mut slot, &mut routing);
    assert_eq!(keys(&state), [(42, 1), (42, 2), (42, 0)]);
    assert_eq!(raw(&slot, &mut routing), [(31, 0)]);
}

#[test]
fn observed_repeat_waits_for_its_own_raw_edge_before_generated_repeat() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = projected(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let input = packet(&[(30, 1), (30, 2), (31, 1), (30, 0), (31, 0)]);
    let work = work(&mut routing, &input);
    slot.admit(&routing, work, Some(&input), ms(10)).unwrap();
    pump(&mut slot, &mut routing);
    assert!(keys(&state).is_empty());
    assert_eq!(raw(&slot, &mut routing), [(30, 1)]);
    pump(&mut slot, &mut routing);
    assert_eq!(keys(&state), [(42, 1)]);
    assert_eq!(raw(&slot, &mut routing), [(30, 2)]);
    pump(&mut slot, &mut routing);
    assert_eq!(keys(&state), [(42, 1), (42, 2)]);
    assert_eq!(raw(&slot, &mut routing), [(31, 1), (30, 0)]);
    pump(&mut slot, &mut routing);
    assert_eq!(keys(&state), [(42, 1), (42, 2), (42, 0)]);
    assert_eq!(raw(&slot, &mut routing), [(31, 0)]);
}

#[test]
fn changed_repeat_snapshot_is_rejected_before_any_work_is_admitted() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = projected(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let input = packet(&[(30, 1), (30, 2), (30, 0)]);
    let work = work(&mut routing, &input);
    let mut forged = input;
    forged.events[1].code = 31;
    assert!(slot.admit(&routing, work, Some(&forged), ms(10)).is_err());
    let Phase::Active(actor) = &slot.phase else {
        panic!("not active")
    };
    assert!(actor.settled());
    assert!(keys(&state).is_empty());
}
