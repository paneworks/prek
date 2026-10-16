//! Conversion against a matched stock compositor layout. This is deliberately
//! not rendered-position authority: stock IPC reports a layer's layout box.
use super::*;
use crate::{
    context::{OutputState, Rect},
    gestures::{Output, Point},
    hyprland::layers::Layer,
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    StockLayerLayout,
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
    #[serde(with = "super::decimal")]
    pub sampled_ns: u64,
    pub surface: Layer,
    pub regions: Vec<ProjectedRegion>,
}
impl LayoutProjection {
    pub(crate) fn from_samples(
        context: &Context,
        output: &OutputState,
        before: &[Layer],
        after: &[Layer],
        producer: (u32, u64),
        at: Duration,
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
        let Some(layer) = select(before) else {
            return Ok(None);
        };
        layer.validate(output)?;
        if select(after).as_ref() != Some(&layer)
            || layer.rect.size
                != (Point {
                    x: f64::from(space.width),
                    y: f64::from(space.height),
                })
        {
            return Ok(None);
        }
        if producer.0 == 0 || producer.1 == 0 || !output.rect().contains_rect(layer.rect) {
            return Err(invalid("invalid native layout projection binding"));
        }
        let regions = frame
            .regions
            .iter()
            .map(|region| {
                let b = region.bounds;
                let scale = space.local_scale;
                let x = (b.x * scale).max(0.0).min(f64::from(space.width));
                let y = (b.y * scale).max(0.0).min(f64::from(space.height));
                let right = ((b.x + b.width) * scale).max(x).min(f64::from(space.width));
                let bottom = ((b.y + b.height) * scale)
                    .max(y)
                    .min(f64::from(space.height));
                let bounds = (right > x && bottom > y).then_some(Rect {
                    origin: Point {
                        x: layer.rect.origin.x + x,
                        y: layer.rect.origin.y + y,
                    },
                    size: Point {
                        x: right - x,
                        y: bottom - y,
                    },
                });
                ProjectedRegion {
                    node: region.node,
                    id: region.id.clone(),
                    bounds,
                    interactive: region.interactive,
                    semantic: region.semantic.clone(),
                }
            })
            .collect();
        let value = Self {
            basis: Basis::StockLayerLayout,
            producer_pid: producer.0,
            producer_start: producer.1,
            output: output.geometry.clone(),
            geometry_generation: output.geometry_generation,
            sampled_ns: at
                .as_nanos()
                .try_into()
                .map_err(|_| invalid("projection time overflow"))?,
            surface: layer,
            regions,
        };
        Ok(Some(value))
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
        let expected = Self::from_samples(
            context,
            output,
            std::slice::from_ref(&self.surface),
            std::slice::from_ref(&self.surface),
            (self.producer_pid, self.producer_start),
            at,
        )?;
        if expected.as_ref() != Some(self) {
            return Err(invalid("native layout projection differs from its frame"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
