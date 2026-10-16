use super::*;
use crate::{input::KeyState, protocol::Epoch};
fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 3,
        session: 4,
    }
}
fn definition(continuous: bool) -> Definition {
    Definition {
        name: "custom".into(),
        selectors: BTreeSet::from(["keys".into()]),
        inputs: BTreeSet::from([InputKind::Key]),
        reply_within: Millis(20),
        continuous,
    }
}
fn candidate(at: Duration) -> Candidate {
    Candidate {
        id: 7,
        revision: 2,
        rule: "rule".into(),
        source: Source {
            device: u64::MAX,
            generation: 4,
            seat: "seat0".into(),
        },
        ownership: Some(Ownership::Pending),
        winner: None,
        expires: at + Duration::from_millis(100),
    }
}
fn input(at: Duration, sequence: u64) -> Event {
    Event::Frame {
        source: candidate(at).source,
        at,
        sequence,
        selectors: vec!["keys".into(), "other".into()],
        changes: vec![
            Change::Key {
                code: 30,
                state: KeyState::Press,
            },
            Change::Relative { code: 0, value: 10 },
        ],
        contacts: vec![],
        contacts_started: vec![],
        contacts_ended: vec![],
        ended_contacts: vec![],
    }
}
fn proposal(ticket: &Ticket, phase: Phase) -> Proposal {
    Proposal {
        fence: ticket.fence,
        ticket: ticket.id,
        phase,
        data: serde_json::json!({"confidence":0.9}),
    }
}
#[test]
fn timer_tickets_retain_original_input_time_and_reject_replayed_or_changed_candidates() {
    let at = Duration::from_secs(1);
    let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
    let mut current = candidate(at);
    current.expires = at + Duration::from_secs(2);
    assert!(gate.offer_timer("custom", &current, at).is_err());
    let first = gate.offer("custom", &current, &input(at, 1), at).unwrap();
    assert_eq!(first.cause, Cause::Frame);
    first.validate_for(&definition(false), at).unwrap();
    gate.validate(proposal(&first, Phase::Pending), &current, at)
        .unwrap();
    let wake = at + Duration::from_secs(1);
    let timer = gate.offer_timer("custom", &current, wake).unwrap();
    assert_eq!(timer.input.at(), at);
    assert_eq!(timer.requested_at, wake);
    assert_eq!(timer.deadline, wake + Duration::from_millis(20));
    timer.validate_for(&definition(false), wake).unwrap();
    let Event::Frame { changes, .. } = &timer.input else {
        unreachable!()
    };
    assert!(changes.is_empty());
    assert!(gate.offer_timer("custom", &current, wake).is_err());
    let mut forged = timer.clone();
    if let Event::Frame { changes, .. } = &mut forged.input {
        changes.push(Change::Key {
            code: 30,
            state: KeyState::Press,
        });
    }
    assert!(forged.validate_for(&definition(false), wake).is_err());
    let mut changed = current.clone();
    changed.revision += 1;
    assert!(
        gate.offer_timer("custom", &changed, wake + Duration::from_millis(1))
            .is_err()
    );
    let matched = gate
        .validate(proposal(&timer, Phase::Discrete), &current, wake)
        .unwrap()
        .unwrap();
    assert_eq!(matched.observed_at, wake);
    assert!(
        gate.offer_timer("custom", &current, wake + Duration::from_millis(1))
            .is_err()
    );
}
#[test]
fn tickets_filter_declared_input_and_keep_original_deadlines_without_claiming() {
    let at = Duration::from_secs(1);
    let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
    let candidate = candidate(at);
    let ticket = gate
        .offer(
            "custom",
            &candidate,
            &input(at, 1),
            at + Duration::from_millis(10),
        )
        .unwrap();
    assert_eq!(ticket.deadline, at + Duration::from_millis(20));
    assert_eq!(ticket.input.selectors(), &["keys".to_owned()]);
    let Event::Frame { changes, .. } = &ticket.input else {
        unreachable!()
    };
    assert_eq!(
        changes,
        &[Change::Key {
            code: 30,
            state: KeyState::Press
        }]
    );
    let reply = proposal(&ticket, Phase::Discrete);
    let validated = gate
        .validate(reply.clone(), &candidate, at + Duration::from_millis(15))
        .unwrap()
        .unwrap();
    assert_eq!(validated.candidate.ownership, Some(Ownership::Pending));
    assert_eq!(validated.observed_at, at);
    assert_eq!(validated.ordinal, 1);
    assert!(
        gate.validate(reply, &candidate, at + Duration::from_millis(16))
            .is_err()
    );
    assert!(gate.offer("custom", &candidate, &input(at, 2), at).is_err());
}

#[test]
fn stationary_touch_wakes_do_not_replay_contact_birth_or_released_positions() {
    use crate::input::{Contact, ContactId};
    let at = Duration::from_secs(1);
    let mut declaration = definition(false);
    declaration.inputs = BTreeSet::from([InputKind::Touch]);
    let mut gate = Gate::new(fence(), vec![declaration.clone()]).unwrap();
    let current = candidate(at);
    let contact = Contact {
        id: ContactId {
            slot: 0,
            tracking: 10,
            generation: 4,
        },
        x: 10,
        y: 20,
        pressure: None,
        fresh: true,
    };
    let mut frame = input(at, 1);
    if let Event::Frame {
        contacts,
        contacts_started,
        contacts_ended,
        ended_contacts,
        ..
    } = &mut frame
    {
        contacts.push(contact.clone());
        contacts_started.push(contact.id);
        let mut ended = contact.clone();
        ended.id.tracking += 1;
        contacts_ended.push(ended.id);
        ended_contacts.push(ended);
    }
    gate.offer("custom", &current, &frame, at).unwrap();
    let timer = gate
        .offer_timer("custom", &current, at + Duration::from_millis(1))
        .unwrap();
    timer
        .validate_for(&declaration, at + Duration::from_millis(1))
        .unwrap();
    let Event::Frame {
        changes,
        contacts,
        contacts_started,
        contacts_ended,
        ended_contacts,
        ..
    } = timer.input
    else {
        unreachable!()
    };
    assert!(
        changes.is_empty()
            && contacts_started.is_empty()
            && contacts_ended.is_empty()
            && ended_contacts.is_empty()
    );
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].id, contact.id);
    assert_eq!((contacts[0].x, contacts[0].y), (10, 20));
    assert!(!contacts[0].fresh);
    gate.retire_source(&current.source);
    assert_eq!(gate.pending(), 0);
    assert!(
        gate.offer_timer("custom", &current, at + Duration::from_millis(2))
            .is_err()
    );
}

#[test]
fn retirement_is_exact_scoped_and_reserved_under_admission_pressure() {
    let at = Duration::from_secs(1);
    for source_lost in [false, true] {
        let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
        let mut current = candidate(at);
        for id in 1..=MAX_PENDING as u64 {
            current.id = id;
            gate.offer("custom", &current, &input(at, 1), at).unwrap();
        }
        if source_lost {
            gate.retire_source(&current.source);
            gate.retire_source(&current.source);
        } else {
            gate.expire(current.expires);
            gate.expire(current.expires);
        }
        assert_eq!(gate.pending(), 0);
        assert!(gate.active.is_empty());
        current.id += 1;
        assert!(gate.offer("custom", &current, &input(at, 1), at).is_err());
        let notifications = gate.take_cancellations();
        assert_eq!(notifications.len(), MAX_PENDING);
        for (index, cancellation) in notifications.iter().enumerate() {
            cancellation.validate().unwrap();
            assert_eq!(cancellation.fence, fence());
            assert_eq!(cancellation.candidate, index as u64 + 1);
            assert_eq!(cancellation.through, index as u64 + 1);
            assert_eq!(cancellation.source, current.source);
            assert_eq!(
                cancellation.reason,
                if source_lost {
                    Retirement::SourceLost
                } else {
                    Retirement::Expired
                }
            );
        }
        assert!(gate.take_cancellations().is_empty());
        gate.offer("custom", &current, &input(at, 1), at).unwrap();
    }
}

#[test]
fn cancellation_freezes_last_issued_ticket_and_revokes_undelivered_replies() {
    let at = Duration::from_secs(1);
    let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
    let current = candidate(at);
    let first = gate.offer("custom", &current, &input(at, 1), at).unwrap();
    let last = gate.offer("custom", &current, &input(at, 2), at).unwrap();
    gate.retire_with(current.id, Retirement::RuleChanged);
    assert!(
        gate.validate(proposal(&first, Phase::Discrete), &current, at)
            .is_err()
    );
    assert!(
        gate.validate(proposal(&last, Phase::Discrete), &current, at)
            .is_err()
    );
    let cancellation = gate.take_cancellations().pop().unwrap();
    assert_eq!(
        cancellation,
        Cancellation::from_ticket(&last, Retirement::RuleChanged)
    );
    let mut forged = serde_json::to_value(&cancellation).unwrap();
    forged["action"] = serde_json::json!({"kind":"command"});
    assert!(serde_json::from_value::<Cancellation>(forged).is_err());
}

#[test]
fn only_the_winning_candidate_can_continue_a_claimed_source() {
    let at = Duration::from_secs(1);
    let mut gate = Gate::new(fence(), vec![definition(true)]).unwrap();
    let mut current = candidate(at);
    let first = gate.offer("custom", &current, &input(at, 1), at).unwrap();
    gate.validate(proposal(&first, Phase::Begin), &current, at)
        .unwrap();
    let next = gate.offer("custom", &current, &input(at, 2), at).unwrap();
    current.ownership = Some(Ownership::Claimed);
    current.winner = Some(current.id + 1);
    assert!(
        gate.validate(proposal(&next, Phase::Update), &current, at)
            .is_err()
    );
    assert!(gate.offer("custom", &current, &input(at, 3), at).is_err());
    current.winner = Some(current.id);
    let final_ticket = gate.offer("custom", &current, &input(at, 3), at).unwrap();
    assert_eq!(
        gate.validate(proposal(&final_ticket, Phase::End), &current, at)
            .unwrap()
            .unwrap()
            .ordinal,
        2
    );
}

#[test]
fn proposals_cannot_supply_targets_actions_or_public_broker_commands() {
    let at = Duration::from_secs(1);
    let candidate = candidate(at);
    let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
    let ticket = gate.offer("custom", &candidate, &input(at, 1), at).unwrap();
    let original = serde_json::to_value(proposal(&ticket, Phase::Discrete)).unwrap();
    for field in ["candidate", "source", "target", "action", "deadline"] {
        let mut forged = original.clone();
        forged[field] = serde_json::json!("caller-selected");
        assert!(serde_json::from_value::<Proposal>(forged).is_err());
    }
    assert!(
        serde_json::from_value::<crate::protocol::Request>(serde_json::json!({
            "kind":"recognize", "fence":fence(), "proposal":original
        }))
        .is_err()
    );
    assert_eq!(gate.pending(), 1);
    assert!(
        gate.validate(proposal(&ticket, Phase::Discrete), &candidate, at)
            .unwrap()
            .is_some()
    );
}
#[test]
fn late_forwarded_draining_retargeted_and_foreign_responses_never_validate() {
    let at = Duration::from_secs(1);
    for variant in 0..8 {
        let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
        let mut current = candidate(at);
        let ticket = gate.offer("custom", &current, &input(at, 1), at).unwrap();
        let mut reply = proposal(&ticket, Phase::Discrete);
        let mut now = at;
        match variant {
            0 => now += Duration::from_millis(20),
            1 => current.ownership = Some(Ownership::Forwarded),
            2 => current.ownership = Some(Ownership::Draining),
            3 => current.revision += 1,
            4 => current.source.generation += 1,
            5 => current.rule = "other".into(),
            6 => current.ownership = None,
            _ => reply.fence.session += 1,
        }
        assert!(
            gate.validate(reply, &current, now).is_err(),
            "variant {variant}"
        );
    }
}
#[test]
fn continuous_phases_have_root_ordinals_and_reordered_frames_cannot_reopen_progress() {
    let at = Duration::from_secs(1);
    let mut gate = Gate::new(fence(), vec![definition(true)]).unwrap();
    let mut candidate = candidate(at);
    let first = gate.offer("custom", &candidate, &input(at, 1), at).unwrap();
    assert_eq!(
        gate.validate(proposal(&first, Phase::Begin), &candidate, at)
            .unwrap()
            .unwrap()
            .ordinal,
        1
    );
    candidate.ownership = Some(Ownership::Claimed);
    candidate.winner = Some(candidate.id);
    let old = gate.offer("custom", &candidate, &input(at, 2), at).unwrap();
    let new = gate.offer("custom", &candidate, &input(at, 3), at).unwrap();
    assert_eq!(
        gate.validate(proposal(&new, Phase::Update), &candidate, at)
            .unwrap()
            .unwrap()
            .ordinal,
        2
    );
    assert!(
        gate.validate(proposal(&old, Phase::Update), &candidate, at)
            .is_err()
    );
    let end = gate.offer("custom", &candidate, &input(at, 4), at).unwrap();
    assert_eq!(
        gate.validate(proposal(&end, Phase::End), &candidate, at)
            .unwrap()
            .unwrap()
            .ordinal,
        3
    );
    assert_eq!(gate.pending(), 0);
    assert!(gate.offer("custom", &candidate, &input(at, 5), at).is_err());
}
#[test]
fn pending_cancel_and_source_retirement_release_all_tickets() {
    let at = Duration::from_secs(1);
    let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
    let candidate = candidate(at);
    let first = gate.offer("custom", &candidate, &input(at, 1), at).unwrap();
    assert!(
        gate.validate(proposal(&first, Phase::Pending), &candidate, at)
            .unwrap()
            .is_none()
    );
    let second = gate.offer("custom", &candidate, &input(at, 2), at).unwrap();
    let third = gate.offer("custom", &candidate, &input(at, 3), at).unwrap();
    let cancelled = gate
        .validate(proposal(&second, Phase::Cancel), &candidate, at)
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.ordinal, 1);
    assert_eq!(gate.pending(), 0);
    assert!(
        gate.validate(proposal(&third, Phase::Discrete), &candidate, at)
            .is_err()
    );
    gate.retire_source(&candidate.source);
    assert!(gate.active.is_empty());
}
#[test]
fn phase_and_data_limits_consume_the_ticket_without_manufacturing_a_match() {
    let at = Duration::from_secs(1);
    for continuous in [false, true] {
        let mut gate = Gate::new(fence(), vec![definition(continuous)]).unwrap();
        let candidate = candidate(at);
        let first = gate.offer("custom", &candidate, &input(at, 1), at).unwrap();
        assert!(
            gate.validate(proposal(&first, Phase::Update), &candidate, at)
                .is_err()
        );
        assert_eq!(gate.pending(), 0);
        let second = gate.offer("custom", &candidate, &input(at, 2), at).unwrap();
        let mut reply = proposal(&second, Phase::Cancel);
        reply.data = Value::String("\\".repeat(MAX_DATA_BYTES));
        assert!(gate.validate(reply, &candidate, at).is_err());
        assert_eq!(gate.pending(), 0);
    }
    let mut deep = Value::Null;
    for _ in 0..10 {
        deep = serde_json::json!([deep]);
    }
    assert!(validate_data(&deep).is_err());
    assert!(validate_data(&Value::Array(vec![Value::Null; MAX_DATA_BYTES + 1])).is_err());
}
#[test]
fn declaration_frame_identity_and_capacity_are_bounded_without_evicting_live_tickets() {
    let at = Duration::from_secs(1);
    assert!(Gate::new(fence(), vec![definition(false), definition(false)]).is_err());
    let mut invalid = definition(false);
    invalid.reply_within = Millis(251);
    assert!(invalid.validate().is_err());
    let mut gate = Gate::new(fence(), vec![definition(false)]).unwrap();
    let candidate = candidate(at);
    for sequence in 1..=MAX_PENDING as u64 {
        gate.offer("custom", &candidate, &input(at, sequence), at)
            .unwrap();
    }
    assert!(
        gate.offer("custom", &candidate, &input(at, MAX_PENDING as u64 + 1), at)
            .is_err()
    );
    assert_eq!(gate.pending(), MAX_PENDING);
    gate.expire(at + Duration::from_millis(20));
    assert_eq!(gate.pending(), 0);
    assert!(
        gate.offer(
            "custom",
            &candidate,
            &input(at, MAX_PENDING as u64 + 1),
            at + Duration::from_millis(20)
        )
        .is_err()
    );
    gate.retire(candidate.id);
    assert!(gate.active.is_empty());
}
