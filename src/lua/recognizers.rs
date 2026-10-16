//! Host-approved callback bindings. No public Lua constructor or live routing
//! admission is provided here; the trusted coordinator still owns arbitration.
use super::*;
use crate::recognition::{self, Phase, Proposal, Ticket};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Associate an approved source subscription with an immutable declaration.
/// This host API is preparatory plumbing for the source-level constructor.
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
        if config.mode != crate::config::Mode::Observe
            || !config.policy.allow_input_events
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
            })
        {
            return Err(error(
                "recognizer binding requires a unique approved input subscription and permitted selectors",
            ));
        }
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
            && crate::input_events::permitted_subscription(
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
            self.failed = true;
            let mut host = self.host.borrow_mut();
            host.running = false;
            host.bytes = 0;
            host.logs.clear();
            host.rules.discard("Lua recognizer failed");
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
}

#[cfg(test)]
mod tests;
