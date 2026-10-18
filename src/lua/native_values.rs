use super::*;

fn expanded_config() -> &'static str {
    r#"
local p=require('prek')
local word=string.rep('x',8192)
local argv={}
for i=1,2049 do argv[i]=word end
local c={version=1,rules={{id='expanded',trigger={kind='key',key='KEY_A'},action={kind='command',argv=argv}}}}
"#
}

#[test]
fn returned_configuration_is_preflighted_before_native_allocation() {
    let error = Definition::compile(
        &format!("{} return c", expanded_config()),
        "expanded-config.lua",
    )
    .unwrap_err();
    assert!(error.0.contains("Lua value exceeds expanded"), "{error}");
}

#[test]
fn exported_configuration_is_preflighted_before_native_allocation() {
    let source = format!(
        "{} p.config.export(c);return {{version=1}}",
        expanded_config()
    );
    let error = Definition::compile(&source, "expanded-export.lua").unwrap_err();
    assert!(error.0.contains("Lua value exceeds expanded"), "{error}");
}

#[test]
fn every_configuration_decoder_preflights_collection_cardinality() {
    for (field, limit) in [
        ("rules", crate::config::MAX_RULES),
        ("devices", crate::config::MAX_SELECTORS),
        ("layers", crate::config::MAX_LAYERS),
        ("shapes", crate::config::MAX_SHAPES),
        ("modes", crate::config::MAX_MODES),
        ("recognizers", crate::recognition::MAX_RECOGNIZERS),
    ] {
        for decode in [
            "return c",
            "p.config.export(c);return {version=1}",
            "return p.config.validate(c)",
            "p.replay.new(c);return {version=1}",
            "p.replay.routing(c);return {version=1}",
        ] {
            let source = format!(
                "local p=require('prek');local item={{}};local collection={{}};\
                 for i=1,{limit} do collection[i]=item end;collection.extra=item;\
                 local c={{version=1,{field}=collection}};{decode}"
            );
            let error = Definition::compile(&source, "collection-cardinality.lua").unwrap_err();
            assert!(
                error.0.contains("configuration collection limit exceeded"),
                "{field}, {decode}: {error}"
            );
        }
    }
}

#[test]
fn collection_preflight_does_not_lower_native_schema_limits() {
    Definition::compile(
        &format!(
            "local p=require('prek');local c={{version=1,rules={{}}}};\
             for i=1,{} do c.rules[i]={{id='r'..i,trigger=p.triggers.key{{key='KEY_A'}},\
             action=p.actions.event{{name='bounded'}}}} end;return p.config.validate(c)",
            crate::config::MAX_RULES
        ),
        "maximum-rules.lua",
    )
    .unwrap();
}

#[test]
fn expanded_action_plans_discard_logs_and_retire_the_callback_runtime() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
local action=p.actions.define('expanded',{requires={'commands'}},function()
 p.log.info('uncommitted')
 local argv={};local word=string.rep('x',1024)
 for i=1,1024 do argv[i]=word end
 return p.actions.command{argv=argv}
end)
return {version=1,rules={{id='expanded',trigger={kind='key',key='KEY_A'},action=action}}}
"#,
        "expanded-actions.lua",
    )
    .unwrap();
    let mut runtime = definition.instantiate().unwrap();
    let action = runtime.config.rules[0].action.clone();
    let error = runtime
        .resolve_action(&action, &serde_json::Value::Null)
        .unwrap_err();
    assert!(error.0.contains("Lua value exceeds expanded"), "{error}");
    assert!(runtime.host.borrow().logs.is_empty());
    assert!(
        runtime
            .resolve_action(&action, &serde_json::Value::Null)
            .is_err()
    );
}

#[test]
fn rejected_expanded_rule_values_do_not_partially_edit_an_open_transaction() {
    let definition = Definition::compile(r#"
local p=require('prek')
p.events.on('ready',function()
 local tx=p.rules.transaction()
 local argv={};local word=string.rep('x',1024)
 for i=1,1024 do argv[i]=word end
 local ok,err=pcall(function()tx:replace{id='expanded',trigger={kind='key',key='KEY_A'},action=p.actions.command{argv=argv}}end)
 assert(not ok and tostring(err):find('Lua value exceeds expanded',1,true))
 assert(tx:status().state=='open')
 tx:replace{id='valid',trigger={kind='key',key='KEY_A'},action=p.actions.event{name='valid'}}
 tx:commit()
end)
return {version=1,policy={allow_runtime_rules=true}}
"#, "expanded-transaction.lua").unwrap();
    let mut runtime = definition.instantiate().unwrap();
    runtime.enable_rule_transactions();
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    assert_eq!(proposal.transaction.changes.len(), 1);
    let crate::rule_transactions::Change::Replace { rule } = &proposal.transaction.changes[0]
    else {
        panic!("missing valid replacement")
    };
    assert_eq!(rule.id, "valid");
}

#[test]
fn configuration_validation_copies_shared_data_and_applies_rust_defaults() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
local trigger=p.triggers.key{key='KEY_A'}
local c={version=1,rules={
 {id='one',trigger=trigger,action=p.actions.event{name='one'}},
 {id='two',trigger=trigger,action=p.actions.event{name='two'}},
}}
local checked=p.config.validate(c)
assert(checked~=c and checked.rules[1].trigger~=checked.rules[2].trigger)
checked.rules[1].trigger.key='KEY_B'
assert(checked.rules[2].trigger.key~='KEY_B' and trigger.key=='KEY_A')
assert(type(checked.timing.join_window)=='string')
return checked
"#,
        "validated-copy.lua",
    )
    .unwrap();
    let config = definition.validate().unwrap();
    assert_eq!(
        config.rules[0].trigger,
        crate::config::Trigger::Key {
            key: crate::config::Key(48)
        }
    );
    assert_eq!(
        config.rules[1].trigger,
        crate::config::Trigger::Key {
            key: crate::config::Key(30)
        }
    );
}

#[test]
fn validation_rejects_invalid_models_without_mutating_the_callers_data() {
    Definition::compile(r#"
local p=require('prek')
for _,c in ipairs({{version=1,unknown=true},{version=1,policy={allow_input_events='false'}},{version=1,timing={join_window='-1ms'}}})do
 assert(not pcall(p.config.validate,c))
end
local c={version=1}
c.rules={c}
local ok,err=pcall(p.config.validate,c)
assert(not ok and tostring(err):find('Lua value exceeds expanded',1,true))
assert(c.rules[1]==c)
return p.config.validate{version=1}
"#, "validation-errors.lua").unwrap();
}

#[test]
fn model_validation_does_not_approve_an_unregistered_lua_action() {
    assert!(Definition::compile(r#"
local p=require('prek')
return p.config.validate{version=1,rules={{id='unapproved',trigger={kind='key',key='KEY_A'},action={kind='lua',handler='missing',requires={'commands'}}}}}
"#, "unapproved-validated.lua").is_err());
}

#[test]
fn configuration_validation_is_not_a_runtime_reload_or_permission_grant() {
    let mut runtime = Runtime::load(
        r#"
local p=require('prek')
p.events.on('ready',function()
 assert(not pcall(p.config.validate,{version=1,policy={allow_power_actions=true}}))
 p.log.info('configuration-only')
end)
return {version=1}
"#,
        "validation-phase.lua",
    )
    .unwrap();
    let logs = runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    assert_eq!(logs[0].message, "configuration-only");
    assert!(!runtime.config.policy.allow_power_actions);
}

#[test]
fn the_shipped_validation_recipe_matches_the_native_model() {
    let definition = Definition::compile(
        include_str!("../../examples/lua/config_validation.lua"),
        "config-validation-example.lua",
    )
    .unwrap();
    let mut expected = Config::preset("desktop").unwrap();
    expected.timing.tap_gap = crate::config::Millis(350);
    assert_eq!(definition.validate().unwrap(), expected);
}

#[test]
fn native_identifier_bounds_fail_without_echoing_large_arguments() {
    let mut runtime = Runtime::load(
        r#"
local p=require('prek')
local ok,err=pcall(p.config.preset,string.rep('p',128*1024))
assert(not ok and #tostring(err)<4096)
p.events.on('ready',function()
 local tx=p.rules.transaction()
 local ok,err=pcall(function()tx:remove(string.rep('r',128*1024))end)
 assert(not ok and #tostring(err)<4096 and tx:status().state=='open')
 tx:cancel()
end)
return {version=1,policy={allow_runtime_rules=true}}
"#,
        "bounded-identifiers.lua",
    )
    .unwrap();
    runtime.enable_rule_transactions();
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    assert!(runtime.take_rule_proposal().is_none());
}

#[test]
fn every_live_namespace_function_has_a_public_manifest_entry() {
    Definition::compile(r#"
local p=require('prek')
local listed={}
for _,entry in ipairs(p.api.symbols)do listed[entry.name]=true end
for namespace,api in pairs(p)do
 if type(api)=='table' and namespace~='api' then
  for name,value in pairs(api)do
   if type(value)=='function' then assert(listed[namespace..'.'..name],namespace..'.'..name..' is undocumented')end
  end
 end
end
return {version=1}
"#, "live-export-parity.lua").unwrap();
}
