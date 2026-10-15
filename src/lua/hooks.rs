//! Source-approved per-rule subscriptions; ordinary event execution owns limits.
use super::*;
use crate::rules::Stage;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleHook {
    pub id: String,
    pub phase: Stage,
}
impl RuleHook {
    fn kind(&self) -> EventKind {
        if self.phase == Stage::Cancel {
            EventKind::Cancel
        } else {
            EventKind::Matched
        }
    }
    pub(super) fn matches(&self, kind: EventKind, payload: &serde_json::Value) -> bool {
        let event = &payload["event"];
        if kind != self.kind() || event["rule"].as_str() != Some(&self.id) {
            return false;
        }
        if self.phase == Stage::Cancel && event["outcome"] == "cancelled" {
            return true;
        }
        event["outcome"] == "matched"
            && serde_json::from_value::<Stage>(event["stage"].clone()).ok() == Some(self.phase)
    }
}
pub(super) fn validate(registrations: &[Registration], config: &Config) -> Result<(), ConfigError> {
    for registration in registrations {
        if let Some(rule) = &registration.rule
            && (registration.kind != rule.kind()
                || !config
                    .rules
                    .iter()
                    .any(|configured| configured.id == rule.id))
        {
            return Err(error(
                "rule hook requires a configured rule and matching event kind",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const RULES: &str = r#"
return p.config.new {rules={
 p.rules.define{id='a',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='a'}},
 p.rules.define{id='b',trigger=p.triggers.key{key='KEY_B'},action=p.actions.event{name='b'}},
}}
"#;
    fn definition(body: &str) -> Definition {
        Definition::compile(
            &format!("local p=require('prek'); {body}\n{RULES}"),
            "hooks.lua",
        )
        .unwrap()
    }
    fn payload(rule: &str, phase: &str) -> serde_json::Value {
        serde_json::json!({"event":{"outcome":"matched","rule":rule,"stage":phase}})
    }
    #[test]
    fn scoped_callbacks_filter_rule_and_phase_without_duplicate_global_delivery() {
        let definition = definition(
            r#"
p.events.on('matched',function()p.log.info('global')end)
for _,phase in ipairs({'discrete','begin','update','end','cancel'}) do
 p.rules.on('a',phase,function(event)p.log.info(phase,{rule=event.event.rule})end)
end
"#,
        );
        let mut forged = definition.clone();
        forged.registrations[1].rule.as_mut().unwrap().id = "b".into();
        assert!(forged.validate().is_ok());
        assert!(
            forged
                .instantiate()
                .err()
                .unwrap()
                .0
                .contains("approved model or registrations")
        );
        let mut runtime = definition.instantiate().unwrap();
        for (phase, kind) in [
            ("discrete", EventKind::Matched),
            ("begin", EventKind::Begin),
            ("update", EventKind::Update),
            ("end", EventKind::End),
            ("cancel", EventKind::Cancel),
        ] {
            let logs = runtime
                .dispatch(EventKind::Matched, &payload("a", phase))
                .unwrap();
            assert_eq!(
                logs.iter()
                    .map(|log| log.message.as_str())
                    .collect::<Vec<_>>(),
                if phase == "cancel" {
                    vec!["global"]
                } else {
                    vec!["global", phase]
                }
            );
            if kind != EventKind::Matched {
                let logs = runtime.dispatch(kind, &payload("a", phase)).unwrap();
                assert_eq!(logs.len(), usize::from(phase == "cancel"));
            }
            assert_eq!(
                runtime
                    .dispatch(EventKind::Matched, &payload("b", phase))
                    .unwrap()
                    .len(),
                1
            );
        }
        let cancelled = serde_json::json!({"event":{"outcome":"cancelled","rule":"a","reason":"source_changed"}});
        assert_eq!(
            runtime.dispatch(EventKind::Cancel, &cancelled).unwrap()[0].message,
            "cancel"
        );
        assert!(
            runtime
                .dispatch(
                    EventKind::Cancel,
                    &serde_json::json!({"event":{"outcome":"skipped","rule":"a"}})
                )
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn inline_callbacks_compile_to_approved_metadata_and_leave_a_plain_rust_rule() {
        let definition=Definition::compile(r#"
local p=require('prek'); local count=0
local function hook(event)count=count+1;p.log.info('inline',{count=count,stage=event.event.stage})end
return p.config.new{rules={p.rules.define{
 id='inline',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='inline'},
 on_discrete=hook,on_begin=hook,on_update=hook,on_end=hook,on_cancel=hook,
}}}
"#,"inline.lua").unwrap();
        assert_eq!(definition.registrations.len(), 5);
        assert!(!definition.model.contains("on_begin"));
        let mut runtime = definition.instantiate().unwrap();
        for (index, phase) in ["discrete", "begin", "update", "end", "cancel"]
            .into_iter()
            .enumerate()
        {
            let logs = runtime
                .dispatch(
                    if phase == "cancel" {
                        EventKind::Cancel
                    } else {
                        EventKind::Matched
                    },
                    &payload("inline", phase),
                )
                .unwrap();
            assert_eq!(logs[0].fields["count"], index + 1);
        }
        let mut bad = definition.clone();
        bad.registrations[0].rule.as_mut().unwrap().id = "missing".into();
        assert!(bad.validate().is_err());
        let mut bad = definition.clone();
        bad.registrations[0].kind = EventKind::Ready;
        assert!(bad.validate().is_err());
        let mut bad = definition.clone();
        bad.registrations[0].rule.as_mut().unwrap().id = "inline-forged".into();
        assert!(bad.instantiate().is_err());
        assert!(
            Definition::compile(
                r#"
local p=require('prek');local c=p.config.preset('minimal')
c.rules={p.rules.define{id='watch',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='watch'}}}
p.rules.on('watch','begin',function()end)
p.config.export(c)
return c
"#,
                "lossy-export.lua"
            )
            .unwrap_err()
            .0
            .contains("original Lua source")
        );
        Definition::compile(
            r#"
local p=require('prek');local c=p.config.preset('minimal')
assert(not pcall(p.rules.on,'watch','begin',{}))
assert(type(p.config.export(c))=='string')
return c
"#,
            "failed-hook-export.lua",
        )
        .unwrap();
    }
    #[test]
    fn registration_is_source_only_and_uses_the_shared_quota() {
        for call in [
            "p.rules.on('missing','begin',function()end)",
            "p.rules.on('a','ready',function()end)",
            "p.rules.on(12,'begin',function()end)",
            "p.rules.on('a','begin',{})",
        ] {
            assert!(
                Definition::compile(
                    &format!("local p=require('prek');{call};{RULES}"),
                    "bad.lua"
                )
                .is_err()
            );
        }
        let source = format!(
            "local p=require('prek'); for i=1,256 do p.events.on('ready',function()end) end; p.rules.on('a','begin',function()end);{RULES}"
        );
        assert!(
            Definition::compile(&source, "quota.lua")
                .unwrap_err()
                .0
                .contains("subscription limit")
        );
        let mut runtime = definition(
            r#"
p.events.on('ready',function()
 assert(not pcall(p.rules.on,'a','begin',function()end))
 assert(not pcall(p.rules.define,{id='a',on_begin=function()end}))
 p.log.info('source-only')
end)
"#,
        )
        .instantiate()
        .unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "source-only"
        );
    }
    #[test]
    fn disposal_order_and_owned_payloads_follow_existing_subscriptions() {
        let mut runtime = definition(
            r#"
local first,second
first=p.rules.on('a','begin',function(event)
 event.event.rule='changed';second:dispose();first:dispose();p.log.info('first')
end)
second=p.rules.on('a','begin',function()error('disposed callback ran')end)
p.rules.on('a','begin',function(event)assert(event.event.rule=='a');p.log.info('third')end)
"#,
        )
        .instantiate()
        .unwrap();
        let logs = runtime
            .dispatch(EventKind::Matched, &payload("a", "begin"))
            .unwrap();
        assert_eq!(
            logs.iter()
                .map(|log| log.message.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "third"]
        );
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &payload("a", "begin"))
                .unwrap()[0]
                .message,
            "third"
        );
    }
    #[test]
    fn errors_returned_effects_and_unbounded_hooks_retire_without_committing_logs() {
        for body in [
            "error('failed')",
            "return p.actions.event{name='forbidden'}",
            "while true do end",
        ] {
            let mut runtime = definition(&format!(
                "p.rules.on('a','begin',function()p.log.info('discarded');{body} end)"
            ))
            .instantiate()
            .unwrap();
            assert!(
                runtime
                    .dispatch(EventKind::Matched, &payload("a", "begin"))
                    .is_err()
            );
            assert!(
                runtime
                    .dispatch(EventKind::Ready, &serde_json::json!({}))
                    .unwrap_err()
                    .0
                    .contains("retired")
            );
            assert!(runtime.host.borrow().logs.is_empty());
        }
    }
}
