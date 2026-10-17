use super::*;

fn subscription(count: usize, at: Duration) -> Subscription {
    let view = crate::context_transfer::tests::native_view(at, count);
    Subscription {
        seat: "seat0".into(),
        fence: view.fence,
        context: view.context,
        capabilities: BTreeSet::from([Capability::Context, Capability::Observations]),
        consumer: None,
    }
}
fn update(revision: u64, count: usize, at: Duration) -> Subscriptions {
    Subscriptions {
        revision,
        seats: vec![subscription(count, at)],
    }
}
pub(crate) fn commands(update: Subscriptions) -> Vec<Command> {
    let transfer = Transfer::new(update, Duration::ZERO).unwrap();
    let mut out = vec![Command::SubscriptionsBegin {
        revision: transfer.revision,
        bytes: transfer.data.len(),
        scopes: transfer.scopes,
    }];
    out.extend(
        transfer
            .data
            .chunks(CHUNK_BYTES)
            .enumerate()
            .map(|(index, data)| Command::SubscriptionsChunk {
                revision: transfer.revision,
                offset: index * CHUNK_BYTES,
                data: data.to_vec(),
            }),
    );
    out.push(Command::SubscriptionsCommit {
        revision: transfer.revision,
    });
    out
}

#[test]
fn complete_native_graph_crosses_real_channel_in_four_packet_bursts() {
    let now = crate::ipc::monotonic().unwrap();
    let update = update(1, 512, now);
    let expected = serde_json::to_value(&update).unwrap();
    assert!(encode(&update).unwrap().len() > protocol::MAX_PACKET);
    let (mut parent, mut worker) = Channel::pair().unwrap();
    let mut sender = Sender::default();
    let mut assembly = Assembly::default();
    sender.replace(update, now).unwrap();
    let mut completed = None;
    let mut bursts = 0;
    while sender.pending() {
        sender.send(&mut parent, now).unwrap();
        let mut packets = 0;
        while let Some(packet) = worker.receive::<Command>().unwrap() {
            packets += 1;
            if let Some(Update::Complete(update)) = assembly.accept(packet.message, now).unwrap() {
                completed = Some(update);
            }
        }
        assert!(packets <= 4);
        bursts += 1;
        assert!(bursts < 256);
    }
    assert!(bursts > 2);
    assert_eq!(serde_json::to_value(completed.unwrap()).unwrap(), expected);
    assert!(assembly.deadline().is_none());
}

#[test]
fn pressure_retains_unsent_offsets_and_normal_refresh_cannot_starve_body() {
    let now = crate::ipc::monotonic().unwrap();
    let (mut parent, mut worker) = Channel::pair().unwrap();
    let mut sender = Sender::default();
    sender.replace(update(1, 512, now), now).unwrap();
    let mut pressure = false;
    for _ in 0..64 {
        let before = sender.active.as_ref().map(|active| active.offset);
        sender.send(&mut parent, now).unwrap();
        let after = sender.active.as_ref().map(|active| active.offset);
        if before == after {
            pressure = true;
            break;
        }
    }
    assert!(pressure);
    let offset = sender.active.as_ref().unwrap().offset;
    sender.send(&mut parent, now).unwrap();
    assert_eq!(sender.active.as_ref().unwrap().offset, offset);
    for revision in 2..=5 {
        sender.replace(update(revision, 512, now), now).unwrap();
    }
    assert_eq!(sender.active.as_ref().unwrap().revision, 1);
    assert_eq!(sender.latest.as_ref().unwrap().revision, 5);
    let mut assembly = Assembly::default();
    let mut received = vec![];
    for _ in 0..256 {
        while let Some(packet) = worker.receive::<Command>().unwrap() {
            if let Some(Update::Complete(update)) = assembly.accept(packet.message, now).unwrap() {
                received.push(update.revision);
            }
        }
        sender.send(&mut parent, now).unwrap();
        if !sender.pending() {
            while let Some(packet) = worker.receive::<Command>().unwrap() {
                if let Some(Update::Complete(update)) =
                    assembly.accept(packet.message, now).unwrap()
                {
                    received.push(update.revision);
                }
            }
            break;
        }
    }
    assert_eq!(received, vec![1, 5]);
}

#[test]
fn changed_security_scope_preempts_but_geometry_refresh_coalesces() {
    let now = crate::ipc::monotonic().unwrap();
    let (mut parent, _worker) = Channel::pair().unwrap();
    let mut sender = Sender::default();
    sender.replace(update(1, 512, now), now).unwrap();
    sender.send(&mut parent, now).unwrap();
    sender.replace(update(2, 511, now), now).unwrap();
    assert_eq!(sender.active.as_ref().unwrap().revision, 1);
    let mut locked = update(3, 1, now);
    locked.seats[0]
        .context
        .as_mut()
        .unwrap()
        .snapshot
        .secure_overlay = true;
    sender.replace(locked, now).unwrap();
    assert_eq!(sender.active.as_ref().unwrap().revision, 3);
    assert!(sender.latest.is_none());
    let mut revoked = update(4, 1, now);
    revoked.seats[0].capabilities.clear();
    sender.replace(revoked, now).unwrap();
    assert_eq!(sender.active.as_ref().unwrap().revision, 4);
}

#[test]
fn superseded_and_cancelled_late_bodies_never_revive() {
    let mut assembly = Assembly::default();
    let mut first = commands(update(1, 1, Duration::ZERO)).into_iter();
    assert!(matches!(
        assembly
            .accept(first.next().unwrap(), Duration::ZERO)
            .unwrap(),
        Some(Update::Scope(_))
    ));
    let mut second = commands(update(2, 1, Duration::ZERO)).into_iter();
    assembly
        .accept(second.next().unwrap(), Duration::ZERO)
        .unwrap();
    for command in first {
        assert!(assembly.accept(command, Duration::ZERO).unwrap().is_none());
    }
    assembly.cancel();
    for command in second {
        assert!(assembly.accept(command, Duration::ZERO).unwrap().is_none());
    }
    assert!(assembly.deadline().is_none());
    assert!(
        assembly
            .accept(Command::SubscriptionsCommit { revision: 3 }, Duration::ZERO)
            .is_err()
    );
}

#[test]
fn incomplete_timeout_releases_bytes_without_refreshing_original_capture() {
    let mut assembly = Assembly::default();
    for command in commands(update(1, 512, Duration::ZERO)).into_iter().take(2) {
        assembly.accept(command, Duration::ZERO).unwrap();
    }
    assert!(!assembly.pending.as_ref().unwrap().data.is_empty());
    assert!(assembly.expire(TIMEOUT - Duration::from_nanos(1)).is_none());
    assert_eq!(assembly.expire(TIMEOUT).unwrap().len(), 1);
    assert!(assembly.pending.is_none());
    assert!(
        assembly
            .accept(Command::SubscriptionsCommit { revision: 1 }, TIMEOUT)
            .unwrap()
            .is_none()
    );
    let mut sender = Sender::default();
    let (mut parent, _worker) = Channel::pair().unwrap();
    sender
        .replace(update(1, 512, Duration::ZERO), Duration::ZERO)
        .unwrap();
    sender.send(&mut parent, Duration::ZERO).unwrap();
    sender
        .replace(update(2, 512, Duration::ZERO), Duration::ZERO)
        .unwrap();
    sender.send(&mut parent, TIMEOUT).unwrap();
    let latest: Subscriptions =
        serde_json::from_slice(&sender.active.as_ref().unwrap().data).unwrap();
    assert_eq!(
        latest.seats[0]
            .context
            .as_ref()
            .unwrap()
            .snapshot
            .captured_at,
        Duration::ZERO
    );
}

#[test]
fn malformed_active_chunks_and_headers_are_rejected() {
    for case in 0..9 {
        let mut assembly = Assembly::default();
        let mut commands = commands(update(1, 1, Duration::ZERO)).into_iter();
        let mut header = commands.next().unwrap();
        if let Command::SubscriptionsBegin {
            revision,
            bytes,
            scopes,
        } = &mut header
        {
            match case {
                0 => *revision = 0,
                1 => *bytes = 0,
                2 => *bytes = MAX_BYTES + 1,
                3 => scopes.push(scopes[0].clone()),
                4 => scopes[0].fence.session = 0,
                _ => {}
            }
        }
        if case < 5 {
            assert!(
                assembly.accept(header, Duration::ZERO).is_err(),
                "case {case}"
            );
            continue;
        }
        assembly.accept(header, Duration::ZERO).unwrap();
        let mut chunk = commands.next().unwrap();
        if let Command::SubscriptionsChunk {
            revision,
            offset,
            data,
        } = &mut chunk
        {
            match case {
                5 => *offset = 1,
                6 => {
                    data.pop();
                }
                7 => *revision = 2,
                8 => *revision = 0,
                _ => unreachable!(),
            }
        }
        assert!(
            assembly.accept(chunk, Duration::ZERO).is_err(),
            "case {case}"
        );
    }
    let mut assembly = Assembly::default();
    let header = commands(update(1, 1, Duration::ZERO)).remove(0);
    assert!(assembly.accept(header, Duration::MAX).is_err());
}

#[test]
fn commit_requires_exact_complete_manifest_and_strict_body() {
    for case in 0..4 {
        let update = update(1, 1, Duration::ZERO);
        let mut data = serde_json::to_value(&update).unwrap();
        match case {
            0 => data["revision"] = 2.into(),
            1 => data["seats"][0]["capabilities"] = serde_json::json!([]),
            2 => data["foreign"] = true.into(),
            3 => {}
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&data).unwrap();
        let mut assembly = Assembly::default();
        assembly
            .accept(
                Command::SubscriptionsBegin {
                    revision: 1,
                    bytes: bytes.len(),
                    scopes: scopes(&update.seats),
                },
                Duration::ZERO,
            )
            .unwrap();
        if case != 3 {
            for (index, data) in bytes.chunks(CHUNK_BYTES).enumerate() {
                assembly
                    .accept(
                        Command::SubscriptionsChunk {
                            revision: 1,
                            offset: index * CHUNK_BYTES,
                            data: data.to_vec(),
                        },
                        Duration::ZERO,
                    )
                    .unwrap();
            }
        }
        assert!(
            assembly
                .accept(Command::SubscriptionsCommit { revision: 1 }, Duration::ZERO)
                .is_err(),
            "case {case}"
        );
        assert!(assembly.pending.is_none());
    }
}

#[test]
fn canonical_scope_set_preserves_capture_free_security_identity() {
    let mut first = subscription(1, Duration::ZERO);
    let mut second = first.clone();
    second.seat = "seat1".into();
    let context = second.context.as_mut().unwrap();
    context.binding.seat = "seat1".into();
    assert_eq!(
        scopes(&[first.clone(), second.clone()]),
        scopes(&[second, first.clone()])
    );
    let old = Scope::from_subscription(&first);
    let context = first.context.as_mut().unwrap();
    context.snapshot.revision += 1;
    context.snapshot.captured_at = TIMEOUT;
    first.capabilities.clear();
    assert!(old.retains(&first));
    first.context.as_mut().unwrap().binding.session = "different".into();
    assert!(!old.retains(&first));
    assert!(old.validate_transition(&first).is_err());
}

#[test]
fn bounds_and_revision_preflight_preserve_last_good_sender() {
    let good = subscription(1, Duration::ZERO);
    assert!(fits(std::slice::from_ref(&good)));
    assert!(!fits(&vec![good.clone(); crate::service::MAX_CLIENTS + 1]));
    let mut oversized = good;
    oversized.context.as_mut().unwrap().snapshot.windows[0].id = "x".repeat(MAX_BYTES);
    assert!(!fits(&[oversized]));
    let mut sender = Sender::default();
    sender
        .replace(update(1, 1, Duration::ZERO), Duration::ZERO)
        .unwrap();
    assert!(
        sender
            .replace(update(1, 1, Duration::ZERO), Duration::ZERO)
            .is_err()
    );
    assert!(
        sender
            .replace(update(2, 1, Duration::ZERO), Duration::MAX)
            .is_err()
    );
    assert_eq!(sender.active.as_ref().unwrap().revision, 1);
    sender.clear();
    assert!(!sender.pending());
    assert!(
        sender
            .replace(update(1, 1, Duration::ZERO), Duration::ZERO)
            .is_err()
    );
}
