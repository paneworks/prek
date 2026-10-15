//! Policy models, not positive root/logind/native acceptance.
use super::*;
use crate::{config::MorfAction, protocol::Epoch};

fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    }
}
fn owner() -> Owner {
    Owner {
        output: "TEST-1".into(),
        epoch: "a".repeat(32),
        ui_generation: 1,
        ownership_generation: 1,
    }
}
fn request(id: u64) -> controls::Request {
    controls::Request {
        version: 1,
        output: "TEST-1".into(),
        epoch: format!("{:016x}{:016x}", 1, 2),
        gesture: format!("{id:032x}"),
        ordinal: 1,
        action: MorfAction::Dashboard,
        phase: controls::Phase::Begin,
        dx: 0.,
        dy: -80.,
        vx: 0.,
        vy: -100.,
    }
}
fn reserve(queue: &mut Queue, request: &controls::Request) -> io::Result<Ticket> {
    queue.reserve(
        Origin {
            fence: fence(),
            owner: owner(),
            source: None,
        },
        request,
        Duration::from_secs(1),
        Duration::from_millis(1100),
        Duration::from_millis(1200),
    )
}
fn reply(id: u64, status: &str) -> native::Reply {
    serde_json::from_value(serde_json::json!({"version":1,"epoch":"a".repeat(32),"ui_generation":"1",
        "id":id.to_string(),"status":status,"error": if status == "rejected" { Some("expired") } else { None }})).unwrap()
}

#[test]
fn queueing_is_not_dispatch_and_tickets_are_exact_and_single_use() {
    let mut queue = Queue::default();
    let ticket = reserve(&mut queue, &request(1)).unwrap();
    queue.submitted(ticket.id(), &Ok(reply(ticket.id(), "queued")));
    assert_eq!(queue.take(&ticket), None);
    queue.observed(ticket.id(), &reply(ticket.id(), "dispatched"));
    for foreign in [
        Ticket {
            fence: Fence {
                session: 2,
                ..fence()
            },
            ..ticket.clone()
        },
        Ticket {
            owner: Owner {
                ui_generation: 2,
                ..owner()
            },
            ..ticket.clone()
        },
        Ticket {
            owner: Owner {
                epoch: "b".repeat(32),
                ..owner()
            },
            ..ticket.clone()
        },
    ] {
        assert_eq!(queue.take(&foreign), None);
    }
    assert_eq!(queue.take(&ticket), Some(Outcome::Dispatched));
    assert_eq!(queue.take(&ticket), None);
    assert!(
        reserve(&mut queue, &request(1)).is_err(),
        "taking a receipt must not permit replay"
    );
}

#[test]
fn frozen_scope_grant_and_strict_progress_cannot_be_replaced() {
    let mut queue = Queue::default();
    let mut bad = request(1);
    bad.epoch = "f".repeat(32);
    assert!(reserve(&mut queue, &bad).is_err());
    let mut bad = request(1);
    bad.output = "other".into();
    assert!(reserve(&mut queue, &bad).is_err());
    assert!(
        queue
            .reserve(
                Origin {
                    fence: fence(),
                    owner: owner(),
                    source: None
                },
                &request(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(2)
            )
            .is_err()
    );
    assert!(
        queue
            .reserve(
                Origin {
                    fence: fence(),
                    owner: owner(),
                    source: None
                },
                &request(1),
                Duration::from_secs(1),
                Duration::from_millis(1251),
                Duration::from_secs(2)
            )
            .is_err()
    );
    assert!(
        queue
            .reserve(
                Origin {
                    fence: fence(),
                    owner: owner(),
                    source: None
                },
                &request(1),
                Duration::from_secs(1),
                Duration::from_millis(1100),
                Duration::from_millis(1099)
            )
            .is_err()
    );
    let mut progress = request(1);
    progress.phase = controls::Phase::Update;
    progress.ordinal = 2;
    assert!(reserve(&mut queue, &progress).is_err());
    reserve(&mut queue, &request(1)).unwrap();
    reserve(&mut queue, &progress).unwrap();
    assert!(reserve(&mut queue, &progress).is_err());
    progress.ordinal = 3;
    progress.action = MorfAction::Notifications;
    assert!(reserve(&mut queue, &progress).is_err());
    progress.action = MorfAction::Dashboard;
    progress.phase = controls::Phase::End;
    reserve(&mut queue, &progress).unwrap();
    progress.phase = controls::Phase::Update;
    progress.ordinal = 4;
    assert!(reserve(&mut queue, &progress).is_err());
}

#[test]
fn coalesced_original_ordinals_become_contiguous_native_attempts() {
    let mut queue = Queue::default();
    let a = reserve(&mut queue, &request(1)).unwrap();
    assert_eq!(queue.wire_ordinal(&a), 1);
    let mut progress = request(1);
    progress.phase = controls::Phase::Update;
    progress.ordinal = 7;
    progress.dx = 200.;
    let b = reserve(&mut queue, &progress).unwrap();
    assert_eq!(queue.wire_ordinal(&b), 2);
    assert_eq!(queue.wire_ordinal(&a), 1);
    assert!(reserve(&mut queue, &progress).is_err());
    progress.phase = controls::Phase::End;
    progress.ordinal = 12;
    let c = reserve(&mut queue, &progress).unwrap();
    assert_eq!(queue.wire_ordinal(&c), 3);
    assert!(reserve(&mut queue, &progress).is_err());
}
fn source(generation: u64) -> Source {
    Source {
        device: 1,
        generation,
        seat: "seat0".into(),
    }
}
fn from_source(queue: &mut Queue, id: u64, source: &Source) -> Ticket {
    queue
        .reserve(
            Origin {
                fence: fence(),
                owner: owner(),
                source: Some(source),
            },
            &request(id),
            Duration::from_secs(1),
            Duration::from_millis(1100),
            Duration::from_millis(1200),
        )
        .unwrap()
}
#[test]
fn source_cancellation_is_exact_and_does_not_retire_another_source() {
    let mut queue = Queue::default();
    let a = from_source(&mut queue, 1, &source(1));
    let b = from_source(&mut queue, 2, &source(2));
    assert!(!queue.cancel_source(&source(3), None));
    assert!(queue.cancel_source(&source(1), Some(1)));
    assert_eq!(queue.cancellation_state(&a), Some(Cancellation::Pending));
    assert_eq!(queue.cancellation_state(&b), None);
    let key = queue.next_cancellation().unwrap();
    assert_eq!(key, request(1).gesture);
    assert_eq!(queue.cancellation_state(&a), Some(Cancellation::Attempted));
    assert!(queue.next_cancellation().is_none());
    queue.cancellation_finished(&key, true);
    assert_eq!(queue.cancellation_state(&a), Some(Cancellation::Confirmed));
    assert_eq!(queue.take(&a), Some(Outcome::Uncertain));
    assert_eq!(queue.take(&b), None);
    queue.observed(b.id(), &reply(b.id(), "dispatched"));
    assert_eq!(queue.take(&b), Some(Outcome::Dispatched));
    assert!(!queue.cancel_source(&source(1), None));
}
#[test]
fn retained_active_progress_and_failed_cancellation_cannot_be_lost_or_retried() {
    let mut queue = Queue::default();
    let a = from_source(&mut queue, 1, &source(1));
    queue.observed(a.id(), &reply(a.id(), "dispatched"));
    assert_eq!(queue.take(&a), Some(Outcome::Dispatched));
    assert!(
        queue.cancel_source(&source(1), None),
        "taking a begin receipt forgot its active controller"
    );
    let key = queue.next_cancellation().unwrap();
    queue.cancellation_finished(&key, false);
    assert!(queue.next_cancellation().is_none());
    assert!(!queue.cancel_source(&source(1), None));
    let mut progress = request(1);
    progress.phase = controls::Phase::Update;
    progress.ordinal = 2;
    assert!(
        queue
            .reserve(
                Origin {
                    fence: fence(),
                    owner: owner(),
                    source: Some(&source(1))
                },
                &progress,
                Duration::from_secs(1),
                Duration::from_millis(1100),
                Duration::from_millis(1200)
            )
            .is_err()
    );
    // A replacement source can start its own gesture, never adopt the old one.
    from_source(&mut queue, 2, &source(2));
}

#[test]
fn reads_are_fair_and_timeouts_and_retirement_never_authorize_retry() {
    let mut queue = Queue::default();
    let a = reserve(&mut queue, &request(1)).unwrap();
    let b = reserve(&mut queue, &request(2)).unwrap();
    let now = Duration::from_secs(1);
    assert_eq!(queue.next_receipt(now), Some(a.id()));
    assert_eq!(queue.next_receipt(now), Some(b.id()));
    assert_eq!(queue.next_receipt(now), Some(a.id()));
    queue.observed(b.id(), &reply(b.id(), "rejected"));
    let mut after_refusal = request(2);
    after_refusal.phase = controls::Phase::Update;
    after_refusal.ordinal = 2;
    assert!(reserve(&mut queue, &after_refusal).is_err());
    assert_eq!(queue.next_receipt(Duration::from_millis(2100)), None);
    assert_eq!(queue.take(&a), Some(Outcome::Uncertain));
    assert_eq!(
        queue.take(&b),
        Some(Outcome::Rejected(native::Refusal::Expired))
    );
    assert!(reserve(&mut queue, &request(1)).is_err());
    let c = reserve(&mut queue, &request(3)).unwrap();
    queue.retire();
    queue.observed(c.id(), &reply(c.id(), "dispatched"));
    assert_eq!(queue.take(&c), Some(Outcome::Uncertain));
}

#[test]
fn lost_mutations_and_wrong_receipts_cannot_be_reported_as_applied() {
    for uncertain in [false, true] {
        let mut queue = Queue::default();
        let ticket = reserve(&mut queue, &request(1)).unwrap();
        queue.submitted(
            ticket.id(),
            &Err(native::Error {
                source: io::Error::other("fixture"),
                uncertain,
            }),
        );
        assert_eq!(
            queue.take(&ticket),
            Some(if uncertain {
                Outcome::Uncertain
            } else {
                Outcome::Unavailable
            })
        );
        assert!(reserve(&mut queue, &request(1)).is_err());
    }
    let mut queue = Queue::default();
    let ticket = reserve(&mut queue, &request(1)).unwrap();
    queue.observed(ticket.id(), &reply(ticket.id() + 1, "dispatched"));
    assert_eq!(queue.take(&ticket), Some(Outcome::Uncertain));
}

#[test]
fn pending_and_unconsumed_terminal_records_are_bounded_without_eviction() {
    let mut queue = Queue::default();
    let mut tickets = vec![];
    for id in 1..=PENDING {
        tickets.push(reserve(&mut queue, &request(id as u64)).unwrap());
    }
    assert!(reserve(&mut queue, &request(9)).is_err());
    queue.retire();
    for id in PENDING + 1..=RECORDS {
        let ticket = reserve(&mut queue, &request(id as u64)).unwrap();
        queue.observed(ticket.id(), &reply(ticket.id(), "dispatched"));
    }
    assert!(reserve(&mut queue, &request(65)).is_err());
    assert_eq!(queue.take(&tickets[0]), Some(Outcome::Uncertain));
    let ticket = reserve(&mut queue, &request(65)).unwrap();
    assert_eq!(ticket.id(), 65);
}
