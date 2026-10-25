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

#[cfg(any(test, feature = "native-test-harness"))]
extern crate self as prek;

#[cfg(target_os = "linux")]
pub mod actions;
mod buttons;
#[cfg(target_os = "linux")]
pub mod compositor;
pub mod config;
#[cfg(target_os = "linux")]
pub mod configuration;
pub mod context;
#[cfg(target_os = "linux")]
pub mod context_transfer;
pub mod diagnostics;
#[cfg(target_os = "linux")]
pub mod display;
#[cfg(target_os = "linux")]
mod fsuid;
pub mod gestures;
#[cfg(target_os = "linux")]
pub mod input;
pub mod input_events;
#[cfg(target_os = "linux")]
pub mod ipc;
#[cfg(target_os = "linux")]
pub mod kernel;
#[cfg(all(target_os = "linux", any(test, feature = "native-test-harness")))]
#[path = "../tests/support/kernel.rs"]
pub(crate) mod kernel_fixture;
pub mod keys;
pub mod learner;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod lua;
#[cfg(target_os = "linux")]
pub mod lua_compiler;
#[cfg(target_os = "linux")]
pub mod paths;
#[cfg(target_os = "linux")]
mod process_clock;
pub mod protocol;
pub mod recognition;
pub mod replay;
#[cfg(target_os = "linux")]
pub mod route_worker;
pub mod router;
pub mod routing;
pub mod rule_transactions;
pub mod rules;
#[cfg(target_os = "linux")]
pub mod service;
#[cfg(target_os = "linux")]
pub mod session;
#[cfg(target_os = "linux")]
pub mod session_launcher;
#[cfg(target_os = "linux")]
pub mod sleep_watch;
#[cfg(target_os = "linux")]
pub mod subscription_transfer;
#[cfg(target_os = "linux")]
pub mod supervisor;
pub mod transport;
#[cfg(target_os = "linux")]
pub mod worker;

pub use buttons::{Button, Buttons, Gesture, Timing};
