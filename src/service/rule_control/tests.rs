use super::*;
use crate::{
    config::{Action, Config},
    rule_transactions::{Change, Transaction},
};
#[test]
fn public_hyprland_registration_requires_protected_route_policy_declared_effects_and_context() {
    use crate::config::{EffectKind, Mode, WindowAction};
    let mut config = Config::preset("desktop").unwrap();
    config.mode = Mode::Route;
    config.rules[0].enabled = true;
    config.rules[0].action = Action::Hyprland {
        action: WindowAction::Close,
    };
    let caps = BTreeSet::from([Capability::Context, Capability::HyprlandActions]);
    let accepted = |config: Option<&Config>, lua, caps: &BTreeSet<Capability>| {
        registration_capabilities(config, None, lua, true, Role::Agent, caps)
    };
    assert!(accepted(Some(&config), false, &caps).is_err());
    config.policy.allow_hyprland_actions = true;
    assert!(accepted(Some(&config), false, &caps).is_ok());
    assert!(accepted(None, false, &caps).is_err());
    assert!(
        accepted(
            Some(&config),
            false,
            &BTreeSet::from([Capability::HyprlandActions])
        )
        .is_err()
    );
    assert!(
        registration_capabilities(Some(&config), None, false, true, Role::Inspect, &caps).is_err()
    );
    for action in [
        WindowAction::ReorderUp,
        WindowAction::ReorderDown,
        WindowAction::ScrollStack,
    ] {
        config.rules[0].action = Action::Hyprland { action };
        assert!(accepted(Some(&config), false, &caps).is_err());
    }
    config.rules[0].action = Action::Lua {
        handler: "window".into(),
        requires: BTreeSet::from([EffectKind::HyprlandActions]),
    };
    let caps = BTreeSet::from([
        Capability::Context,
        Capability::Observations,
        Capability::LuaEvents,
        Capability::LuaActions,
        Capability::HyprlandActions,
    ]);
    assert!(accepted(Some(&config), true, &caps).is_ok());
    assert!(accepted(Some(&config), false, &caps).is_err());
    config.rules[0].enabled = false;
    assert!(accepted(Some(&config), true, &caps).is_err());
    config.rules[0].enabled = true;
    config.mode = Mode::Observe;
    assert!(accepted(Some(&config), true, &caps).is_err());
}
#[test]
fn public_power_registration_requires_route_policy_declared_effects_and_context() {
    let mut config = Config::preset("desktop").unwrap();
    config.mode = crate::config::Mode::Route;
    config.rules[0].enabled = true;
    config.rules[0].action = Action::Power {
        action: crate::config::PowerAction::Blank,
    };
    let caps = BTreeSet::from([Capability::Context, Capability::PowerActions]);
    let accepted = |config: Option<&Config>, lua, caps: &BTreeSet<Capability>| {
        registration_capabilities(config, None, lua, true, Role::Agent, caps)
    };
    assert!(accepted(Some(&config), false, &caps).is_err());
    config.policy.allow_power_actions = true;
    assert!(accepted(Some(&config), false, &caps).is_ok());
    assert!(accepted(None, false, &caps).is_err());
    assert!(
        accepted(
            Some(&config),
            false,
            &BTreeSet::from([Capability::PowerActions])
        )
        .is_err()
    );
    assert!(
        registration_capabilities(Some(&config), None, false, true, Role::Inspect, &caps).is_err()
    );
    config.rules[0].action = Action::Power {
        action: crate::config::PowerAction::LockAndBlank,
    };
    assert!(accepted(Some(&config), false, &caps).is_err());
    config.rules[0].action = Action::Lua {
        handler: "power".into(),
        requires: BTreeSet::from([crate::config::EffectKind::PowerActions]),
    };
    let caps = BTreeSet::from([
        Capability::Context,
        Capability::Observations,
        Capability::LuaEvents,
        Capability::LuaActions,
        Capability::PowerActions,
    ]);
    assert!(accepted(Some(&config), true, &caps).is_ok());
    assert!(accepted(Some(&config), false, &caps).is_err());
    config.mode = crate::config::Mode::Observe;
    assert!(accepted(Some(&config), true, &caps).is_err());
}
#[test]
fn power_offer_capabilities_are_rechecked_before_a_grant() {
    let snapshot = crate::context_transfer::tests::view(Duration::ZERO)
        .context
        .unwrap()
        .snapshot;
    let mut ledger = ActionLedger::default();
    let event = crate::rules::Event::Matched {
        version: 1,
        id: 1,
        ordinal: 1,
        rule: "power".into(),
        source: crate::input::Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        at: Duration::ZERO,
        config_generation: 1,
        session_generation: 1,
        stage: crate::rules::Stage::Begin,
        action: Action::Lua {
            handler: "power".into(),
            requires: BTreeSet::from([crate::config::EffectKind::PowerActions]),
        },
        evidence: crate::rules::Evidence::Key { down: true },
        window: None,
        output: Some("fixture-output".into()),
        workspace: None,
    };
    ledger.offer(&event, &snapshot, Duration::ZERO).unwrap();
    let offer = &ledger.pending[&(1, 1)];
    let caps = BTreeSet::from([Capability::LuaActions, Capability::PowerActions]);
    assert!(offer.capabilities_available(&caps, &caps));
    assert!(!offer.capabilities_available(&caps, &BTreeSet::from([Capability::LuaActions])));
    assert!(!offer.capabilities_available(&BTreeSet::from([Capability::LuaActions]), &caps));
}
fn config() -> Arc<crate::config::Config> {
    Arc::new(crate::config::Config::parse_named("version=1\n[policy]\nallow_runtime_rules=true\n[[rules]]\nid='a'\n[rules.trigger]\nkind='key'\nkey='KEY_A'\n[rules.action]\nkind='event'\nname='a'","system.toml").unwrap())
}
#[test]
fn layer_activation_rechecks_capabilities_of_existing_enabled_rules() {
    use crate::config::{Key, Layer, LayerActivation};
    let mut system = (*config()).clone();
    system.layers.push(Layer {
        id: "touch".into(),
        key: Key(125),
        toggle: false,
        activation: LayerActivation::Inactive,
    });
    system.rules[0].layer = Some("touch".into());
    system.rules[0].action = Action::Command {
        argv: vec!["/usr/bin/true".into()],
    };
    let caps = BTreeSet::from([Capability::Observations]);
    for activation in [LayerActivation::Active, LayerActivation::Physical] {
        let mut state = Rules::new(Some(Arc::new(system.clone())));
        assert!(
            state
                .prepare(
                    proposal(
                        1,
                        1,
                        vec![Change::Layer {
                            id: "touch".into(),
                            activation
                        }]
                    ),
                    &caps,
                    &[],
                    Duration::ZERO
                )
                .is_err()
        );
        assert!(!state.busy());
        assert_eq!(
            state.config.as_ref().unwrap().layers[0].activation,
            LayerActivation::Inactive
        );
    }
    let mut state = Rules::new(Some(Arc::new(system)));
    state
        .prepare(
            proposal(
                1,
                1,
                vec![Change::Layer {
                    id: "touch".into(),
                    activation: LayerActivation::Inactive,
                }],
            ),
            &caps,
            &[],
            Duration::ZERO,
        )
        .unwrap();
    assert!(state.busy());
}

#[test]
fn mutation_eligibility_requires_the_owning_active_fresh_desktop_and_grant() {
    let capabilities = BTreeSet::from([Capability::RuntimeRules]);
    assert!(eligible(
        Role::Agent,
        &capabilities,
        true,
        true,
        State::Desktop,
        false
    ));
    assert!(!eligible(
        Role::Inspect,
        &capabilities,
        true,
        true,
        State::Desktop,
        false
    ));
    assert!(!eligible(
        Role::Agent,
        &BTreeSet::new(),
        true,
        true,
        State::Desktop,
        false
    ));
    assert!(!eligible(
        Role::Agent,
        &capabilities,
        false,
        true,
        State::Desktop,
        false
    ));
    assert!(!eligible(
        Role::Agent,
        &capabilities,
        true,
        false,
        State::Desktop,
        false
    ));
    assert!(!eligible(
        Role::Agent,
        &capabilities,
        true,
        true,
        State::Desktop,
        true
    ));
    for state in [
        State::Unknown,
        State::Greeter,
        State::Locking,
        State::Locked,
    ] {
        assert!(!eligible(
            Role::Agent,
            &capabilities,
            true,
            true,
            state,
            false
        ));
    }
}
fn proposal(id: u64, revision: u64, changes: Vec<Change>) -> RuleProposal {
    RuleProposal {
        id,
        transaction: Transaction { revision, changes },
    }
}
fn disable(id: u64, revision: u64) -> RuleProposal {
    proposal(
        id,
        revision,
        vec![Change::Enabled {
            id: "a".into(),
            enabled: false,
        }],
    )
}
#[test]
fn protected_policy_and_action_capabilities_bound_rule_changes() {
    let mut denied = (*config()).clone();
    denied.policy.allow_runtime_rules = false;
    let mut state = Rules::new(Some(Arc::new(denied)));
    assert!(
        state
            .prepare(disable(1, 1), &BTreeSet::new(), &[], Duration::ZERO)
            .is_err()
    );
    assert!(state.pending.is_none());
    let mut state = Rules::new(Some(config()));
    let mut command = config().rules[0].clone();
    command.action = Action::Command {
        argv: vec!["/usr/bin/true".into()],
    };
    assert!(
        state
            .prepare(
                proposal(
                    1,
                    1,
                    vec![Change::Replace {
                        rule: Box::new(command.clone())
                    }]
                ),
                &BTreeSet::from([Capability::Observations]),
                &[],
                Duration::ZERO
            )
            .is_err()
    );
    command.enabled = false;
    let mut system = (*config()).clone();
    system.rules[0] = command;
    let mut state = Rules::new(Some(Arc::new(system)));
    assert!(
        state
            .prepare(
                proposal(
                    1,
                    1,
                    vec![Change::Enabled {
                        id: "a".into(),
                        enabled: true
                    }]
                ),
                &BTreeSet::from([Capability::Observations]),
                &[],
                Duration::ZERO
            )
            .is_err()
    );
    assert!(!state.config.as_ref().unwrap().rules[0].enabled);
}
#[test]
fn named_handler_metadata_and_effect_capabilities_must_match_approval() {
    let mut state = Rules::new(Some(config()));
    let mut rule = config().rules[0].clone();
    let requires = BTreeSet::from([crate::config::EffectKind::Observations]);
    rule.action = Action::Lua {
        handler: "approved".into(),
        requires: requires.clone(),
    };
    let caps = BTreeSet::from([Capability::Observations, Capability::LuaActions]);
    assert!(
        state
            .prepare(
                proposal(
                    1,
                    1,
                    vec![Change::Replace {
                        rule: Box::new(rule.clone())
                    }]
                ),
                &caps,
                &[],
                Duration::ZERO
            )
            .is_err()
    );
    let handler = crate::lua::HandlerRegistration {
        id: 1,
        name: "approved".into(),
        requires,
    };
    state
        .prepare(
            proposal(
                2,
                1,
                vec![Change::Replace {
                    rule: Box::new(rule),
                }],
            ),
            &caps,
            &[handler],
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        state.pending.as_ref().unwrap().config.rules[0].action,
        Action::Lua { .. }
    ));
}
#[test]
fn receipts_commit_atomically_only_after_send_with_exact_identity_revision_and_time() {
    let mut state = Rules::new(Some(config()));
    state
        .prepare(disable(1, 1), &BTreeSet::new(), &[], Duration::ZERO)
        .unwrap();
    let good = RuleReceipt {
        id: 1,
        revision: 2,
        error: None,
    };
    assert!(state.receipt(&good, Duration::ZERO).is_err());
    assert!(
        state
            .prepare(disable(2, 1), &BTreeSet::new(), &[], Duration::ZERO)
            .is_err()
    );
    state.pending.as_mut().unwrap().sent = true;
    assert!(
        state
            .receipt(
                &RuleReceipt {
                    id: 2,
                    ..good.clone()
                },
                Duration::ZERO
            )
            .is_err()
    );
    assert!(
        state
            .receipt(
                &RuleReceipt {
                    revision: 3,
                    ..good.clone()
                },
                Duration::ZERO
            )
            .is_err()
    );
    assert!(state.receipt(&good, RECEIPT_TIME).is_err());
    assert!(state.config.as_ref().unwrap().rules[0].enabled);
    state.receipt(&good, Duration::from_millis(1)).unwrap();
    assert!(!state.config.as_ref().unwrap().rules[0].enabled);
    assert_eq!(state.revision, 2);
    assert!(state.pending.is_none());
    assert!(
        state
            .prepare(
                disable(1, 2),
                &BTreeSet::new(),
                &[],
                Duration::from_millis(2)
            )
            .is_err()
    );
    state
        .prepare(
            proposal(
                2,
                2,
                vec![Change::Enabled {
                    id: "a".into(),
                    enabled: true,
                }],
            ),
            &BTreeSet::from([Capability::Observations]),
            &[],
            Duration::from_millis(2),
        )
        .unwrap();
    state.pending.as_mut().unwrap().sent = true;
    state
        .receipt(
            &RuleReceipt {
                id: 2,
                revision: 2,
                error: Some("rejected".into()),
            },
            Duration::from_millis(3),
        )
        .unwrap();
    assert!(!state.config.as_ref().unwrap().rules[0].enabled);
    assert_eq!(state.revision, 2);
}
#[test]
fn public_rule_packets_reject_privilege_fields_and_invalid_identities() {
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let request = Request::Rules {
        fence,
        proposal: disable(1, 1),
    };
    request.validate().unwrap();
    let mut value = serde_json::to_value(request).unwrap();
    value["seat"] = "other".into();
    assert!(serde_json::from_value::<Request>(value).is_err());
    assert!(
        Request::Rules {
            fence,
            proposal: disable(0, 1)
        }
        .validate()
        .is_err()
    );
    assert!(
        Reply::RulesChanged {
            fence,
            receipt: RuleReceipt {
                id: 1,
                revision: 2,
                error: Some("bad\nreceipt".into())
            }
        }
        .validate()
        .is_err()
    );
}
#[test]
fn rule_retirement_removes_unsent_offers_and_cancels_delivered_work() {
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let source = crate::input::Source {
        device: 1,
        generation: 1,
        seat: "seat0".into(),
    };
    let snapshot = crate::context::Snapshot {
        generation: 1,
        revision: 1,
        captured_at: Duration::ZERO,
        state: State::Desktop,
        outputs: vec![],
        windows: vec![],
        regions: vec![],
        secure_overlay: false,
    };
    let offer = |delivered| Offered {
        source: source.clone(),
        snapshot: snapshot.clone(),
        output: None,
        window: None,
        deadline: RECEIPT_TIME,
        granted: true,
        sent: delivered,
        delivered,
        cancelled: false,
        required: BTreeSet::from([Capability::Commands]),
    };
    let mut ledger = ActionLedger {
        pending: BTreeMap::from([((1, 1), offer(false)), ((2, 1), offer(true))]),
        seen: BTreeMap::from([(1, 1), (2, 1)]),
    };
    let event = crate::rules::Event::Matched {
        version: 1,
        id: 1,
        ordinal: 1,
        rule: "a".into(),
        source: source.clone(),
        at: Duration::ZERO,
        config_generation: 1,
        session_generation: 1,
        stage: crate::rules::Stage::Discrete,
        action: Action::Event { name: "a".into() },
        evidence: crate::rules::Evidence::Key { down: true },
        window: None,
        output: None,
        workspace: None,
    };
    let mut outgoing = VecDeque::from([
        Reply::Dispatch {
            fence,
            event: Box::new(event.clone()),
        },
        Reply::Observation {
            fence,
            event: Box::new(event),
        },
        Reply::Subscribed { fence },
    ]);
    let replies = retire_rule_actions(&mut ledger, &mut outgoing, fence);
    assert_eq!(
        replies,
        vec![Reply::ActionCancelled {
            fence,
            id: 2,
            ordinal: 1
        }]
    );
    assert_eq!(outgoing, VecDeque::from([Reply::Subscribed { fence }]));
    assert!(!ledger.pending.contains_key(&(1, 1)) && ledger.pending[&(2, 1)].cancelled);
    assert_eq!(ledger.seen.len(), 2);
}
