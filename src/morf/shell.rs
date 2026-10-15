//! Native connection lifetime, not rendered-region or action authority.
use super::*;
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pending,
    Configured,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub role: String,
    pub epoch: String,
    pub revision: u64,
    pub phase: Phase,
    pub compositor_pid: u32,
    pub compositor_start: u64,
    pub compositor_uid: u32,
    pub output: String,
}
impl sealed::Sealed for State {}
impl NativeState for State {
    const SUFFIX: &'static str = "-shell-provider.sock";
    fn validate(&self) -> io::Result<()> {
        if self.version != 1
            || self.role != "shell"
            || self.revision
                != match self.phase {
                    Phase::Pending => 1,
                    Phase::Configured => 2,
                }
            || self.epoch.len() != 32
            || !self
                .epoch
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.compositor_pid == 0
            || self.compositor_start == 0
            || self.compositor_uid == 0
            || self.output.is_empty()
            || self.output.len() > 256
            || self.output.chars().any(char::is_control)
        {
            return Err(invalid("invalid native Morf shell binding"));
        }
        Ok(())
    }
    fn compositor(&self) -> (u32, u64, u32) {
        (
            self.compositor_pid,
            self.compositor_start,
            self.compositor_uid,
        )
    }
    fn epoch(&self) -> &str {
        &self.epoch
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn preserves(&self, old: &Self) -> bool {
        self.epoch == old.epoch
            && self.output == old.output
            && self.compositor() == old.compositor()
            && self.revision >= old.revision
            && (self.revision != old.revision || self == old)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn state() -> State {
        State {
            version: 1,
            role: "shell".into(),
            epoch: "a".repeat(32),
            revision: 1,
            phase: Phase::Pending,
            compositor_pid: 123,
            compositor_start: u64::MAX,
            compositor_uid: 1000,
            output: "HEADLESS-1".into(),
        }
    }
    #[test]
    fn native_shell_role_cannot_be_retargeted_or_regressed() {
        let old = state();
        old.validate().unwrap();
        let mut next = old.clone();
        next.phase = Phase::Configured;
        next.revision = 2;
        next.validate().unwrap();
        assert!(next.preserves(&old));
        assert!(!old.preserves(&next));
        for field in ["output", "epoch", "role"] {
            let mut value = serde_json::to_value(&next).unwrap();
            value[field] = serde_json::json!("different");
            let changed: State = serde_json::from_value(value).unwrap();
            assert!(changed.validate().is_err() || !changed.preserves(&old));
        }
        next.compositor_start -= 1;
        assert!(!next.preserves(&old));
        next = old.clone();
        next.revision = 2;
        assert!(next.validate().is_err());
        let mut value = serde_json::to_value(old).unwrap();
        value["geometry"] = serde_json::json!({});
        assert!(serde_json::from_value::<State>(value).is_err());
    }
}
