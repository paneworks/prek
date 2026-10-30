use super::*;

const BASE: &str = r#"
local p=require('prek')
local rule=p.rules.define{id='a',delivery='reserve',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='a'}}
"#;
fn load(script: &str) -> Runtime {
    Runtime::load(
        &format!("{BASE}\n{script}\nreturn p.config.new{{rules={{rule}}}}"),
        "rule-api.lua",
    )
    .unwrap()
}
fn ready(runtime: &mut Runtime) -> Result<Vec<LogRecord>, ConfigError> {
    runtime.dispatch(EventKind::Ready, &serde_json::json!({}))
}

#[test]
fn layer_queries_and_mixed_transactions_follow_acknowledgement_and_shared_revision() {
    let source = format!(
        r#"{BASE}
local tx
p.events.on('ready',function()
 assert(p.layers.get('missing')==nil and p.layers.revision()==p.rules.revision())
 local layers=p.layers.list();assert(layers[1].activation=='physical');layers[1].activation='active'
 assert(p.layers.get('touch').activation=='physical')
 assert(not pcall(function() local tx=p.layers.transaction();tx:activate_layer('missing');tx:commit() end))
 tx=p.layers.transaction();tx:activate_layer('touch');tx:disable('a');tx:commit()
 assert(p.layers.get('touch').activation=='physical' and p.rules.get('a').enabled)
end)
p.events.on('rules_changed',function()
 assert(p.layers.get('touch').activation=='active' and not p.rules.get('a').enabled)
 assert(p.layers.revision()=='2' and p.rules.revision()=='2' and tx:status().state=='accepted')
 tx=p.layers.transaction();tx:deactivate_layer('touch');tx:reset_layer('touch');tx:commit()
end)
p.events.on('reload',function()assert(p.layers.get('touch').activation=='physical' and p.layers.revision()=='3')end)
return p.config.new{{layers={{p.layers.define{{id='touch',key='KEY_LEFTMETA',toggle=true}}}},rules={{rule}}}}
"#
    );
    let mut runtime = Runtime::load(&source, "layers.lua").unwrap();
    let mut engine = crate::rules::Engine::new(runtime.config.clone()).unwrap();
    runtime.enable_rule_transactions();
    ready(&mut runtime).unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    let revision = engine
        .transact_rules(&proposal.transaction, Duration::ZERO)
        .unwrap();
    runtime
        .acknowledge_rules(RuleReceipt {
            id: proposal.id,
            revision,
            error: None,
        })
        .unwrap();
    runtime
        .dispatch(EventKind::RulesChanged, &serde_json::json!({}))
        .unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    let revision = engine
        .transact_rules(&proposal.transaction, Duration::ZERO)
        .unwrap();
    runtime
        .acknowledge_rules(RuleReceipt {
            id: proposal.id,
            revision,
            error: None,
        })
        .unwrap();
    runtime
        .dispatch(EventKind::Reload, &serde_json::json!({}))
        .unwrap();
    for query in ["list()", "get('touch')", "revision()", "transaction()"] {
        assert!(
            Definition::compile(
                &format!("local p=require('prek');p.layers.{query};return {{version=1}}"),
                "phase.lua"
            )
            .is_err()
        );
    }
}

#[test]
fn private_proposals_update_owned_queries_only_after_the_actual_engine_receipt() {
    let mut runtime = load(
        r#"
local tx
p.events.on('ready',function()
  assert(p.rules.revision()=='1' and p.rules.get('missing')==nil)
  local copy=p.rules.list(); copy[1].enabled=false
  assert(p.rules.get('a').enabled)
  tx=p.rules.transaction(); tx.id=999; tx.revision=999
  tx:disable('a'); tx:commit()
  assert(tx:status().state=='pending' and p.rules.get('a').enabled)
end)
p.events.on('reload',function()
  assert(tx:status().state=='accepted' and tx:status().revision=='2')
  assert(p.rules.revision()=='2' and not p.rules.get('a').enabled)
  local copy=tx:status(); copy.state='forged'
  assert(tx:status().state=='accepted')
end)
"#,
    );
    let mut engine = crate::rules::Engine::new(runtime.config.clone()).unwrap();
    runtime.enable_rule_transactions();
    ready(&mut runtime).unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    assert_eq!(proposal.id, 1);
    assert_eq!(proposal.transaction.revision, 1);
    assert!(runtime.take_rule_proposal().is_none());
    assert!(
        runtime
            .acknowledge_rules(RuleReceipt {
                id: 999,
                revision: 2,
                error: None
            })
            .is_err()
    );
    assert!(
        runtime
            .acknowledge_rules(RuleReceipt {
                id: 1,
                revision: 3,
                error: None
            })
            .is_err()
    );
    assert!(runtime.host.borrow().rules.config.as_ref().unwrap().rules[0].enabled);
    let revision = engine
        .transact_rules(&proposal.transaction, Duration::from_millis(1))
        .unwrap();
    runtime
        .acknowledge_rules(RuleReceipt {
            id: proposal.id,
            revision,
            error: None,
        })
        .unwrap();
    runtime
        .dispatch(EventKind::Reload, &serde_json::json!({}))
        .unwrap();
    assert!(!engine.config().rules[0].enabled);
    assert_eq!(engine.generation(), 1);
    assert!(
        runtime
            .acknowledge_rules(RuleReceipt {
                id: 1,
                revision,
                error: None
            })
            .is_err()
    );
}

#[test]
fn phases_invalid_batches_handler_forgery_and_builder_quotas_are_enforced() {
    for call in [
        "p.rules.list()",
        "p.rules.get('a')",
        "p.rules.revision()",
        "p.rules.transaction()",
    ] {
        assert!(
            Runtime::load(
                &format!("{BASE}\n{call}\nreturn p.config.new{{rules={{rule}}}}"),
                "configuration.lua"
            )
            .is_err()
        );
    }
    let mut runtime = load(
        r#"
p.events.on('ready',function()
  local tx=p.rules.transaction()
  tx:disable('a'); tx:remove('missing')
  assert(not pcall(function()tx:commit()end))
  assert(tx:status().state=='open' and p.rules.get('a').enabled)
  tx:cancel(); tx:cancel()
  assert(not pcall(function()tx:enable('a')end))
  local forged=p.rules.get('a')
  forged.action={kind='lua',handler='not-approved',requires={'commands'}}
  local bad=p.rules.transaction(); bad:replace(forged)
  assert(not pcall(function()bad:commit()end)); bad:cancel()
  local handles={}
  for i=1,64 do handles[i]=p.rules.transaction() end
  assert(not pcall(function()p.rules.transaction()end))
  handles[1]:cancel()
  local reuse=p.rules.transaction(); reuse:cancel()
  assert(not pcall(function()handles[1]:remove('a')end))
end)
"#,
    );
    runtime.enable_rule_transactions();
    ready(&mut runtime).unwrap();
    assert!(runtime.take_rule_proposal().is_none());
    let mut unavailable = load(
        "p.events.on('ready',function()assert(not pcall(function()p.rules.transaction()end))end)",
    );
    ready(&mut unavailable).unwrap();
}

#[test]
fn failed_callbacks_and_self_cancelled_tasks_discard_new_rule_proposals() {
    let mut failed = load(
        "p.events.on('ready',function()local tx=p.rules.transaction();tx:disable('a');tx:commit();error('fail after proposal')end)",
    );
    failed.enable_rule_transactions();
    assert!(ready(&mut failed).is_err());
    assert!(failed.take_rule_proposal().is_none());
    assert!(failed.host.borrow().rules.config.as_ref().unwrap().rules[0].enabled);
    let mut cancelled = load(
        r#"
local task
task=p.scheduler.spawn(function()
  local tx=p.rules.transaction(); tx:disable('a'); tx:commit(); task:cancel()
end)
"#,
    );
    cancelled.enable_rule_transactions();
    cancelled.set_clock(Duration::ZERO).unwrap();
    cancelled
        .tick(Duration::from_millis(1), &serde_json::json!({}))
        .unwrap();
    assert!(cancelled.take_rule_proposal().is_none());
    assert!(cancelled.host.borrow().rules.pending.is_none());
}

#[test]
fn rejected_and_stale_receipts_do_not_invent_an_active_rule_view() {
    let mut runtime = load(
        r#"
local tx
p.events.on('ready',function()
 tx=p.rules.transaction();tx:disable('a');tx:commit()
 assert(not pcall(function()local second=p.rules.transaction();second:disable('a');second:commit()end))
end)
p.events.on('reload',function()
 assert(tx:status().state=='rejected' and tx:status().error=='owner expired')
 assert(not pcall(function()p.rules.get('a')end))
end)
"#,
    );
    runtime.enable_rule_transactions();
    ready(&mut runtime).unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    runtime
        .acknowledge_rules(RuleReceipt {
            id: proposal.id,
            revision: 0,
            error: Some("owner expired".into()),
        })
        .unwrap();
    runtime
        .dispatch(EventKind::Reload, &serde_json::json!({}))
        .unwrap();
}

#[test]
fn revisions_remain_exact_above_lua_numeric_precision() {
    let mut runtime = load(
        "p.events.on('ready',function()assert(p.rules.revision()=='18446744073709551614')end)",
    );
    runtime.host.borrow_mut().rules.revision = u64::MAX - 1;
    ready(&mut runtime).unwrap();
}

#[test]
fn named_modes_are_owned_acknowledged_and_share_atomic_rule_layer_transactions() {
    let source = format!(
        r#"{BASE}
local tx
p.events.on('ready',function()
 assert(p.modes.current().id=='normal' and p.modes.get('missing')==nil)
 assert(p.modes.revision()==p.rules.revision())
 local copy=p.modes.list();copy[1].active=false;assert(p.modes.current().id=='normal')
 local missing=p.modes.transaction();missing:select_mode('missing')
 assert(not pcall(function()missing:commit()end));missing:cancel()
 tx=p.modes.transaction();tx:select_mode('navigate');tx:disable('a');tx:activate_layer('touch');tx:commit()
 assert(p.modes.current().id=='normal' and p.rules.get('a').enabled)
end)
p.events.on('rules_changed',function()
 assert(tx:status().state=='accepted' and p.modes.current().id=='navigate')
 assert(p.modes.revision()=='2' and not p.rules.get('a').enabled)
 assert(p.layers.get('touch').activation=='active')
 tx=p.modes.transaction();tx:reset_mode();tx:commit()
end)
p.events.on('reload',function()assert(p.modes.current()==nil and p.modes.revision()=='3')end)
return p.config.new{{modes={{p.modes.define{{id='normal',active=true}},p.modes.define{{id='navigate'}}}},layers={{p.layers.define{{id='touch',key='KEY_LEFTMETA'}}}},rules={{rule}}}}
"#
    );
    let mut runtime = Runtime::load(&source, "modes.lua").unwrap();
    let mut engine = crate::rules::Engine::new(runtime.config.clone()).unwrap();
    runtime.enable_rule_transactions();
    ready(&mut runtime).unwrap();
    for event in [EventKind::RulesChanged, EventKind::Reload] {
        let proposal = runtime.take_rule_proposal().unwrap();
        let revision = engine
            .transact_rules(&proposal.transaction, Duration::ZERO)
            .unwrap();
        runtime
            .acknowledge_rules(RuleReceipt {
                id: proposal.id,
                revision,
                error: None,
            })
            .unwrap();
        runtime.dispatch(event, &serde_json::json!({})).unwrap();
    }
    for query in [
        "list()",
        "get('normal')",
        "current()",
        "revision()",
        "transaction()",
    ] {
        assert!(
            Definition::compile(
                &format!("local p=require('prek');p.modes.{query};return {{version=1}}"),
                "phase.lua"
            )
            .is_err()
        );
    }
    let mut unavailable = load(
        "p.events.on('ready',function()assert(p.modes.current()==nil and #p.modes.list()==0);assert(not pcall(p.modes.transaction))end)",
    );
    ready(&mut unavailable).unwrap();
}

#[test]
fn user_shortcuts_layer_over_system_rules_and_restore_them_when_removed() {
    let source = format!(
        "{BASE}\nlocal b=p.rules.define{{id='b',priority=1,delivery='reserve',trigger=p.triggers.key{{key='KEY_B'}},action=p.actions.event{{name='b'}}}}\nreturn p.config.new{{rules={{rule,b}}}}"
    );
    let mut runtime = Runtime::load(&source, "shortcuts.lua").unwrap();
    let mut engine = crate::rules::Engine::new(runtime.config.clone()).unwrap();
    let overlay = |rules: &str| {
        crate::lua::compile_shortcuts(&crate::lua::Sources::single(
            format!("local p=require('prek')\nreturn p.shortcuts.new{{{rules}}}"),
            "init.lua".into(),
        ))
        .unwrap()
    };
    let rule = |id: &str, key: &str, name: &str| {
        format!(
            "p.rules.define{{id='{id}',trigger=p.triggers.key{{key='{key}'}},action=p.actions.event{{name='{name}'}}}},"
        )
    };
    let describe = |proposal: &RuleProposal| -> Vec<String> {
        proposal
            .transaction
            .changes
            .iter()
            .map(|change| match change {
                Change::Replace { rule } => format!("replace {}", rule.id),
                Change::Remove { id } => format!("remove {id}"),
                Change::Enabled { id, enabled } => format!("{id} {enabled}"),
                _ => "other".into(),
            })
            .collect()
    };
    let layered = overlay(&format!(
        "{}{}disable={{'b'}}",
        rule("a", "KEY_X", "a2"),
        rule("c", "KEY_C", "c")
    ));
    assert!(runtime.submit_shortcuts(layered.clone()).is_err());
    runtime.enable_rule_transactions();
    assert!(runtime.submit_shortcuts(layered.clone()).unwrap());
    let proposal = runtime.take_rule_proposal().unwrap();
    assert_eq!(describe(&proposal), ["replace a", "replace c", "b false"]);
    let revision = engine
        .transact_rules(&proposal.transaction, Duration::ZERO)
        .unwrap();
    runtime
        .acknowledge_rules(RuleReceipt {
            id: proposal.id,
            revision,
            error: None,
        })
        .unwrap();
    let applied = runtime.take_shortcut_outcome().unwrap();
    assert_eq!(applied.message, "shortcuts applied");
    assert_eq!(
        applied.fields,
        serde_json::json!({"rules": 2, "disabled": 1})
    );
    assert!(!runtime.submit_shortcuts(layered).unwrap());
    assert_eq!(
        runtime.take_shortcut_outcome().unwrap().message,
        "shortcuts unchanged"
    );

    // Dropping a shortcut restores the system rule; dropping a disable restores it.
    assert!(
        runtime
            .submit_shortcuts(overlay(&rule("c", "KEY_D", "c")))
            .unwrap()
    );
    let proposal = runtime.take_rule_proposal().unwrap();
    assert_eq!(describe(&proposal), ["replace a", "replace c", "b true"]);
    let revision = engine
        .transact_rules(&proposal.transaction, Duration::ZERO)
        .unwrap();
    runtime
        .acknowledge_rules(RuleReceipt {
            id: proposal.id,
            revision,
            error: None,
        })
        .unwrap();
    runtime.take_shortcut_outcome().unwrap();

    for (text, message) in [
        ("disable={'missing'}".to_string(), "unknown runtime rule missing"),
        (
            "p.rules.define{id='d',devices={'touch'},trigger=p.triggers.key{key='KEY_D'},action=p.actions.event{name='d'}}"
                .to_string(),
            "exceeds system policy",
        ),
        (format!("{}{}", rule("e", "KEY_E", "e"), rule("e", "KEY_F", "e")), "duplicate shortcut e"),
    ] {
        let error = runtime.submit_shortcuts(overlay(&text)).unwrap_err().to_string();
        assert!(error.contains(message), "{text}: {error}");
    }

    // A refused proposal reports why and keeps the applied shortcuts.
    assert!(runtime.submit_shortcuts(overlay("")).unwrap());
    let proposal = runtime.take_rule_proposal().unwrap();
    assert_eq!(describe(&proposal), ["remove c"]);
    runtime
        .acknowledge_rules(RuleReceipt {
            id: proposal.id,
            revision,
            error: Some("worker refused".into()),
        })
        .unwrap();
    let rejected = runtime.take_shortcut_outcome().unwrap();
    assert_eq!(rejected.message, "shortcuts rejected");
    assert_eq!(
        rejected.fields,
        serde_json::json!({"detail": "worker refused"})
    );
    assert!(runtime.submit_shortcuts(overlay("")).unwrap());
    assert_eq!(
        describe(&runtime.take_rule_proposal().unwrap()),
        ["remove c"]
    );
}
