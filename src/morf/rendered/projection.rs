//! Conversion against a matched stock compositor layout. Stock IPC reports a
//! layer's layout box; it is the rendered position only once the layer's
//! map-in animation is provably over.
use super::*;
use crate::{
    context::{OutputState, Rect},
    gestures::{Output, Point},
    hyprland::layers::{Inventory, Layer},
};

/// Covers the compositor's final animation tick after the curve completes.
const SETTLE_MARGIN: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// The layout box, which can differ from an animating rendered box.
    StockLayerLayout,
    /// The layout box after the map-in animation bound elapsed before the
    /// first bracketing sample. Stock layers move without animation after
    /// mapping, so it is their current position, not their presentation.
    SettledStockLayer,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedRegion {
    #[serde(with = "super::decimal")]
    pub node: u64,
    pub id: String,
    pub bounds: Option<Rect>,
    pub interactive: bool,
    pub semantic: Option<Semantic>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutProjection {
    pub basis: Basis,
    pub producer_pid: u32,
    #[serde(with = "super::decimal")]
    pub producer_start: u64,
    pub output: Output,
    #[serde(with = "super::decimal")]
    pub geometry_generation: u64,
    /// Before the first bracketing layer sample.
    #[serde(with = "super::decimal")]
    pub started_ns: u64,
    #[serde(with = "super::decimal")]
    pub sampled_ns: u64,
    /// The `layersIn` bound, present only when both samples agree.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "super::decimal::optional"
    )]
    pub map_in_ns: Option<u64>,
    /// When the layout box became the layer's position: its map bound plus
    /// the map-in bound and a final tick.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "super::decimal::optional"
    )]
    pub rendered_from_ns: Option<u64>,
    pub surface: Layer,
    pub regions: Vec<ProjectedRegion>,
    /// The committed input region as global logical boxes, rounded outward
    /// and clipped to the layer; None when Morf did not describe it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<Rect>>,
    /// No other process had a bottom, top or overlay layer on this output in
    /// either bracketing sample.
    pub exclusive: bool,
}
impl LayoutProjection {
    pub(crate) fn from_samples(
        context: &Context,
        output: &OutputState,
        before: &Inventory,
        after: &Inventory,
        producer: (u32, u64),
        (started, at): (Duration, Duration),
    ) -> io::Result<Option<Self>> {
        let Some(frame) = context.frame.as_ref().filter(|_| context.fresh(at)) else {
            return Ok(None);
        };
        let Some(space) = &frame.surface else {
            return Ok(None);
        };
        let select = |layers: &[Layer]| -> Option<Layer> {
            let mut matching = layers.iter().filter(|layer| {
                layer.output == output.geometry.name
                    && layer.pid == Some(producer.0)
                    && layer.namespace == space.namespace
            });
            let value = matching.next()?;
            if matching.next().is_some() {
                return None;
            }
            Some(value.clone())
        };
        let Some(layer) = select(&before.layers) else {
            return Ok(None);
        };
        layer.validate(output)?;
        if select(&after.layers).as_ref() != Some(&layer)
            || layer.rect.size
                != (Point {
                    x: f64::from(space.width),
                    y: f64::from(space.height),
                })
        {
            return Ok(None);
        }
        if producer.0 == 0
            || producer.1 == 0
            || started > at
            || !output.rect().contains_rect(layer.rect)
        {
            return Err(invalid("invalid native layout projection binding"));
        }
        let nanos = |time: Duration| {
            u64::try_from(time.as_nanos()).map_err(|_| invalid("projection time overflow"))
        };
        let started_ns = nanos(started)?;
        let map_in = before.map_in.filter(|_| before.map_in == after.map_in);
        let map_in_ns = map_in.map(nanos).transpose()?;
        let rendered_from_ns = match (space.mapped_ns, map_in) {
            (Some(mapped), Some(map_in)) => mapped.checked_add(nanos(map_in + SETTLE_MARGIN)?),
            _ => None,
        };
        let basis = if rendered_from_ns.is_some_and(|from| from <= started_ns) {
            Basis::SettledStockLayer
        } else {
            Basis::StockLayerLayout
        };
        let (width, height, scale) = (
            f64::from(space.width),
            f64::from(space.height),
            space.local_scale,
        );
        let place = |x: f64, y: f64, right: f64, bottom: f64| {
            let (x, y) = (x.clamp(0.0, width), y.clamp(0.0, height));
            let (right, bottom) = (right.clamp(x, width), bottom.clamp(y, height));
            (right > x && bottom > y).then_some(Rect {
                origin: Point {
                    x: layer.rect.origin.x + x,
                    y: layer.rect.origin.y + y,
                },
                size: Point {
                    x: right - x,
                    y: bottom - y,
                },
            })
        };
        let regions = frame
            .regions
            .iter()
            .map(|region| {
                let b = region.bounds;
                ProjectedRegion {
                    node: region.node,
                    id: region.id.clone(),
                    bounds: place(
                        b.x * scale,
                        b.y * scale,
                        (b.x + b.width) * scale,
                        (b.y + b.height) * scale,
                    ),
                    interactive: region.interactive,
                    semantic: region.semantic.clone(),
                }
            })
            .collect();
        // Morf scales its own region outward before committing it.
        let input = frame.input.as_ref().map(|rects| {
            rects
                .iter()
                .filter_map(|r| {
                    let edge = |n: i32| f64::from(n) * scale;
                    place(
                        edge(r.x).floor(),
                        edge(r.y).floor(),
                        edge(r.x + r.width).ceil(),
                        edge(r.y + r.height).ceil(),
                    )
                })
                .collect()
        });
        let exclusive = before.layers.iter().chain(&after.layers).all(|other| {
            other.output != output.geometry.name
                || other.level == 0
                || other.pid == Some(producer.0)
        });
        let value = Self {
            basis,
            producer_pid: producer.0,
            producer_start: producer.1,
            output: output.geometry.clone(),
            geometry_generation: output.geometry_generation,
            started_ns,
            sampled_ns: nanos(at)?,
            map_in_ns,
            rendered_from_ns,
            surface: layer,
            regions,
            input,
            exclusive,
        };
        Ok(Some(value))
    }
    /// Whether the projected bounds are the layer's current position rather
    /// than only its layout goal.
    pub fn settled(&self) -> bool {
        self.basis == Basis::SettledStockLayer
    }
    pub(crate) fn validate(
        &self,
        context: &Context,
        output: &OutputState,
        now: Duration,
    ) -> io::Result<()> {
        let at = Duration::from_nanos(self.sampled_ns);
        if at > now
            || now - at >= crate::context::SNAPSHOT_MAX_AGE
            || self.output != output.geometry
            || self.geometry_generation != output.geometry_generation
            || !context.fresh(now)
        {
            return Err(invalid("stale or retargeted native layout projection"));
        }
        let inventory = Inventory {
            layers: vec![self.surface.clone()],
            map_in: self.map_in_ns.map(Duration::from_nanos),
        };
        let mut expected = Self::from_samples(
            context,
            output,
            &inventory,
            &inventory,
            (self.producer_pid, self.producer_start),
            (Duration::from_nanos(self.started_ns), at),
        )?;
        // Other layers are not retained; exclusivity is the sampler's word.
        if let Some(expected) = &mut expected {
            expected.exclusive = self.exclusive;
        }
        if expected.as_ref() != Some(self) {
            return Err(invalid("native layout projection differs from its frame"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
