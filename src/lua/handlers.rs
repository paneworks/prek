//! Approved named Lua handlers resolve bounded typed effect plans. Execution
//! belongs to the active-session executor after its broker grant, never here.
use super::*;
use crate::config::{Action, EffectKind};
use luna::{Context, Function};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerRegistration {
    pub id: u16,
    pub name: String,
    pub requires: BTreeSet<EffectKind>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResult {
    pub actions: Vec<Action>,
    pub logs: Vec<LogRecord>,
}

pub(super) fn guard<'gc>(
    ctx: &Context<'gc>,
    host: std::rc::Rc<std::cell::RefCell<runtime::Host>>,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |_, _, _| {
        if !host.borrow().configuring {
            return Err(io::Error::other(
                "named action registration requires configuration evaluation",
            )
            .into());
        }
        Ok(CallbackReturn::Return)
    })
}

impl Runtime {
    pub fn handler_registrations(&mut self) -> Result<Vec<HandlerRegistration>, ConfigError> {
        self.lua
            .try_enter(|ctx| {
                let registry = ctx.fetch(&self.handlers);
                let mut registrations = Vec::new();
                for index in 1..=registry.length(&ctx) {
                    let entry: Table = registry.get(ctx, index)?;
                    let name: luna::String = entry.get(ctx, "name")?;
                    let requires: Value = entry.get(ctx, "requires")?;
                    let requires = luna_util::serde::from_value_with(
                        ctx,
                        requires,
                        luna_util::serde::DeOptions::default().max_depth(4),
                    )
                    .map_err(io::Error::other)?;
                    registrations.push(HandlerRegistration {
                        id: index as u16,
                        name: name.to_str()?.into(),
                        requires,
                    });
                }
                Ok(registrations)
            })
            .map_err(error)
    }

    /// This evaluates a plan, without performing its effects. Callers enforce
    /// identity deduplication, frozen targets, capabilities and execution grants.
    pub fn resolve_action(
        &mut self,
        action: &Action,
        payload: &serde_json::Value,
    ) -> Result<ActionResult, ConfigError> {
        if self.failed {
            return Err(error("Lua runtime was retired after callback failure"));
        }
        let Action::Lua { handler, requires } = action else {
            return Err(error("expected a named Lua action"));
        };
        runtime::bounded_json(payload, 64 * 1024).map_err(error)?;
        let metadata = self.handler_registrations()?;
        let registration = metadata
            .iter()
            .find(|entry| entry.name == *handler && entry.requires == *requires)
            .ok_or_else(|| error("Lua handler differs from its approved registration"))?;
        let executor = self
            .lua
            .try_enter(|ctx| {
                let entry: Table = ctx.fetch(&self.handlers).get(ctx, registration.id)?;
                let callback: Function = entry.get(ctx, "callback")?;
                let value = luna_util::serde::to_value(ctx, payload).map_err(io::Error::other)?;
                Ok(ctx.stash(Executor::start(ctx, callback, (value,))))
            })
            .map_err(error)?;
        self.host.borrow_mut().running = true;
        let started = Instant::now();
        let result = (|| {
            drive_budget(
                &mut self.lua,
                &executor,
                started,
                Duration::from_millis(20),
                100_000,
            )?;
            let actions = self.lua.try_enter(|ctx| {
                let executor = ctx.fetch(&executor);
                let values: luna::Variadic<Vec<Value>> = executor.take_result(ctx).map_err(io::Error::other)??;
                if executor.mode() != luna::ExecutorMode::Stopped || values.0.len() > 1 {
                    return Err(io::Error::other("Lua action must return nil, an action, or an action array; it cannot yield").into());
                }
                let Some(value) = values.0.first().copied() else { return Ok(Vec::<Action>::new()); };
                if matches!(value, Value::Nil) { return Ok(Vec::new()); }
                let options = luna_util::serde::DeOptions::default().max_depth(8);
                let table = match value { Value::Table(table) => table, _ => return Err(io::Error::other("invalid Lua action result").into()) };
                let kind: Value = table.get(ctx, "kind")?;
                let actions: Vec<Action> = if matches!(kind, Value::Nil) {
                    if table.length(&ctx) > 32 { return Err(io::Error::other("Lua action plan exceeds 32 effects").into()); }
                    let length=table.length(&ctx);
                    let mut count=0;
                    for (key, _) in table.iter(ctx) {
                        let Value::Integer(index)=key else { return Err(io::Error::other("Lua action arrays require integer keys").into()); };
                        if index<1 || index>length { return Err(io::Error::other("Lua action array must be dense").into()); }
                        count+=1;
                    }
                    if count!=length { return Err(io::Error::other("Lua action array must be dense").into()); }
                    luna_util::serde::from_value_with(ctx, value, options).map_err(io::Error::other)?
                } else { vec![luna_util::serde::from_value_with(ctx, value, options).map_err(io::Error::other)?] };
                Ok(actions)
            }).map_err(error)?;
            if actions.len() > 32 {
                return Err(error("Lua action plan exceeds 32 effects"));
            }
            for action in &actions {
                crate::config::validate_action(action).map_err(error)?;
                if !action
                    .effect_kind()
                    .is_some_and(|kind| requires.contains(&kind))
                {
                    return Err(error("Lua action result exceeds its declared capabilities"));
                }
            }
            runtime::bounded_json(&actions, 32 * 1024).map_err(error)?;
            if started.elapsed() > Duration::from_millis(20) {
                return Err(error("Lua action result exceeded callback deadline"));
            }
            Ok(actions)
        })();
        let mut host = self.host.borrow_mut();
        host.running = false;
        host.bytes = 0;
        match result {
            Ok(actions) => Ok(ActionResult {
                actions,
                logs: std::mem::take(&mut host.logs),
            }),
            Err(reason) => {
                self.failed = true;
                host.logs.clear();
                Err(reason)
            }
        }
    }
}

impl Definition {
    pub(super) fn validate_handlers(&self, config: &Config) -> Result<(), ConfigError> {
        let mut names = BTreeSet::new();
        for (index, handler) in self.handlers.iter().enumerate() {
            if usize::from(handler.id) != index + 1
                || !crate::config::valid_handler_name(&handler.name)
                || handler.requires.is_empty()
                || !names.insert(&handler.name)
            {
                return Err(error("invalid or duplicated Lua handler registration"));
            }
        }
        for rule in &config.rules {
            if let Action::Lua { handler, requires } = &rule.action
                && !self
                    .handlers
                    .iter()
                    .any(|entry| entry.name == *handler && entry.requires == *requires)
            {
                return Err(error(
                    "Lua action reference has no matching approved handler",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = r#"
local p=require('prek')
local count=0
local action=p.actions.define('launch',{requires={'commands'}},function(event)
    count=count+1
    p.log.info('resolved',{count=count})
    return {p.actions.command{argv={'echo',event.event.rule}},
        p.actions.command{argv={'printf',tostring(count)}}}
end)
return p.config.new{rules={p.rules.define{id='launch-rule',trigger=p.triggers.key{key='KEY_A'},action=action}}}
"#;
    #[test]
    fn approved_handlers_keep_closures_and_return_typed_plans() {
        let definition = Definition::compile(SOURCE, "handlers.lua").unwrap();
        assert_eq!(definition.handlers.len(), 1);
        let mut runtime = definition.instantiate().unwrap();
        let action = runtime.config.rules[0].action.clone();
        for count in 1..=2 {
            let result = runtime
                .resolve_action(
                    &action,
                    &serde_json::json!({"event":{"rule":"launch-rule"}}),
                )
                .unwrap();
            assert_eq!(result.actions.len(), 2);
            assert_eq!(
                result.actions[0],
                Action::Command {
                    argv: vec!["echo".into(), "launch-rule".into()]
                }
            );
            assert_eq!(result.logs[0].fields, serde_json::json!({"count":count}));
        }
        let mut forged = definition;
        forged.handlers[0].requires = BTreeSet::from([EffectKind::SyntheticInput]);
        assert!(forged.instantiate().is_err());
    }
    #[test]
    fn failed_or_expanding_handlers_discard_results_and_retire() {
        for body in [
            "error('failure')",
            "while true do end",
            "coroutine.yield()",
            "return p.actions.keys{keys={'KEY_A'}}",
            "p.actions.define('extra',{requires={'commands'}},function()end)",
            "return 42",
            "return {[1]=p.actions.command{argv={'echo'}},[3]=p.actions.command{argv={'echo'}}}",
            "return {bad=p.actions.command{argv={'echo'}}}",
            "return p.actions.command{argv={string.rep('x',4097)}}",
        ] {
            let source = format!(
                "local p=require('prek'); local a=p.actions.define('handler',{{requires={{'commands'}}}},function()p.log.info('uncommitted'); {body} end); return {{version=1,rules={{{{id='x',trigger={{kind='key',key='KEY_A'}},action=a}}}}}};"
            );
            let mut runtime = Runtime::load(&source, "failure.lua").unwrap();
            let action = runtime.config.rules[0].action.clone();
            assert!(
                runtime
                    .resolve_action(&action, &serde_json::Value::Null)
                    .is_err(),
                "{body}"
            );
            assert!(runtime.host.borrow().logs.is_empty());
            assert!(
                runtime
                    .resolve_action(&action, &serde_json::Value::Null)
                    .err()
                    .unwrap()
                    .0
                    .contains("retired")
            );
        }
    }
}
