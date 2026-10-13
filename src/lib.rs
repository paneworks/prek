//! Prek's input recognition core. Recognition is independent of Linux and the desktop.
//!
//! Complete [`input::Frame`] values preserve the raw stream while normalization
//! supplies key changes and stable contact generations. [`replay::Replay`] joins
//! button/touch recognition for live observation and offline fixtures.
//! [`keys`] contains chords/layers and logical output ownership; [`paths`] bounds
//! armed trajectories and compares explicit templates. All recognizers use one
//! monotonic timeline. They produce observations; ownership, forwarding and
//! action dispatch belong to the routing/session layers still being implemented.
//! [`transport`] and [`router`] model virtual-device preflight and bounded raw
//! forwarding; kernel takeover and session dispatch remain separate gates.

mod buttons;
pub mod config;
pub mod context;
pub mod diagnostics;
pub mod gestures;
pub mod input;
#[cfg(target_os = "linux")]
pub mod kernel;
pub mod keys;
pub mod paths;
pub mod replay;
pub mod router;
pub mod rules;
pub mod transport;

pub use buttons::{Button, Buttons, Gesture, Timing};
