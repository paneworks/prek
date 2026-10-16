//! Native, read-only named bounds from Morf's submitted primary frame.
//! These are surface-local inspection data, not input regions or presentation
//! acknowledgement. They cannot establish a keyboard's absence or a safe scope.
use super::*;
use std::collections::BTreeSet;
mod projection;
pub use projection::{LayoutProjection, ProjectedRegion};

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
    /// Declarative meaning attached to this actual native frame node. This
    /// supplies no screen mapping, exact hit shape or input ownership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<Semantic>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticRole {
    Keyboard,
    SecureOverlay,
    ClockBackground,
    ContentBottom,
    PhysicalBottom,
    TopLeft,
    TopRight,
    SideLeft,
    SideRight,
    LocalControl,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Semantic {
    pub role: SemanticRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
}
impl Semantic {
    fn validate(&self) -> io::Result<()> {
        if match &self.layout {
            Some(layout) => {
                self.role != SemanticRole::Keyboard
                    || layout.is_empty()
                    || layout.len() > 128
                    || layout.chars().any(char::is_control)
            }
            None => self.role == SemanticRole::Keyboard,
        } {
            return Err(invalid("invalid native semantic region"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub scale_120: u32,
    pub regions: Vec<Region>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<SurfaceSpace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<KeyboardRegistry>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceSpace {
    pub namespace: String,
    pub width: u32,
    pub height: u32,
    pub local_scale: f64,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryScope {
    RegisteredMorfPrimary,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardState {
    Unknown,
    Hidden,
    Visible,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardRegistry {
    pub scope: RegistryScope,
    pub registered: usize,
    pub state: KeyboardState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
}
impl KeyboardRegistry {
    fn validate(&self, regions: &[Region]) -> io::Result<()> {
        let visible_count = regions
            .iter()
            .filter(|region| {
                region
                    .semantic
                    .as_ref()
                    .is_some_and(|s| s.role == SemanticRole::Keyboard)
            })
            .count();
        if self.registered < visible_count {
            return Err(invalid("keyboard registry omits visible declarations"));
        }
        if self.registered > 65_536 {
            return Err(invalid("excessive keyboard registry"));
        }
        if self.state == KeyboardState::Unknown {
            return if self.node.is_none() && self.layout.is_none() {
                Ok(())
            } else {
                Err(invalid("unknown keyboard carries a target"))
            };
        }
        let node = self
            .node
            .as_ref()
            .and_then(|node| {
                node.parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0 && value.to_string() == *node)
            })
            .ok_or_else(|| invalid("invalid registered keyboard node"))?;
        Semantic {
            role: SemanticRole::Keyboard,
            layout: self.layout.clone(),
        }
        .validate()?;
        let visible = regions.iter().find(|region| region.node == node);
        if self.registered != 1
            || match self.state {
                KeyboardState::Hidden => visible.is_some() || visible_count != 0,
                KeyboardState::Visible => {
                    visible_count != 1
                        || visible.is_none_or(|region| {
                            region.semantic.as_ref().is_none_or(|s| {
                                s.role != SemanticRole::Keyboard || s.layout != self.layout
                            })
                        })
                }
                KeyboardState::Unknown => unreachable!(),
            }
        {
            return Err(invalid("keyboard registry contradicts its frame"));
        }
        Ok(())
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutProjection>,
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
            if let Some(surface) = &frame.surface
                && (surface.namespace.is_empty()
                    || surface.namespace.len() > 256
                    || surface.namespace.chars().any(char::is_control)
                    || surface.width == 0
                    || surface.height == 0
                    || surface.width > 1_000_000
                    || surface.height > 1_000_000
                    || !surface.local_scale.is_finite()
                    || surface.local_scale <= 0.0
                    || surface.local_scale > 64.0
                    || (f64::from(frame.width) * surface.local_scale - f64::from(surface.width))
                        .abs()
                        > surface.local_scale + 1e-6
                    || (f64::from(frame.height) * surface.local_scale - f64::from(surface.height))
                        .abs()
                        > surface.local_scale + 1e-6)
            {
                return Err(invalid("invalid native surface coordinate space"));
            }
            if let Some(keyboard) = &frame.keyboard {
                keyboard.validate(&frame.regions)?;
            }
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
                if let Some(semantic) = &region.semantic {
                    semantic.validate()?;
                }
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
            if context.frame.as_ref().is_some_and(|frame| {
                frame.surface.is_some() && !capabilities.has_layer_space()
                    || frame.keyboard.is_some() != capabilities.has_keyboard_registry()
            }) {
                return Err(invalid(
                    "native surface/registry metadata was not negotiated",
                ));
            }
            if !capabilities.has_semantic_regions()
                && context.frame.as_ref().is_some_and(|frame| {
                    frame.regions.iter().any(|region| region.semantic.is_some())
                })
            {
                return Err(invalid("native semantic regions were not negotiated"));
            }
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
