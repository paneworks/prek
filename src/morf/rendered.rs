//! Native, read-only named bounds from Morf's submitted primary frame.
//! These are surface-local inspection data, not input regions or presentation
//! acknowledgement. They cannot establish a keyboard's absence or a safe scope.
use super::*;
use std::collections::BTreeSet;

pub const MAX_BYTES: usize = 128 * 1024;
pub const MAX_REGIONS: usize = 256;

mod decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        let number = value.parse::<u64>().map_err(D::Error::custom)?;
        if number.to_string() != value {
            return Err(D::Error::custom("noncanonical decimal"));
        }
        Ok(number)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    /// Opaque generational native node identity, scoped by endpoint and VM.
    #[serde(with = "decimal")]
    pub node: u64,
    pub id: String,
    pub bounds: Bounds,
    /// Whether this node itself is an enabled native input target. Bounds
    /// still do not express its exact hit shape or the surface's input mask.
    pub interactive: bool,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub scale_120: u32,
    pub regions: Vec<Region>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    PrimarySurfaceNamedBounds,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputContext {
    pub output: String,
    pub context: Context,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub version: u32,
    pub epoch: String,
    #[serde(with = "decimal")]
    pub ui_generation: u64,
    #[serde(with = "decimal")]
    pub revision: u64,
    #[serde(with = "decimal")]
    pub sampled_ns: u64,
    pub coverage: Coverage,
    pub frame: Option<Frame>,
}
impl Context {
    pub fn fresh(&self, now: Duration) -> bool {
        self.frame.is_some()
            && Duration::from_nanos(self.sampled_ns) <= now
            && now - Duration::from_nanos(self.sampled_ns) < crate::context::SNAPSHOT_MAX_AGE
    }
    pub fn validate(
        &self,
        state: &ShellState,
        now: Duration,
        previous: Option<&Self>,
    ) -> io::Result<()> {
        state.validate()?;
        if self.version != 1
            || self.epoch != state.epoch
            || self.ui_generation == 0
            || state.phase != ShellPhase::Configured
            || previous.is_some_and(|old| {
                old.epoch != self.epoch
                    || old.ui_generation > self.ui_generation
                    || old.ui_generation == self.ui_generation
                        && (old.revision > self.revision
                            || old.revision == self.revision
                                && old.frame.is_some()
                                && self.frame.is_some()
                                && old.frame != self.frame)
            })
        {
            return Err(invalid(
                "native rendered context changed binding or regressed",
            ));
        }
        if let Some(frame) = &self.frame {
            if self.revision == 0
                || !self.fresh(now)
                || frame.width == 0
                || frame.height == 0
                || frame.width > 1_000_000
                || frame.height > 1_000_000
                || frame.scale_120 == 0
                || frame.scale_120 > 120 * 64
                || frame.regions.len() > MAX_REGIONS
            {
                return Err(invalid("invalid or stale native rendered frame"));
            }
            let mut nodes = BTreeSet::new();
            for region in &frame.regions {
                let b = region.bounds;
                if region.id.is_empty()
                    || region.id.len() > 128
                    || region.id.chars().any(char::is_control)
                    || region.node == 0
                    || !nodes.insert(region.node)
                    || [b.x, b.y, b.width, b.height]
                        .into_iter()
                        .any(|n| !n.is_finite() || n.abs() > 1_000_000.0)
                    || b.x < 0.0
                    || b.y < 0.0
                    || b.width <= 0.0
                    || b.height <= 0.0
                    || b.x + b.width > f64::from(frame.width) + 1e-6
                    || b.y + b.height > f64::from(frame.height) + 1e-6
                {
                    return Err(invalid("invalid native named bounds"));
                }
            }
        }
        Ok(())
    }
}
impl Client<ShellState> {
    /// The whole capability/query/binding bracket shares one RPC deadline.
    pub fn rendered_context(
        &mut self,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> io::Result<Option<Context>> {
        self.rendered_context_until(logind, compositor, Instant::now() + RPC_TIME)
    }
    pub(crate) fn rendered_context_until(
        &mut self,
        logind: &Logind,
        compositor: &hyprland::Client,
        until: Instant,
    ) -> io::Result<Option<Context>> {
        let result = (|| {
            let capabilities = self.control_capabilities_until(logind, compositor, until)?;
            if !capabilities.has_rendered_context() {
                return Ok(None);
            }
            let bytes = self.transport.exchange_limit(
                b"{\"version\":1,\"op\":\"rendered-context\"}\n",
                until,
                &mut false,
                MAX_BYTES,
            )?;
            let context: Context = serde_json::from_slice(&bytes)?;
            let state = self.sample_until(logind, compositor, until)?;
            context.validate(&state, crate::ipc::monotonic()?, self.rendered.as_ref())?;
            if context.ui_generation != capabilities.ui_generation() {
                return Err(invalid(
                    "native rendered context changed UI generation during query",
                ));
            }
            // Retain a known frame across temporarily unavailable observations
            // so an equal revision cannot replace its geometry after a gap.
            if context.frame.is_some()
                || self
                    .rendered
                    .as_ref()
                    .is_none_or(|old| old.ui_generation != context.ui_generation)
            {
                self.rendered = Some(context.clone());
            }
            Ok(Some(context))
        })();
        if result.is_err() {
            self.transport.retire();
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod tests;
