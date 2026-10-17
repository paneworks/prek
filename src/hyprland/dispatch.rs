use crate::{
    config::{Screen, hyprland_window_address},
    context::{OutputState, Window},
    gestures::Transform,
};
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Dialect {
    Hyprlang,
    Lua,
}

impl Dialect {
    pub(super) fn parse(provider: &serde_json::Value) -> io::Result<Self> {
        match provider.as_str() {
            Some("hyprlang") => Ok(Self::Hyprlang),
            Some("lua") => Ok(Self::Lua),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Hyprland config provider is unknown or malformed",
            )),
        }
    }
    pub(crate) fn window(
        self,
        window: &Window,
        output: &OutputState,
        destination: Option<i64>,
    ) -> io::Result<String> {
        let address = hyprland_window_address(&window.id)
            .ok_or_else(|| io::Error::other("invalid frozen window lifetime"))?;
        let geometry = &output.geometry;
        let native_scale = geometry.scale as f32;
        if !window.mapped
            || !window.rect.valid()
            || window.workspace <= 0
            || window.workspace > i64::from(i32::MAX)
            || output.active_workspace != Some(window.workspace)
            || window.output != geometry.name
            || geometry.name.is_empty()
            || geometry.name.len() > 256
            || output.geometry_generation == 0
            || output.screen != Screen::On
            || !output.rect().valid()
            || !geometry.scale.is_finite()
            || geometry.scale <= 0.
            || !native_scale.is_finite()
            || native_scale <= 0.
            || [geometry.pixel_width, geometry.pixel_height]
                .iter()
                .any(|value| !value.is_finite() || *value <= 0.)
        {
            return Err(io::Error::other("invalid frozen window/output geometry"));
        }
        if destination.is_some_and(|value| !(1..=i64::from(i32::MAX)).contains(&value)) {
            return Err(io::Error::other("invalid absolute workspace"));
        }
        match self {
            Self::Hyprlang => Ok(if let Some(destination) = destination {
                format!("/dispatch movetoworkspace {destination},address:{address}")
            } else {
                format!("/dispatch closewindow address:{address}")
            }),
            Self::Lua => {
                let stable = window.id.split_once(':').unwrap().1;
                let effect = if let Some(destination) = destination {
                    format!(
                        "hl.dsp.window.move({{window=w,workspace=\"{destination}\",follow=true}})"
                    )
                } else {
                    "hl.dsp.window.close({window=w})".into()
                };
                let workspace = window.workspace;
                let name: String = geometry
                    .name
                    .bytes()
                    .map(|byte| format!("\\{byte:03}"))
                    .collect();
                let (x, y) = (window.rect.origin.x, window.rect.origin.y);
                let (width, height) = (window.rect.size.x, window.rect.size.y);
                let (ox, oy) = (geometry.origin.x, geometry.origin.y);
                let (ow, oh) = (geometry.pixel_width, geometry.pixel_height);
                let scale = f64::from(native_scale);
                let transform = match geometry.transform {
                    Transform::Normal => 0,
                    Transform::Rotate90 => 1,
                    Transform::Rotate180 => 2,
                    Transform::Rotate270 => 3,
                    Transform::Flipped => 4,
                    Transform::Flipped90 => 5,
                    Transform::Flipped180 => 6,
                    Transform::Flipped270 => 7,
                };
                Ok(format!(
                    "/dispatch (function() local w=hl.get_window(\"address:{address}\"); \
                    assert(w and w.stable_id==0x{stable},\"Prek window retired\"); \
                    assert(w.mapped and w.workspace and w.workspace.id=={workspace},\"Prek workspace changed\"); \
                    local m=w.monitor; assert(m and m.name==\"{name}\" and m.dpms_status and \
                    m.active_workspace and m.active_workspace.id=={workspace} and not m.active_special_workspace,\"Prek output changed\"); \
                    assert(m.x=={ox:?} and m.y=={oy:?} and m.width=={ow:?} and m.height=={oh:?} and \
                    m.scale=={scale:?} and m.transform=={transform},\"Prek output geometry changed\"); \
                    local p,s=w.at,w.size; assert(p.x=={x:?} and p.y=={y:?} and s.x=={width:?} and s.y=={height:?},\"Prek window geometry changed\"); \
                    return {effect} end)()"
                ))
            }
        }
    }
    pub(crate) fn power(self, output: &str, screen: Screen) -> io::Result<String> {
        if !crate::config::valid_power_output(output) {
            return Err(io::Error::other("invalid frozen output name"));
        }
        let action = if screen == Screen::On { "on" } else { "off" };
        Ok(match self {
            Self::Hyprlang => format!("/dispatch dpms {action} {output}"),
            Self::Lua => format!(
                "/dispatch (function() local m=hl.get_monitor(\"{output}\"); assert(m and m.name==\"{output}\",\"Prek output retired\"); return hl.dsp.dpms({{action=\"{action}\",monitor=m}}) end)()"
            ),
        })
    }
}

#[cfg(test)]
mod tests;
