//! Thread-local Luna subscriptions and owned, portable source snapshots.
//! Callers must run this component outside the physical input loop and in an
//! unprivileged, resource-limited process. Closures never cross that boundary.
use super::*;
use luna::{Context, Function, StashedTable};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};

pub const MAX_SNAPSHOT_BYTES: usize = 4 * MAX_CONFIG_BYTES;
pub const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_LOG_BYTES: usize = 16 * 1024;
pub(super) const CALLBACK_TIME: Duration = Duration::from_millis(20);
pub(super) const CALLBACK_INSTRUCTIONS: i32 = 100_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Input,
    Matched,
    Begin,
    Update,
    End,
    Cancel,
    Skipped,
    Ready,
    Reload,
    Stop,
    Recovery,
    SessionChanged,
    ContextChanged,
    RulesChanged,
}
impl EventKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Matched => "matched",
            Self::Begin => "begin",
            Self::Update => "update",
            Self::End => "end",
            Self::Cancel => "cancel",
            Self::Skipped => "skipped",
            Self::Ready => "ready",
            Self::Reload => "reload",
            Self::Stop => "stop",
            Self::Recovery => "recovery",
            Self::SessionChanged => "session_changed",
            Self::ContextChanged => "context_changed",
            Self::RulesChanged => "rules_changed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub id: u16,
    pub kind: EventKind,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<super::hooks::RuleHook>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recognizer: Option<crate::recognition::Definition>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub api_version: u32,
    pub name: String,
    #[serde(with = "source_bytes")]
    pub source: String,
    #[serde(default)]
    pub modules: std::collections::BTreeMap<String, super::Module>,
    pub model: String,
    pub registrations: Vec<Registration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recognizers: Vec<super::recognizers::Binding>,
    #[serde(default)]
    pub timers: Vec<super::TimerRegistration>,
    #[serde(default)]
    pub handlers: Vec<super::HandlerRegistration>,
    #[serde(default)]
    pub tasks: Vec<super::TaskRegistration>,
}
impl Definition {
    pub fn compile(source: &str, name: &str) -> Result<Self, ConfigError> {
        Self::compile_sources(&super::Sources::single(source.into(), name.into()))
    }
    pub fn compile_sources(sources: &super::Sources) -> Result<Self, ConfigError> {
        sources.validate()?;
        let mut runtime = instantiate(
            &sources.source,
            &sources.name,
            &sources.modules,
            &mut || Ok(()),
        )?;
        Self::snapshot(sources, &mut runtime)
    }
    fn snapshot(sources: &super::Sources, runtime: &mut Runtime) -> Result<Self, ConfigError> {
        let definition = Self {
            api_version: API_VERSION,
            name: sources.name.clone(),
            source: sources.source.clone(),
            modules: sources.modules.clone(),
            model: toml::to_string(&runtime.config).map_err(error)?,
            registrations: runtime.registrations()?,
            recognizers: runtime.recognizers.clone(),
            timers: runtime.timer_registrations()?,
            handlers: runtime.handler_registrations()?,
            tasks: runtime.task_registrations()?,
        };
        // The VM already decoded and validated this Config. Re-parsing its
        // newly serialized TOML here adds no validation and wastes the child
        // process budget, particularly under emulation. Consumers validate it.
        definition.validate_metadata()?;
        definition.validate_handlers(&runtime.config)?;
        super::hooks::validate(&definition.registrations, &runtime.config)?;
        Ok(definition)
    }
    pub fn validate(&self) -> Result<Config, ConfigError> {
        self.validate_metadata()?;
        let config = Config::parse(&self.model)?;
        self.validate_handlers(&config)?;
        super::hooks::validate(&self.registrations, &config)?;
        super::recognizers::validate(&self.recognizers, &self.registrations, &config)?;
        Ok(config)
    }
    fn validate_metadata(&self) -> Result<(), ConfigError> {
        super::modules::validate(&self.name, &self.source, &self.modules)?;
        if self.model.len() > MAX_CONFIG_BYTES {
            return Err(error("expanded Lua model exceeds 1 MiB"));
        }
        if self.api_version != API_VERSION
            || self.name.is_empty()
            || self.name.len() > 4096
            || self.name.contains('\0')
            || self.source.len() > MAX_CONFIG_BYTES
            || self.model.len() > MAX_CONFIG_BYTES
            || self.registrations.len() > MAX_SUBSCRIPTIONS
            || self.timers.len() > MAX_SUBSCRIPTIONS
            || self.handlers.len() > MAX_SUBSCRIPTIONS
            || self.tasks.len() > super::tasks::MAX_TASKS
            || self
                .tasks
                .iter()
                .any(|task| task.id == 0 || task.id > super::tasks::MAX_TASK_ID)
            || self.tasks.windows(2).any(|pair| pair[0].id >= pair[1].id)
            || self.timers.iter().enumerate().any(|(index, timer)| {
                usize::from(timer.id) != index + 1 || !(1..=3_600_000).contains(&timer.delay_ms)
            })
            || self
                .registrations
                .iter()
                .enumerate()
                .any(|(i, entry)| usize::from(entry.id) != i + 1)
        {
            return Err(error("invalid or oversized Lua source snapshot"));
        }
        Ok(())
    }
    pub fn instantiate(&self) -> Result<Runtime, ConfigError> {
        self.instantiate_progress(&mut || Ok(()))
    }
    pub(crate) fn instantiate_progress(
        &self,
        progress: &mut dyn FnMut() -> io::Result<()>,
    ) -> Result<Runtime, ConfigError> {
        let expected = self.validate()?;
        let mut runtime = instantiate(&self.source, &self.name, &self.modules, progress)?;
        if runtime.config != expected
            || runtime.registrations()? != self.registrations
            || runtime.timer_registrations()? != self.timers
            || runtime.handler_registrations()? != self.handlers
            || runtime.task_registrations()? != self.tasks
        {
            return Err(error(
                "Lua source differs from its approved model or registrations",
            ));
        }
        runtime.recognizers = self.recognizers.clone();
        runtime.host.borrow_mut().source = Some(std::sync::Arc::new(self.clone()));
        Ok(runtime)
    }
}
pub(super) mod source_bytes {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(source: &str, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(source))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > MAX_CONFIG_BYTES.div_ceil(3) * 4 {
            return Err(D::Error::custom("Lua source snapshot exceeds byte limit"));
        }
        let source = STANDARD.decode(encoded).map_err(D::Error::custom)?;
        if source.len() > MAX_CONFIG_BYTES {
            return Err(D::Error::custom("Lua source snapshot exceeds byte limit"));
        }
        String::from_utf8(source).map_err(D::Error::custom)
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogRecord {
    pub message: String,
    pub fields: serde_json::Value,
}
#[derive(Default)]
pub(super) struct Host {
    pub(super) offline: bool,
    pub(super) source: Option<std::sync::Arc<Definition>>,
    #[cfg(target_os = "linux")]
    pub(super) replay_budget: crate::replay::configured::Budget,
    pub(super) configuring: bool,
    pub(super) running: bool,
    pub(super) rule_hooks: bool,
    pub(super) recognizer_callbacks: bool,
    pub(super) logs: Vec<LogRecord>,
    pub(super) bytes: usize,
    pub(super) now: Option<Duration>,
    pub(super) task: Option<u64>,
    pub(super) sleep: Option<Duration>,
    pub(super) waiting: Option<StashedTable>,
    pub(super) task_joins: std::collections::BTreeMap<u64, u64>,
    pub(super) task_clock: Option<(Duration, Instant)>,
    pub(super) rules: super::rule_api::State,
    #[cfg(target_os = "linux")]
    pub(super) capabilities: Option<super::CapabilityView>,
    #[cfg(target_os = "linux")]
    pub(super) status: Option<super::StatusView>,
    #[cfg(target_os = "linux")]
    pub(super) context: Option<crate::context_transfer::View>,
    #[cfg(target_os = "linux")]
    pub(super) context_clock: Option<Instant>,
    #[cfg(target_os = "linux")]
    pub(super) devices: super::devices::Catalog,
}
pub(super) fn logger<'gc>(ctx: &Context<'gc>, host: Rc<RefCell<Host>>) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        if !host.borrow().running {
            return Err(
                io::Error::other("logging is unavailable during configuration evaluation").into(),
            );
        }
        let (message, fields): (luna::String, Value) = stack.consume(ctx)?;
        let message = message.to_str().map_err(io::Error::other)?;
        if message.len() > 1024 {
            return Err(io::Error::other("Lua log message exceeds byte limit").into());
        }
        values::budget(ctx, fields, 16, 32768, MAX_LOG_BYTES * 32)?;
        let fields: serde_json::Value = luna_util::serde::from_value_with(
            ctx,
            fields,
            luna_util::serde::DeOptions::default().max_depth(16),
        )
        .map_err(io::Error::other)?;
        let entry = LogRecord {
            message: message.into(),
            fields,
        };
        let bytes = bounded_json(&entry, MAX_LOG_BYTES)?.len();
        let mut state = host.borrow_mut();
        if state.logs.len() >= 32 || state.bytes + bytes > MAX_LOG_BYTES {
            return Err(io::Error::other("Lua log queue exceeds limit").into());
        }
        state.bytes += bytes;
        state.logs.push(entry);
        stack.replace(ctx, ());
        Ok(CallbackReturn::Return)
    })
}

pub struct Runtime {
    pub(super) lua: Lua,
    pub(super) registry: StashedTable,
    pub(super) timers: StashedTable,
    pub(super) handlers: StashedTable,
    pub(super) tasks: StashedTable,
    pub(super) task_executors: std::collections::BTreeMap<u64, super::tasks::TaskExecution>,
    pub(super) host: Rc<RefCell<Host>>,
    pub config: Config,
    pub(super) failed: bool,
    pub(super) recognizers: Vec<super::recognizers::Binding>,
    pub(super) recognition_last: Option<(crate::protocol::Fence, u64)>,
    pub(super) recognition_candidates:
        std::collections::BTreeMap<u64, crate::recognition::Cancellation>,
}
impl Runtime {
    #[cfg(target_os = "linux")]
    pub(crate) fn offline(&mut self, config: Config) -> Result<(), ConfigError> {
        let handlers = self.handler_registrations()?;
        self.config = config.clone();
        let mut host = self.host.borrow_mut();
        host.offline = true;
        host.context_clock = None;
        host.rules.initialize(config, handlers);
        Ok(())
    }
    pub fn load(source: &str, name: &str) -> Result<Self, ConfigError> {
        let mut runtime = instantiate(source, name, &Default::default(), &mut || Ok(()))?;
        let definition = Definition::snapshot(
            &super::Sources::single(source.into(), name.into()),
            &mut runtime,
        )?;
        runtime.host.borrow_mut().source = Some(std::sync::Arc::new(definition));
        Ok(runtime)
    }
    pub fn registrations(&mut self) -> Result<Vec<Registration>, ConfigError> {
        self.lua
            .try_enter(|ctx| {
                let registry = ctx.fetch(&self.registry);
                let mut result = Vec::new();
                for index in 1..=registry.length(&ctx) {
                    let entry: Table = registry.get(ctx, index)?;
                    let kind: luna::String = entry.get(ctx, "kind")?;
                    let kind =
                        serde_json::from_value(serde_json::Value::String(kind.to_str()?.into()))
                            .map_err(io::Error::other)?;
                    result.push(Registration {
                        id: index as u16,
                        kind,
                        active: entry.get(ctx, "active")?,
                        rule: values::decode(ctx, entry.get(ctx, "rule")?, 4, 4096, 64 * 1024)?,
                        recognizer: values::decode(
                            ctx,
                            entry.get(ctx, "recognizer")?,
                            4,
                            65536,
                            1024 * 1024,
                        )?,
                    });
                }
                Ok(result)
            })
            .map_err(error)
    }
    /// A callback failure retires this VM. Logs are committed only after every
    /// callback for the event succeeds; failed work is never retried silently.
    pub fn dispatch(
        &mut self,
        kind: EventKind,
        payload: &serde_json::Value,
    ) -> Result<Vec<LogRecord>, ConfigError> {
        if self.failed {
            return Err(error("Lua runtime was retired after callback failure"));
        }
        bounded_json(payload, MAX_EVENT_BYTES).map_err(error)?;
        #[cfg(target_os = "linux")]
        if kind == EventKind::Input {
            let result = self.host.borrow_mut().devices.apply(&self.config, payload);
            if let Err(reason) = result {
                self.failed = true;
                self.host
                    .borrow_mut()
                    .rules
                    .discard("invalid permitted device metadata");
                return Err(reason);
            }
        }
        self.host.borrow_mut().running = true;
        let started = Instant::now();
        let result = self.invoke(kind, payload, started);
        let mut host = self.host.borrow_mut();
        host.running = false;
        host.bytes = 0;
        match result {
            Ok(()) => Ok(std::mem::take(&mut host.logs)),
            Err(reason) => {
                self.failed = true;
                host.logs.clear();
                host.rules.discard("Lua callback failed");
                Err(reason)
            }
        }
    }
    fn invoke(
        &mut self,
        kind: EventKind,
        payload: &serde_json::Value,
        started: Instant,
    ) -> Result<(), ConfigError> {
        let count = self.lua.enter(|ctx| ctx.fetch(&self.registry).length(&ctx));
        // Subscribers added by a callback start receiving the next event.
        for index in 1..=count {
            if self
                .recognizers
                .iter()
                .any(|binding| i64::from(binding.subscription) == index)
            {
                continue;
            }
            let executor = self
                .lua
                .try_enter(|ctx| {
                    let entry: Table = ctx.fetch(&self.registry).get(ctx, index)?;
                    let active: bool = entry.get(ctx, "active")?;
                    let event: luna::String = entry.get(ctx, "kind")?;
                    if !active || event.to_str()? != kind.name() {
                        return Ok(None);
                    }
                    let rule: Option<super::hooks::RuleHook> =
                        values::decode(ctx, entry.get(ctx, "rule")?, 4, 4096, 64 * 1024)?;
                    if rule
                        .as_ref()
                        .is_some_and(|rule| !rule.matches(kind, payload))
                    {
                        return Ok(None);
                    }
                    let callback: Function = entry.get(ctx, "callback")?;
                    let value = values::event_payload(ctx, kind, payload)?;
                    Ok(Some(ctx.stash(Executor::start(ctx, callback, (value,)))))
                })
                .map_err(error)?;
            let Some(executor) = executor else {
                continue;
            };
            self.invoke_executor(&executor, started)?;
        }
        Ok(())
    }
    pub(super) fn invoke_executor(
        &mut self,
        executor: &StashedExecutor,
        started: Instant,
    ) -> Result<(), ConfigError> {
        drive_budget(
            &mut self.lua,
            executor,
            started,
            CALLBACK_TIME,
            CALLBACK_INSTRUCTIONS,
        )?;
        self.lua
            .try_enter(|ctx| {
                let executor = ctx.fetch(executor);
                let values: luna::Variadic<Vec<Value>> =
                    executor.take_result(ctx).map_err(io::Error::other)??;
                if executor.mode() != luna::ExecutorMode::Stopped || !values.0.is_empty() {
                    return Err(io::Error::other(
                        "Lua event callbacks must return no values and cannot yield",
                    )
                    .into());
                }
                Ok(())
            })
            .map_err(error)?;
        Ok(())
    }
}
pub(crate) fn bounded_json<T: Serialize + ?Sized>(
    value: &T,
    maximum: usize,
) -> io::Result<Vec<u8>> {
    struct Buffer {
        bytes: Vec<u8>,
        maximum: usize,
    }
    impl io::Write for Buffer {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            if self.bytes.len() + input.len() > self.maximum {
                return Err(io::Error::other("Lua event or log exceeds byte limit"));
            }
            self.bytes.extend_from_slice(input);
            Ok(input.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer {
        bytes: Vec::new(),
        maximum,
    };
    serde_json::to_writer(&mut buffer, value).map_err(io::Error::other)?;
    Ok(buffer.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_registration_limits_survive_replaced_lua_globals() {
        for registration in [
            "p.events.on('matched',function()end)",
            "p.scheduler.after('1s',function()end)",
        ] {
            let source = format!(
                "local p=require('prek'); p.events.on('ready',function()assert=function()end;type=function()return 'function'end; for i=1,257 do {registration} end end);return {{version=1}}"
            );
            let mut runtime = Runtime::load(&source, "runtime-quota.lua").unwrap();
            let error = runtime
                .dispatch(EventKind::Ready, &serde_json::Value::Null)
                .expect_err("mutable Lua globals must not disable a host registration quota");
            assert!(error.to_string().contains("limit exceeded"), "{error}");
        }
    }
    #[test]
    fn subscriptions_keep_closures_and_dispose_before_later_callbacks() {
        let mut runtime = Runtime::load(
            r#"
local p=require('prek')
local count=0
local second
p.events.on('matched', function(event)
  count=count+1
  second:dispose()
  p.log.info('match', {count=count, rule=event.rule})
end)
second=p.events.on('matched', function() error('disposed callback executed') end)
return {version=1}
"#,
            "closures.lua",
        )
        .unwrap();
        for count in 1..=2 {
            let logs = runtime
                .dispatch(EventKind::Matched, &serde_json::json!({"rule":"fixture"}))
                .unwrap();
            assert_eq!(logs.len(), 1);
            assert_eq!(
                logs[0].fields,
                serde_json::json!({"count":count,"rule":"fixture"})
            );
        }
    }
    #[test]
    fn callback_failure_discards_logs_and_retires_the_vm() {
        for body in [
            "error('fixture')",
            "while true do end",
            "coroutine.yield()",
            "return 1",
        ] {
            let source = format!(
                "local p=require('prek'); p.events.on('matched', function() p.log.info('uncommitted'); {body} end); return {{version=1}}"
            );
            let mut runtime = Runtime::load(&source, "failure.lua").unwrap();
            assert!(
                runtime
                    .dispatch(EventKind::Matched, &serde_json::Value::Null)
                    .is_err(),
                "{body}"
            );
            assert!(runtime.host.borrow().logs.is_empty());
            assert!(
                runtime
                    .dispatch(EventKind::Matched, &serde_json::Value::Null)
                    .err()
                    .unwrap()
                    .0
                    .contains("retired")
            );
        }
    }
    #[test]
    fn snapshot_round_trip_preserves_source_and_verifies_model_and_handlers() {
        let source =
            "local p=require('prek'); p.events.on('ready', function() end); return {version=1}";
        let snapshot = Definition::compile(source, "approved.lua").unwrap();
        let text = serde_json::to_vec(&snapshot).unwrap();
        let mut decoded: Definition = serde_json::from_slice(&text).unwrap();
        assert_eq!(decoded.source, source);
        decoded.instantiate().unwrap();
        decoded.model = toml::to_string(&Config::preset("phone").unwrap()).unwrap();
        assert!(decoded.instantiate().is_err());
        decoded.model = snapshot.model;
        decoded.registrations[0].kind = EventKind::Stop;
        assert!(decoded.instantiate().is_err());
    }
    #[test]
    fn registration_and_log_limits_are_enforced_without_configuration_effects() {
        assert!(
            Runtime::load(
                "require('prek').log.info('forbidden'); return {version=1}",
                "effects.lua"
            )
            .is_err()
        );
        assert!(Runtime::load("local p=require('prek'); for i=1,257 do p.events.on('ready',function() end) end; return {version=1}", "too-many.lua").is_err());
        let mut runtime=Runtime::load("local p=require('prek'); p.events.on('ready', function() for i=1,33 do p.log.info('overflow') end end); return {version=1}", "logs.lua").unwrap();
        assert!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::Value::Null)
                .is_err()
        );
        assert!(runtime.host.borrow().logs.is_empty());
    }
}
