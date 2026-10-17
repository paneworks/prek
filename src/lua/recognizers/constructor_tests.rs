use super::*;
use crate::config::{Millis, Trigger};

const DECLARE: &str = r#"
local trigger=p.recognizers.define('hold',{
 selectors={'keys'},inputs={'key'},reply_within='250ms',wake='20ms',
},function(ticket) assert(ticket.cause=='frame' or ticket.cause=='timer' or ticket.cause=='cancel'); return nil end)
"#;

#[test]
fn source_constructor_compiles_nested_callbacks_with_exact_snapshot_and_no_unfenced_execution() {
    let source = include_str!("../../../examples/lua/composed_recognizers.lua").replace(
        "steps = {first, last}",
        "steps = {first, p.triggers.sequence {steps={last,last},timeout='500ms'}}",
    );
    let definition = Definition::compile(&source, "nested-source.lua").unwrap();
    let config = definition.validate().unwrap();
    assert_eq!(definition.recognizers.len(), 2);
    assert_eq!(
        config.rules[0]
            .trigger
            .recognizers()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "last", "last"]
    );
    assert_eq!(
        Config::parse(&toml::to_string(&config).unwrap()).unwrap(),
        config
    );
    assert!(crate::rules::Engine::new(config).is_err());
    let portable: Definition =
        serde_json::from_slice(&serde_json::to_vec(&definition).unwrap()).unwrap();
    portable.instantiate().unwrap();
    let mut missing = portable;
    missing.recognizers.pop();
    assert!(missing.validate().is_err());
}
fn source(body: &str) -> String {
    format!(
        r#"local p=require('prek'); {body}
return p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keys'}}}},devices={{keys={{keys={{'KEY_A'}}}}}},
rules={{p.rules.define{{id='hold-rule',trigger=trigger,action=p.actions.event{{name='hold'}}}}}}}}
"#
    )
}
fn compile(body: &str) -> Definition {
    Definition::compile(&source(body), "constructor.lua").unwrap()
}

#[test]
fn source_constructor_records_canonical_binding_and_native_trigger() {
    let definition = compile(DECLARE);
    let config = definition.validate().unwrap();
    assert_eq!(definition.recognizers.len(), 1);
    assert_eq!(
        config.recognizers,
        vec![definition.recognizers[0].definition.clone()]
    );
    let binding = &definition.recognizers[0];
    assert_eq!(binding.subscription, 1);
    assert_eq!(binding.definition.name, "hold");
    assert!(!binding.definition.continuous);
    assert_eq!(binding.definition.reply_within, Millis(250));
    assert_eq!(
        definition.registrations[0].recognizer.as_ref(),
        Some(&binding.definition)
    );
    assert_eq!(
        config.rules[0].trigger,
        Trigger::Custom {
            recognizer: binding.definition.clone(),
            timeout: Millis(1000),
            wake: Some(Millis(20))
        }
    );
    let snapshot: Definition =
        serde_json::from_slice(&serde_json::to_vec(&definition).unwrap()).unwrap();
    assert_eq!(snapshot, definition);
    snapshot.instantiate().unwrap();
}

#[test]
fn nested_native_declarations_still_require_the_exact_original_source_binding() {
    let definition = compile(DECLARE);
    let mut config = definition.validate().unwrap();
    let mut changed = config.rules[0].trigger.clone();
    let Trigger::Custom { recognizer, .. } = &mut changed else {
        unreachable!()
    };
    recognizer.reply_within = Millis(100);
    config.rules[0].trigger = Trigger::Sequence {
        steps: vec![
            Trigger::Key {
                key: crate::config::Key(30),
            },
            changed,
        ],
        timeout: Millis(1000),
    };
    assert!(
        super::validate(&definition.recognizers, &definition.registrations, &config)
            .unwrap_err()
            .0
            .contains("binding differs from its native rule declaration")
    );
}

#[test]
fn unused_source_declarations_survive_model_transport_and_cannot_lose_their_binding() {
    let definition = Definition::compile(&format!(r#"local p=require('prek');{DECLARE}
return p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keys'}}}},devices={{keys={{keys={{'KEY_A'}}}}}}}}
"#), "unused.lua").unwrap();
    let config = definition.validate().unwrap();
    assert!(config.rules.is_empty());
    assert_eq!(
        config.recognizers,
        vec![definition.recognizers[0].definition.clone()]
    );
    assert_eq!(
        Config::parse(&toml::to_string(&config).unwrap()).unwrap(),
        config
    );
    definition.instantiate().unwrap();
    assert!(super::super::export_config(&config).is_err());
    for change in ["missing", "changed", "unbound"] {
        let mut changed = definition.clone();
        let mut model = config.clone();
        match change {
            "missing" => model.recognizers.clear(),
            "changed" => model.recognizers[0].reply_within = Millis(1),
            "unbound" => changed.recognizers.clear(),
            _ => unreachable!(),
        }
        changed.model = toml::to_string(&model).unwrap();
        assert!(changed.validate().is_err(), "{change}");
        assert!(changed.instantiate().is_err(), "{change}");
    }
}

#[test]
fn declaration_registry_rejects_duplicates_conflicts_and_permission_expansion() {
    let mut config = compile(DECLARE).validate().unwrap();
    let declaration = config.recognizers[0].clone();
    config.recognizers.push(declaration.clone());
    assert!(config.validate().is_err());
    config.recognizers.pop();
    config.recognizers[0].continuous = true;
    assert!(config.validate().is_err());
    config.rules.clear();
    config.recognizers[0] = declaration;
    config.recognizers[0].selectors.insert("foreign".into());
    assert!(config.validate().is_err());
    config.recognizers[0].selectors.remove("foreign");
    config.policy.allow_input_events = false;
    assert!(config.validate().is_err());
}

#[test]
fn lua_rule_preparation_accepts_unused_source_definition_and_rejects_a_forged_name() {
    let source = format!(
        r#"local p=require('prek');{DECLARE}
p.events.on('ready',function()
 local function rule()return p.rules.define{{id='dynamic',trigger=trigger,action=p.actions.event{{name='dynamic'}}}}end
 trigger.recognizer.name='forged'
 local bad=p.rules.transaction();bad:replace(rule())
 assert(not pcall(function()bad:commit()end))
 assert(bad:status().state=='open');bad:cancel()
 trigger.recognizer.name='hold'
 local good=p.rules.transaction();good:replace(rule());good:commit()
 assert(good:status().state=='pending' and #p.rules.list()==0)
end)
return p.config.new{{policy={{allow_runtime_rules=true,allow_input_events=true,allowed_devices={{'keys'}}}},devices={{keys={{keys={{'KEY_A'}}}}}}}}
"#
    );
    let mut runtime = Definition::compile(&source, "dynamic-source.lua")
        .unwrap()
        .instantiate()
        .unwrap();
    runtime.enable_rule_transactions();
    runtime
        .dispatch(EventKind::Ready, &serde_json::json!({}))
        .unwrap();
    let proposal = runtime.take_rule_proposal().unwrap();
    assert_eq!(proposal.transaction.revision, 1);
    let next = proposal.transaction.prepare(&runtime.config, 1).unwrap();
    assert_eq!(next.rules.len(), 1);
    assert_eq!(next.recognizers, runtime.config.recognizers);
}

#[test]
fn private_declaration_is_not_an_alias_of_options_or_the_returned_trigger() {
    let definition = compile(
        r#"
local options={selectors={'keys'},inputs={'key'},reply_within='250ms'}
local trigger=p.recognizers.define('hold',options,function()return nil end)
options.selectors[1]='foreign'; options.inputs[1]='touch';options.reply_within='1ms'
"#,
    );
    assert_eq!(
        definition.recognizers[0].definition.selectors,
        BTreeSet::from(["keys".into()])
    );
    assert_eq!(
        definition.recognizers[0].definition.inputs,
        BTreeSet::from([recognition::InputKind::Key])
    );
    for change in [
        "trigger.recognizer.selectors[1]='foreign'",
        "trigger.recognizer.inputs[1]='touch'",
        "trigger.recognizer.name='foreign'",
        "trigger.recognizer.reply_within='1ms'",
        "trigger.recognizer.continuous=true",
    ] {
        assert!(
            Definition::compile(&source(&format!("{DECLARE}\n{change}")), "mutated.lua").is_err(),
            "{change}"
        );
    }
    // Rule-local timing remains an ordinary validated native property.
    let definition = compile(&format!(
        "{DECLARE};trigger.timeout='2s';trigger.wake='25ms'"
    ));
    assert!(matches!(
        definition.validate().unwrap().rules[0].trigger,
        Trigger::Custom {
            timeout: Millis(2000),
            wake: Some(Millis(25)),
            ..
        }
    ));
}

#[test]
fn source_metadata_cannot_be_removed_rebound_or_forged_in_an_approved_snapshot() {
    let original = compile(DECLARE);
    let mut changed = original.clone();
    changed.recognizers.clear();
    assert!(changed.validate().is_err());
    let mut changed = original.clone();
    changed.recognizers[0].subscription = 2;
    assert!(changed.validate().is_err());
    let mut changed = original.clone();
    changed.recognizers[0].definition.continuous = true;
    assert!(changed.validate().is_err());
    let mut changed = original.clone();
    changed.registrations[0].recognizer = None;
    assert!(changed.validate().is_err());
    assert!(changed.instantiate().is_err());
    let mut changed = original.clone();
    changed.source = changed.source.replace("'hold'", "'other'");
    assert!(changed.instantiate().is_err());
    let mut changed = original;
    changed.registrations[0].active = false;
    assert!(changed.validate().is_err());
}

#[test]
fn callback_is_private_to_ticket_dispatch_and_generic_input_still_works() {
    let definition = compile(&format!(
        "{DECLARE}\np.events.on('input',function()p.log.info('ordinary')end)"
    ));
    let mut runtime = definition.instantiate().unwrap();
    assert_eq!(
        runtime
            .dispatch(EventKind::Input, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "ordinary"
    );
}

#[test]
fn declarations_need_protected_selectors_and_each_custom_rule_needs_its_callback() {
    for changed in [
        source(DECLARE).replace("allow_input_events=true", "allow_input_events=false"),
        source(DECLARE).replace("allowed_devices={'keys'}", "allowed_devices={}"),
        source(DECLARE).replace("selectors={'keys'}", "selectors={'missing'}"),
    ] {
        assert!(Definition::compile(&changed, "policy.lua").is_err());
    }
    let raw = r#"local trigger={kind='custom',recognizer={name='hold',selectors={'keys'},inputs={'key'},reply_within='250ms'},timeout='1s'}"#;
    assert!(
        Definition::compile(&source(raw), "unbound.lua")
            .unwrap_err()
            .0
            .contains("callback bindings")
    );
}

#[test]
fn native_options_reject_unknown_fields_types_and_out_of_range_budgets() {
    for options in [
        "{}",
        "{selectors={},inputs={'key'},reply_within='1ms'}",
        "{selectors={'keys'},inputs={},reply_within='1ms'}",
        "{selectors={'keys'},inputs={'future'},reply_within='1ms'}",
        "{selectors={'keys'},inputs={'key'},reply_within=250}",
        "{selectors={'keys'},inputs={'key'},reply_within='0ms'}",
        "{selectors={'keys'},inputs={'key'},reply_within='251ms'}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',timeout='0ms'}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',timeout='10001ms'}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',wake='4ms'}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',wake='251ms'}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',action={kind='command',argv={'bad'}}}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',target='caller-selected'}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',continuous='yes'}",
        "{selectors={'keys'},inputs={'key'},reply_within='1ms',continuous=0}",
        "{selectors={[1]='keys',hidden='foreign'},inputs={'key'},reply_within='1ms'}",
        "{selectors={[2]='keys'},inputs={'key'},reply_within='1ms'}",
        "{selectors={'keys'},inputs={[1]='key',hidden='touch'},reply_within='1ms'}",
        "nil",
        "function()end",
    ] {
        let code = format!("local trigger=p.recognizers.define('hold',{options},function()end)");
        assert!(
            Definition::compile(&source(&code), "options.lua").is_err(),
            "{options}"
        );
    }
    for name in ["''", "string.rep('x',129)", "'bad\\nname'", "{}", "42"] {
        assert!(Definition::compile(&source(&format!("local trigger=p.recognizers.define({name},{{selectors={{'keys'}},inputs={{'key'}},reply_within='1ms'}},function()end)")), "name.lua").is_err(), "{name}");
    }
}

#[test]
fn protected_permission_fields_reject_lua_truthiness_in_models_and_exports() {
    for field in [
        "allow_runtime_rules",
        "allow_input_events",
        "allow_power_actions",
        "allow_hyprland_actions",
    ] {
        for value in ["'false'", "0", "{}", "function()end"] {
            let code = format!(
                "local p=require('prek');return p.config.new{{policy={{{field}={value}}}}}"
            );
            assert!(
                Definition::compile(&code, "boolean-policy.lua")
                    .unwrap_err()
                    .0
                    .contains("must be a boolean"),
                "{field}={value}"
            );
            let export = format!(
                "local p=require('prek');local c=p.config.preset('minimal');c.policy.{field}={value};p.config.export(c);return c"
            );
            assert!(
                Definition::compile(&export, "boolean-export.lua")
                    .unwrap_err()
                    .0
                    .contains("must be a boolean")
            );
        }
    }
}

#[test]
fn failed_constructor_does_not_reserve_a_name_or_prevent_declarative_export() {
    let body = format!(
        r#"
assert(not pcall(p.recognizers.define,'hold',{{selectors={{}},inputs={{'key'}},reply_within='1ms'}},function()end))
assert(not pcall(p.recognizers.define,'hold',{{selectors={{'keys'}},inputs={{'key'}},reply_within='1ms'}},false))
assert(type(p.config.export(p.config.preset('minimal')))=='string')
{DECLARE}
"#
    );
    assert_eq!(compile(&body).recognizers.len(), 1);
    let duplicate = format!(
        "{DECLARE}\np.recognizers.define('hold',{{selectors={{'keys'}},inputs={{'key'}},reply_within='1ms'}},function()end)"
    );
    assert!(
        Definition::compile(&source(&duplicate), "duplicate.lua")
            .unwrap_err()
            .0
            .contains("duplicate recognizer")
    );
}

#[test]
fn registration_is_configuration_only_and_callbacks_require_original_source() {
    let definition = compile(&format!(
        "{DECLARE}\np.events.on('ready',function()p.recognizers.define('runtime',{{selectors={{'keys'}},inputs={{'key'}},reply_within='1ms'}},function()end)end)"
    ));
    assert!(
        definition
            .instantiate()
            .unwrap()
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap_err()
            .0
            .contains("configuration evaluation")
    );
    assert!(
        super::super::export_config(&definition.validate().unwrap())
            .unwrap_err()
            .0
            .contains("original approved source")
    );
    assert!(
        Definition::compile(
            &source(&format!(
                "{DECLARE}\np.config.export(p.config.preset('minimal'))"
            )),
            "lossy.lua"
        )
        .unwrap_err()
        .0
        .contains("original Lua source")
    );
}

#[test]
fn bounds_apply_to_declarations_and_expanded_tables_before_deserialization() {
    let prelude = "local o={selectors={'keys'},inputs={'key'},reply_within='1ms'};local trigger";
    let maximum =
        format!("{prelude};for i=1,64 do trigger=p.recognizers.define('r'..i,o,function()end)end");
    assert_eq!(compile(&maximum).recognizers.len(), 64);
    assert!(
        Definition::compile(
            &source(&format!(
                "{prelude};for i=1,65 do trigger=p.recognizers.define('r'..i,o,function()end)end"
            )),
            "limit.lua"
        )
        .unwrap_err()
        .0
        .contains("limit exceeded")
    );
    for options in [
        "local o={};o.selectors=o;o.inputs={'key'};o.reply_within='1ms'",
        "local o={selectors={},inputs={'key'},reply_within='1ms'};for i=1,5000 do o.selectors[i]='keys'end",
        "local o={selectors={string.rep('x',70000)},inputs={'key'},reply_within='1ms'}",
    ] {
        assert!(
            Definition::compile(
                &source(&format!(
                    "{options};local trigger=p.recognizers.define('hold',o,function()end)"
                )),
                "expanded.lua"
            )
            .is_err()
        );
    }
}
