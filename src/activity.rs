//! Activity policy is independent from model choice, sampling, and run controls.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Goal,
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
