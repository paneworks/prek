use super::super::*;
fn context() -> ProvidedContext {
    let (snapshot, layouts) = crate::hyprland::reorder::tests::fixture(true);
    ProvidedContext {
        binding: crate::context::Binding {
            seat: "seat0".into(),
            session: "model".into(),
            uid: 1000,
            compositor_pid: 1,
            compositor_start: 1,
            kind: crate::context::SessionKind::User,
        },
        snapshot,
        layouts: Some(layouts),
    }
}
fn event(lua: bool) -> crate::rules::Event {
    crate::rules::Event::Matched {
        version: 1,
        id: 1,
        ordinal: 1,
        rule: "reorder".into(),
        source: crate::input::Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        at: Duration::from_secs(1),
        config_generation: 1,
        session_generation: 1,
        stage: crate::rules::Stage::Discrete,
        action: if lua {
            crate::config::Action::Lua {
                handler: "window".into(),
                requires: BTreeSet::from([crate::config::EffectKind::HyprlandActions]),
            }
        } else {
            crate::config::Action::Hyprland {
                action: crate::config::WindowAction::ReorderUp,
            }
        },
        evidence: crate::rules::Evidence::Key { down: true },
        window: Some("0x2:2".into()),
        output: Some("panel".into()),
        workspace: Some(1),
    }
}
fn ledger(lua: bool) -> (ActionLedger, ProvidedContext) {
    let context = context();
    let fence = Fence {
        broker: protocol::Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let mut ledger = ActionLedger::default();
    ledger
        .offer_scoped(
            fence,
            &event(lua),
            &context.snapshot,
            context.reorder_scope("0x2:2"),
            context.snapshot.captured_at,
        )
        .unwrap();
    (ledger, context)
}
#[test]
fn native_grants_preserve_unchanged_samples_but_missing_scopes_do_not_authorize_reorder() {
    let (mut ledger, mut context) = ledger(false);
    let scope = context.reorder_scope("0x2:2").unwrap();
    context.snapshot.revision += 1;
    context.layouts.as_mut().unwrap().revision += 1;
    assert_eq!(
        ledger
            .authorize_native(1, 1, &context, context.snapshot.captured_at)
            .unwrap(),
        Some(scope)
    );
    let (mut ledger, context) = self::ledger(false);
    ledger.pending.get_mut(&(1, 1)).unwrap().reorder_scope = None;
    assert!(
        ledger
            .authorize_native(1, 1, &context, context.snapshot.captured_at)
            .is_err()
    );
}
#[test]
fn native_scope_loss_is_permanent_without_denial_of_generic_named_window_actions() {
    for lua in [false, true] {
        let (mut ledger, context) = ledger(lua);
        assert_eq!(ledger.retire_native(None).len(), usize::from(!lua));
        assert!(ledger.pending[&(1, 1)].reorder_scope.is_none());
        let result = ledger.authorize_native(1, 1, &context, context.snapshot.captured_at);
        if lua {
            assert_eq!(result.unwrap(), None);
        } else {
            assert!(result.is_err());
        }
    }
}
#[test]
fn changed_neighbor_lifetime_cannot_rebind_an_already_offered_native_action() {
    let (mut ledger, original) = ledger(false);
    let mut changed = original.clone();
    changed.snapshot.windows[0].id = "0x9:9".into();
    let native = changed.layouts.as_mut().unwrap();
    native.workspaces[0].windows[0].id = "0x9:9".into();
    native.workspaces[0].columns[0].members[0] = "0x9:9".into();
    assert_ne!(
        changed.reorder_scope("0x2:2"),
        original.reorder_scope("0x2:2")
    );
    assert_eq!(ledger.retire_native(Some(&changed)).len(), 1);
    assert!(
        ledger
            .authorize_native(1, 1, &original, original.snapshot.captured_at)
            .is_err()
    );
}

#[test]
fn restored_graph_with_a_new_sampled_incarnation_cannot_grant_a_delayed_old_scope() {
    let (mut ledger, original) = ledger(false);
    let mut restored = original.clone();
    restored.snapshot.revision += 1;
    let native = restored.layouts.as_mut().unwrap();
    native.revision = restored.snapshot.revision;
    native.continuity = native.revision;
    assert_ne!(
        original.reorder_scope("0x2:2"),
        restored.reorder_scope("0x2:2")
    );
    assert!(
        ledger
            .authorize_native(1, 1, &restored, restored.snapshot.captured_at)
            .is_err()
    );
}
