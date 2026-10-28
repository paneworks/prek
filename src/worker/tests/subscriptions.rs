use super::*;
use crate::subscription_transfer::{Scope, Sender, tests::commands};

fn wide(count: usize, now: Duration) -> Subscription {
    let view = crate::context_transfer::tests::wide_view(now, count);
    Subscription {
        seat: "seat0".into(),
        fence: view.fence,
        context: view.context,
        capabilities: BTreeSet::new(),
        consumer: None,
    }
}
fn send(stream: &mut Stream, parent: &mut Channel, command: Command) {
    assert!(
        parent
            .send(&command, crate::ipc::monotonic().unwrap())
            .unwrap()
    );
    stream.service().unwrap();
}

#[test]
fn private_subscription_large_snapshot_activates_only_after_commit_and_services_input() {
    let now = crate::ipc::monotonic().unwrap();
    let (mut parent, child) = Channel::pair().unwrap();
    let mut stream = Stream::new(config(), child).unwrap();
    stream.push(Record::Header { version: 1 }).unwrap();
    let subscription = wide(256, now);
    let mut sender = Sender::default();
    sender
        .replace(
            Subscriptions {
                revision: 1,
                seats: vec![subscription.clone()],
            },
            now,
        )
        .unwrap();
    let mut rounds = 0;
    while sender.pending() {
        sender
            .send(&mut parent, crate::ipc::monotonic().unwrap())
            .unwrap();
        stream.service().unwrap();
        if sender.pending() {
            assert!(stream.registry.observers.is_empty());
        }
        let at = crate::ipc::monotonic().unwrap();
        stream.push(Record::Tick { at }).unwrap();
        rounds += 1;
        assert!(rounds < 256);
    }
    let observer = stream.registry.observers.get("seat0").unwrap();
    assert_eq!(observer.subscription, subscription);
    assert_eq!(stream.registry.revision, 1);
    assert!(rounds > 2);
    assert!(
        stream
            .registry
            .reports
            .iter()
            .all(|(report, _)| report.subscription().context.is_none())
    );
}

#[test]
fn private_subscription_header_only_revokes_and_never_refreshes_lease() {
    let mut config = config();
    config.policy.allow_input_events = true;
    let mut registry = Registry::new(config).unwrap();
    let initial = input_subscription("seat0", 100);
    registry
        .update(
            Subscriptions {
                revision: 1,
                seats: vec![initial.clone()],
            },
            ms(100),
        )
        .unwrap();
    let expires = registry.observers["seat0"].expires;
    let capture = registry.observers["seat0"]
        .subscription
        .context
        .as_ref()
        .unwrap()
        .snapshot
        .captured_at;
    let mut updated = initial.clone();
    updated.capabilities.clear();
    let mut absent = subscription("seat1", 1);
    absent.capabilities = initial.capabilities.clone();
    registry
        .restrict_scopes(
            &[
                Scope::from_subscription(&updated),
                Scope::from_subscription(&absent),
            ],
            ms(110),
        )
        .unwrap();
    let observer = &registry.observers["seat0"];
    assert!(observer.subscription.capabilities.is_empty());
    assert!(!observer.input_enabled);
    assert_eq!(observer.expires, expires);
    assert_eq!(
        observer
            .subscription
            .context
            .as_ref()
            .unwrap()
            .snapshot
            .captured_at,
        capture
    );
    assert!(!registry.observers.contains_key("seat1"));
    assert!(inputs(&registry).is_empty());
    registry
        .restrict_scopes(&[Scope::from_subscription(&initial)], ms(120))
        .unwrap();
    assert!(
        registry.observers["seat0"]
            .subscription
            .capabilities
            .is_empty()
    );
    assert_eq!(registry.revision, 1);
}

#[test]
fn private_subscription_scope_policy_validation_is_atomic_across_seats() {
    let mut registry = Registry::new(config()).unwrap();
    let initial = with_context(subscription("seat0", 1), 1, 100);
    registry
        .update(
            Subscriptions {
                revision: 1,
                seats: vec![initial.clone()],
            },
            ms(100),
        )
        .unwrap();
    let mut foreign = Scope::from_subscription(&subscription("seat1", 1));
    foreign.fence.configuration = 2;
    let header = vec![Scope::from_subscription(&initial), foreign];
    assert!(registry.restrict_scopes(&header, ms(110)).is_err());
    assert_eq!(registry.observers["seat0"].subscription, initial);
    assert_eq!(registry.revision, 1);
}

#[test]
fn private_subscription_security_changes_remove_old_observer_but_binding_needs_fence() {
    for case in 0..6 {
        let mut registry = Registry::new(config()).unwrap();
        let initial = with_context(subscription("seat0", 1), 1, 100);
        registry
            .update(
                Subscriptions {
                    revision: 1,
                    seats: vec![initial.clone()],
                },
                ms(100),
            )
            .unwrap();
        let mut next = initial.clone();
        match case {
            0 => next.context.as_mut().unwrap().snapshot.state = crate::context::State::Locked,
            1 => next.context.as_mut().unwrap().snapshot.state = crate::context::State::Locking,
            2 => {
                next.consumer =
                    Some(crate::transport::Consumer::IsolatedReceiver { pid: 1, start: 1 })
            }
            3 => {
                next.context.as_mut().unwrap().binding.session = "rebound".into();
            }
            4 => {
                next.context = None;
            }
            5 => {
                next.fence.session = 2;
                next.context.as_mut().unwrap().snapshot.generation = 2;
            }
            _ => unreachable!(),
        }
        let result = registry.restrict_scopes(&[Scope::from_subscription(&next)], ms(110));
        if matches!(case, 3 | 4) {
            assert!(result.is_err());
            assert_eq!(registry.observers["seat0"].subscription, initial);
        } else {
            result.unwrap();
            assert!(registry.observers.is_empty());
        }
    }
}

#[test]
fn private_subscription_transfer_timeout_is_nonfatal_and_drops_partial_state() {
    let now = crate::ipc::monotonic().unwrap();
    let (mut parent, child) = Channel::pair().unwrap();
    let mut stream = Stream::new(config(), child).unwrap();
    stream.push(Record::Header { version: 1 }).unwrap();
    let owner = wide(1, now);
    for command in commands(Subscriptions {
        revision: 1,
        seats: vec![owner.clone()],
    })
    .into_iter()
    .take(2)
    {
        send(&mut stream, &mut parent, command);
    }
    let deadline = stream.subscriptions.deadline().unwrap();
    stream.expire_subscriptions(deadline).unwrap();
    assert!(stream.subscriptions.deadline().is_none());
    assert!(stream.registry.observers.is_empty());
    assert!(stream.registry.reports.iter().any(|(report,_)|matches!(report,Report::ContextLost {subscription} if subscription.fence==owner.fence)));
    send(
        &mut stream,
        &mut parent,
        Command::SubscriptionsCommit { revision: 1 },
    );
    stream
        .push(Record::Tick {
            at: crate::ipc::monotonic().unwrap(),
        })
        .unwrap();
}

#[test]
fn private_subscription_stale_committed_capture_cannot_gain_receipt_freshness() {
    let now = crate::ipc::monotonic().unwrap();
    let (mut parent, child) = Channel::pair().unwrap();
    let mut stream = Stream::new(config(), child).unwrap();
    for command in commands(Subscriptions {
        revision: 1,
        seats: vec![wide(
            1,
            now - crate::context::SNAPSHOT_MAX_AGE - Duration::from_secs(1),
        )],
    }) {
        send(&mut stream, &mut parent, command);
    }
    assert!(stream.registry.observers.is_empty());
    assert!(
        stream
            .registry
            .reports
            .iter()
            .any(|(report, _)| matches!(report, Report::ContextLost { .. }))
    );
}

#[test]
fn private_subscription_config_install_discards_old_body_and_ignores_retired_chunks() {
    let now = crate::ipc::monotonic().unwrap();
    let (mut parent, child) = Channel::pair().unwrap();
    let mut stream = Stream::new(config(), child).unwrap();
    stream.flush().unwrap();
    parent.receive::<Message>().unwrap().unwrap();
    let mut old = commands(Subscriptions {
        revision: 1,
        seats: vec![wide(1, now)],
    })
    .into_iter();
    send(&mut stream, &mut parent, old.next().unwrap());
    let mut transfer =
        crate::configuration::Transfer::new(toml::to_string(&config()).unwrap(), 2).unwrap();
    while !transfer
        .send(&mut parent, crate::ipc::monotonic().unwrap())
        .unwrap()
    {
        stream.service().unwrap();
    }
    stream.service().unwrap();
    assert_eq!(stream.generation(), 2);
    assert!(stream.subscriptions.deadline().is_none());
    for command in old {
        send(&mut stream, &mut parent, command);
    }
    for command in commands(Subscriptions {
        revision: 2,
        seats: vec![wide(1, now)],
    }) {
        send(&mut stream, &mut parent, command);
    }
    assert!(stream.registry.observers.is_empty());
    let mut fresh = wide(1, crate::ipc::monotonic().unwrap());
    fresh.fence.configuration = 2;
    for command in commands(Subscriptions {
        revision: 3,
        seats: vec![fresh.clone()],
    }) {
        send(&mut stream, &mut parent, command);
    }
    assert_eq!(stream.registry.observers["seat0"].subscription, fresh);
}
