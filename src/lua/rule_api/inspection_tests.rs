use crate::lua::{Definition, EventKind};

#[test]
fn configuration_and_policy_are_owned_acknowledged_copies() {
    let definition = Definition::compile(r#"
local p=require('prek')
local tx
p.events.on('ready',function()
 local v=p.config.current();local policy=p.policy.current()
 assert(v.revision=='1' and policy.revision=='1')
 assert(v.config.rules[1].enabled and not policy.policy.allow_input_events)
 v.config.rules[1].enabled=false;policy.policy.allow_input_events=true
 assert(p.config.current().config.rules[1].enabled and not p.policy.current().policy.allow_input_events)
 tx=p.rules.transaction();tx:disable('sample_double_tap');tx:commit()
 assert(p.config.current().revision=='1' and p.config.current().config.rules[1].enabled)
end)
p.events.on('rules_changed',function()
 local v=p.config.current();assert(v.revision=='2' and not v.config.rules[1].enabled)
 assert(p.policy.current().revision=='2' and not p.policy.current().policy.allow_input_events)
 p.log.info('acknowledged')
end)
local c=p.config.preset('desktop');c.rules[1].enabled=true
return c
"#, "inspection.lua").unwrap();
    let mut runtime = definition.instantiate().unwrap();
    runtime.enable_rule_transactions();
    runtime
        .dispatch(EventKind::Ready, &serde_json::json!({}))
        .unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    runtime
        .acknowledge_rules(crate::lua::RuleReceipt {
            id: proposal.id,
            revision: 2,
            error: None,
        })
        .unwrap();
    assert_eq!(
        runtime
            .dispatch(EventKind::RulesChanged, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "acknowledged"
    );
}

#[test]
fn inspection_requires_runtime_and_retires_an_unacknowledged_configuration() {
    for namespace in ["config", "policy"] {
        assert!(
            Definition::compile(
                &format!("require('prek').{namespace}.current();return {{version=1}}"),
                "phase.lua"
            )
            .is_err()
        );
    }
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 local tx=p.rules.transaction();tx:select_mode('normal');tx:commit()
end)
p.events.on('rules_changed',function()
 assert(p.config.current()==nil and p.policy.current()==nil)
end)
return {version=1,modes={{id='normal'}}}
"#,
        "retired-inspection.lua",
    )
    .unwrap();
    let mut runtime = definition.instantiate().unwrap();
    runtime.enable_rule_transactions();
    runtime
        .dispatch(EventKind::Ready, &serde_json::json!({}))
        .unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    runtime
        .acknowledge_rules(crate::lua::RuleReceipt {
            id: proposal.id,
            revision: 5,
            error: Some("worker revision advanced".into()),
        })
        .unwrap();
    runtime
        .dispatch(EventKind::RulesChanged, &serde_json::json!({}))
        .unwrap();
}
