use super::*;

mod arbitration;
mod custom;

fn packet(id: u64, sequence: u64, at: u64, events: &[(u16, i32)]) -> Frame {
    let mut frame = frame(id, sequence, at, 30, 1);
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
fn submit(routing: &mut Routing, frame: Frame) -> Vec<KeyWork> {
    let at = frame.at;
    routing.push(Record::Frame { frame }).unwrap();
    routing
        .take_scoped_dispatches(at)
        .into_iter()
        .map(|dispatch| dispatch.into_key_work().ok().unwrap())
        .collect()
}
fn consuming(id: &str, key: u16, target: u16, priority: i32) -> Rule {
    let mut rule = rule(id, Action::Remap { key: Key(target) });
    rule.trigger = Trigger::Key { key: Key(key) };
    rule.delivery = Delivery::Buffer;
    rule.priority = priority;
    rule
}
fn raw(routing: &mut Routing, seat: &Seat<TestSink>, id: u64, at: u64) -> Vec<(u16, i32)> {
    let mut result = Vec::new();
    while let Some(scheduled) = routing.due(&source(id), ms(at)).unwrap().cloned() {
        let span = routing.input_span(&source(id), scheduled.ticket).unwrap();
        if seat.blocks_ordered_span(routing, &source(id), span) {
            break;
        }
        result.extend(
            scheduled
                .frame
                .events
                .iter()
                .filter(|event| event.kind == EV_KEY)
                .map(|event| (event.code, event.value)),
        );
        routing
            .delivered(&source(id), scheduled.ticket, ms(at))
            .unwrap();
    }
    result
}
fn poll(seat: &mut Seat<TestSink>, routing: &Routing, at: u64) {
    for _ in 0..8 {
        seat.poll(routing, ms(at)).unwrap();
    }
}

#[test]
fn a_held_consumed_key_does_not_swallow_unrelated_typing() {
    let mut routing = projected(vec![consuming("shift", 30, 42, 0)]);
    let (mut seat, writes) = model();
    let down = submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
    assert_eq!(down.len(), 1);
    seat.enqueue(down.into_iter().next().unwrap(), ms(10))
        .unwrap();
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert!(raw(&mut routing, &seat, 1, 10).is_empty());
    assert!(submit(&mut routing, packet(1, 2, 20, &[(31, 1), (31, 0)])).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 20), [(31, 1), (31, 0)]);
    let up = submit(&mut routing, packet(1, 3, 30, &[(30, 0)]))
        .pop()
        .unwrap();
    seat.enqueue(up, ms(30)).unwrap();
    poll(&mut seat, &routing, 30);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn same_report_raw_and_generated_edges_interleave_by_original_position() {
    let mut routing = projected(vec![consuming("shift", 30, 42, 0)]);
    let (mut seat, writes) = model();
    let work = submit(
        &mut routing,
        packet(1, 1, 10, &[(30, 1), (31, 1), (30, 0), (31, 0)]),
    );
    assert_eq!(work.len(), 2);
    for work in work {
        seat.enqueue(work, ms(10)).unwrap();
    }
    assert!(raw(&mut routing, &seat, 1, 10).is_empty());
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 1)]);
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 0)]);
}

#[test]
fn pending_modifier_writes_fence_other_sources_not_only_its_own() {
    let mut routing = projected(vec![consuming("shift", 30, 42, 0)]);
    let (mut seat, writes) = model();
    writes
        .borrow_mut()
        .steps
        .extend([Step::Prefix(1), Step::Complete]);
    seat.enqueue(
        submit(&mut routing, packet(1, 1, 10, &[(30, 1)]))
            .pop()
            .unwrap(),
        ms(10),
    )
    .unwrap();
    submit(&mut routing, packet(2, 1, 10, &[(31, 1), (31, 0)]));
    assert!(raw(&mut routing, &seat, 2, 10).is_empty());
    seat.poll(&routing, ms(10)).unwrap();
    assert!(raw(&mut routing, &seat, 2, 10).is_empty());
    seat.poll(&routing, ms(10)).unwrap();
    assert!(raw(&mut routing, &seat, 2, 10).is_empty());
    seat.poll(&routing, ms(10)).unwrap();
    assert_eq!(raw(&mut routing, &seat, 2, 10), [(31, 1), (31, 0)]);
}

#[test]
fn competing_consumers_choose_one_winner_per_press_not_one_per_keyboard() {
    let mut routing = projected(vec![
        consuming("high", 30, 42, 10),
        consuming("low", 30, 29, 0),
        consuming("other", 31, 56, -10),
    ]);
    let work = submit(
        &mut routing,
        packet(1, 1, 10, &[(30, 1), (31, 1), (30, 0), (31, 0)]),
    );
    assert_eq!(
        work.iter()
            .map(|work| match work.event() {
                Event::Matched { rule, .. } => rule.as_str(),
                _ => panic!("not a match"),
            })
            .collect::<Vec<_>>(),
        ["high", "other", "high", "other"]
    );
    assert!(routing.due(&source(1), ms(10)).unwrap().is_none());
}

#[test]
fn observer_actions_wait_for_their_own_edge_but_not_later_same_report_typing() {
    let mut routing = projected(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    for work in submit(
        &mut routing,
        packet(1, 1, 10, &[(30, 1), (31, 1), (30, 0), (31, 0)]),
    ) {
        seat.enqueue(work, ms(10)).unwrap();
    }
    poll(&mut seat, &routing, 10);
    assert!(key_events(&writes).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(30, 1)]);
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 1), (30, 0)]);
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 0)]);
}

#[test]
fn cancelling_a_held_scope_releases_output_but_keeps_its_physical_up_consumed() {
    let mut rule = consuming("shift", 30, 42, 0);
    rule.require_keys = vec![Key(31)];
    rule.keys_throughout = true;
    let mut routing = projected(vec![rule]);
    let (mut seat, writes) = model();
    for work in submit(&mut routing, packet(1, 1, 10, &[(31, 1), (30, 1)])) {
        seat.enqueue(work, ms(10)).unwrap();
    }
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 1)]);
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert!(submit(&mut routing, packet(1, 2, 20, &[(31, 0), (31, 1)])).is_empty());
    for event in routing.take_events() {
        if matches!(event, Event::Cancelled { .. }) {
            seat.cancel(fence(), &event, ms(20)).unwrap();
        }
    }
    assert!(raw(&mut routing, &seat, 1, 20).is_empty());
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(raw(&mut routing, &seat, 1, 20), [(31, 0), (31, 1)]);
    assert!(submit(&mut routing, packet(1, 3, 30, &[(30, 0), (31, 0)])).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 30), [(31, 0)]);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn a_previously_held_observer_keeps_repeat_before_same_report_release() {
    let mut routing = projected(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    let first = packet(1, 1, 10, &[(30, 1)]);
    let work = submit(&mut routing, first.clone());
    seat.admit(&routing, work, Some(&first), ms(10)).unwrap();
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(30, 1)]);
    poll(&mut seat, &routing, 10);
    let next = packet(1, 2, 20, &[(30, 2), (30, 0)]);
    let work = submit(&mut routing, next.clone());
    seat.admit(&routing, work, Some(&next), ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert_eq!(raw(&mut routing, &seat, 1, 20), [(30, 2)]);
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 2)]);
    assert_eq!(raw(&mut routing, &seat, 1, 20), [(30, 0)]);
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 2), (42, 0)]);
}

fn observed_sequence() -> Rule {
    let mut rule = rule(
        "sequence",
        Action::Keys {
            keys: vec![Key(42)],
        },
    );
    rule.trigger = Trigger::Sequence {
        steps: vec![Trigger::Key { key: Key(30) }, Trigger::Key { key: Key(31) }],
        timeout: crate::config::Millis(100),
    };
    rule
}

#[test]
fn observed_sequences_cut_raw_reports_at_each_original_completing_key() {
    let mut routing = projected(vec![observed_sequence()]);
    let (mut seat, writes) = model();
    let input = packet(
        1,
        1,
        10,
        &[
            (30, 1),
            (30, 0),
            (31, 1),
            (31, 0),
            (30, 1),
            (30, 0),
            (31, 1),
            (31, 0),
        ],
    );
    let work = submit(&mut routing, input.clone());
    assert_eq!(work.len(), 2);
    seat.admit(&routing, work, Some(&input), ms(10)).unwrap();
    poll(&mut seat, &routing, 10);
    assert!(key_events(&writes).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(30, 1), (30, 0), (31, 1)]);
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(
        raw(&mut routing, &seat, 1, 10),
        [(31, 0), (30, 1), (30, 0), (31, 1)]
    );
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0), (42, 1), (42, 0)]);
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 0)]);
    routing.push(Record::Tick { at: ms(40) }).unwrap();
}

#[test]
fn a_cross_source_sequence_cuts_the_finishing_source_not_only_its_admission_owner() {
    let mut routing = projected(vec![observed_sequence()]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1), (30, 0)])).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(30, 1), (30, 0)]);
    let input = packet(2, 1, 20, &[(31, 1), (31, 0), (30, 1), (30, 0)]);
    let work = submit(&mut routing, input.clone());
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].source(), &source(1));
    seat.admit(&routing, work, Some(&input), ms(20)).unwrap();
    assert_eq!(raw(&mut routing, &seat, 2, 20), [(31, 1)]);
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(raw(&mut routing, &seat, 2, 20), [(31, 0), (30, 1), (30, 0)]);
}

#[test]
fn an_observed_chord_cuts_after_its_actual_completing_press_before_releases() {
    let mut rule = rule(
        "chord",
        Action::Keys {
            keys: vec![Key(42)],
        },
    );
    rule.trigger = Trigger::Chord {
        keys: vec![Key(30), Key(31)],
        ordered: false,
        timeout: crate::config::Millis(100),
    };
    let mut routing = projected(vec![rule]);
    let (mut seat, writes) = model();
    let input = packet(1, 1, 10, &[(30, 1), (31, 1), (30, 0), (31, 0)]);
    let work = submit(&mut routing, input.clone());
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, Some(&input), ms(10)).unwrap();
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(30, 1), (31, 1)]);
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(30, 0), (31, 0)]);
}
