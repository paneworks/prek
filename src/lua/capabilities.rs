//! Read-only negotiated flags and current parent executor availability.
use super::*;
use crate::protocol::{Capability, Fence};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

pub const FRESHNESS: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityView {
    pub fence: Fence,
    pub negotiated: BTreeSet<Capability>,
    pub ready: BTreeSet<Capability>,
    pub sampled_at: Duration,
}
impl CapabilityView {
    pub(crate) fn validate(&self) -> io::Result<()> {
        self.fence.validate()?;
        if !self.ready.is_subset(&self.negotiated)
            || self
                .ready
                .iter()
                .any(|cap| !crate::protocol::execution_capability(*cap))
        {
            return Err(io::Error::other("invalid executor capability availability"));
        }
        Ok(())
    }
}

pub(super) fn current(host: &runtime::Host) -> Option<&CapabilityView> {
    let now = host.now?.checked_add(
        host.context_clock
            .map_or(Duration::ZERO, |clock| clock.elapsed()),
    )?;
    host.capabilities.as_ref().filter(|view| {
        now >= view.sampled_at
            && now - view.sampled_at < FRESHNESS
            && host
                .context
                .as_ref()
                .is_none_or(|context| context.fence == view.fence)
    })
}

pub(super) fn getter<'gc>(
    ctx: &luna::Context<'gc>,
    host: Rc<RefCell<runtime::Host>>,
    field: &'static str,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let host = host.borrow();
        if host.configuring {
            return Err(io::Error::other("capability queries require runtime evaluation").into());
        }
        let requested = if field != "current" {
            let Value::String(name) = stack.from_front::<Value>(ctx)? else {
                return Err(io::Error::other("expected a capability name string").into());
            };
            let name = name.to_str().map_err(io::Error::other)?;
            if name.is_empty() || name.len() > 128 {
                return Err(io::Error::other("capability name must contain 1 to 128 bytes").into());
            }
            // Unknown names are false so scripts can probe future API families.
            serde_json::from_value::<Capability>(serde_json::Value::String(name.into())).ok()
        } else {
            None
        };
        let view = current(&host);
        let mut value = match field {
            "current" => view
                .map(|view| {
                    let value = luna_util::serde::to_value(ctx, view).map_err(io::Error::other)?;
                    let Value::Table(table) = value else {
                        unreachable!()
                    };
                    table.set(ctx, "fence", values::context_fence(ctx, &view.fence)?)?;
                    Ok::<_, luna::Error>(value)
                })
                .transpose()?
                .unwrap_or(Value::Nil),
            "has" | "ready" => Value::Boolean(view.is_some_and(|view| {
                requested.is_some_and(|cap| {
                    if field == "has" {
                        view.negotiated.contains(&cap)
                    } else {
                        view.ready.contains(&cap)
                    }
                })
            })),
            _ => return Err(io::Error::other("unknown capability query").into()),
        };
        if current(&host).is_none() {
            value = if field == "current" {
                Value::Nil
            } else {
                Value::Boolean(false)
            };
        }
        stack.replace(ctx, value);
        Ok(CallbackReturn::Return)
    })
}

impl Runtime {
    pub fn set_capabilities(
        &mut self,
        view: CapabilityView,
        now: Duration,
    ) -> Result<(), ConfigError> {
        view.validate().map_err(error)?;
        {
            let host = self.host.borrow();
            if view.sampled_at > now
                || host
                    .context
                    .as_ref()
                    .is_some_and(|context| context.fence != view.fence)
                || host.capabilities.as_ref().is_some_and(|old| {
                    old.fence != view.fence
                        || old.negotiated != view.negotiated
                        || old.sampled_at > view.sampled_at
                })
            {
                return Err(error(
                    "capability snapshot does not match the owning session or clock",
                ));
            }
        }
        self.set_clock(now)?;
        self.host.borrow_mut().capabilities = Some(view);
        Ok(())
    }
    pub(crate) fn clear_capabilities(&mut self) {
        self.host.borrow_mut().capabilities = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Epoch;

    fn view(at: Duration) -> CapabilityView {
        CapabilityView {
            fence: Fence {
                broker: Epoch([u64::MAX, 1 << 63]),
                configuration: u64::MAX - 1,
                session: u64::MAX,
            },
            negotiated: BTreeSet::from([
                Capability::Context,
                Capability::LuaEvents,
                Capability::Commands,
            ]),
            ready: BTreeSet::from([Capability::Commands]),
            sampled_at: at,
        }
    }
    fn runtime(body: &str) -> Runtime {
        let source = format!(
            "local p=require('prek'); p.events.on('matched',function(event) {body} end); return {{version=1}}"
        );
        Definition::compile(&source, "capabilities.lua")
            .unwrap()
            .instantiate()
            .unwrap()
    }
    fn dispatch(runtime: &mut Runtime) {
        runtime
            .dispatch(EventKind::Matched, &serde_json::json!({}))
            .unwrap();
    }

    #[test]
    fn capability_queries_require_runtime_and_strict_bounded_names() {
        for call in ["current()", "has('commands')", "ready('commands')"] {
            assert!(
                Definition::compile(
                    &format!("local p=require('prek');p.capabilities.{call};return {{version=1}}"),
                    "config.lua"
                )
                .unwrap_err()
                .0
                .contains("runtime evaluation")
            );
        }
        for argument in ["nil", "true", "12", "{}", "''", "string.rep('x',129)"] {
            for field in ["has", "ready"] {
                let mut runtime = runtime(&format!("p.capabilities.{field}({argument})"));
                assert!(
                    runtime
                        .dispatch(EventKind::Matched, &serde_json::json!({}))
                        .is_err()
                );
            }
        }
        dispatch(&mut runtime(
            "assert(not p.capabilities.has('future_family'));assert(not p.capabilities.ready(string.rep('x',128)))",
        ));
    }

    #[test]
    fn capability_views_preserve_identity_bits_and_are_owned_copies() {
        let mut runtime = runtime(
            r#"
local v=p.capabilities.current()
assert(v.fence.broker[1]=='18446744073709551615' and v.fence.broker[2]=='9223372036854775808')
assert(v.fence.configuration=='18446744073709551614' and v.fence.session=='18446744073709551615')
assert(#v.negotiated==3 and #v.ready==1 and v.ready[1]=='commands')
assert(v.sampled_at.secs==0 and v.sampled_at.nanos==0)
assert(p.capabilities.has('context') and p.capabilities.has('commands'))
assert(not p.capabilities.ready('context') and not p.capabilities.has('power_actions'))
assert(p.capabilities.ready('commands') and not p.capabilities.ready('unknown'))
v.fence.session='forged';v.negotiated[1]='power_actions';v.ready[1]='power_actions'
assert(p.capabilities.current().fence.session=='18446744073709551615')
assert(p.capabilities.has('context') and not p.capabilities.has('power_actions'))
assert(p.capabilities.ready('commands') and not p.capabilities.ready('power_actions'))
"#,
        );
        runtime
            .set_capabilities(view(Duration::ZERO), Duration::ZERO)
            .unwrap();
        dispatch(&mut runtime);
    }

    #[test]
    fn capability_freshness_keeps_original_timestamp_and_elapsed_callback_time() {
        let mut runtime = runtime(
            "assert(p.capabilities.current()==nil);assert(not p.capabilities.has('commands'));assert(not p.capabilities.ready('commands'))",
        );
        dispatch(&mut runtime);
        runtime
            .set_capabilities(view(Duration::ZERO), FRESHNESS)
            .unwrap();
        dispatch(&mut runtime);
        // A repeated, delayed snapshot cannot renew its lease.
        runtime
            .set_capabilities(view(Duration::ZERO), FRESHNESS + Duration::from_millis(1))
            .unwrap();
        dispatch(&mut runtime);
        runtime
            .set_capabilities(view(Duration::from_secs(1)), Duration::from_secs(1))
            .unwrap();
        runtime.host.borrow_mut().context_clock = Some(Instant::now() - FRESHNESS);
        dispatch(&mut runtime);
        runtime
            .set_capabilities(view(Duration::from_secs(2)), Duration::from_secs(2))
            .unwrap();
        runtime.clear_capabilities();
        dispatch(&mut runtime);
    }

    #[test]
    fn capability_updates_cannot_expand_negotiated_authority_or_regress_clocks() {
        let mut runtime = runtime(
            "assert(p.capabilities.has('commands'));assert(not p.capabilities.ready('commands'))",
        );
        let initial = view(Duration::from_secs(1));
        runtime
            .set_capabilities(initial.clone(), Duration::from_secs(1))
            .unwrap();
        let mut unavailable = initial.clone();
        unavailable.ready.clear();
        runtime
            .set_capabilities(unavailable.clone(), Duration::from_secs(1))
            .unwrap();
        dispatch(&mut runtime);
        let mut invalid = Vec::new();
        let mut changed = unavailable.clone();
        changed.negotiated.insert(Capability::PowerActions);
        invalid.push(changed);
        let mut changed = unavailable.clone();
        changed.ready.insert(Capability::PowerActions);
        invalid.push(changed);
        let mut changed = unavailable.clone();
        changed.ready.insert(Capability::Context);
        invalid.push(changed);
        let mut changed = unavailable.clone();
        changed.fence.session -= 1;
        invalid.push(changed);
        let mut changed = unavailable.clone();
        changed.sampled_at = Duration::ZERO;
        invalid.push(changed);
        let mut changed = unavailable.clone();
        changed.sampled_at = Duration::from_secs(2);
        invalid.push(changed);
        for changed in invalid {
            assert!(
                runtime
                    .set_capabilities(changed, Duration::from_secs(1))
                    .is_err()
            );
            assert_eq!(
                runtime.host.borrow().capabilities.as_ref(),
                Some(&unavailable)
            );
        }
        dispatch(&mut runtime);
    }

    #[test]
    fn a_different_context_fence_makes_capabilities_unavailable() {
        let mut runtime = runtime(
            "assert(p.capabilities.current()==nil);assert(not p.capabilities.has('context'))",
        );
        runtime
            .set_capabilities(view(Duration::ZERO), Duration::ZERO)
            .unwrap();
        runtime
            .set_context(
                crate::context_transfer::tests::view(Duration::ZERO),
                Duration::ZERO,
            )
            .unwrap();
        dispatch(&mut runtime);
        assert!(
            runtime
                .set_capabilities(view(Duration::ZERO), Duration::ZERO)
                .is_err()
        );
    }
}
