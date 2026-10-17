use crate::{config::Screen, config::hyprland_window_address};
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
    pub(crate) fn window(self, id: &str, destination: Option<i64>) -> io::Result<String> {
        let address = hyprland_window_address(id)
            .ok_or_else(|| io::Error::other("invalid frozen window lifetime"))?;
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
                let stable = id.split_once(':').unwrap().1;
                let effect = if let Some(destination) = destination {
                    format!(
                        "hl.dsp.window.move({{window=w,workspace=\"{destination}\",follow=true}})"
                    )
                } else {
                    "hl.dsp.window.close({window=w})".into()
                };
                Ok(format!(
                    "/dispatch (function() local w=hl.get_window(\"address:{address}\"); assert(w and w.stable_id==0x{stable},\"Prek window retired\"); return {effect} end)()"
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
