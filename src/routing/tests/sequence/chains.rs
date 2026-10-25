//! Configured native sequence admission, raw ownership and dispatch lifetimes.
use super::*;

fn buffered() -> (Routing, Device, Device) {
    setup_profile(
        vec![Trigger::Key { key: Key(30) }, hold(48)],
        Delivery::Buffer,
    )
}
fn tick(routing: &mut Routing, at: u64) {
    routing.push(Record::Tick { at: ms(at) }).unwrap();
}
fn parent(routing: &Routing) -> u64 {
    routing
        .engine
        .route_intents()
        .into_iter()
        .find(|intent| matches!(intent.rule.trigger, Trigger::Sequence { .. }))
        .unwrap()
        .id()
}

#[test]
fn same_source_groups_retain_one_lease_and_dispatch_with_a_sealed_neutral_prefix() {
    let (mut routing, first, _) = buffered();
    key(&mut routing, &first, 1, 0, 30, 1);
    let id = parent(&routing);
    let token = routing.chains[&id][&first.source].clone();
    key(&mut routing, &first, 2, 10, 30, 0);
    assert!(received(&mut routing, &first, 10).is_empty());
    key(&mut routing, &first, 3, 20, 48, 1);
    let next = &routing.chains[&id][&first.source];
    assert_eq!(next.candidate(), token.candidate());
    assert!(next.segment() > token.segment());
    tick(&mut routing, 120);
    assert_eq!(routing.dispatches.len(), 1, "{:?}", routing.take_events());
    assert_eq!(
        routing.dispatches[0].chain.as_ref().unwrap()[0].segments(),
        2
    );
    assert!(received(&mut routing, &first, 120).is_empty());
    key(&mut routing, &first, 4, 130, 48, 0);
    key(&mut routing, &first, 5, 140, 48, 1);
    assert_eq!(
        received(&mut routing, &first, 140)
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [5]
    );
    key(&mut routing, &first, 6, 150, 48, 0);
    assert_eq!(
        received(&mut routing, &first, 150)
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [6]
    );
    let events = routing.take_dispatches(ms(150));
    assert!(matches!(&events[..],[Event::Matched{id:actual,..}] if *actual==id));
    assert!(routing.take_dispatches(ms(150)).is_empty());
}

#[test]
fn buffered_cross_source_sequence_claims_every_bound_prefix_before_dispatch() {
    let (mut routing, first, second) = buffered();
    key(&mut routing, &first, 1, 0, 30, 1);
    let id = parent(&routing);
    key(&mut routing, &first, 2, 10, 30, 0);
    key(&mut routing, &second, 1, 20, 48, 1);
    assert_eq!(routing.chains[&id].len(), 2);
    assert!(received(&mut routing, &first, 20).is_empty());
    assert!(received(&mut routing, &second, 20).is_empty());
    tick(&mut routing, 120);
    assert_eq!(routing.dispatches.len(), 1);
    assert_eq!(routing.dispatches[0].chain.as_ref().unwrap().len(), 2);
    assert_eq!(routing.take_dispatches(ms(120)).len(), 1);
    key(&mut routing, &second, 2, 130, 48, 0);
    assert!(received(&mut routing, &first, 130).is_empty());
    assert!(received(&mut routing, &second, 130).is_empty());
}

#[test]
fn original_buffer_expiry_retires_the_parent_and_later_suffix_input_stays_ordinary() {
    let (mut routing, first, _) = buffered();
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &first, 2, 10, 30, 0);
    tick(&mut routing, 300);
    assert!(routing.engine.route_intents().is_empty());
    assert_eq!(received(&mut routing, &first, 300)[0].sequence, 1);
    assert_eq!(received(&mut routing, &first, 310)[0].sequence, 2);
    key(&mut routing, &first, 3, 320, 48, 1);
    assert_eq!(received(&mut routing, &first, 320)[0].sequence, 3);
    tick(&mut routing, 420);
    assert!(routing.take_dispatches(ms(420)).is_empty());
    assert!(
        routing
            .take_events()
            .iter()
            .any(|event| matches!(event,Event::Cancelled{reason,..}
        if reason=="sequence raw-group ownership changed"))
    );
}

#[test]
fn unrelated_same_source_input_cannot_join_or_renew_the_old_parent_scope() {
    let (mut routing, first, _) = buffered();
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &first, 2, 10, 30, 0);
    key(&mut routing, &first, 3, 50, 30, 1);
    assert!(routing.engine.route_intents().is_empty());
    assert_eq!(received(&mut routing, &first, 50)[0].sequence, 1);
    key(&mut routing, &first, 4, 55, 30, 0);
    assert_eq!(received(&mut routing, &first, 60)[0].sequence, 2);
    assert_eq!(received(&mut routing, &first, 100)[0].sequence, 3);
    assert_eq!(received(&mut routing, &first, 105)[0].sequence, 4);
    key(&mut routing, &first, 5, 110, 48, 1);
    assert_eq!(received(&mut routing, &first, 110)[0].sequence, 5);
    tick(&mut routing, 210);
    assert!(routing.take_dispatches(ms(210)).is_empty());
}

#[test]
fn a_prefix_started_inside_an_already_forwarded_keyboard_group_cannot_be_owned_later() {
    let (mut routing, first, _) = buffered();
    key(&mut routing, &first, 1, 0, 48, 1);
    assert_eq!(received(&mut routing, &first, 0).len(), 1);
    key(&mut routing, &first, 2, 10, 30, 1);
    assert_eq!(received(&mut routing, &first, 10).len(), 1);
    assert!(routing.engine.route_intents().is_empty());
    key(&mut routing, &first, 3, 20, 30, 0);
    assert_eq!(received(&mut routing, &first, 20).len(), 1);
    key(&mut routing, &first, 4, 30, 48, 0);
    assert_eq!(received(&mut routing, &first, 30).len(), 1);
    key(&mut routing, &first, 5, 40, 48, 1);
    assert_eq!(received(&mut routing, &first, 40).len(), 1);
    tick(&mut routing, 140);
    assert!(routing.take_dispatches(ms(140)).is_empty());
}

fn chord() -> Trigger {
    Trigger::Chord {
        keys: vec![Key(30), Key(48)],
        ordered: false,
        timeout: Millis(200),
    }
}
#[test]
fn chord_source_admission_requires_an_actual_required_key_not_any_selected_frame() {
    let (mut routing, first, second) = setup_profile(vec![chord(), hold(48)], Delivery::Reserve);
    key(&mut routing, &first, 1, 0, 30, 1);
    let id = parent(&routing);
    key(&mut routing, &second, 1, 10, 46, 1);
    assert_eq!(received(&mut routing, &second, 10).len(), 1);
    let intent = routing
        .engine
        .route_intents()
        .into_iter()
        .find(|intent| intent.id() == id)
        .unwrap();
    assert!(!intent.sources().contains(&second.source));
    assert_eq!(routing.chains[&id].len(), 1);
    key(&mut routing, &second, 2, 20, 46, 0);
    assert_eq!(received(&mut routing, &second, 20).len(), 1);
    key(&mut routing, &second, 3, 30, 48, 1);
    assert!(received(&mut routing, &second, 30).is_empty());
    tick(&mut routing, 130);
    assert_eq!(routing.take_dispatches(ms(130)).len(), 1);
}

#[test]
fn a_required_chord_key_in_an_ordinary_group_invalidates_all_prefix_ownership() {
    let (mut routing, first, second) = setup_profile(vec![chord(), hold(48)], Delivery::Reserve);
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &second, 1, 10, 46, 1);
    assert_eq!(received(&mut routing, &second, 10).len(), 1);
    key(&mut routing, &second, 2, 20, 48, 1);
    assert_eq!(received(&mut routing, &second, 20).len(), 1);
    assert!(routing.engine.route_intents().is_empty());
    key(&mut routing, &second, 3, 30, 46, 0);
    assert_eq!(received(&mut routing, &second, 30).len(), 1);
    key(&mut routing, &second, 4, 40, 48, 0);
    assert_eq!(received(&mut routing, &second, 40).len(), 1);
    key(&mut routing, &second, 5, 50, 46, 1);
    assert_eq!(received(&mut routing, &second, 50).len(), 1);
    tick(&mut routing, 150);
    assert!(routing.take_dispatches(ms(150)).is_empty());
}

#[test]
fn reused_tap_candidates_admit_each_fresh_group_without_renewing_the_lease() {
    let (mut routing, first, _) = setup_profile(
        vec![
            Trigger::ButtonTaps {
                key: Key(30),
                count: 2,
            },
            hold(48),
        ],
        Delivery::Buffer,
    );
    let mut original_ticket = 0;
    for (sequence, at, code, value) in
        [(1, 0, 30, 1), (2, 5, 30, 0), (3, 15, 30, 1), (4, 20, 30, 0)]
    {
        key(&mut routing, &first, sequence, at, code, value);
        if sequence == 1 {
            original_ticket = routing.routes[&first.source].ticket;
        }
    }
    tick(&mut routing, 40);
    let id = parent(&routing);
    key(&mut routing, &first, 5, 50, 48, 1);
    tick(&mut routing, 150);
    assert_eq!(routing.dispatches.len(), 1, "{:?}", routing.take_events());
    assert_eq!(
        routing.dispatches[0].chain.as_ref().unwrap()[0].segments(),
        3
    );
    assert_eq!(
        routing.chains[&id][&first.source].candidate(),
        original_ticket
    );
    assert_eq!(routing.take_dispatches(ms(150)).len(), 1);
    assert!(received(&mut routing, &first, 150).is_empty());
}

#[test]
fn transport_retirement_cancels_native_parent_and_invalidates_queued_receipts() {
    for completed in [false, true] {
        let (mut routing, first, second) = buffered();
        key(&mut routing, &first, 1, 0, 30, 1);
        key(&mut routing, &first, 2, 10, 30, 0);
        key(&mut routing, &second, 1, 20, 48, 1);
        if completed {
            tick(&mut routing, 120);
            assert_eq!(routing.dispatches.len(), 1);
        }
        routing.recover(&first.source);
        assert!(routing.engine.route_intents().is_empty());
        assert!(routing.take_dispatches(ms(130)).is_empty());
        assert_eq!(routing.take_recoveries().len(), 1);
    }
}

#[test]
fn delayed_frame_admission_cannot_claim_after_the_original_buffer_wall_deadline() {
    let (mut routing, first, _) = setup_profile(
        vec![Trigger::Key { key: Key(30) }, Trigger::Key { key: Key(48) }],
        Delivery::Buffer,
    );
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &first, 2, 10, 30, 0);
    let frame = Frame {
        source: first.source.clone(),
        at: ms(100),
        sequence: 3,
        events: vec![
            RawEvent {
                kind: EV_KEY,
                code: 48,
                value: 1,
            },
            RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ],
    };
    assert!(routing.push_at(Record::Frame { frame }, ms(350)).is_err());
    assert!(routing.engine.route_intents().is_empty());
    assert!(routing.take_dispatches(ms(350)).is_empty());
    assert!(!routing.routes.contains_key(&first.source));
    assert_eq!(routing.take_recoveries().len(), 1);
}

fn touch_key_sequence() -> Routing {
    let mut config = config();
    config.timing.tap_gap = Millis(20);
    config.rules[0].trigger = Trigger::Sequence {
        steps: vec![
            Trigger::TouchTaps {
                fingers: 2,
                count: 1,
            },
            hold(48),
        ],
        timeout: Millis(1000),
    };
    config.rules[0].scope = Scope::Everywhere;
    let mut routing = Routing::new(config).unwrap();
    context(&mut routing, 0);
    let mut device = device();
    device.capabilities.entry(EV_KEY).or_default().push(48);
    device
        .capabilities
        .get_mut(&EV_KEY)
        .unwrap()
        .sort_unstable();
    device.capabilities.get_mut(&EV_KEY).unwrap().dedup();
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.output = Some(output());
    mapping.generation = 2;
    routing
        .attach(device.clone(), &ready(&device), Some(mapping), ms(0))
        .unwrap();
    routing
}

#[test]
fn a_native_match_waits_for_the_latest_join_window_under_the_original_scope_and_expiry() {
    let mut routing = touch_key_sequence();
    down(&mut routing, 2);
    up(&mut routing, 2, 10, 2);
    tick(&mut routing, 30);
    frame(&mut routing, 3, 40, &[(EV_KEY, 48, 1)]);
    tick(&mut routing, 140);
    assert!(routing.take_dispatches(ms(140)).is_empty());
    assert_eq!(
        routing.deferred_custom.len(),
        1,
        "{:?}",
        routing.take_events()
    );
    assert_eq!(routing.deferred_custom[0].expires, Some(ms(300)));
    tick(&mut routing, 160);
    assert!(routing.deferred_custom.is_empty());
    assert_eq!(
        routing.take_dispatches(ms(160)).len(),
        1,
        "{:?}",
        routing.take_events()
    );
    assert!(drain(&mut routing, 160).is_empty());
}

fn continuous_touch() -> Routing {
    let mut config = config();
    config.timing.tap_gap = Millis(20);
    config.rules[0].scope = Scope::Everywhere;
    config.rules[0].trigger = Trigger::Sequence {
        steps: vec![
            Trigger::TouchTaps {
                fingers: 2,
                count: 1,
            },
            Trigger::Swipe {
                fingers: 2,
                direction: Direction::Down,
                travel: Distance::Millimetres(12.0),
                fallback_travel: Distance::Normalized(0.04),
                continuous: true,
            },
        ],
        timeout: Millis(1000),
    };
    let mut routing = super::super::routing(config, true);
    down(&mut routing, 2);
    up(&mut routing, 2, 10, 2);
    tick(&mut routing, 30);
    frame(
        &mut routing,
        3,
        40,
        &[
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 30),
            (EV_ABS, ABS_MT_POSITION_X, 1000),
            (EV_ABS, ABS_MT_POSITION_Y, 1000),
            (EV_ABS, ABS_MT_SLOT, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, 31),
            (EV_ABS, ABS_MT_POSITION_X, 1500),
            (EV_ABS, ABS_MT_POSITION_Y, 1000),
        ],
    );
    movement(&mut routing, 4, 180, 3000, 2);
    movement(&mut routing, 5, 190, 3500, 2);
    routing
}

#[test]
fn continuous_sequence_dispatches_share_one_ownership_snapshot_after_neutral_completion() {
    let mut routing = continuous_touch();
    up(&mut routing, 6, 200, 2);
    assert_eq!(routing.dispatches.len(), 3, "{:?}", routing.take_events());
    let scope = routing.dispatches[0].chain.as_ref().unwrap();
    assert_eq!(scope[0].segments(), 2);
    assert!(
        routing
            .dispatches
            .iter()
            .all(|dispatch| Arc::ptr_eq(scope, dispatch.chain.as_ref().unwrap()))
    );
    let events = routing.take_dispatches(ms(210));
    assert_eq!(events.len(), 3);
    for (event, stage) in events.iter().zip([
        crate::rules::Stage::Begin,
        crate::rules::Stage::Update,
        crate::rules::Stage::End,
    ]) {
        assert!(matches!(event,Event::Matched{stage:actual,..} if *actual==stage));
    }
    assert!(drain(&mut routing, 210).is_empty());
}

#[test]
fn expiry_of_a_queued_begin_cancels_the_series_without_orphan_updates() {
    let mut routing = continuous_touch();
    assert_eq!(routing.dispatches.len(), 2);
    context(&mut routing, 200);
    assert_eq!(routing.dispatches[0].outcome.expires, Some(ms(300)));
    assert!(
        routing.dispatches[1]
            .outcome
            .expires
            .is_some_and(|expiry| expiry > ms(310))
    );
    assert!(routing.take_dispatches(ms(310)).is_empty());
    assert!(routing.engine.route_intents().is_empty());
    assert_eq!(
        routing.routes[&device().source].router.ownership(),
        Some(Ownership::Draining)
    );
}

#[test]
fn aggregate_native_scope_overflow_retires_transports_instead_of_retaining_unbounded_proofs() {
    let (mut routing, first, _) = buffered();
    key(&mut routing, &first, 1, 0, 30, 1);
    let token = routing.chains.values().next().unwrap()[&first.source].clone();
    for id in 1..=crate::routing::chain::MAX_CHAIN_BINDINGS as u64 + 1 {
        routing
            .chains
            .entry(id)
            .or_default()
            .insert(first.source.clone(), token.clone());
    }
    assert!(routing.bound_chains().is_err());
    assert!(routing.routes.is_empty());
    assert!(routing.chains.is_empty());
    assert!(routing.engine.route_intents().is_empty());
    assert_eq!(routing.take_recoveries().len(), 2);
}

#[test]
fn dispatch_requires_its_sealed_receipt_and_the_original_transport_authorization() {
    for missing in [false, true] {
        let (mut routing, first, second) = buffered();
        key(&mut routing, &first, 1, 0, 30, 1);
        key(&mut routing, &first, 2, 10, 30, 0);
        key(&mut routing, &second, 1, 20, 48, 1);
        tick(&mut routing, 120);
        assert_eq!(routing.dispatches.len(), 1);
        if missing {
            routing.dispatches[0].chain = None;
        } else {
            let route = routing.routes.get_mut(&first.source).unwrap();
            route.authorization += 100;
            route.router = Router::new(
                first.clone(),
                &ready(&first),
                route.authorization,
                Limits::default(),
                ms(120),
            )
            .unwrap();
        }
        assert!(
            routing.take_dispatches(ms(130)).is_empty(),
            "missing={missing}"
        );
    }
}
