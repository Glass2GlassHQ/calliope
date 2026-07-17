//! Serializable results: one report per invocation, grouped by scenario,
//! written as JSON so regression tracking can diff runs across machines.

use serde::Serialize;

use crate::compare::Comparison;
use crate::runner::{RunResult, RunStatus};

#[derive(Debug, Serialize)]
pub struct Report {
    pub scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Serialize)]
pub struct ScenarioReport {
    pub scenario: String,
    pub reference: String,
    /// robustness scenario: judged on graceful degradation, not frame equality
    pub robustness: bool,
    pub runs: Vec<EngineReport>,
}

#[derive(Debug, Serialize)]
pub struct EngineReport {
    #[serde(flatten)]
    pub run: RunResult,
    /// None for the reference engine and for runs that produced no hashes
    pub comparison: Option<Comparison>,
}

impl ScenarioReport {
    pub fn passed(&self) -> bool {
        if self.robustness {
            return self
                .runs
                .iter()
                .all(|r| r.run.status.survived_corrupt_input());
        }
        self.runs.iter().all(|r| {
            matches!(r.run.status, RunStatus::Ok) && r.comparison.as_ref().is_none_or(|c| c.matched)
        })
    }
}

impl Report {
    pub fn passed(&self) -> bool {
        self.scenarios.iter().all(ScenarioReport::passed)
    }
}
