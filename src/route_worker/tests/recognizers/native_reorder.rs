use super::*;

fn fixture() -> Fixture {
    fixture_with_action(false)
}
fn fixture_with_action(lua: bool) -> Fixture {
    let mut config = profile();
    config.policy.allow_hyprland_actions = true;
    let rule = &mut config.rules[0];
    rule.contexts = vec![crate::config::Context::Desktop];
    rule.delivery = Delivery::Reserve;
    rule.output = Some("panel".into());
    rule.trigger = Trigger::Key {
        key: crate::config::Key(30),
    };
    rule.action = if lua {
        Action::Lua {
            handler: "window".into(),
            requires: BTreeSet::from([crate::config::EffectKind::HyprlandActions]),
        }
    } else {
        Action::Hyprland {
            action: crate::config::WindowAction::ReorderUp,
        }
    };
    let mut owner = owner();
    let (mut snapshot, mut layouts) = crate::hyprland::reorder::tests::fixture(true);
    snapshot.captured_at = monotonic().unwrap();
    snapshot.windows[1].focused = true;
    layouts.captured_at = snapshot.captured_at;
    let context = owner.context.as_mut().unwrap();
    context.snapshot = snapshot;
    context.layouts = Some(layouts);
    owner.capabilities.insert(Capability::HyprlandActions);
    if lua {
        owner.capabilities.insert(Capability::LuaActions);
    }
    Fixture::new(config, owner)
}
fn scopes(messages: &[Message]) -> Vec<&crate::protocol::ReorderScope> {
    messages
        .iter()
        .filter_map(|message| {
            if let Message::Outcome {
                report: Report::Dispatch { reorder_scope, .. },
            } = message
            {
                Some(
                    reorder_scope
                        .as_ref()
                        .expect("native dispatch lost its claim proof"),
                )
            } else {
                None
            }
        })
        .collect()
}
fn update(fixture: &mut Fixture, graph: bool, revision: u64) {
    let mut next = fixture.owner.clone();
    let context = next.context.as_mut().unwrap();
    context.snapshot.revision = revision;
    context.snapshot.captured_at = monotonic().unwrap();
    if graph {
        let layouts = context.layouts.as_mut().unwrap();
        layouts.revision = revision;
        layouts.captured_at = context.snapshot.captured_at;
    } else {
        context.layouts = None;
    }
    fixture.subscribe(next, revision);
}
#[test]
fn private_worker_dispatch_carries_the_original_preclaim_scope_across_refresh() {
    let mut fixture = fixture();
    let scope = fixture
        .owner
        .context
        .as_ref()
        .unwrap()
        .reorder_scope("0x2:2")
        .unwrap();
    fixture.key(1, true);
    let begin = fixture.drain();
    assert_eq!(scopes(&begin), vec![&scope], "{begin:?}");
    update(&mut fixture, true, 2);
    fixture.key(2, false);
    let end = fixture.drain();
    assert_eq!(scopes(&end), vec![&scope], "{end:?}");
}
#[test]
fn private_worker_cannot_restore_a_claim_after_native_graph_loss() {
    let mut fixture = fixture();
    fixture.key(1, true);
    assert_eq!(scopes(&fixture.drain()).len(), 1);
    update(&mut fixture, false, 2);
    update(&mut fixture, true, 3);
    fixture.drain();
    fixture.key(2, false);
    let end = fixture.drain();
    assert!(scopes(&end).is_empty(), "{end:?}");
}

#[test]
fn queued_private_dispatches_retire_native_permission_before_graph_or_capability_restoration() {
    for lua in [false, true] {
        for loss in ["graph", "capability"] {
            let mut fixture = fixture_with_action(lua);
            fixture.key(1, true);
            assert!(
                fixture
                    .stream
                    .messages
                    .messages
                    .iter()
                    .any(|(message, _)| matches!(
                        message,
                        Message::Outcome {
                            report: Report::Dispatch {
                                reorder_scope: Some(_),
                                ..
                            }
                        }
                    ))
            );
            if loss == "graph" {
                update(&mut fixture, false, 2);
            } else {
                let mut owner = fixture.owner.clone();
                owner.capabilities.remove(&Capability::HyprlandActions);
                fixture
                    .stream
                    .restrict_scopes(
                        &[crate::subscription_transfer::Scope::from_subscription(
                            &owner,
                        )],
                        monotonic().unwrap(),
                    )
                    .unwrap();
            }
            update(&mut fixture, true, 3);
            let messages = fixture.drain();
            let dispatches: Vec<_> = messages
                .iter()
                .filter_map(|message| {
                    if let Message::Outcome {
                        report: Report::Dispatch { reorder_scope, .. },
                    } = message
                    {
                        Some(reorder_scope)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(dispatches.len(), usize::from(lua), "{loss}: {messages:?}");
            assert!(dispatches.iter().all(|scope| scope.is_none()));
            assert_eq!(fixture.stream.messages.bytes, 0);
        }
    }
}
