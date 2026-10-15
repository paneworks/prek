//! Typed wire values for Morf's ordinary shell controllers. These values are
//! not provider authentication or an execution grant. Production negotiation
//! remains unavailable until native binding and recognizer handoff exist.
use crate::config::MorfAction;
use serde::{Deserialize, Serialize};
use std::io;

pub const MAX_PAYLOAD: usize = 2048;
pub const MAX_ORDINAL: u32 = i32::MAX as u32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Discrete,
    Begin,
    Update,
    End,
    Cancel,
}

/// Surface logical displacement since begin and surface logical units/second.
/// Updates carry cumulative displacement, so coalescing does not lose motion.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub output: String,
    pub epoch: String,
    pub gesture: String,
    pub ordinal: u32,
    pub action: MorfAction,
    pub phase: Phase,
    pub dx: f64,
    pub dy: f64,
    pub vx: f64,
    pub vy: f64,
}
impl Request {
    pub(crate) fn from_event(
        fence: crate::protocol::Fence,
        event: &crate::rules::Event,
        action: MorfAction,
    ) -> io::Result<Self> {
        use crate::{
            gestures::{Space, TouchGesture},
            rules::{Event, Evidence, Stage},
        };
        let Event::Matched {
            id,
            ordinal,
            config_generation,
            session_generation,
            stage,
            output: Some(output),
            evidence,
            ..
        } = event
        else {
            return Err(io::Error::other(
                "native action requires a frozen matched output",
            ));
        };
        fence.validate()?;
        if *config_generation != fence.configuration || *session_generation != fence.session {
            return Err(io::Error::other(
                "native event belongs to a retired configuration/session",
            ));
        }
        let continuous = matches!(
            action,
            MorfAction::Dashboard
                | MorfAction::Notifications
                | MorfAction::QuickSettings
                | MorfAction::WorkspacePreview
        );
        let phase = match stage {
            Stage::Discrete => Phase::Discrete,
            Stage::Begin if !continuous => Phase::Discrete,
            Stage::Begin => Phase::Begin,
            Stage::Update => Phase::Update,
            Stage::End => Phase::End,
            Stage::Cancel => Phase::Cancel,
        };
        let (dx, dy, vx, vy) = match evidence {
            Evidence::Touch {
                space: Space::Logical,
                gesture:
                    TouchGesture::Pan {
                        total, velocity, ..
                    },
            } => (total.x, total.y, velocity.x, velocity.y),
            _ if phase == Phase::Discrete => (0., 0., 0., 0.),
            _ => {
                return Err(io::Error::other(
                    "native progress requires output-logical pan evidence",
                ));
            }
        };
        if *id == 0 {
            return Err(io::Error::other("native gesture identity is zero"));
        }
        let request = Self {
            version: 1,
            output: output.clone(),
            epoch: format!("{:016x}{:016x}", fence.broker.0[0], fence.broker.0[1]),
            gesture: format!("{id:032x}"),
            ordinal: (*ordinal).try_into().map_err(io::Error::other)?,
            action,
            phase,
            dx,
            dy,
            vx,
            vy,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> io::Result<()> {
        let hex = |value: &str| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        let discrete = !matches!(
            self.action,
            MorfAction::WorkspacePreview | MorfAction::RevealClockForm
        );
        let continuous = matches!(
            self.action,
            MorfAction::Dashboard
                | MorfAction::Notifications
                | MorfAction::QuickSettings
                | MorfAction::WorkspacePreview
        );
        if self.version != 1
            || self.output.is_empty()
            || self.output.len() > 256
            || self.output.chars().any(char::is_control)
            || !hex(&self.epoch)
            || !hex(&self.gesture)
            || self.ordinal == 0
            || self.ordinal > MAX_ORDINAL
            || matches!(self.phase, Phase::Discrete | Phase::Begin) && self.ordinal != 1
            || matches!(self.phase, Phase::Update | Phase::End | Phase::Cancel) && self.ordinal < 2
            || self.phase == Phase::Discrete && !discrete
            || self.phase != Phase::Discrete && !continuous
            || [self.dx, self.dy, self.vx, self.vy]
                .iter()
                .any(|value| !value.is_finite() || value.abs() > 1_000_000.0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Morf controller request",
            ));
        }
        Ok(())
    }
    /// The payload is the one string argument to the ordinary `prek-control`
    /// IPC verb. Calling that verb still belongs to the authorized executor.
    pub fn payload(&self) -> io::Result<String> {
        self.validate()?;
        let payload = serde_json::to_string(self).map_err(io::Error::other)?;
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Morf controller request exceeds limit",
            ));
        }
        Ok(payload)
    }
    pub fn parse(payload: &str) -> io::Result<Self> {
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Morf controller request exceeds limit",
            ));
        }
        let request: Self = serde_json::from_str(payload).map_err(io::Error::other)?;
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Accepted,
    Rejected,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    InvalidRequest,
    Blocked,
    Duplicate,
    Busy,
    Capacity,
    UnsupportedPhase,
    Unavailable,
    Retired,
    OutOfOrder,
    ControllerFailed,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub output: String,
    pub epoch: String,
    pub gesture: String,
    pub ordinal: u32,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Refusal>,
}
impl Reply {
    pub fn validate_for(&self, request: &Request) -> io::Result<()> {
        request.validate()?;
        if self.version != 1
            || self.output != request.output
            || self.epoch != request.epoch
            || self.gesture != request.gesture
            || self.ordinal != request.ordinal
            || (self.status == Status::Rejected) != self.error.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Morf controller reply does not match request",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            version: 1,
            output: "HEADLESS-1".into(),
            epoch: "a".repeat(32),
            gesture: format!("{:032x}", 1),
            ordinal: 1,
            action: MorfAction::Dashboard,
            phase: Phase::Begin,
            dx: 0.,
            dy: -80.,
            vx: 0.,
            vy: 0.,
        }
    }
    #[test]
    fn controller_protocol_rejects_unknown_fields_versions_and_nonfinite_motion() {
        let mut value = serde_json::to_value(request()).unwrap();
        assert_eq!(
            Request::parse(&request().payload().unwrap()).unwrap(),
            request()
        );
        for (key, invalid) in [
            ("version", serde_json::json!(2)),
            ("epoch", serde_json::json!("A".repeat(32))),
            ("gesture", serde_json::json!("bad")),
            ("output", serde_json::json!("bad\noutput")),
            ("ordinal", serde_json::json!(0)),
            ("ordinal", serde_json::json!(2147483648u32)),
            ("ordinal", serde_json::json!(1.5)),
            ("phase", serde_json::json!("unlock")),
            ("unknown", serde_json::json!(true)),
        ] {
            let old = value.as_object_mut().unwrap().insert(key.into(), invalid);
            assert!(Request::parse(&value.to_string()).is_err(), "{key}");
            match old {
                Some(old) => {
                    value[key] = old;
                }
                None => {
                    value.as_object_mut().unwrap().remove(key);
                }
            }
        }
        assert!(Request::parse(&" ".repeat(MAX_PAYLOAD + 1)).is_err());
        for invalid in [f64::NAN, f64::INFINITY, -1_000_001., 1_000_001.] {
            let mut r = request();
            r.dy = invalid;
            assert!(r.payload().is_err());
        }
    }
    #[test]
    fn controller_protocol_distinguishes_discrete_and_continuous_actions() {
        for action in [
            MorfAction::KeyboardShow,
            MorfAction::KeyboardHide,
            MorfAction::KeyboardToggleLayout,
            MorfAction::Apps,
            MorfAction::Web,
        ] {
            let mut r = request();
            r.action = action;
            assert!(r.validate().is_err());
            r.phase = Phase::Discrete;
            r.validate().unwrap();
        }
        let mut r = request();
        r.action = MorfAction::WorkspacePreview;
        r.validate().unwrap();
        r.phase = Phase::Discrete;
        assert!(r.validate().is_err());
        r.action = MorfAction::RevealClockForm;
        assert!(r.validate().is_err());
        r = request();
        r.phase = Phase::Update;
        assert!(r.validate().is_err());
        r.ordinal = 2;
        r.validate().unwrap();
        r.phase = Phase::Begin;
        assert!(r.validate().is_err());
    }
    #[test]
    fn controller_reply_binds_the_frozen_output_and_refusal_status() {
        let r = request();
        let mut reply = Reply {
            version: 1,
            output: r.output.clone(),
            epoch: r.epoch.clone(),
            gesture: r.gesture.clone(),
            ordinal: r.ordinal,
            status: Status::Accepted,
            error: None,
        };
        reply.validate_for(&r).unwrap();
        let mut wrong = reply.clone();
        wrong.epoch = "b".repeat(32);
        assert!(wrong.validate_for(&r).is_err());
        wrong = reply.clone();
        wrong.gesture = "b".repeat(32);
        assert!(wrong.validate_for(&r).is_err());
        wrong = reply.clone();
        wrong.ordinal = 2;
        assert!(wrong.validate_for(&r).is_err());
        reply.error = Some(Refusal::Blocked);
        assert!(reply.validate_for(&r).is_err());
        reply.status = Status::Rejected;
        reply.validate_for(&r).unwrap();
        reply.output = "another-output".into();
        assert!(reply.validate_for(&r).is_err());
    }
}
