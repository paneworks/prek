//! Source-approved recognizer declarations and bounded callback execution.
//! Native coordinators retain authority over eligibility, claims and effects.
use super::*;
use crate::recognition::{self, Phase, Proposal, Ticket};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Associate an approved source subscription with an immutable declaration.
/// Source constructors populate these immutable bindings automatically.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub subscription: u16,
    pub definition: recognition::Definition,
}
#[derive(Debug, Default)]
pub struct RecognitionResult {
    pub proposal: Option<Proposal>,
    pub logs: Vec<LogRecord>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    selectors: BTreeSet<String>,
    inputs: BTreeSet<recognition::InputKind>,
    reply_within: crate::config::Millis,
    #[serde(default)]
    continuous: bool,
    #[serde(default = "timeout")]
    timeout: crate::config::Millis,
    #[serde(default)]
    wake: Option<crate::config::Millis>,
}
fn timeout() -> crate::config::Millis {
    crate::config::Millis(1000)
}
fn string_list<'gc>(ctx: luna::Context<'gc>, value: Value<'gc>, max: i64) -> io::Result<()> {
    let Value::Table(table) = value else {
        return Err(io::Error::other(
            "recognizer selectors and inputs require nonempty lists",
        ));
    };
    let length = table.length(&ctx);
    if length == 0 || length > max {
        return Err(io::Error::other("recognizer list exceeds bounds"));
    }
    let mut count = 0;
    for (key, value) in table.iter(ctx) {
        if !matches!(key, Value::Integer(index) if (1..=length).contains(&index))
            || !matches!(value, Value::String(_))
        {
            return Err(io::Error::other(
                "recognizer lists require consecutive integer keys and string values",
            ));
        }
        count += 1;
    }
    if count != length {
        return Err(io::Error::other("recognizer list contains holes"));
    }
    Ok(())
}

#[cfg(test)]
mod constructor_tests;
pub(super) fn constructor<'gc>(
    ctx: &luna::Context<'gc>,
    host: std::rc::Rc<std::cell::RefCell<runtime::Host>>,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        if !host.borrow().configuring {
            return Err(io::Error::other(
                "recognizer registration requires configuration evaluation",
            )
            .into());
        }
        let (name, options): (luna::String, Value) = stack.consume(ctx)?;
        values::budget(ctx, options, 4, 4096, 64 * 1024)?;
        let Value::Table(table) = options else {
            return Err(io::Error::other("recognizer options must be a table").into());
        };
        values::optional_boolean(ctx, table, "continuous")?;
        string_list(ctx, table.get(ctx, "selectors")?, 64)?;
        string_list(ctx, table.get(ctx, "inputs")?, 6)?;
        let options: Options = luna_util::serde::from_value_with(
            ctx,
            options,
            luna_util::serde::DeOptions::default().max_depth(4),
        )
        .map_err(io::Error::other)?;
        let definition = recognition::Definition {
            name: name.to_str().map_err(io::Error::other)?.into(),
            selectors: options.selectors,
            inputs: options.inputs,
            reply_within: options.reply_within,
            continuous: options.continuous,
        };
        let trigger = crate::config::Trigger::Custom {
            recognizer: definition.clone(),
            timeout: options.timeout,
            wake: options.wake,
        };
        crate::config::validate_trigger(&trigger, &BTreeSet::new(), 0).map_err(io::Error::other)?;
        // Separate serializations keep the registry independent of the table
        // returned to user code, including its nested selector/input arrays.
        let public = luna_util::serde::to_value(ctx, &trigger).map_err(io::Error::other)?;
        let private = luna_util::serde::to_value(ctx, &definition).map_err(io::Error::other)?;
        host.borrow_mut().recognizer_callbacks = true;
        stack.replace(ctx, (public, private));
        Ok(CallbackReturn::Return)
    })
}
pub(super) fn bindings(registrations: &[Registration]) -> Vec<Binding> {
    registrations
        .iter()
        .filter_map(|entry| {
            entry.recognizer.as_ref().map(|definition| Binding {
                subscription: entry.id,
                definition: definition.clone(),
            })
        })
        .collect()
}
pub(super) fn merge(config: &mut Config, bindings: &[Binding]) -> Result<(), ConfigError> {
    for binding in bindings {
        if let Some(existing) = config
            .recognizers
            .iter()
            .find(|definition| definition.name == binding.definition.name)
        {
            if existing != &binding.definition {
                return Err(error(
                    "recognizer registry differs from its canonical source declaration",
                ));
            }
        } else {
            config.recognizers.push(binding.definition.clone());
        }
    }
    config.recognizers.sort_by(|a, b| a.name.cmp(&b.name));
    config.validate()
}

pub(super) fn validate(
    bindings: &[Binding],
    registrations: &[Registration],
    config: &Config,
) -> std::result::Result<(), ConfigError> {
    if bindings.len() > recognition::MAX_RECOGNIZERS {
        return Err(error("too many Lua recognizer bindings"));
    }
    let mut names = BTreeSet::new();
    let mut subscriptions = BTreeSet::new();
    for binding in bindings {
        binding.definition.validate().map_err(error)?;
        if config.rules.iter().any(|rule| matches!(&rule.trigger, crate::config::Trigger::Custom { recognizer, .. } if recognizer.name == binding.definition.name && recognizer != &binding.definition)) {
            return Err(error("recognizer binding differs from its native rule declaration"));
        }
        if !config.policy.allow_input_events
            || !binding.definition.selectors.iter().all(|selector| {
                config.devices.contains_key(selector)
                    && config.policy.allowed_devices.contains(selector)
            })
            || !names.insert(&binding.definition.name)
            || !subscriptions.insert(binding.subscription)
            || !registrations.iter().any(|entry| {
                entry.id == binding.subscription
                    && entry.kind == EventKind::Input
                    && entry.active
                    && entry.rule.is_none()
                    && entry
                        .recognizer
                        .as_ref()
                        .is_none_or(|declared| declared == &binding.definition)
            })
        {
            return Err(error(
                "recognizer binding requires a unique approved input subscription and permitted selectors",
            ));
        }
    }
    if registrations.iter().any(|entry| {
        entry.recognizer.as_ref().is_some_and(|definition| {
            !config.recognizers.contains(definition)
                || !bindings.iter().any(|binding| {
                    binding.subscription == entry.id && &binding.definition == definition
                })
        })
    }) || config.recognizer_definitions().iter().any(|recognizer| {
        !bindings.iter().any(|binding| {
            &binding.definition == recognizer
                && registrations.iter().any(|entry| {
                    entry.id == binding.subscription
                        && entry.recognizer.as_ref() == Some(recognizer)
                })
        })
    }) {
        return Err(error(
            "custom recognizer rules and source registrations require exact approved callback bindings",
        ));
    }
    Ok(())
}
impl Definition {
    /// Bind only after the embedding has approved the source and declaration.
    /// Ordinary Input dispatch skips this subscription after instantiation.
    pub fn bind_recognizer(
        &mut self,
        subscription: u16,
        definition: recognition::Definition,
    ) -> std::result::Result<(), ConfigError> {
        let mut bindings = self.recognizers.clone();
        bindings.push(Binding {
            subscription,
            definition,
        });
        validate(&bindings, &self.registrations, &self.validate()?)?;
        self.recognizers = bindings;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    phase: Phase,
    #[serde(default)]
    data: serde_json::Value,
}

#[cfg(target_os = "linux")]
impl Runtime {
    fn recognition_permitted(&self, ticket: &Ticket) -> bool {
        let host = self.host.borrow();
        let Some(view) = super::context::desktop(&host) else {
            return false;
        };
        let Some(capabilities) = super::capabilities::current(&host) else {
            return false;
        };
        let Some(now) = host.now.and_then(|now| {
            now.checked_add(
                host.context_clock
                    .map_or(Duration::ZERO, |clock| clock.elapsed()),
            )
        }) else {
            return false;
        };
        view.fence == ticket.fence
            && capabilities.fence == ticket.fence
            && now >= ticket.requested_at
            && now < ticket.deadline
            && self
                .config
                .policy
                .allowed_seats
                .contains(&ticket.input.source().seat)
            && crate::recognition::permitted_subscription(
                &self.config,
                &capabilities.negotiated,
                view.context.as_deref(),
                &ticket.input.source().seat,
                ticket.fence.session,
                now,
            )
    }
    /// Run exactly one source-approved closure under its original Rust ticket.
    /// No claim, target selection or effect execution occurs in this helper.
    pub fn recognize(
        &mut self,
        ticket: &Ticket,
    ) -> std::result::Result<RecognitionResult, ConfigError> {
        if self.failed {
            return Err(error("Lua runtime was retired after callback failure"));
        }
        let result = self.recognize_inner(ticket);
        if result.is_err() {
            self.recognizer_failed();
        }
        result
    }
    fn recognize_inner(
        &mut self,
        ticket: &Ticket,
    ) -> std::result::Result<RecognitionResult, ConfigError> {
        let started = Instant::now();
        let binding = self
            .recognizers
            .iter()
            .find(|binding| binding.definition.name == ticket.recognizer)
            .ok_or_else(|| error("recognizer has no approved source binding"))?
            .clone();
        let now = {
            let host = self.host.borrow();
            host.now
                .and_then(|now| {
                    now.checked_add(
                        host.context_clock
                            .map_or(Duration::ZERO, |clock| clock.elapsed()),
                    )
                })
                .ok_or_else(|| error("recognizer requires a monotonic runtime clock"))?
        };
        // An expired queued request never enters Lua. It also cannot be retried
        // later, since the caller and this VM both retain the ticket high water.
        if self
            .recognition_last
            .is_some_and(|(fence, last)| fence != ticket.fence || ticket.id <= last)
        {
            return Err(error(
                "recognizer ticket was repeated, reordered or has a foreign fence",
            ));
        }
        self.recognition_last = Some((ticket.fence, ticket.id));
        if !self.recognition_permitted(ticket) {
            return Ok(RecognitionResult::default());
        }
        ticket
            .validate_for(&binding.definition, now)
            .map_err(error)?;
        let executor = self
            .lua
            .try_enter(|ctx| {
                let entry: Table = ctx.fetch(&self.registry).get(ctx, binding.subscription)?;
                if !entry.get::<_, bool>(ctx, "active")? {
                    return Ok(None);
                }
                let callback: luna::Function = entry.get(ctx, "callback")?;
                let value = values::recognition(ctx, ticket)?;
                Ok(Some(ctx.stash(Executor::start(ctx, callback, (value,)))))
            })
            .map_err(error)?;
        let Some(executor) = executor else {
            return Ok(RecognitionResult::default());
        };
        // Building the copied payload must not turn an expired request into
        // one last invocation of a stateful closure.
        if !self.recognition_permitted(ticket) {
            return Ok(RecognitionResult::default());
        }
        let scope =
            recognition::Cancellation::from_ticket(ticket, recognition::Retirement::Cancelled);
        if let Some(current) = self.recognition_candidates.get(&ticket.candidate) {
            if !current.same_candidate(&scope) {
                return Err(error("recognizer candidate changed without cancellation"));
            }
        } else if self.recognition_candidates.len() >= recognition::MAX_PENDING {
            return Err(error("too many delivered Lua recognizer candidates"));
        }
        self.recognition_candidates.insert(ticket.candidate, scope);
        self.host.borrow_mut().running = true;
        drive_budget(
            &mut self.lua,
            &executor,
            started,
            runtime::CALLBACK_TIME,
            runtime::CALLBACK_INSTRUCTIONS,
        )?;
        let reply = self
            .lua
            .try_enter(|ctx| {
                let executor = ctx.fetch(&executor);
                let result: luna::Variadic<Vec<Value>> =
                    executor.take_result(ctx).map_err(io::Error::other)??;
                if executor.mode() != luna::ExecutorMode::Stopped || result.0.len() > 1 {
                    return Err(io::Error::other(
                        "recognizer must return one proposal or nil and cannot yield",
                    )
                    .into());
                }
                let value = result.0.first().copied().unwrap_or(Value::Nil);
                if value.is_nil() {
                    return Ok(Reply {
                        phase: Phase::Pending,
                        data: serde_json::Value::Null,
                    });
                }
                values::budget(ctx, value, 10, 8192, 64 * 1024)?;
                let reply: Reply = luna_util::serde::from_value_with(
                    ctx,
                    value,
                    luna_util::serde::DeOptions::default().max_depth(10),
                )
                .map_err(io::Error::other)?;
                recognition::validate_data(&reply.data)?;
                Ok(reply)
            })
            .map_err(error)?;
        if started.elapsed() >= runtime::CALLBACK_TIME || !self.recognition_permitted(ticket) {
            return Err(error(
                "recognizer authority or original deadline expired during callback",
            ));
        }
        let mut host = self.host.borrow_mut();
        host.running = false;
        host.bytes = 0;
        Ok(RecognitionResult {
            proposal: Some(Proposal {
                fence: ticket.fence,
                ticket: ticket.id,
                phase: reply.phase,
                data: reply.data,
            }),
            logs: std::mem::take(&mut host.logs),
        })
    }

    fn recognizer_failed(&mut self) {
        self.failed = true;
        self.recognition_candidates.clear();
        let mut host = self.host.borrow_mut();
        host.running = false;
        host.bytes = 0;
        host.logs.clear();
        host.rules.discard("Lua recognizer failed");
    }

    /// Cleanup of previously delivered state remains possible after desktop
    /// authority expires. This path carries no new input or proposal result.
    pub fn cancel_recognition(
        &mut self,
        cancellation: &recognition::Cancellation,
    ) -> std::result::Result<Vec<LogRecord>, ConfigError> {
        if self.failed {
            return Err(error("Lua runtime was retired after callback failure"));
        }
        let result = self.cancel_recognition_inner(cancellation);
        if result.is_err() {
            self.recognizer_failed();
        }
        result
    }
    fn cancel_recognition_inner(
        &mut self,
        cancellation: &recognition::Cancellation,
    ) -> std::result::Result<Vec<LogRecord>, ConfigError> {
        let started = Instant::now();
        cancellation.validate().map_err(error)?;
        if self
            .recognition_last
            .is_some_and(|(fence, _)| fence != cancellation.fence)
        {
            return Err(error("recognizer cancellation has a foreign fence"));
        }
        let Some(current) = self.recognition_candidates.get(&cancellation.candidate) else {
            return Ok(Vec::new());
        };
        if !current.same_candidate(cancellation) || cancellation.through < current.through {
            return Err(error(
                "recognizer cancellation has a stale or changed candidate",
            ));
        }
        let binding = self
            .recognizers
            .iter()
            .find(|binding| binding.definition.name == cancellation.recognizer)
            .ok_or_else(|| error("recognizer cancellation has no approved source binding"))?;
        let subscription = binding.subscription;
        self.recognition_candidates.remove(&cancellation.candidate);
        let executor = self
            .lua
            .try_enter(|ctx| {
                let entry: Table = ctx.fetch(&self.registry).get(ctx, subscription)?;
                if !entry.get::<_, bool>(ctx, "active")? {
                    return Ok(None);
                }
                let callback: luna::Function = entry.get(ctx, "callback")?;
                let value = values::recognition_cancellation(ctx, cancellation)?;
                Ok(Some(ctx.stash(Executor::start(ctx, callback, (value,)))))
            })
            .map_err(error)?;
        let Some(executor) = executor else {
            return Ok(Vec::new());
        };
        self.host.borrow_mut().running = true;
        drive_budget(
            &mut self.lua,
            &executor,
            started,
            runtime::CALLBACK_TIME,
            runtime::CALLBACK_INSTRUCTIONS,
        )?;
        self.lua.try_enter(|ctx| {
            let executor = ctx.fetch(&executor);
            let values: luna::Variadic<Vec<Value>> = executor.take_result(ctx).map_err(io::Error::other)??;
            if executor.mode() != luna::ExecutorMode::Stopped || values.0.len() > 1 || values.0.first().is_some_and(|value| !value.is_nil()) {
                return Err(io::Error::other("recognizer cleanup must return nil and cannot yield or propose recognition").into());
            }
            Ok(())
        }).map_err(error)?;
        if started.elapsed() >= runtime::CALLBACK_TIME {
            return Err(error("Lua recognizer cleanup exceeded time limit"));
        }
        let mut host = self.host.borrow_mut();
        host.running = false;
        host.bytes = 0;
        Ok(std::mem::take(&mut host.logs))
    }
}

#[cfg(test)]
mod tests;
