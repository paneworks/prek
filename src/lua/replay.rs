//! Generation-local offline replay. No input descriptors or action executor.
use super::*;
use crate::replay::Record;
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::BTreeSet,
    rc::{Rc, Weak},
};

pub(super) const MAX_HANDLES: usize = 4;
pub(super) const MAX_RECORDS: usize = 4096;
pub(super) const MAX_RECORD_BYTES: usize = 32 * 1024;
pub(super) const MAX_BYTES: usize = 1024 * 1024;
pub(super) const MAX_CONFIG_VALUES: usize = 65_536;
const MAX_SOURCES: usize = 16;
const MAX_OUTCOMES: usize = 4096;

enum Engine {
    Observations(Box<crate::replay::Replay>),
    Rules(Box<crate::rules::Engine>),
    #[cfg(target_os = "linux")]
    Configured(Box<crate::replay::configured::Configured>),
}
impl Engine {
    fn push(&mut self, record: Record) -> io::Result<()> {
        match self {
            Self::Observations(replay) => replay.push(record),
            Self::Rules(engine) => engine.push(record),
            #[cfg(target_os = "linux")]
            Self::Configured(engine) => engine.push(record),
        }
    }
    fn check(&self) -> io::Result<()> {
        match self {
            Self::Observations(replay) => replay.check(),
            Self::Rules(engine) => engine.check(),
            #[cfg(target_os = "linux")]
            Self::Configured(engine) => engine.check(),
        }
    }
    fn count(&self) -> usize {
        match self {
            Self::Observations(replay) => replay.observations().len(),
            Self::Rules(engine) => engine.events().len(),
            #[cfg(target_os = "linux")]
            Self::Configured(engine) => engine.events().len(),
        }
    }
    fn encoded(&self, from: usize, limit: usize) -> io::Result<String> {
        let bytes = match self {
            Self::Observations(replay) => {
                runtime::bounded_json(&replay.observations()[from..], limit)
            }
            Self::Rules(engine) => runtime::bounded_json(&engine.events()[from..], limit),
            #[cfg(target_os = "linux")]
            Self::Configured(engine) => runtime::bounded_json(&engine.events()[from..], limit),
        }?;
        String::from_utf8(bytes).map_err(io::Error::other)
    }
    fn finish(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Self::Configured(engine) = self {
            engine.finish(false)?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct Session {
    #[serde(skip)]
    engine: Option<Engine>,
    mode: &'static str,
    state: &'static str,
    records: usize,
    bytes: usize,
    #[serde(skip)]
    sources: BTreeSet<u64>,
    #[serde(skip)]
    outcome_bytes: usize,
    outcomes: usize,
}
impl Session {
    #[cfg(target_os = "linux")]
    fn approved(
        config: Config,
        source: Option<std::sync::Arc<Definition>>,
        budget: crate::replay::configured::Budget,
    ) -> io::Result<Self> {
        let mut session = Self::new(None)?;
        runtime::bounded_json(&config, MAX_CONFIG_BYTES)?;
        session.engine = Some(Engine::Configured(Box::new(
            crate::replay::configured::Configured::with_budget(config, source, budget)?,
        )));
        session.mode = "rules";
        Ok(session)
    }
    fn new(config: Option<Config>) -> io::Result<Self> {
        let (mode, engine) = match config {
            Some(config) => {
                runtime::bounded_json(&config, MAX_CONFIG_BYTES)?;
                (
                    "rules",
                    Engine::Rules(Box::new(crate::rules::Engine::new(config)?)),
                )
            }
            None => ("observations", Engine::Observations(Box::default())),
        };
        Ok(Self {
            engine: Some(engine),
            mode,
            state: "open",
            records: 0,
            bytes: 0,
            sources: BTreeSet::new(),
            outcome_bytes: 2,
            outcomes: 0,
        })
    }
    fn open(&mut self) -> io::Result<&mut Engine> {
        self.engine
            .as_mut()
            .ok_or_else(|| io::Error::other("offline replay is closed"))
    }
    fn fail<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.engine = None;
            self.sources.clear();
            self.state = "failed";
        }
        result
    }
    fn push(&mut self, text: &str) -> io::Result<()> {
        self.open()?;
        let result = (|| {
            if text.len() > MAX_RECORD_BYTES
                || self.records >= MAX_RECORDS
                || self.bytes + text.len() > MAX_BYTES
            {
                return Err(io::Error::other("offline replay input limit exceeded"));
            }
            let record: Record = serde_json::from_str(text)?;
            if let Record::Device { device, .. } = &record {
                self.sources.insert(device.source.device);
                if self.sources.len() > MAX_SOURCES {
                    return Err(io::Error::other("offline replay source limit exceeded"));
                }
            }
            let previous = self.outcomes;
            let engine = self.open()?;
            engine.push(record)?;
            let count = engine.count();
            if count > MAX_OUTCOMES {
                return Err(io::Error::other("offline replay outcome limit exceeded"));
            }
            let remaining = MAX_BYTES.saturating_sub(self.outcome_bytes);
            let added = self
                .engine
                .as_ref()
                .unwrap()
                .encoded(previous, remaining)?
                .len();
            self.outcome_bytes += added;
            self.outcomes = count;
            self.records += 1;
            self.bytes += text.len();
            Ok(())
        })();
        self.fail(result)
    }
    fn finish(&mut self, check: bool) -> io::Result<String> {
        self.open()?;
        let result = (|| {
            if self.records == 0 {
                return Err(io::Error::other("empty offline replay"));
            }
            let engine = self.open()?;
            if check {
                engine.check()?;
            }
            engine.finish()?;
            engine.encoded(0, MAX_BYTES)
        })();
        let json = self.fail(result)?;
        self.engine = None;
        self.sources.clear();
        self.state = "finished";
        Ok(json)
    }
    fn close(&mut self) {
        self.engine = None;
        self.sources.clear();
        self.state = "closed";
    }
}

// Replay envelopes contain only the native recording/outcome schemas, never
// caller-defined objects. All their opaque unsigned identities use strings.
fn identities(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                if matches!(
                    name.as_str(),
                    "device"
                        | "generation"
                        | "sequence"
                        | "id"
                        | "ordinal"
                        | "config_generation"
                        | "session_generation"
                ) && let Some(number) = value.as_u64()
                {
                    *value = serde_json::Value::String(number.to_string());
                } else {
                    identities(value);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(identities),
        serde_json::Value::Number(number)
            if number.as_u64().is_some_and(|n| n > i64::MAX as u64) =>
        {
            *value = serde_json::Value::String(number.to_string());
        }
        _ => {}
    }
}

// Count expanded Lua values before deserialization can duplicate shared tables
// into native vectors. Tiny cyclic/aliased tables must not bypass host limits.
fn configuration_budget<'gc>(ctx: luna::Context<'gc>, value: Value<'gc>) -> io::Result<()> {
    fn visit<'gc>(
        ctx: luna::Context<'gc>,
        value: Value<'gc>,
        depth: usize,
        values: &mut usize,
        bytes: &mut usize,
    ) -> io::Result<()> {
        *values += 1;
        *bytes += match value {
            Value::String(text) => text.as_bytes().len(),
            _ => 16,
        };
        if depth > 64 || *values > MAX_CONFIG_VALUES || *bytes > MAX_BYTES {
            return Err(io::Error::other(
                "offline replay configuration value limit exceeded",
            ));
        }
        if let Value::Table(table) = value {
            for (key, value) in table.iter(ctx) {
                visit(ctx, key, depth + 1, values, bytes)?;
                visit(ctx, value, depth + 1, values, bytes)?;
            }
        }
        Ok(())
    }
    visit(ctx, value, 0, &mut 0, &mut 0)
}

pub(super) fn register<'gc>(
    ctx: &luna::Context<'gc>,
    api: Table<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Result<(), luna::Error<'gc>> {
    let sessions: Rc<RefCell<Vec<Weak<RefCell<Session>>>>> = Rc::default();
    api.set(
        *ctx,
        "new",
        Callback::from_fn(ctx, move |ctx, _, mut stack| {
            let value: Value = stack.consume(ctx)?;
            configuration_budget(ctx, value)?;
            let config: Option<Config> = luna_util::serde::from_value_with(
                ctx,
                value,
                luna_util::serde::DeOptions::default().max_depth(64),
            )
            .map_err(io::Error::other)?;
            let mut active = sessions.borrow_mut();
            active.retain(|session| {
                session
                    .upgrade()
                    .is_some_and(|session| session.borrow().engine.is_some())
            });
            if active.len() >= MAX_HANDLES {
                return Err(io::Error::other("too many open offline replays").into());
            }
            #[cfg(target_os = "linux")]
            let session = match config {
                Some(config) => {
                    let host = host.borrow();
                    if !config.recognizer_definitions().is_empty() && (host.configuring || host.offline) {
                        return Err(io::Error::other("custom replay requires an approved runtime source and cannot nest inside replay").into());
                    }
                    Session::approved(config, host.source.clone(), host.replay_budget.clone())?
                }
                None => Session::new(None)?,
            };
            #[cfg(not(target_os = "linux"))]
            let session = { let _ = &host; Session::new(config)? };
            let session = Rc::new(RefCell::new(session));
            active.push(Rc::downgrade(&session));
            let handle = Table::new(&ctx);
            for operation in ["push", "finish", "status", "close"] {
                let session = session.clone();
                handle.set(
                    ctx,
                    operation,
                    Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                        let mut session = session.borrow_mut();
                        match operation {
                            "push" => {
                                let (_, record): (Value, luna::String) = stack.consume(ctx)?;
                                session.push(record.to_str().map_err(io::Error::other)?)?;
                                stack.replace(ctx, ());
                            }
                            "finish" => {
                                let (_, check): (Value, Option<bool>) = stack.consume(ctx)?;
                                let checked = check.unwrap_or(false);
                                let json = session.finish(checked)?;
                                let mut outcomes: serde_json::Value = serde_json::from_str(&json)?;
                                identities(&mut outcomes);
                                let result = Table::new(&ctx);
                                result.set(ctx, "mode", session.mode)?;
                                result.set(ctx, "records", session.records)?;
                                result.set(ctx, "bytes", session.bytes)?;
                                result.set(ctx, "checked", checked)?;
                                result.set(ctx, "json", ctx.intern(json.as_bytes()))?;
                                result.set(
                                    ctx,
                                    "outcomes",
                                    luna_util::serde::to_value(ctx, &outcomes)
                                        .map_err(io::Error::other)?,
                                )?;
                                stack.replace(ctx, result);
                            }
                            "status" => stack.replace(
                                ctx,
                                luna_util::serde::to_value(ctx, &*session)
                                    .map_err(io::Error::other)?,
                            ),
                            "close" => {
                                session.close();
                                stack.replace(ctx, ());
                            }
                            _ => unreachable!(),
                        }
                        Ok(CallbackReturn::Return)
                    }),
                )?;
            }
            stack.replace(ctx, handle);
            Ok(CallbackReturn::Return)
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
