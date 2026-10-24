//! Activity policy is independent from model choice, sampling, and run controls.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Goal,
    #[serde(rename = "type2", alias = "chaos")]
    Chaos,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Goal => "Project goal",
            Self::Chaos => "No goals, only chaos",
        }
    }

    pub fn is_chaos(self) -> bool {
        self == Self::Chaos
    }
}

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn neutral_identifier_preserves_existing_sessions_and_human_label() {
        assert_eq!(serde_json::to_value(Mode::Chaos).unwrap(), "type2");
        for name in ["type2", "chaos"] {
            assert_eq!(
                serde_json::from_value::<Mode>(serde_json::json!(name)).unwrap(),
                Mode::Chaos
            );
        }
        assert_eq!(serde_json::to_value(Mode::Goal).unwrap(), "goal");
        assert_eq!(Mode::Chaos.label(), "No goals, only chaos");
    }
}
