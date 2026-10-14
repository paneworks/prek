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
