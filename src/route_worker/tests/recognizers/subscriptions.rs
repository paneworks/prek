use super::*;
use crate::subscription_transfer::{Scope, Sender, tests::commands};

#[test]
fn private_subscription_route_large_context_commits_complete_without_kernel_devices() {
    let mut fixture = Fixture::new(profile(), owner());
    let mut next = fixture.owner.clone();
    next.fence.session += 1;
    let context = next.context.as_mut().unwrap();
    context.snapshot.generation = next.fence.session;
    context.snapshot.captured_at = monotonic().unwrap();
    context.snapshot.outputs = (0..256)
        .map(|index| crate::context::OutputState {
            geometry: crate::gestures::Output {
                name: format!("{}-{index}", "o".repeat(200)),
                pixel_width: 1000.0,
                pixel_height: 800.0,
                scale: 1.0,
                origin: crate::gestures::Point {
                    x: f64::from(index) * 1000.0,
                    y: 0.0,
                },
                transform: crate::gestures::Transform::Normal,
            },
            geometry_generation: 1,
            screen: crate::config::Screen::On,
        })
        .collect();
    let mut sender = Sender::default();
    sender
        .replace(
            Subscriptions {
                revision: 2,
                seats: vec![next.clone()],
            },
            monotonic().unwrap(),
        )
        .unwrap();
    let mut rounds = 0;
    while sender.pending() {
        sender
            .send(&mut fixture.parent, monotonic().unwrap())
            .unwrap();
        fixture.stream.service().unwrap();
        if sender.pending() {
            assert!(!fixture.stream.seats.contains_key("seat0"));
        }
        fixture
            .stream
            .push(Record::Tick {
                at: monotonic().unwrap(),
            })
            .unwrap();
        rounds += 1;
        assert!(rounds < 256);
    }
    assert_eq!(fixture.stream.seats["seat0"].subscription, next);
    assert!(fixture.stream.outputs.is_empty());
    assert_eq!(fixture.stream.revision, 2);
    assert!(
        fixture
            .stream
            .messages
            .messages
            .iter()
            .all(|(message, _)| !matches!(message,
        Message::Outcome {report} if report.subscription().context.is_some()))
    );
}

#[test]
fn private_subscription_route_header_cannot_add_rights_or_renew_authority() {
    let mut fixture = Fixture::new(profile(), owner());
    let expires = fixture.stream.seats["seat0"].expires;
    let snapshot = fixture.stream.seats["seat0"].subscription.context.clone();
    let mut next = fixture.owner.clone();
    next.capabilities.clear();
    fixture
        .stream
        .restrict_scopes(&[Scope::from_subscription(&next)], monotonic().unwrap())
        .unwrap();
    assert!(
        fixture.stream.seats["seat0"]
            .subscription
            .capabilities
            .is_empty()
    );
    assert!(!fixture.stream.seats["seat0"].input_enabled);
    fixture
        .stream
        .restrict_scopes(
            &[Scope::from_subscription(&fixture.owner)],
            monotonic().unwrap(),
        )
        .unwrap();
    assert!(
        fixture.stream.seats["seat0"]
            .subscription
            .capabilities
            .is_empty()
    );
    assert_eq!(fixture.stream.seats["seat0"].expires, expires);
    assert_eq!(fixture.stream.seats["seat0"].subscription.context, snapshot);
}

#[test]
fn private_subscription_route_scope_lock_revokes_and_timeout_is_nonfatal() {
    let mut fixture = Fixture::new(profile(), owner());
    let mut next = fixture.owner.clone();
    next.context.as_mut().unwrap().snapshot.state = State::Locked;
    let mut packets = commands(Subscriptions {
        revision: 2,
        seats: vec![next],
    })
    .into_iter();
    fixture.send(packets.next().unwrap());
    assert!(fixture.stream.seats.is_empty());
    let deadline = fixture.stream.subscriptions.deadline().unwrap();
    fixture.stream.expire_subscriptions(deadline).unwrap();
    assert!(fixture.stream.subscriptions.deadline().is_none());
    for command in packets {
        fixture.send(command);
    }
    assert!(fixture.stream.seats.is_empty());
    assert!(fixture.drain().iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::ContextLost { .. }
        }
    )));
}

#[test]
fn private_subscription_route_reload_retires_body_and_stale_config_headers() {
    let mut fixture = Fixture::new(profile(), owner());
    let mut packets = commands(Subscriptions {
        revision: 2,
        seats: vec![fixture.owner.clone()],
    })
    .into_iter();
    fixture.send(packets.next().unwrap());
    let mut transfer =
        crate::configuration::Transfer::new(toml::to_string(&profile()).unwrap(), 2).unwrap();
    while !transfer
        .send(&mut fixture.parent, monotonic().unwrap())
        .unwrap()
    {
        fixture.stream.service().unwrap();
    }
    fixture.stream.service().unwrap();
    assert_eq!(fixture.stream.generation, 2);
    assert!(fixture.stream.subscriptions.deadline().is_none());
    for command in packets {
        fixture.send(command);
    }
    for command in commands(Subscriptions {
        revision: 3,
        seats: vec![fixture.owner.clone()],
    }) {
        fixture.send(command);
    }
    assert!(fixture.stream.seats.is_empty());
    let mut fresh = fixture.owner.clone();
    fresh.fence.configuration = 2;
    fresh.context.as_mut().unwrap().snapshot.captured_at = monotonic().unwrap();
    for command in commands(Subscriptions {
        revision: 4,
        seats: vec![fresh.clone()],
    }) {
        fixture.send(command);
    }
    assert_eq!(fixture.stream.seats["seat0"].subscription, fresh);
}
