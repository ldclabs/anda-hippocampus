//! Longitudinal memory evaluation harness.
//!
//! The harness intentionally drives Anda Brain through the same deep interface
//! used by callers: formation, recall, maintenance, and read-only KIP probes.
//! This keeps evals implementation-agnostic while still producing attribution
//! that points back to Formation, Recall, or Maintenance behavior.

pub mod judge;
pub mod optimize;

use anda_core::{AgentOutput, BoxError, CompletionRequest, ContentPart, Json, Message, Usage};
use anda_engine::rfc3339_datetime_now;
use anda_kip::{Request, Response};
use judge::JudgeVerdict;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::time::{Instant, sleep};

use crate::{
    agents::SELF_USER_ID,
    payload::StringOr,
    space::Space,
    types::{FormationInput, InputContext, MaintenanceInput, MaintenanceScope, RecallInput},
};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 180_000;
const DEFAULT_POLL_INTERVAL_MS: u64 = 250;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvalScenario {
    pub id: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Visible only to scenario authors and simulated users. The Brain never
    /// receives this directly; checkpoints and rubrics decide what matters.
    #[serde(default)]
    pub hidden_profile: Json,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context: Option<InputContext>,

    /// Deterministic noise pressure: inserts irrelevant chit-chat turns between
    /// adjacent timeline entries so Formation must keep the needle in a
    /// growing haystack and Maintenance has something real to metabolize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise: Option<NoiseConfig>,

    #[serde(default)]
    pub timeline: Vec<EvalTurn>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseConfig {
    /// Number of noise turns inserted between each pair of adjacent timeline
    /// turns. Anchors keep their authoritative rubrics; noise only adds scale.
    pub between_turns: usize,

    /// Optional custom corpus. Defaults to a built-in chit-chat corpus.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub corpus: Vec<String>,

    /// Seed for the deterministic corpus picker; same seed => same timeline.
    #[serde(default = "default_noise_seed")]
    pub seed: u64,
}

fn default_noise_seed() -> u64 {
    42
}

// `deny_unknown_fields` on the fixture-facing types turns a typo'd field
// (e.g. `forbidden_terms` for `forbidden_answer_terms`) into a load error
// instead of a silently weakened rubric that still passes validation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvalTurn {
    pub turn: u64,

    #[serde(rename = "type")]
    pub turn_type: EvalTurnType,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<InputContext>,

    /// Convenience field for one-message turns in hand-written scenarios.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Message>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,

    /// For `simulated` turns: what the simulated user wants to accomplish this
    /// turn. The simulator writes the actual message from `hidden_profile`,
    /// this intent, and the recent transcript/satisfaction trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<EvalRubric>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<MaintenanceInput>,

    /// Set on synthesized noise turns; never set by scenario authors.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub noise: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalTurnType {
    Normal,
    /// A user turn whose message text is written by the eval user simulator
    /// from `hidden_profile` + `intent`, then encoded through Formation.
    Simulated,
    CheckpointOrganic,
    CheckpointSynthetic,
    Maintenance,
}

impl EvalTurnType {
    fn is_checkpoint(self) -> bool {
        matches!(
            self,
            EvalTurnType::CheckpointOrganic | EvalTurnType::CheckpointSynthetic
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EvalRubric {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring_rubric: Option<String>,

    /// Terms that should appear in the final answer. These are deliberately
    /// simple and deterministic; LLM-as-judge can be layered on top later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_answer_terms: Vec<String>,

    /// Terms whose presence usually means the answer is stale or overconfident.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_answer_terms: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_memories: Vec<ExpectedMemory>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedMemory {
    pub id: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default)]
    pub mode: MemoryExpectationMode,

    #[serde(default = "default_expectation_weight")]
    pub weight: f64,

    /// Read-only KIP probe used to inspect whether the graph has the expected
    /// memory state before Recall answers the checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<Request>,

    /// Semantic probe: a natural-language statement of the memory being
    /// probed, e.g. "an active, non-superseded BBQ preference for user_042".
    /// The harness runs a semantic graph search and asks the judge whether the
    /// evidence shows the statement; `mode` then decides satisfaction. This is
    /// robust to valid encoding variation, unlike hand-written KQL.
    /// Requires the `llm` judge; otherwise the raw `probe` (if any) is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion: Option<String>,

    /// Optional search text for the semantic probe. Defaults to `assertion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,

    /// Terms expected in the final answer when this memory is relevant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answer_terms: Vec<String>,

    /// Terms expected in recall tool traces if grounding succeeded. Defaults to
    /// answer_terms when omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trace_terms: Vec<String>,
}

fn default_expectation_weight() -> f64 {
    1.0
}

impl Default for ExpectedMemory {
    fn default() -> Self {
        Self {
            id: String::new(),
            description: None,
            mode: MemoryExpectationMode::default(),
            weight: default_expectation_weight(),
            probe: None,
            assertion: None,
            search: None,
            answer_terms: Vec::new(),
            trace_terms: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryExpectationMode {
    /// The memory should be present and usable.
    #[default]
    ShouldExist,
    /// The memory should not be active anymore, usually because it was
    /// superseded, forgotten, or cleaned up.
    ShouldNotExist,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvalProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    #[serde(default = "default_wait_timeout_ms")]
    pub wait_timeout_ms: u64,

    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,

    /// Run maintenance after every N normal turns. `None` means only explicit
    /// maintenance turns run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_every_n_turns: Option<usize>,

    #[serde(default)]
    pub maintenance_scope: MaintenanceScope,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_latency_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_total_tokens: Option<u64>,

    /// Number of independent Recall samples per checkpoint. Values above 1
    /// report the mean score plus a standard deviation, so gates can use a
    /// confidence lower bound instead of a single noisy roll.
    #[serde(default = "default_checkpoint_samples")]
    pub checkpoint_samples: usize,

    /// Judge used to score checkpoint answers. `lexical` is deterministic and
    /// cheap; `llm` reads the rubric, hidden profile, probes, and trace, and is
    /// robust to paraphrase and meta-references.
    #[serde(default)]
    pub judge: EvalJudgeKind,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvalJudgeKind {
    /// Deterministic term-overlap scoring; suitable for smoke tests.
    #[default]
    Lexical,
    /// LLM-as-judge over `scoring_rubric`, `hidden_profile`, probes, and trace.
    Llm,
}

fn default_checkpoint_samples() -> usize {
    1
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalValidationReport {
    pub passed: bool,
    pub planned_runs: usize,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<EvalScenarioPlan>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<EvalProfilePlan>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<EvalValidationIssue>,
}

impl EvalValidationReport {
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == EvalValidationSeverity::Error)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalScenarioPlan {
    pub id: String,
    pub normal_turns: usize,
    pub simulated_turns: usize,
    pub noise_turns: usize,
    pub checkpoint_turns: usize,
    pub maintenance_turns: usize,
    pub expected_memories: usize,
    pub probes: usize,
    pub assertions: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvalProfilePlan {
    pub id: String,
    pub wait_timeout_ms: u64,
    pub poll_interval_ms: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_every_n_turns: Option<usize>,

    pub maintenance_scope: MaintenanceScope,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_latency_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_total_tokens: Option<u64>,

    pub checkpoint_samples: usize,
    pub judge: EvalJudgeKind,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalValidationSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvalValidationIssue {
    pub severity: EvalValidationSeverity,
    pub path: String,
    pub message: String,
}

fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

fn default_poll_interval_ms() -> u64 {
    DEFAULT_POLL_INTERVAL_MS
}

impl Default for EvalProfile {
    fn default() -> Self {
        Self {
            id: None,
            wait_timeout_ms: DEFAULT_WAIT_TIMEOUT_MS,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            maintenance_every_n_turns: None,
            maintenance_scope: MaintenanceScope::Daydream,
            max_checkpoint_latency_ms: None,
            max_checkpoint_total_tokens: None,
            checkpoint_samples: default_checkpoint_samples(),
            judge: EvalJudgeKind::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalAgentResult {
    pub content: String,
    pub usage: Usage,
    pub conversation: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl From<AgentOutput> for EvalAgentResult {
    fn from(output: AgentOutput) -> Self {
        Self {
            content: output.content,
            usage: output.usage,
            conversation: output.conversation,
            failed_reason: output.failed_reason,
            model: output.model,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RecallTrace {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolTrace>,
}

impl RecallTrace {
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut tools: Vec<ToolTrace> = Vec::new();

        for message in messages {
            for part in &message.content {
                match part {
                    ContentPart::ToolCall {
                        name,
                        args,
                        call_id,
                    } => tools.push(ToolTrace {
                        name: name.clone(),
                        args: args.clone(),
                        call_id: call_id.clone(),
                        output: None,
                        is_error: None,
                    }),
                    ContentPart::ToolOutput {
                        name,
                        output,
                        is_error,
                        call_id,
                        ..
                    } => {
                        if let Some(existing) = tools.iter_mut().rev().find(|trace| {
                            trace.output.is_none()
                                && trace.name == *name
                                && (call_id.is_none() || trace.call_id == *call_id)
                        }) {
                            existing.output = Some(output.clone());
                            existing.is_error = *is_error;
                        } else {
                            tools.push(ToolTrace {
                                name: name.clone(),
                                args: Json::Null,
                                call_id: call_id.clone(),
                                output: Some(output.clone()),
                                is_error: *is_error,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        Self { tools }
    }

    pub fn contains_any_term(&self, terms: &[String]) -> bool {
        if terms.is_empty() {
            return false;
        }

        let haystack = serde_json::to_string(self)
            .unwrap_or_default()
            .to_lowercase();
        terms
            .iter()
            .any(|term| !term.trim().is_empty() && haystack.contains(&term.to_lowercase()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolTrace {
    pub name: String,
    pub args: Json,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Json>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalReport {
    pub scenario_id: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    pub score: EvalScore,

    /// Standard deviation of the total score, propagated from checkpoint
    /// samples when `checkpoint_samples > 1`. Gates can subtract
    /// `confidence_z * total_stddev` to test a lower confidence bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_stddev: Option<f64>,

    pub attribution: AttributionSummary,
    pub usage: Usage,

    /// Simulated-user satisfaction after each checkpoint, in timeline order.
    /// This is the survival-pressure signal: it drops when answers violate the
    /// hidden profile and recovers when memory serves the user well.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub satisfaction_trajectory: Vec<SatisfactionPoint>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<EvalGateReport>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<EvalTurnReport>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SatisfactionPoint {
    pub turn: u64,
    pub satisfaction: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalSuiteReport {
    pub suite_id: String,
    pub score: EvalScore,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_stddev: Option<f64>,

    pub attribution: AttributionSummary,
    pub usage: Usage,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<EvalGateReport>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<EvalReport>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalExperimentReport {
    pub experiment_id: String,
    pub score: EvalScore,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_stddev: Option<f64>,

    pub attribution: AttributionSummary,
    pub usage: Usage,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_suite_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<EvalGateReport>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comparisons: Vec<EvalSuiteComparison>,

    /// Present in shared-formation experiments: one formation-phase report per
    /// scenario, replayed once and forked to every profile. Its usage is not
    /// attributed to any single profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_formation: Vec<EvalReport>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suites: Vec<EvalSuiteReport>,
}

impl EvalExperimentReport {
    pub fn from_suites(experiment_id: String, suites: Vec<EvalSuiteReport>) -> Self {
        let mut usage = Usage::default();
        let mut attribution = AttributionSummary::default();
        for suite in &suites {
            usage.accumulate(&suite.usage);
            attribution.accumulate(&suite.attribution);
        }

        let score = aggregate_suite_scores(&suites);
        let total_stddev = propagate_stddev(suites.iter().map(|suite| suite.total_stddev));
        let comparisons = compare_suites(&suites);
        let best_suite_id = comparisons
            .first()
            .map(|comparison| comparison.suite_id.clone());
        Self {
            experiment_id,
            score,
            total_stddev,
            attribution,
            usage,
            best_suite_id,
            gate: None,
            comparisons,
            shared_formation: Vec::new(),
            suites,
        }
    }
}

impl EvalSuiteReport {
    pub fn from_reports(suite_id: String, reports: Vec<EvalReport>) -> Self {
        let mut usage = Usage::default();
        let mut attribution = AttributionSummary::default();
        for report in &reports {
            usage.accumulate(&report.usage);
            attribution.accumulate(&report.attribution);
        }

        let score = aggregate_report_scores(&reports);
        let total_stddev = propagate_stddev(reports.iter().map(|report| report.total_stddev));
        Self {
            suite_id,
            score,
            total_stddev,
            attribution,
            usage,
            gate: None,
            reports,
        }
    }
}

/// Standard deviation of a mean of independent components: when the aggregate
/// total is the mean of `n` child totals, its variance is `sum(var_i) / n^2`.
/// Children without a stddev contribute zero variance; returns `None` when no
/// child reports one.
fn propagate_stddev(stddevs: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum_var = 0.0;
    let mut n = 0usize;
    let mut any = false;
    for stddev in stddevs {
        n += 1;
        if let Some(stddev) = stddev {
            any = true;
            sum_var += stddev * stddev;
        }
    }
    if !any || n == 0 {
        return None;
    }
    Some(sum_var.sqrt() / n as f64)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalTurnReport {
    pub turn: u64,
    pub turn_type: EvalTurnTypeReport,
    pub latency_ms: u64,
    pub usage: Usage,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,

    /// Mean score across `checkpoint_samples` recall samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<EvalScore>,

    /// Standard deviation of the sample totals when `checkpoint_samples > 1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_stddev: Option<f64>,

    /// Per-sample answers and scores when `checkpoint_samples > 1`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<EvalCheckpointSample>,

    /// Judge satisfaction estimate for this checkpoint (0..1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satisfaction: Option<f64>,

    /// Judge reasoning for the representative sample, when the LLM judge ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_reasoning: Option<String>,

    /// Real graph metabolism counters captured before this checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_stats: Option<GraphStats>,

    /// Set on synthesized noise turns.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub noise: bool,

    /// For simulated turns: the message the user simulator produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated_message: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<MemoryProbeReport>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_trace: Option<RecallTrace>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<EvalFinding>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalCheckpointSample {
    pub answer: String,
    pub latency_ms: u64,
    pub usage: Usage,
    pub score: EvalScore,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<EvalFinding>,
}

/// Objective knowledge-graph health counters, read through the same deep
/// interface (`formation_status` + read-only KIP) that callers use.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GraphStats {
    pub concepts: u64,
    pub propositions: u64,

    /// Concepts still in the `Unsorted` domain (maintenance backlog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsorted: Option<u64>,

    /// Concepts without any `belongs_to_domain` proposition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphans: Option<u64>,
}

impl GraphStats {
    /// Health in 0..1: the fraction of concepts that are properly organized.
    /// Falls back to `None` when backlog counters are unavailable.
    pub fn health(&self) -> Option<f64> {
        let unsorted = self.unsorted?;
        let orphans = self.orphans?;
        let backlog = unsorted.saturating_add(orphans) as f64;
        let base = (self.concepts.max(1)) as f64;
        Some((1.0 - (backlog / base)).clamp(0.0, 1.0))
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvalTurnTypeReport {
    #[default]
    Normal,
    Simulated,
    Checkpoint,
    Maintenance,
    AutoMaintenance,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemoryProbeReport {
    pub expectation_id: String,
    pub mode: MemoryExpectationMode,
    pub hit_count: usize,
    pub satisfied: bool,

    /// Semantic-probe statement, when the expectation used one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion: Option<String>,

    /// Judge reasoning for the semantic-probe verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Response>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalScore {
    pub total: f64,
    pub memory_utility: f64,
    pub evolution_quality: f64,
    pub uncertainty_calibration: f64,
    pub forgetting_quality: f64,
    pub graph_health: f64,
    pub latency_penalty: f64,
    pub token_cost_penalty: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalSuiteComparison {
    pub suite_id: String,
    pub rank: usize,
    pub score: EvalScore,
    pub delta_from_best_total: f64,
    pub total_findings: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AttributionSummary {
    pub formation_miss: u64,
    pub bad_consolidation: u64,
    pub bad_grounding: u64,
    pub bad_synthesis: u64,
    pub overconfidence: u64,
    pub graph_probe_error: u64,
    pub latency_cost: u64,
    pub token_cost: u64,

    #[serde(default)]
    pub judge_error: u64,
}

impl AttributionSummary {
    pub fn accumulate(&mut self, other: &Self) {
        self.formation_miss = self.formation_miss.saturating_add(other.formation_miss);
        self.bad_consolidation = self
            .bad_consolidation
            .saturating_add(other.bad_consolidation);
        self.bad_grounding = self.bad_grounding.saturating_add(other.bad_grounding);
        self.bad_synthesis = self.bad_synthesis.saturating_add(other.bad_synthesis);
        self.overconfidence = self.overconfidence.saturating_add(other.overconfidence);
        self.graph_probe_error = self
            .graph_probe_error
            .saturating_add(other.graph_probe_error);
        self.latency_cost = self.latency_cost.saturating_add(other.latency_cost);
        self.token_cost = self.token_cost.saturating_add(other.token_cost);
        self.judge_error = self.judge_error.saturating_add(other.judge_error);
    }

    pub fn total_findings(&self) -> u64 {
        self.formation_miss
            .saturating_add(self.bad_consolidation)
            .saturating_add(self.bad_grounding)
            .saturating_add(self.bad_synthesis)
            .saturating_add(self.overconfidence)
            .saturating_add(self.graph_probe_error)
            .saturating_add(self.latency_cost)
            .saturating_add(self.token_cost)
            .saturating_add(self.judge_error)
    }

    fn add_finding(&mut self, kind: EvalFindingKind) {
        match kind {
            EvalFindingKind::FormationMiss => self.formation_miss += 1,
            EvalFindingKind::BadConsolidation => self.bad_consolidation += 1,
            EvalFindingKind::BadGrounding => self.bad_grounding += 1,
            EvalFindingKind::BadSynthesis => self.bad_synthesis += 1,
            EvalFindingKind::Overconfidence => self.overconfidence += 1,
            EvalFindingKind::GraphProbeError => self.graph_probe_error += 1,
            EvalFindingKind::LatencyCost => self.latency_cost += 1,
            EvalFindingKind::TokenCost => self.token_cost += 1,
            EvalFindingKind::JudgeError => self.judge_error += 1,
        }
    }

    fn add_turn(&mut self, turn: &EvalTurnReport) {
        for finding in &turn.findings {
            self.add_finding(finding.kind);
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalGate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_total_score: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_findings: Option<u64>,

    /// When set and a total stddev is available (`checkpoint_samples > 1`),
    /// the gate tests `total - confidence_z * stddev` against
    /// `min_total_score`, so a lucky single roll cannot pass the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_z: Option<f64>,
}

impl EvalGate {
    pub fn is_configured(&self) -> bool {
        self.min_total_score.is_some() || self.max_total_findings.is_some()
    }

    pub fn evaluate(
        &self,
        score: &EvalScore,
        attribution: &AttributionSummary,
        total_stddev: Option<f64>,
    ) -> EvalGateReport {
        let mut failures = Vec::new();
        let gated_total = match (self.confidence_z, total_stddev) {
            (Some(z), Some(stddev)) => score.total - z * stddev,
            _ => score.total,
        };
        if let Some(min_total_score) = self.min_total_score
            && gated_total < min_total_score
        {
            failures.push(format!(
                "total score {gated_total:.4} (mean {:.4}) is below required minimum {min_total_score:.4}",
                score.total
            ));
        }

        if let Some(max_total_findings) = self.max_total_findings {
            let total_findings = attribution.total_findings();
            if total_findings > max_total_findings {
                failures.push(format!(
                    "total findings {total_findings} exceeds maximum {max_total_findings}"
                ));
            }
        }

        EvalGateReport {
            criteria: self.clone(),
            passed: failures.is_empty(),
            failures,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EvalGateReport {
    pub criteria: EvalGate,
    pub passed: bool,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
}

pub fn validate_eval_plan(
    scenarios: &[EvalScenario],
    profiles: &[EvalProfile],
) -> EvalValidationReport {
    let mut report = EvalValidationReport {
        planned_runs: scenarios.len().saturating_mul(profiles.len()),
        ..Default::default()
    };
    let mut scenario_ids = BTreeSet::new();
    let mut profile_ids = BTreeSet::new();

    if scenarios.is_empty() {
        push_validation_issue(
            &mut report,
            EvalValidationSeverity::Error,
            "scenarios",
            "at least one scenario is required",
        );
    }

    if profiles.is_empty() {
        push_validation_issue(
            &mut report,
            EvalValidationSeverity::Error,
            "profiles",
            "at least one profile is required",
        );
    }

    for (index, scenario) in scenarios.iter().enumerate() {
        let path = format!("scenarios[{index}]");
        let id = scenario.id.trim();
        if id.is_empty() {
            push_validation_issue(
                &mut report,
                EvalValidationSeverity::Error,
                &path,
                "scenario id must not be empty",
            );
        } else if !scenario_ids.insert(id.to_string()) {
            push_validation_issue(
                &mut report,
                EvalValidationSeverity::Error,
                &path,
                format!("duplicate scenario id `{id}`"),
            );
        }

        report
            .scenarios
            .push(validate_scenario_plan(scenario, index, &mut report.issues));
    }

    for (index, profile) in profiles.iter().enumerate() {
        let path = format!("profiles[{index}]");
        let id = profile.id.as_deref().unwrap_or("default").trim();
        if id.is_empty() {
            push_validation_issue(
                &mut report,
                EvalValidationSeverity::Error,
                &path,
                "profile id must not be empty",
            );
        } else if !profile_ids.insert(id.to_string()) {
            push_validation_issue(
                &mut report,
                EvalValidationSeverity::Error,
                &path,
                format!("duplicate profile id `{id}`"),
            );
        }

        report
            .profiles
            .push(validate_profile_plan(profile, index, &mut report.issues));
    }

    // Semantic assertions only add power with the LLM judge; when every
    // profile is lexical they silently degrade to raw probes/hit counts.
    let any_assertions = report.scenarios.iter().any(|plan| plan.assertions > 0);
    let any_llm_judge = profiles
        .iter()
        .any(|profile| profile.judge == EvalJudgeKind::Llm);
    if any_assertions && !profiles.is_empty() && !any_llm_judge {
        push_validation_issue(
            &mut report,
            EvalValidationSeverity::Warning,
            "profiles",
            "scenarios use semantic `assertion` probes but no profile enables the `llm` judge; assertions will degrade to raw probes or hit counts",
        );
    }

    report.passed = !report.has_errors();
    report
}

/// Shared-formation experiments replay formation once and fork it per
/// profile, so all user turns must precede the first checkpoint; otherwise a
/// checkpoint would observe memory from its own future.
pub fn shared_formation_issues(scenarios: &[EvalScenario]) -> Vec<EvalValidationIssue> {
    let mut issues = Vec::new();
    for (scenario_index, scenario) in scenarios.iter().enumerate() {
        let mut checkpoint_seen = false;
        for (turn_index, turn) in scenario.timeline.iter().enumerate() {
            if turn.turn_type.is_checkpoint() {
                checkpoint_seen = true;
            } else if checkpoint_seen
                && matches!(
                    turn.turn_type,
                    EvalTurnType::Normal | EvalTurnType::Simulated
                )
            {
                issues.push(EvalValidationIssue {
                    severity: EvalValidationSeverity::Error,
                    path: format!("scenarios[{scenario_index}].timeline[{turn_index}]"),
                    message: format!(
                        "shared-formation mode requires all user turns before the first checkpoint; turn {} comes after one (use the interleaved mode for this scenario)",
                        turn.turn
                    ),
                });
            }
        }

        if scenario
            .noise
            .as_ref()
            .is_some_and(|noise| noise.between_turns > 0)
            && scenario
                .timeline
                .iter()
                .position(|turn| turn.turn_type.is_checkpoint())
                .is_some_and(|checkpoint_index| checkpoint_index + 1 < scenario.timeline.len())
        {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Error,
                path: format!("scenarios[{scenario_index}].noise"),
                message:
                    "shared-formation mode cannot synthesize noise user turns after the first checkpoint; disable noise or use the interleaved mode for this scenario"
                        .to_string(),
            });
        }
    }
    issues
}

fn validate_scenario_plan(
    scenario: &EvalScenario,
    scenario_index: usize,
    issues: &mut Vec<EvalValidationIssue>,
) -> EvalScenarioPlan {
    let mut plan = EvalScenarioPlan {
        id: scenario.id.clone(),
        ..Default::default()
    };
    let mut seen_turns = BTreeSet::new();
    let mut previous_turn = None;

    if scenario.timeline.is_empty() {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Warning,
            path: format!("scenarios[{scenario_index}].timeline"),
            message: "scenario has no turns".to_string(),
        });
    }

    if let Some(noise) = &scenario.noise {
        if noise.between_turns == 0 {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Warning,
                path: format!("scenarios[{scenario_index}].noise.between_turns"),
                message: "`between_turns` is zero, so the noise config is a no-op".to_string(),
            });
        } else if !scenario.timeline.is_empty() {
            plan.noise_turns = noise
                .between_turns
                .saturating_mul(scenario.timeline.len() - 1);
        }
        if noise.corpus.iter().any(|entry| entry.trim().is_empty()) {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Error,
                path: format!("scenarios[{scenario_index}].noise.corpus"),
                message: "noise corpus entries must not be empty".to_string(),
            });
        }
    }

    for (turn_index, turn) in scenario.timeline.iter().enumerate() {
        let path = format!("scenarios[{scenario_index}].timeline[{turn_index}]");
        if !seen_turns.insert(turn.turn) {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Error,
                path: path.clone(),
                message: format!("duplicate turn number {}", turn.turn),
            });
        }
        if let Some(previous) = previous_turn
            && turn.turn < previous
        {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Warning,
                path: path.clone(),
                message: format!(
                    "turn number {} is lower than previous turn {previous}",
                    turn.turn
                ),
            });
        }
        previous_turn = Some(turn.turn);

        match turn.turn_type {
            EvalTurnType::Normal => {
                plan.normal_turns += 1;
                if !turn_has_input_messages(turn) {
                    issues.push(EvalValidationIssue {
                        severity: EvalValidationSeverity::Error,
                        path,
                        message: "normal turn must include `user` text or `messages`".to_string(),
                    });
                }
            }
            EvalTurnType::Simulated => {
                plan.simulated_turns += 1;
                if turn
                    .intent
                    .as_ref()
                    .map(|intent| intent.trim().is_empty())
                    .unwrap_or(true)
                {
                    issues.push(EvalValidationIssue {
                        severity: EvalValidationSeverity::Error,
                        path,
                        message: "simulated turn must include a non-empty `intent`".to_string(),
                    });
                }
            }
            EvalTurnType::Maintenance => {
                plan.maintenance_turns += 1;
            }
            kind if kind.is_checkpoint() => {
                plan.checkpoint_turns += 1;
                validate_checkpoint_turn(turn, &path, issues, &mut plan);
            }
            _ => {}
        }
    }

    if plan.checkpoint_turns == 0 {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Warning,
            path: format!("scenarios[{scenario_index}].timeline"),
            message: "scenario has no checkpoint turns, so aggregate score will be zero"
                .to_string(),
        });
    }

    plan
}

fn validate_checkpoint_turn(
    turn: &EvalTurn,
    path: &str,
    issues: &mut Vec<EvalValidationIssue>,
    plan: &mut EvalScenarioPlan,
) {
    if turn
        .query
        .as_ref()
        .map(|query| query.trim().is_empty())
        .unwrap_or(true)
    {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Error,
            path: path.to_string(),
            message: "checkpoint turn must include a non-empty `query`".to_string(),
        });
    }

    let Some(rubric) = &turn.evaluation else {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Warning,
            path: format!("{path}.evaluation"),
            message: "checkpoint has no evaluation rubric".to_string(),
        });
        return;
    };

    if rubric.required_answer_terms.is_empty()
        && rubric.forbidden_answer_terms.is_empty()
        && rubric.expected_memories.is_empty()
    {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Warning,
            path: format!("{path}.evaluation"),
            message: "checkpoint rubric has no answer terms or memory expectations".to_string(),
        });
    }

    validate_term_overlap(path, rubric, issues);

    let mut expectation_ids = BTreeSet::new();
    for (expectation_index, expectation) in rubric.expected_memories.iter().enumerate() {
        let expectation_path = format!("{path}.evaluation.expected_memories[{expectation_index}]");
        plan.expected_memories += 1;
        if expectation.id.trim().is_empty() {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Error,
                path: expectation_path.clone(),
                message: "expected memory id must not be empty".to_string(),
            });
        } else if !expectation_ids.insert(expectation.id.trim().to_string()) {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Error,
                path: expectation_path.clone(),
                message: format!("duplicate expected memory id `{}`", expectation.id),
            });
        }

        if !expectation.weight.is_finite() || expectation.weight <= 0.0 {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Error,
                path: format!("{expectation_path}.weight"),
                message: "expected memory weight must be a positive finite number".to_string(),
            });
        }

        if let Some(assertion) = &expectation.assertion {
            plan.assertions += 1;
            if assertion.trim().is_empty() {
                issues.push(EvalValidationIssue {
                    severity: EvalValidationSeverity::Error,
                    path: format!("{expectation_path}.assertion"),
                    message: "semantic assertion must not be empty".to_string(),
                });
            }
        }

        match &expectation.probe {
            Some(probe) => {
                plan.probes += 1;
                if !probe.readonly {
                    issues.push(EvalValidationIssue {
                        severity: EvalValidationSeverity::Error,
                        path: format!("{expectation_path}.probe.readonly"),
                        message: "memory probe must set `readonly` to true".to_string(),
                    });
                }
                if probe.command.trim().is_empty() && probe.commands.is_empty() {
                    issues.push(EvalValidationIssue {
                        severity: EvalValidationSeverity::Warning,
                        path: format!("{expectation_path}.probe"),
                        message: "memory probe has neither `command` nor `commands`".to_string(),
                    });
                }
            }
            None if expectation.mode == MemoryExpectationMode::ShouldNotExist
                && expectation.assertion.is_none() =>
            {
                issues.push(EvalValidationIssue {
                    severity: EvalValidationSeverity::Warning,
                    path: expectation_path.clone(),
                    message:
                        "`should_not_exist` expectations are strongest with a probe or assertion"
                            .to_string(),
                });
            }
            None => {}
        }

        if expectation.mode == MemoryExpectationMode::ShouldExist
            && expectation.probe.is_none()
            && expectation.assertion.is_none()
            && expectation.answer_terms.is_empty()
            && expectation.trace_terms.is_empty()
        {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Warning,
                path: expectation_path,
                message: "`should_exist` expectation has no probe, assertion, answer terms, or trace terms"
                    .to_string(),
            });
        }
    }
}

fn validate_term_overlap(path: &str, rubric: &EvalRubric, issues: &mut Vec<EvalValidationIssue>) {
    let required: BTreeSet<String> = rubric
        .required_answer_terms
        .iter()
        .map(|term| term.trim().to_lowercase())
        .filter(|term| !term.is_empty())
        .collect();

    for forbidden in &rubric.forbidden_answer_terms {
        let forbidden = forbidden.trim().to_lowercase();
        if !forbidden.is_empty() && required.contains(&forbidden) {
            issues.push(EvalValidationIssue {
                severity: EvalValidationSeverity::Warning,
                path: format!("{path}.evaluation"),
                message: format!("answer term `{forbidden}` is both required and forbidden"),
            });
        }
    }
}

fn validate_profile_plan(
    profile: &EvalProfile,
    profile_index: usize,
    issues: &mut Vec<EvalValidationIssue>,
) -> EvalProfilePlan {
    let path = format!("profiles[{profile_index}]");
    let id = profile.id.clone().unwrap_or_else(|| "default".to_string());

    if profile.wait_timeout_ms == 0 {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Error,
            path: format!("{path}.wait_timeout_ms"),
            message: "`wait_timeout_ms` must be greater than zero".to_string(),
        });
    }

    if profile.poll_interval_ms == 0 {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Error,
            path: format!("{path}.poll_interval_ms"),
            message: "`poll_interval_ms` must be greater than zero".to_string(),
        });
    } else if profile.wait_timeout_ms > 0 && profile.poll_interval_ms > profile.wait_timeout_ms {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Warning,
            path: format!("{path}.poll_interval_ms"),
            message: "`poll_interval_ms` is greater than `wait_timeout_ms`".to_string(),
        });
    }

    if profile.maintenance_every_n_turns == Some(0) {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Error,
            path: format!("{path}.maintenance_every_n_turns"),
            message: "`maintenance_every_n_turns` must be greater than zero when set".to_string(),
        });
    }

    if profile.max_checkpoint_latency_ms == Some(0) {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Error,
            path: format!("{path}.max_checkpoint_latency_ms"),
            message: "`max_checkpoint_latency_ms` must be greater than zero when set".to_string(),
        });
    }

    if profile.max_checkpoint_total_tokens == Some(0) {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Error,
            path: format!("{path}.max_checkpoint_total_tokens"),
            message: "`max_checkpoint_total_tokens` must be greater than zero when set".to_string(),
        });
    }

    if profile.checkpoint_samples == 0 {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Error,
            path: format!("{path}.checkpoint_samples"),
            message: "`checkpoint_samples` must be at least 1".to_string(),
        });
    } else if profile.checkpoint_samples > 9 {
        issues.push(EvalValidationIssue {
            severity: EvalValidationSeverity::Warning,
            path: format!("{path}.checkpoint_samples"),
            message: "`checkpoint_samples` above 9 multiplies recall cost with little extra statistical power".to_string(),
        });
    }

    EvalProfilePlan {
        id,
        wait_timeout_ms: profile.wait_timeout_ms,
        poll_interval_ms: profile.poll_interval_ms,
        maintenance_every_n_turns: profile.maintenance_every_n_turns,
        maintenance_scope: profile.maintenance_scope,
        max_checkpoint_latency_ms: profile.max_checkpoint_latency_ms,
        max_checkpoint_total_tokens: profile.max_checkpoint_total_tokens,
        checkpoint_samples: profile.checkpoint_samples,
        judge: profile.judge,
    }
}

fn push_validation_issue(
    report: &mut EvalValidationReport,
    severity: EvalValidationSeverity,
    path: impl Into<String>,
    message: impl Into<String>,
) {
    report.issues.push(EvalValidationIssue {
        severity,
        path: path.into(),
        message: message.into(),
    });
}

fn turn_has_input_messages(turn: &EvalTurn) -> bool {
    turn.user
        .as_ref()
        .map(|user| !user.trim().is_empty())
        .unwrap_or(false)
        || !turn.messages.is_empty()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvalFinding {
    pub kind: EvalFindingKind,
    pub message: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expectation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalFindingKind {
    FormationMiss,
    BadConsolidation,
    BadGrounding,
    BadSynthesis,
    Overconfidence,
    GraphProbeError,
    LatencyCost,
    TokenCost,
    /// The LLM judge failed and the sample degraded to lexical scoring.
    JudgeError,
}

#[async_trait::async_trait]
pub trait EvalDriver: Send + Sync {
    async fn remember(&self, input: FormationInput) -> Result<EvalAgentResult, BoxError>;
    async fn recall(&self, input: RecallInput) -> Result<EvalAgentResult, BoxError>;
    async fn maintain(&self, input: MaintenanceInput) -> Result<EvalAgentResult, BoxError>;
    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError>;

    /// One-shot LLM completion used by the eval-only judge, user simulator,
    /// and prompt optimizer. Drivers without a model can leave the default.
    async fn complete(&self, _req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        Err("eval driver does not support LLM completions".into())
    }

    /// Objective graph metabolism counters; `None` when unsupported.
    async fn graph_stats(&self) -> Result<Option<GraphStats>, BoxError> {
        Ok(None)
    }

    async fn wait_for_formation(
        &self,
        _conversation: u64,
        _timeout: Duration,
        _poll_interval: Duration,
    ) -> Result<(), BoxError> {
        Ok(())
    }

    async fn wait_for_maintenance(
        &self,
        _conversation: u64,
        _timeout: Duration,
        _poll_interval: Duration,
    ) -> Result<(), BoxError> {
        Ok(())
    }

    async fn recall_trace(&self, _conversation: u64) -> Result<Option<RecallTrace>, BoxError> {
        Ok(None)
    }
}

#[async_trait::async_trait]
impl EvalDriver for Space {
    async fn remember(&self, input: FormationInput) -> Result<EvalAgentResult, BoxError> {
        self.ingest(SELF_USER_ID, StringOr::Value(input))
            .await
            .map(EvalAgentResult::from)
    }

    async fn recall(&self, input: RecallInput) -> Result<EvalAgentResult, BoxError> {
        self.query(SELF_USER_ID, StringOr::Value(input))
            .await
            .map(EvalAgentResult::from)
    }

    async fn maintain(&self, input: MaintenanceInput) -> Result<EvalAgentResult, BoxError> {
        self.maintenance(SELF_USER_ID, input)
            .await
            .map(EvalAgentResult::from)
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError> {
        self.execute_kip_readonly(request).await
    }

    async fn complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        self.eval_complete(req).await
    }

    async fn graph_stats(&self) -> Result<Option<GraphStats>, BoxError> {
        let status = self.formation_status();
        let mut stats = GraphStats {
            concepts: status.concepts as u64,
            propositions: status.propositions as u64,
            unsorted: None,
            orphans: None,
        };

        // The same assessment queries the Maintenance prompt prescribes.
        stats.unsorted = kip_count(
            self,
            "FIND(COUNT(?n)) WHERE { (?n, \"belongs_to_domain\", {type: \"Domain\", name: \"Unsorted\"}) }",
        )
        .await;
        stats.orphans = kip_count(
            self,
            "FIND(COUNT(?n)) WHERE { ?n {} NOT { (?n, \"belongs_to_domain\", ?d) } }",
        )
        .await;
        Ok(Some(stats))
    }

    async fn wait_for_formation(
        &self,
        conversation: u64,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), BoxError> {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.formation_status();
            if !status.formation_processing && status.formation_processed_id >= conversation {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "formation conversation {conversation} did not complete within {} ms",
                    timeout.as_millis()
                )
                .into());
            }
            sleep(poll_interval).await;
        }
    }

    async fn wait_for_maintenance(
        &self,
        conversation: u64,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), BoxError> {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.formation_status();
            if !status.maintenance_processing {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "maintenance conversation {conversation} did not complete within {} ms",
                    timeout.as_millis()
                )
                .into());
            }
            sleep(poll_interval).await;
        }
    }

    async fn recall_trace(&self, conversation: u64) -> Result<Option<RecallTrace>, BoxError> {
        let conversation = self
            .get_conversation(Some("recall".to_string()), conversation)
            .await?;
        let messages: Vec<Message> = conversation
            .messages
            .into_iter()
            .filter_map(|message| serde_json::from_value::<Message>(message).ok())
            .collect();
        Ok(Some(RecallTrace::from_messages(&messages)))
    }
}

#[async_trait::async_trait]
impl EvalDriver for Arc<Space> {
    async fn remember(&self, input: FormationInput) -> Result<EvalAgentResult, BoxError> {
        self.as_ref().remember(input).await
    }

    async fn recall(&self, input: RecallInput) -> Result<EvalAgentResult, BoxError> {
        self.as_ref().recall(input).await
    }

    async fn maintain(&self, input: MaintenanceInput) -> Result<EvalAgentResult, BoxError> {
        self.as_ref().maintain(input).await
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError> {
        self.as_ref().execute_kip_readonly(request).await
    }

    async fn complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        EvalDriver::complete(self.as_ref(), req).await
    }

    async fn graph_stats(&self) -> Result<Option<GraphStats>, BoxError> {
        EvalDriver::graph_stats(self.as_ref()).await
    }

    async fn wait_for_formation(
        &self,
        conversation: u64,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), BoxError> {
        self.as_ref()
            .wait_for_formation(conversation, timeout, poll_interval)
            .await
    }

    async fn wait_for_maintenance(
        &self,
        conversation: u64,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), BoxError> {
        self.as_ref()
            .wait_for_maintenance(conversation, timeout, poll_interval)
            .await
    }

    async fn recall_trace(&self, conversation: u64) -> Result<Option<RecallTrace>, BoxError> {
        self.as_ref().recall_trace(conversation).await
    }
}

/// Which part of a timeline a run executes. `Full` is the realistic
/// interleaved replay. `FormationOnly`/`PolicyOnly` implement the
/// shared-formation experiment: formation is replayed once, snapshotted, and
/// each maintenance profile is evaluated on a fork of the identical encoded
/// memory — removing formation variance as a confound between profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelinePhase {
    Full,
    FormationOnly,
    PolicyOnly,
}

pub async fn run_scenario<D>(
    driver: &D,
    scenario: &EvalScenario,
    profile: &EvalProfile,
) -> Result<EvalReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    run_timeline(driver, scenario, profile, TimelinePhase::Full).await
}

/// Replays only the user turns (normal + simulated + noise) through Formation.
/// Used as the shared phase of a shared-formation experiment.
pub async fn run_formation_phase<D>(
    driver: &D,
    scenario: &EvalScenario,
    profile: &EvalProfile,
) -> Result<EvalReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    run_timeline(driver, scenario, profile, TimelinePhase::FormationOnly).await
}

/// Runs maintenance (explicit + profile cadence) and checkpoints on a fork of
/// already-formed memory. User turns only advance the maintenance cadence.
pub async fn run_policy_phase<D>(
    driver: &D,
    scenario: &EvalScenario,
    profile: &EvalProfile,
) -> Result<EvalReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    run_timeline(driver, scenario, profile, TimelinePhase::PolicyOnly).await
}

async fn run_timeline<D>(
    driver: &D,
    scenario: &EvalScenario,
    profile: &EvalProfile,
    phase: TimelinePhase,
) -> Result<EvalReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    let mut report = EvalReport {
        scenario_id: scenario.id.clone(),
        description: scenario.description.clone(),
        ..Default::default()
    };
    let timeline = effective_timeline(scenario);
    let mut normal_turns_since_maintenance = 0usize;
    let timeout = Duration::from_millis(profile.wait_timeout_ms);
    let poll_interval = Duration::from_millis(profile.poll_interval_ms);
    // Rolling context for the user simulator: recent exchanges plus how
    // satisfied the simulated user has been, so it can adapt its behavior.
    let mut transcript: Vec<String> = Vec::new();

    for turn in &timeline {
        match turn.turn_type {
            EvalTurnType::Normal | EvalTurnType::Simulated => {
                if phase != TimelinePhase::PolicyOnly {
                    let turn_report = if turn.turn_type == EvalTurnType::Simulated {
                        run_simulated_turn(
                            driver,
                            scenario,
                            turn,
                            &transcript,
                            &report.satisfaction_trajectory,
                            timeout,
                            poll_interval,
                        )
                        .await?
                    } else {
                        run_normal_turn(driver, scenario, turn, timeout, poll_interval).await?
                    };
                    if !turn.noise {
                        push_transcript(&mut transcript, turn, &turn_report);
                    }
                    report.usage.accumulate(&turn_report.usage);
                    report.turns.push(turn_report);
                }

                if phase != TimelinePhase::FormationOnly {
                    normal_turns_since_maintenance += 1;
                    if let Some(every) = profile.maintenance_every_n_turns
                        && every > 0
                        && normal_turns_since_maintenance >= every
                    {
                        let turn_report =
                            run_auto_maintenance(driver, profile, turn, timeout, poll_interval)
                                .await?;
                        normal_turns_since_maintenance = 0;
                        report.usage.accumulate(&turn_report.usage);
                        report.turns.push(turn_report);
                    }
                }
            }
            EvalTurnType::Maintenance => {
                if phase == TimelinePhase::FormationOnly {
                    continue;
                }
                let turn_report =
                    run_maintenance_turn(driver, turn, timeout, poll_interval).await?;
                report.usage.accumulate(&turn_report.usage);
                report.turns.push(turn_report);
                normal_turns_since_maintenance = 0;
            }
            kind if kind.is_checkpoint() => {
                if phase == TimelinePhase::FormationOnly {
                    continue;
                }
                let turn_report = run_checkpoint_turn(driver, scenario, turn, profile).await?;
                if let Some(satisfaction) = turn_report.satisfaction {
                    report.satisfaction_trajectory.push(SatisfactionPoint {
                        turn: turn.turn,
                        satisfaction,
                    });
                }
                if let Some(answer) = &turn_report.answer {
                    transcript.push(format!("assistant (memory answer): {answer}"));
                    trim_transcript(&mut transcript);
                }
                report.usage.accumulate(&turn_report.usage);
                report.turns.push(turn_report);
            }
            _ => {}
        }
    }

    // Attribution covers every turn: a formation or maintenance failure is a
    // memory-system failure even when no checkpoint has run yet.
    for turn in &report.turns {
        report.attribution.add_turn(turn);
    }
    let (score, total_stddev) = aggregate_scores(&report.turns);
    report.score = score;
    report.total_stddev = total_stddev;
    Ok(report)
}

const TRANSCRIPT_TAIL: usize = 12;

fn push_transcript(transcript: &mut Vec<String>, turn: &EvalTurn, report: &EvalTurnReport) {
    if let Some(message) = &report.simulated_message {
        transcript.push(format!("user: {message}"));
    } else if let Some(user) = &turn.user {
        transcript.push(format!("user: {user}"));
    } else if let Some(first) = turn.messages.first() {
        let text: String = first
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        if !text.is_empty() {
            transcript.push(format!("user: {text}"));
        }
    }
    trim_transcript(transcript);
}

fn trim_transcript(transcript: &mut Vec<String>) {
    if transcript.len() > TRANSCRIPT_TAIL {
        let excess = transcript.len() - TRANSCRIPT_TAIL;
        transcript.drain(..excess);
    }
}

/// Built-in noise corpus: plausible, memory-worthless chatter that stresses
/// Formation's signal extraction without adding scoreable facts.
const DEFAULT_NOISE_CORPUS: &[&str] = &[
    "What's the weather looking like this weekend?",
    "Ha, that reminds me of a meme I saw today.",
    "Can you convert 3.5 miles to kilometers?",
    "What time is it in Tokyo right now?",
    "Tell me a quick joke.",
    "What's a synonym for 'interesting'?",
    "How do you spell 'accommodate'?",
    "Random question: why is the sky blue?",
    "What's 18% tip on $62?",
    "Can you summarize what photosynthesis is in one line?",
    "Who won the world cup in 2018?",
    "What's the capital of Australia?",
    "How many ounces in a cup?",
    "Give me a fun fact about octopuses.",
    "What does 'ad hoc' mean?",
    "Is it 'affect' or 'effect' here: the rain ___ my mood?",
    "How long should I boil an egg?",
    "What's a good password manager?",
    "Never mind, I forgot what I was going to ask.",
    "What's the square root of 361?",
    "Recommend a random podcast episode topic.",
    "How do I say 'thank you' in Japanese?",
    "What year did the Berlin Wall fall?",
    "Quick, name three primary colors.",
];

/// Deterministic stateless PRNG (splitmix64) so noise timelines are exactly
/// reproducible across runs and machines, independent of any rand crate.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Expands the authored timeline with deterministic noise turns between
/// adjacent entries. Anchor turns keep their ids and rubrics; noise turns are
/// marked and reuse the preceding anchor's id and timestamp.
pub fn effective_timeline(scenario: &EvalScenario) -> Vec<EvalTurn> {
    let Some(noise) = &scenario.noise else {
        return scenario.timeline.clone();
    };
    if noise.between_turns == 0 || scenario.timeline.is_empty() {
        return scenario.timeline.clone();
    }

    let corpus: Vec<&str> = if noise.corpus.is_empty() {
        DEFAULT_NOISE_CORPUS.to_vec()
    } else {
        noise.corpus.iter().map(String::as_str).collect()
    };

    let mut timeline = Vec::with_capacity(
        scenario
            .timeline
            .len()
            .saturating_mul(1 + noise.between_turns),
    );
    for (index, turn) in scenario.timeline.iter().enumerate() {
        if index > 0 {
            let previous = &scenario.timeline[index - 1];
            for j in 0..noise.between_turns {
                let pick = splitmix64(noise.seed ^ ((index as u64) << 16) ^ j as u64) as usize
                    % corpus.len();
                timeline.push(EvalTurn {
                    turn: previous.turn,
                    turn_type: EvalTurnType::Normal,
                    timestamp: previous.timestamp.clone(),
                    context: None,
                    user: Some(corpus[pick].to_string()),
                    messages: Vec::new(),
                    query: None,
                    intent: None,
                    evaluation: None,
                    maintenance: None,
                    noise: true,
                });
            }
        }
        timeline.push(turn.clone());
    }
    timeline
}

async fn run_normal_turn<D>(
    driver: &D,
    scenario: &EvalScenario,
    turn: &EvalTurn,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<EvalTurnReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    let messages = turn_messages(turn)?;
    let input = FormationInput {
        messages,
        context: turn_context(scenario, turn),
        timestamp: Some(turn_timestamp(turn)),
    };

    let started = Instant::now();
    let output = driver.remember(input).await?;
    let mut findings = agent_failure_finding(output.failed_reason);
    if let Some(conversation) = output.conversation {
        findings.extend(wait_failure_finding(
            EvalFindingKind::FormationMiss,
            driver
                .wait_for_formation(conversation, timeout, poll_interval)
                .await,
        ));
    }

    Ok(EvalTurnReport {
        turn: turn.turn,
        turn_type: EvalTurnTypeReport::Normal,
        latency_ms: started.elapsed().as_millis() as u64,
        usage: output.usage,
        conversation: output.conversation,
        noise: turn.noise,
        findings,
        ..Default::default()
    })
}

const SIMULATOR_INSTRUCTIONS: &str = r#"You simulate a real user talking to an assistant that has long-term memory. You will receive the user's hidden profile (ground truth about who they are), the user's intent for this turn, the recent conversation transcript, and the user's recent satisfaction with the assistant's memory (0..1, 1 = fully satisfied).

Write the user's next message. Rules:
- Stay consistent with the hidden profile; never contradict it.
- Pursue the given intent naturally; reveal profile facts only as a real user would.
- Adapt to satisfaction: if recent satisfaction was low, show mild frustration, restate facts the assistant got wrong, or avoid relying on the assistant for things it failed at.
- Output ONLY the user's message text. No quotes, no commentary."#;

async fn run_simulated_turn<D>(
    driver: &D,
    scenario: &EvalScenario,
    turn: &EvalTurn,
    transcript: &[String],
    satisfaction: &[SatisfactionPoint],
    timeout: Duration,
    poll_interval: Duration,
) -> Result<EvalTurnReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    let intent = turn
        .intent
        .as_deref()
        .ok_or_else(|| format!("simulated turn {} has no `intent`", turn.turn))?;
    let trail: Vec<String> = satisfaction
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(|point| format!("turn {} -> {:.2}", point.turn, point.satisfaction))
        .collect();
    let prompt = format!(
        "# Hidden user profile\n{}\n\n# Intent for this turn\n{}\n\n# Recent transcript\n{}\n\n# Recent satisfaction with the assistant's memory\n{}",
        serde_json::to_string(&scenario.hidden_profile).unwrap_or_default(),
        intent,
        if transcript.is_empty() {
            "(start of relationship)".to_string()
        } else {
            transcript.join("\n")
        },
        if trail.is_empty() {
            "(no checkpoints yet)".to_string()
        } else {
            trail.join("\n")
        },
    );

    let started = Instant::now();
    let sim_output = driver
        .complete(CompletionRequest {
            instructions: SIMULATOR_INSTRUCTIONS.to_string(),
            prompt,
            ..Default::default()
        })
        .await?;
    let message = sim_output.content.trim().to_string();
    if message.is_empty() {
        return Err(format!("user simulator produced no message for turn {}", turn.turn).into());
    }

    let input = FormationInput {
        messages: vec![Message {
            role: "user".to_string(),
            content: vec![message.clone().into()],
            ..Default::default()
        }],
        context: turn_context(scenario, turn),
        timestamp: Some(turn_timestamp(turn)),
    };
    let output = driver.remember(input).await?;
    let mut findings = agent_failure_finding(output.failed_reason);
    if let Some(conversation) = output.conversation {
        findings.extend(wait_failure_finding(
            EvalFindingKind::FormationMiss,
            driver
                .wait_for_formation(conversation, timeout, poll_interval)
                .await,
        ));
    }

    let mut usage = sim_output.usage;
    usage.accumulate(&output.usage);
    Ok(EvalTurnReport {
        turn: turn.turn,
        turn_type: EvalTurnTypeReport::Simulated,
        latency_ms: started.elapsed().as_millis() as u64,
        usage,
        conversation: output.conversation,
        simulated_message: Some(message),
        findings,
        ..Default::default()
    })
}

async fn run_maintenance_turn<D>(
    driver: &D,
    turn: &EvalTurn,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<EvalTurnReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    let input = turn
        .maintenance
        .clone()
        .unwrap_or_else(|| MaintenanceInput {
            timestamp: turn.timestamp.clone(),
            ..Default::default()
        });

    run_maintenance(
        driver,
        turn.turn,
        EvalTurnTypeReport::Maintenance,
        input,
        timeout,
        poll_interval,
    )
    .await
}

async fn run_auto_maintenance<D>(
    driver: &D,
    profile: &EvalProfile,
    turn: &EvalTurn,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<EvalTurnReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    let input = MaintenanceInput {
        trigger: "threshold".to_string(),
        scope: profile.maintenance_scope,
        timestamp: turn.timestamp.clone(),
        ..Default::default()
    };

    run_maintenance(
        driver,
        turn.turn,
        EvalTurnTypeReport::AutoMaintenance,
        input,
        timeout,
        poll_interval,
    )
    .await
}

async fn run_maintenance<D>(
    driver: &D,
    turn: u64,
    turn_type: EvalTurnTypeReport,
    input: MaintenanceInput,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<EvalTurnReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    let started = Instant::now();
    let output = driver.maintain(input).await?;
    // `conversation` is None when a cycle was already in flight (e.g. one the
    // hook auto-triggered after formation). The wait polls the processing
    // flag either way, so the next turn never overlaps a consolidation.
    let mut findings = agent_failure_finding(output.failed_reason);
    findings.extend(wait_failure_finding(
        EvalFindingKind::BadConsolidation,
        driver
            .wait_for_maintenance(
                output.conversation.unwrap_or_default(),
                timeout,
                poll_interval,
            )
            .await,
    ));

    Ok(EvalTurnReport {
        turn,
        turn_type,
        latency_ms: started.elapsed().as_millis() as u64,
        usage: output.usage,
        conversation: output.conversation,
        findings,
        ..Default::default()
    })
}

async fn run_checkpoint_turn<D>(
    driver: &D,
    scenario: &EvalScenario,
    turn: &EvalTurn,
    profile: &EvalProfile,
) -> Result<EvalTurnReport, BoxError>
where
    D: EvalDriver + ?Sized,
{
    let rubric = turn.evaluation.clone().unwrap_or_default();
    // Hook-triggered maintenance can still be consolidating right after the
    // last formation turn; probing mid-consolidation would read a graph in
    // flux. Wait for idle first, degrading to a finding on timeout.
    let mut pre_findings = Vec::new();
    pre_findings.extend(wait_failure_finding(
        EvalFindingKind::BadConsolidation,
        driver
            .wait_for_maintenance(
                0,
                Duration::from_millis(profile.wait_timeout_ms),
                Duration::from_millis(profile.poll_interval_ms),
            )
            .await,
    ));
    // Probes read the graph before Recall answers; recall itself never writes
    // to the graph, so one probe pass covers every sample.
    let (probes, mut total_usage) = run_memory_probes(driver, &rubric, profile.judge).await?;
    let graph_stats = driver.graph_stats().await.unwrap_or(None);
    let query = checkpoint_query(turn)?;
    let input = RecallInput {
        query,
        context: turn_context(scenario, turn),
    };

    let sample_count = profile.checkpoint_samples.max(1);
    let mut samples: Vec<EvalCheckpointSample> = Vec::with_capacity(sample_count);
    let mut verdicts: Vec<Option<JudgeVerdict>> = Vec::with_capacity(sample_count);
    let mut conversations: Vec<Option<u64>> = Vec::with_capacity(sample_count);
    let mut traces: Vec<Option<RecallTrace>> = Vec::with_capacity(sample_count);
    let mut judge_errors = 0usize;

    for _ in 0..sample_count {
        let started = Instant::now();
        let output = driver.recall(input.clone()).await?;
        let latency_ms = started.elapsed().as_millis() as u64;
        let trace = match output.conversation {
            Some(conversation) => driver.recall_trace(conversation).await?,
            None => None,
        };

        let verdict = if profile.judge == EvalJudgeKind::Llm {
            let judge_input = judge::JudgeCheckpointInput {
                query: input.query.as_str(),
                answer: output.content.as_str(),
                scoring_rubric: rubric.scoring_rubric.as_deref(),
                hidden_profile: &scenario.hidden_profile,
                required_terms: &rubric.required_answer_terms,
                forbidden_terms: &rubric.forbidden_answer_terms,
                probes: &probes,
                expectations: rubric
                    .expected_memories
                    .iter()
                    .map(|expectation| judge::JudgeExpectation {
                        id: expectation.id.clone(),
                        mode: expectation.mode,
                        description: expectation.description.clone(),
                        probe_satisfied: probes
                            .iter()
                            .find(|probe| probe.expectation_id == expectation.id)
                            .map(|probe| probe.satisfied),
                    })
                    .collect(),
                trace_summary: trace.as_ref().map(|trace| {
                    judge::truncate_chars(&serde_json::to_string(trace).unwrap_or_default(), 6_000)
                }),
            };
            match judge::judge_checkpoint(driver, judge_input).await {
                Ok(call) => {
                    total_usage.accumulate(&call.usage);
                    Some(call.verdict)
                }
                Err(err) => {
                    judge_errors += 1;
                    log::warn!(target: "eval", "judge failed, falling back to lexical: {err}");
                    None
                }
            }
        } else {
            None
        };

        let observation = CheckpointObservation {
            answer: output.content.as_str(),
            probes: &probes,
            recall_trace: trace.as_ref(),
            latency_ms,
            usage: &output.usage,
            graph_stats: graph_stats.as_ref(),
        };
        let scored = score_checkpoint(&rubric, &observation, profile, verdict.as_ref());
        let mut findings = agent_failure_finding(output.failed_reason.clone());
        findings.extend(scored.findings);

        total_usage.accumulate(&output.usage);
        conversations.push(output.conversation);
        traces.push(trace);
        verdicts.push(verdict);
        samples.push(EvalCheckpointSample {
            answer: output.content,
            latency_ms,
            usage: output.usage,
            score: scored.score,
            findings,
        });
    }

    // Representative sample: the median by total score, so the reported
    // answer/trace reflect typical (not best or worst) behavior.
    let representative = median_sample_index(&samples);
    let (mean_score, score_stddev) = mean_sample_scores(&samples);
    let mut findings = pre_findings;
    findings.extend(majority_findings(&samples));
    for _ in 0..judge_errors {
        findings.push(EvalFinding {
            kind: EvalFindingKind::JudgeError,
            expectation_id: None,
            message: "LLM judge failed; sample scored lexically".to_string(),
        });
    }
    let satisfaction = {
        let values: Vec<f64> = verdicts
            .iter()
            .zip(samples.iter())
            .map(|(verdict, sample)| {
                verdict
                    .as_ref()
                    .map(|verdict| verdict.satisfaction)
                    // Lexical proxy: score total stands in for satisfaction.
                    .unwrap_or(sample.score.total)
            })
            .collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };

    let mean_latency =
        samples.iter().map(|sample| sample.latency_ms).sum::<u64>() / samples.len().max(1) as u64;
    Ok(EvalTurnReport {
        turn: turn.turn,
        turn_type: EvalTurnTypeReport::Checkpoint,
        latency_ms: mean_latency,
        usage: total_usage,
        conversation: conversations[representative],
        answer: Some(samples[representative].answer.clone()),
        score: Some(mean_score),
        score_stddev,
        satisfaction,
        judge_reasoning: verdicts[representative]
            .as_ref()
            .map(|verdict| verdict.reasoning.clone()),
        graph_stats,
        probes,
        recall_trace: traces[representative].clone(),
        samples: if sample_count > 1 {
            samples
        } else {
            Vec::new()
        },
        findings,
        ..Default::default()
    })
}

fn median_sample_index(samples: &[EvalCheckpointSample]) -> usize {
    let mut order: Vec<usize> = (0..samples.len()).collect();
    order.sort_by(|&a, &b| {
        samples[a]
            .score
            .total
            .partial_cmp(&samples[b].score.total)
            .unwrap_or(Ordering::Equal)
    });
    order[order.len() / 2]
}

/// Field-wise mean over sample scores plus the sample stddev of totals.
fn mean_sample_scores(samples: &[EvalCheckpointSample]) -> (EvalScore, Option<f64>) {
    let n = samples.len().max(1) as f64;
    let mut mean = EvalScore::default();
    for sample in samples {
        mean.total += sample.score.total / n;
        mean.memory_utility += sample.score.memory_utility / n;
        mean.evolution_quality += sample.score.evolution_quality / n;
        mean.uncertainty_calibration += sample.score.uncertainty_calibration / n;
        mean.forgetting_quality += sample.score.forgetting_quality / n;
        mean.graph_health += sample.score.graph_health / n;
        mean.latency_penalty += sample.score.latency_penalty / n;
        mean.token_cost_penalty += sample.score.token_cost_penalty / n;
    }
    let stddev = if samples.len() > 1 {
        let variance = samples
            .iter()
            .map(|sample| {
                let delta = sample.score.total - mean.total;
                delta * delta
            })
            .sum::<f64>()
            / (samples.len() - 1) as f64;
        Some(variance.sqrt())
    } else {
        None
    };
    (mean, stddev)
}

/// A finding counts for the turn when it shows up in at least half of the
/// samples, so one unlucky roll neither hides nor invents a problem.
fn majority_findings(samples: &[EvalCheckpointSample]) -> Vec<EvalFinding> {
    if samples.len() == 1 {
        return samples[0].findings.clone();
    }
    let mut counts: BTreeMap<(String, Option<String>), (usize, EvalFinding)> = BTreeMap::new();
    for sample in samples {
        let mut seen = BTreeSet::new();
        for finding in &sample.findings {
            let kind_key = serde_json::to_string(&finding.kind).unwrap_or_default();
            let key = (kind_key, finding.expectation_id.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            counts
                .entry(key)
                .and_modify(|(count, _)| *count += 1)
                .or_insert_with(|| (1, finding.clone()));
        }
    }
    let threshold = samples.len().div_ceil(2);
    counts
        .into_values()
        .filter(|(count, _)| *count >= threshold)
        .map(|(_, finding)| finding)
        .collect()
}

async fn run_memory_probes<D>(
    driver: &D,
    rubric: &EvalRubric,
    judge_kind: EvalJudgeKind,
) -> Result<(Vec<MemoryProbeReport>, Usage), BoxError>
where
    D: EvalDriver + ?Sized,
{
    let mut probes = Vec::new();
    let mut usage = Usage::default();
    for expectation in &rubric.expected_memories {
        // Semantic probes state intent ("an active BBQ preference") instead of
        // hand-written KQL, so they stay correct across valid graph encodings.
        if let (Some(assertion), EvalJudgeKind::Llm) = (&expectation.assertion, judge_kind) {
            let search = expectation.search.as_deref().unwrap_or(assertion.as_str());
            let command = format!(
                "SEARCH CONCEPT \"{}\" MODE \"semantic\" THRESHOLD 0.35 LIMIT 8",
                search.replace('"', "\\\"")
            );
            let response = driver
                .execute_kip_readonly(Request {
                    command,
                    readonly: true,
                    ..Default::default()
                })
                .await?;
            let hit_count = response_hit_count(&response);
            let evidence = match &response {
                Response::Ok { result, .. } => result.clone(),
                Response::Err { .. } => Json::Null,
            };

            let (satisfied, judge_reason) =
                match judge::judge_assertion(driver, assertion, &evidence).await {
                    Ok(call) => {
                        usage.accumulate(&call.usage);
                        let satisfied = match expectation.mode {
                            MemoryExpectationMode::ShouldExist => call.verdict.holds,
                            MemoryExpectationMode::ShouldNotExist => !call.verdict.holds,
                        };
                        (satisfied, Some(call.verdict.reason))
                    }
                    Err(err) => {
                        // Degrade to hit-count semantics rather than failing
                        // the whole checkpoint.
                        let satisfied = match expectation.mode {
                            MemoryExpectationMode::ShouldExist => hit_count > 0,
                            MemoryExpectationMode::ShouldNotExist => hit_count == 0,
                        };
                        (satisfied, Some(format!("judge error: {err}")))
                    }
                };

            probes.push(MemoryProbeReport {
                expectation_id: expectation.id.clone(),
                mode: expectation.mode,
                hit_count,
                satisfied,
                assertion: Some(assertion.clone()),
                judge_reason,
                response: Some(response),
            });
            continue;
        }

        let Some(request) = expectation.probe.clone() else {
            continue;
        };
        let response = driver.execute_kip_readonly(request).await?;
        let hit_count = response_hit_count(&response);
        let satisfied = match expectation.mode {
            MemoryExpectationMode::ShouldExist => {
                hit_count > 0 && !matches!(response, Response::Err { .. })
            }
            MemoryExpectationMode::ShouldNotExist => {
                hit_count == 0 && !matches!(response, Response::Err { .. })
            }
        };
        probes.push(MemoryProbeReport {
            expectation_id: expectation.id.clone(),
            mode: expectation.mode,
            hit_count,
            satisfied,
            assertion: None,
            judge_reason: None,
            response: Some(response),
        });
    }
    Ok((probes, usage))
}

struct CheckpointScore {
    score: EvalScore,
    findings: Vec<EvalFinding>,
}

/// One recall sample's observable outcome, scored against the rubric.
struct CheckpointObservation<'a> {
    answer: &'a str,
    probes: &'a [MemoryProbeReport],
    recall_trace: Option<&'a RecallTrace>,
    latency_ms: u64,
    usage: &'a Usage,
    graph_stats: Option<&'a GraphStats>,
}

fn score_checkpoint(
    rubric: &EvalRubric,
    observation: &CheckpointObservation<'_>,
    profile: &EvalProfile,
    verdict: Option<&JudgeVerdict>,
) -> CheckpointScore {
    let answer = observation.answer;
    let mut findings = Vec::new();

    let required_answer_score = fraction_present(&rubric.required_answer_terms, answer);
    if verdict.is_none() {
        for term in missing_terms(&rubric.required_answer_terms, answer) {
            findings.push(EvalFinding {
                kind: EvalFindingKind::BadSynthesis,
                expectation_id: None,
                message: format!("answer is missing required term `{term}`"),
            });
        }
    }

    let mut expected_present_weight = 0.0;
    let mut expected_present_score = 0.0;
    let mut forgetting_weight = 0.0;
    let mut forgetting_score = 0.0;
    let mut probe_errors = 0usize;
    let probe_by_id: BTreeMap<&str, &MemoryProbeReport> = observation
        .probes
        .iter()
        .map(|probe| (probe.expectation_id.as_str(), probe))
        .collect();

    for expectation in &rubric.expected_memories {
        let probe = probe_by_id.get(expectation.id.as_str()).copied();
        let probe_satisfied = probe.map(|p| p.satisfied).unwrap_or(true);
        let probe_error = probe
            .and_then(|p| p.response.as_ref())
            .is_some_and(|response| matches!(response, Response::Err { .. }));
        if probe_error {
            probe_errors += 1;
            findings.push(EvalFinding {
                kind: EvalFindingKind::GraphProbeError,
                expectation_id: Some(expectation.id.clone()),
                message: "read-only KIP probe returned an error".to_string(),
            });
        }

        match expectation.mode {
            MemoryExpectationMode::ShouldExist => {
                expected_present_weight += expectation.weight;
                if probe_satisfied {
                    expected_present_score += expectation.weight;
                } else {
                    findings.push(EvalFinding {
                        kind: EvalFindingKind::FormationMiss,
                        expectation_id: Some(expectation.id.clone()),
                        message: "expected memory was not present in the graph before recall"
                            .to_string(),
                    });
                }

                // Lexical grounding/synthesis attribution only runs without a
                // judge verdict; the judge sees probes and trace directly and
                // reports its own attributed findings.
                if verdict.is_none() {
                    let expectation_terms = expectation_terms(expectation);
                    let missing = missing_terms(&expectation.answer_terms, answer);
                    if probe_satisfied && !missing.is_empty() {
                        let trace_has_evidence = observation
                            .recall_trace
                            .is_some_and(|trace| trace.contains_any_term(&expectation_terms));
                        let kind = if trace_has_evidence {
                            EvalFindingKind::BadSynthesis
                        } else {
                            EvalFindingKind::BadGrounding
                        };
                        findings.push(EvalFinding {
                            kind,
                            expectation_id: Some(expectation.id.clone()),
                            message: format!(
                                "answer did not use expected memory terms: {}",
                                missing.join(", ")
                            ),
                        });
                    }
                }
            }
            MemoryExpectationMode::ShouldNotExist => {
                forgetting_weight += expectation.weight;
                if probe_satisfied {
                    forgetting_score += expectation.weight;
                } else {
                    findings.push(EvalFinding {
                        kind: EvalFindingKind::BadConsolidation,
                        expectation_id: Some(expectation.id.clone()),
                        message: "stale or superseded memory is still active".to_string(),
                    });
                }
            }
        }
    }

    // Forbidden terms: without a judge, term presence is treated as a stale
    // leak (legacy behavior). With a judge, a term mention alone is only a
    // hard failure when the judge also finds the answer overconfident —
    // correct meta-references ("unlike your old BBQ preference…") pass.
    let forbidden_present = present_terms(&rubric.forbidden_answer_terms, answer);
    let judge_confirms_stale = verdict
        .map(|verdict| {
            verdict.uncertainty_calibration < 0.5
                || verdict
                    .findings
                    .iter()
                    .any(|finding| finding.kind == EvalFindingKind::Overconfidence)
        })
        .unwrap_or(true);
    if judge_confirms_stale {
        for term in &forbidden_present {
            findings.push(EvalFinding {
                kind: EvalFindingKind::Overconfidence,
                expectation_id: None,
                message: format!("answer contains forbidden or stale term `{term}`"),
            });
        }
    }

    let lexical_calibration = if rubric.forbidden_answer_terms.is_empty() {
        1.0
    } else {
        1.0 - forbidden_present.len() as f64 / rubric.forbidden_answer_terms.len() as f64
    };

    let expected_present_score = if expected_present_weight == 0.0 {
        1.0
    } else {
        expected_present_score / expected_present_weight
    };
    let lexical_utility = if rubric.required_answer_terms.is_empty() {
        expected_present_score
    } else {
        (required_answer_score + expected_present_score) / 2.0
    };
    let probe_forgetting = if forgetting_weight == 0.0 {
        1.0
    } else {
        forgetting_score / forgetting_weight
    };

    // The judge scores answer quality; graph-state components stay objective.
    // Utility blends judge quality with probe-verified presence so an answer
    // cannot score high while the graph provably lacks the memory.
    let (memory_utility, forgetting_quality, uncertainty_calibration) = match verdict {
        Some(verdict) => (
            0.7 * verdict.memory_utility + 0.3 * expected_present_score,
            0.5 * verdict.forgetting_quality + 0.5 * probe_forgetting,
            verdict.uncertainty_calibration,
        ),
        None => (lexical_utility, probe_forgetting, lexical_calibration),
    };
    if let Some(verdict) = verdict {
        for finding in &verdict.findings {
            // A judge finding of a kind the harness already recorded for the
            // same expectation (or with no expectation at all) is the same
            // root cause; keeping both would double-count against gates.
            let duplicate = findings.iter().any(|existing| {
                existing.kind == finding.kind
                    && (finding.expectation_id.is_none()
                        || existing.expectation_id == finding.expectation_id)
            });
            if !duplicate {
                findings.push(finding.clone());
            }
        }
    }

    // Prefer real metabolism counters over probe execution success.
    let graph_health = observation
        .graph_stats
        .and_then(GraphStats::health)
        .unwrap_or_else(|| {
            if observation.probes.is_empty() {
                1.0
            } else {
                1.0 - probe_errors as f64 / observation.probes.len() as f64
            }
        });
    let evolution_quality = (memory_utility + forgetting_quality) / 2.0;

    let latency_penalty = profile
        .max_checkpoint_latency_ms
        .map(|max| over_budget_ratio(observation.latency_ms, max))
        .unwrap_or_default();
    if latency_penalty > 0.0 {
        findings.push(EvalFinding {
            kind: EvalFindingKind::LatencyCost,
            expectation_id: None,
            message: format!(
                "checkpoint latency {} ms exceeded budget",
                observation.latency_ms
            ),
        });
    }

    let token_cost_penalty = profile
        .max_checkpoint_total_tokens
        .map(|max| over_budget_ratio(usage_total_tokens(observation.usage), max))
        .unwrap_or_default();
    if token_cost_penalty > 0.0 {
        findings.push(EvalFinding {
            kind: EvalFindingKind::TokenCost,
            expectation_id: None,
            message: "checkpoint token usage exceeded budget".to_string(),
        });
    }

    let total = clamp01(
        memory_utility * 0.45
            + evolution_quality * 0.2
            + uncertainty_calibration * 0.15
            + forgetting_quality * 0.1
            + graph_health * 0.1
            - latency_penalty * 0.05
            - token_cost_penalty * 0.05,
    );

    CheckpointScore {
        score: EvalScore {
            total,
            memory_utility: clamp01(memory_utility),
            evolution_quality: clamp01(evolution_quality),
            uncertainty_calibration: clamp01(uncertainty_calibration),
            forgetting_quality: clamp01(forgetting_quality),
            graph_health: clamp01(graph_health),
            latency_penalty: clamp01(latency_penalty),
            token_cost_penalty: clamp01(token_cost_penalty),
        },
        findings,
    }
}

/// Aggregates checkpoint scores with time weighting: later checkpoints weigh
/// more, so an established memory failing late (forgetting regression) costs
/// more than an early miss before the fact was ever stated. Also computes a
/// trajectory-based `evolution_quality` (did the system get better over the
/// timeline?) and propagates sample stddev into a report-level stddev.
fn aggregate_scores(turns: &[EvalTurnReport]) -> (EvalScore, Option<f64>) {
    let checkpoints: Vec<&EvalTurnReport> =
        turns.iter().filter(|turn| turn.score.is_some()).collect();
    if checkpoints.is_empty() {
        return (EvalScore::default(), None);
    }

    // Linear time weights: checkpoint i (0-based) has weight (i + 1).
    let weights: Vec<f64> = (0..checkpoints.len()).map(|i| (i + 1) as f64).collect();
    let weight_sum: f64 = weights.iter().sum();
    let weighted = |field: fn(&EvalScore) -> f64| -> f64 {
        checkpoints
            .iter()
            .zip(weights.iter())
            .map(|(turn, weight)| field(turn.score.as_ref().unwrap()) * weight)
            .sum::<f64>()
            / weight_sum
    };

    // Trajectory: compare the later half of checkpoints against the earlier
    // half. 0.5 = flat, above = improving, below = regressing. With a single
    // checkpoint there is no trajectory; keep the per-turn estimate.
    let totals: Vec<f64> = checkpoints
        .iter()
        .map(|turn| turn.score.as_ref().unwrap().total)
        .collect();
    let evolution_quality = if totals.len() >= 2 {
        let split = totals.len().div_ceil(2);
        let early = totals[..split].iter().sum::<f64>() / split as f64;
        let late = totals[split..].iter().sum::<f64>() / (totals.len() - split) as f64;
        clamp01(0.5 + (late - early) / 2.0)
    } else {
        weighted(|score| score.evolution_quality)
    };

    // Var of a weighted mean: sum((w_i / W)^2 * var_i).
    let mut sum_var = 0.0;
    let mut any_stddev = false;
    for (turn, weight) in checkpoints.iter().zip(weights.iter()) {
        if let Some(stddev) = turn.score_stddev {
            any_stddev = true;
            let normalized = weight / weight_sum;
            sum_var += normalized * normalized * stddev * stddev;
        }
    }
    let total_stddev = any_stddev.then(|| sum_var.sqrt());

    let score = EvalScore {
        total: weighted(|score| score.total),
        memory_utility: weighted(|score| score.memory_utility),
        evolution_quality,
        uncertainty_calibration: weighted(|score| score.uncertainty_calibration),
        forgetting_quality: weighted(|score| score.forgetting_quality),
        graph_health: weighted(|score| score.graph_health),
        latency_penalty: weighted(|score| score.latency_penalty),
        token_cost_penalty: weighted(|score| score.token_cost_penalty),
    };
    (score, total_stddev)
}

fn aggregate_report_scores(reports: &[EvalReport]) -> EvalScore {
    if reports.is_empty() {
        return EvalScore::default();
    }

    let len = reports.len() as f64;
    EvalScore {
        total: reports.iter().map(|r| r.score.total).sum::<f64>() / len,
        memory_utility: reports.iter().map(|r| r.score.memory_utility).sum::<f64>() / len,
        evolution_quality: reports
            .iter()
            .map(|r| r.score.evolution_quality)
            .sum::<f64>()
            / len,
        uncertainty_calibration: reports
            .iter()
            .map(|r| r.score.uncertainty_calibration)
            .sum::<f64>()
            / len,
        forgetting_quality: reports
            .iter()
            .map(|r| r.score.forgetting_quality)
            .sum::<f64>()
            / len,
        graph_health: reports.iter().map(|r| r.score.graph_health).sum::<f64>() / len,
        latency_penalty: reports.iter().map(|r| r.score.latency_penalty).sum::<f64>() / len,
        token_cost_penalty: reports
            .iter()
            .map(|r| r.score.token_cost_penalty)
            .sum::<f64>()
            / len,
    }
}

fn aggregate_suite_scores(suites: &[EvalSuiteReport]) -> EvalScore {
    if suites.is_empty() {
        return EvalScore::default();
    }

    let len = suites.len() as f64;
    EvalScore {
        total: suites.iter().map(|s| s.score.total).sum::<f64>() / len,
        memory_utility: suites.iter().map(|s| s.score.memory_utility).sum::<f64>() / len,
        evolution_quality: suites
            .iter()
            .map(|s| s.score.evolution_quality)
            .sum::<f64>()
            / len,
        uncertainty_calibration: suites
            .iter()
            .map(|s| s.score.uncertainty_calibration)
            .sum::<f64>()
            / len,
        forgetting_quality: suites
            .iter()
            .map(|s| s.score.forgetting_quality)
            .sum::<f64>()
            / len,
        graph_health: suites.iter().map(|s| s.score.graph_health).sum::<f64>() / len,
        latency_penalty: suites.iter().map(|s| s.score.latency_penalty).sum::<f64>() / len,
        token_cost_penalty: suites
            .iter()
            .map(|s| s.score.token_cost_penalty)
            .sum::<f64>()
            / len,
    }
}

fn compare_suites(suites: &[EvalSuiteReport]) -> Vec<EvalSuiteComparison> {
    let Some(best_total) = suites
        .iter()
        .map(|suite| suite.score.total)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
    else {
        return Vec::new();
    };

    let mut comparisons: Vec<EvalSuiteComparison> = suites
        .iter()
        .map(|suite| EvalSuiteComparison {
            suite_id: suite.suite_id.clone(),
            rank: 0,
            score: suite.score.clone(),
            delta_from_best_total: suite.score.total - best_total,
            total_findings: suite.attribution.total_findings(),
            total_tokens: usage_total_tokens(&suite.usage),
        })
        .collect();

    comparisons.sort_by(|a, b| {
        b.score
            .total
            .partial_cmp(&a.score.total)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.total_findings.cmp(&b.total_findings))
            .then_with(|| a.total_tokens.cmp(&b.total_tokens))
            .then_with(|| a.suite_id.cmp(&b.suite_id))
    });

    for (index, comparison) in comparisons.iter_mut().enumerate() {
        comparison.rank = index + 1;
    }

    comparisons
}

fn turn_messages(turn: &EvalTurn) -> Result<Vec<Message>, BoxError> {
    if !turn.messages.is_empty() {
        return Ok(turn.messages.clone());
    }

    let Some(user) = &turn.user else {
        return Err(format!("turn {} has no messages or user text", turn.turn).into());
    };

    Ok(vec![Message {
        role: "user".to_string(),
        content: vec![user.clone().into()],
        ..Default::default()
    }])
}

fn checkpoint_query(turn: &EvalTurn) -> Result<String, BoxError> {
    if let Some(query) = &turn.query
        && !query.trim().is_empty()
    {
        return Ok(query.clone());
    }
    if let Some(user) = &turn.user
        && !user.trim().is_empty()
    {
        return Ok(user.clone());
    }
    if let Some(message) = turn.messages.first()
        && let Some(text) = message.text()
    {
        return Ok(text);
    }

    Err(format!("checkpoint turn {} has no query", turn.turn).into())
}

fn turn_context(scenario: &EvalScenario, turn: &EvalTurn) -> Option<InputContext> {
    turn.context
        .clone()
        .or_else(|| scenario.default_context.clone())
}

fn turn_timestamp(turn: &EvalTurn) -> String {
    turn.timestamp.clone().unwrap_or_else(rfc3339_datetime_now)
}

fn agent_failure_finding(reason: Option<String>) -> Vec<EvalFinding> {
    reason
        .map(|reason| {
            vec![EvalFinding {
                kind: EvalFindingKind::BadSynthesis,
                expectation_id: None,
                message: format!("agent execution failed: {reason}"),
            }]
        })
        .unwrap_or_default()
}

/// Converts a background-wait failure (usually a timeout) into an attributed
/// finding so one stuck stage degrades the score instead of aborting the
/// whole suite and discarding every completed scenario.
fn wait_failure_finding(
    kind: EvalFindingKind,
    result: Result<(), BoxError>,
) -> Option<EvalFinding> {
    result.err().map(|err| EvalFinding {
        kind,
        expectation_id: None,
        message: format!("background stage did not complete: {err}"),
    })
}

fn expectation_terms(expectation: &ExpectedMemory) -> Vec<String> {
    if expectation.trace_terms.is_empty() {
        expectation.answer_terms.clone()
    } else {
        expectation.trace_terms.clone()
    }
}

/// Runs a read-only KIP count query and digs out the first integer in the
/// result. Returns `None` on error so callers degrade gracefully.
async fn kip_count<D>(driver: &D, command: &str) -> Option<u64>
where
    D: EvalDriver + ?Sized,
{
    let request = Request {
        command: command.to_string(),
        readonly: true,
        ..Default::default()
    };
    match driver.execute_kip_readonly(request).await {
        Ok(Response::Ok { result, .. }) => first_integer(&result),
        _ => None,
    }
}

fn first_integer(value: &Json) -> Option<u64> {
    match value {
        Json::Number(number) => number.as_u64(),
        Json::Array(items) => items.iter().find_map(first_integer),
        Json::Object(map) => map.values().find_map(first_integer),
        _ => None,
    }
}

fn response_hit_count(response: &Response) -> usize {
    match response {
        Response::Ok { result, .. } => json_hit_count(result),
        Response::Err { result, .. } => result.as_ref().map(json_hit_count).unwrap_or_default(),
    }
}

fn json_hit_count(value: &Json) -> usize {
    match value {
        Json::Null => 0,
        Json::Bool(false) => 0,
        Json::Bool(true) => 1,
        Json::Number(number) => {
            if number.as_f64().unwrap_or_default() == 0.0 {
                0
            } else {
                1
            }
        }
        Json::String(text) => usize::from(!text.trim().is_empty()),
        Json::Array(items) => {
            if items.iter().all(looks_like_serialized_kip_response) {
                items.iter().map(json_hit_count).sum()
            } else {
                items.len()
            }
        }
        Json::Object(map) => {
            if map.is_empty() {
                0
            } else if let Some(result) = map.get("result") {
                json_hit_count(result)
            } else if map.contains_key("error") {
                0
            } else {
                1
            }
        }
    }
}

fn looks_like_serialized_kip_response(value: &Json) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.contains_key("result") || map.contains_key("error"))
}

fn fraction_present(terms: &[String], text: &str) -> f64 {
    if terms.is_empty() {
        return 1.0;
    }
    present_terms(terms, text).len() as f64 / terms.len() as f64
}

fn present_terms(terms: &[String], text: &str) -> Vec<String> {
    let text = text.to_lowercase();
    terms
        .iter()
        .filter(|term| {
            let term = term.trim();
            !term.is_empty() && text.contains(&term.to_lowercase())
        })
        .cloned()
        .collect()
}

fn missing_terms(terms: &[String], text: &str) -> Vec<String> {
    let text = text.to_lowercase();
    terms
        .iter()
        .filter(|term| {
            let term = term.trim();
            !term.is_empty() && !text.contains(&term.to_lowercase())
        })
        .cloned()
        .collect()
}

fn over_budget_ratio(actual: u64, max: u64) -> f64 {
    if max == 0 || actual <= max {
        return 0.0;
    }
    ((actual - max) as f64 / max as f64).min(1.0)
}

/// Budgeted tokens: input + output only. `cached_tokens` is excluded because
/// provider semantics differ — the OpenAI adapter already counts cached
/// tokens inside `input_tokens` (adding them would double-count), while the
/// Anthropic adapter reports cache reads separately at a fraction of the
/// cost. Excluding them keeps budgets comparable across providers.
fn usage_total_tokens(usage: &Usage) -> u64 {
    usage.input_tokens.saturating_add(usage.output_tokens)
}

fn clamp01(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_core::ToolOutput;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeEvalDriver {
        recall_answer: String,
        /// When non-empty, recall answers rotate through these per call.
        recall_answers: Vec<String>,
        trace: Option<RecallTrace>,
        probes: Mutex<BTreeMap<String, Response>>,
        remembered: Mutex<Vec<FormationInput>>,
        maintained: Mutex<Vec<MaintenanceInput>>,
        /// Canned outputs for `complete`, keyed by a substring of the
        /// instructions ("judge", "simulat", "optimiz"); missing => error.
        completions: Mutex<Vec<(String, String)>>,
        recall_calls: Mutex<usize>,
        /// Simulate stuck background stages: waits return an error.
        fail_formation_wait: bool,
        fail_maintenance_wait: bool,
    }

    #[async_trait::async_trait]
    impl EvalDriver for FakeEvalDriver {
        async fn remember(&self, input: FormationInput) -> Result<EvalAgentResult, BoxError> {
            self.remembered.lock().unwrap().push(input);
            Ok(EvalAgentResult {
                conversation: Some(1),
                ..Default::default()
            })
        }

        async fn recall(&self, _input: RecallInput) -> Result<EvalAgentResult, BoxError> {
            let mut calls = self.recall_calls.lock().unwrap();
            let content = if self.recall_answers.is_empty() {
                self.recall_answer.clone()
            } else {
                self.recall_answers[*calls % self.recall_answers.len()].clone()
            };
            *calls += 1;
            Ok(EvalAgentResult {
                content,
                conversation: Some(2),
                usage: Usage {
                    input_tokens: 60,
                    output_tokens: 40,
                    ..Default::default()
                },
                ..Default::default()
            })
        }

        async fn maintain(&self, input: MaintenanceInput) -> Result<EvalAgentResult, BoxError> {
            self.maintained.lock().unwrap().push(input);
            Ok(EvalAgentResult {
                conversation: Some(3),
                ..Default::default()
            })
        }

        async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError> {
            let key = request.command.clone();
            Ok(self
                .probes
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Response::ok(Json::Array(Vec::new()))))
        }

        async fn complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
            let completions = self.completions.lock().unwrap();
            for (needle, content) in completions.iter() {
                if req.instructions.to_lowercase().contains(needle) {
                    return Ok(AgentOutput {
                        content: content.clone(),
                        usage: Usage {
                            input_tokens: 10,
                            output_tokens: 5,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                }
            }
            Err("no canned completion".into())
        }

        async fn recall_trace(&self, _conversation: u64) -> Result<Option<RecallTrace>, BoxError> {
            Ok(self.trace.clone())
        }

        async fn wait_for_formation(
            &self,
            conversation: u64,
            _timeout: Duration,
            _poll_interval: Duration,
        ) -> Result<(), BoxError> {
            if self.fail_formation_wait {
                return Err(format!("formation conversation {conversation} timed out").into());
            }
            Ok(())
        }

        async fn wait_for_maintenance(
            &self,
            _conversation: u64,
            _timeout: Duration,
            _poll_interval: Duration,
        ) -> Result<(), BoxError> {
            if self.fail_maintenance_wait {
                return Err("maintenance still processing".into());
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_scenario_attributes_grounding_failure() {
        let driver = FakeEvalDriver {
            recall_answer: "I do not know.".to_string(),
            ..Default::default()
        };
        driver.probes.lock().unwrap().insert(
            "find_style".to_string(),
            Response::ok(json!([{"name": "concise direct style"}])),
        );
        let scenario = EvalScenario {
            id: "style".to_string(),
            default_context: Some(InputContext {
                counterparty: Some("user_042".to_string()),
                ..Default::default()
            }),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    turn_type: EvalTurnType::Normal,
                    user: Some("I prefer concise, direct writing.".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 50,
                    turn_type: EvalTurnType::CheckpointOrganic,
                    query: Some("Can you rewrite this to sound more like me?".to_string()),
                    evaluation: Some(EvalRubric {
                        required_answer_terms: vec!["concise".to_string()],
                        expected_memories: vec![ExpectedMemory {
                            id: "style_pref".to_string(),
                            probe: Some(Request {
                                command: "find_style".to_string(),
                                readonly: true,
                                ..Default::default()
                            }),
                            answer_terms: vec!["concise".to_string()],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };

        let report = run_scenario(&driver, &scenario, &EvalProfile::default())
            .await
            .unwrap();

        assert_eq!(driver.remembered.lock().unwrap().len(), 1);
        assert_eq!(report.attribution.bad_grounding, 1);
        assert!(report.score.total < 1.0);
    }

    #[tokio::test]
    async fn wait_timeouts_become_findings_instead_of_aborting() {
        let driver = FakeEvalDriver {
            recall_answer: "an answer".to_string(),
            fail_formation_wait: true,
            fail_maintenance_wait: true,
            ..Default::default()
        };
        let scenario = EvalScenario {
            id: "timeouts".to_string(),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    user: Some("remember this fact".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 2,
                    turn_type: EvalTurnType::Maintenance,
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 3,
                    turn_type: EvalTurnType::CheckpointSynthetic,
                    query: Some("what fact?".to_string()),
                    evaluation: Some(EvalRubric::default()),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };

        let report = run_scenario(&driver, &scenario, &EvalProfile::default())
            .await
            .unwrap();

        // Every turn still ran; the stuck stages degraded to findings.
        assert_eq!(report.turns.len(), 3);
        assert_eq!(
            report.turns[0].findings[0].kind,
            EvalFindingKind::FormationMiss
        );
        assert_eq!(
            report.turns[1].findings[0].kind,
            EvalFindingKind::BadConsolidation
        );
        let checkpoint = &report.turns[2];
        assert!(checkpoint.answer.is_some());
        assert!(
            checkpoint
                .findings
                .iter()
                .any(|finding| finding.kind == EvalFindingKind::BadConsolidation)
        );
        // Attribution counts failures from every turn, not just checkpoints.
        assert_eq!(report.attribution.formation_miss, 1);
        assert_eq!(report.attribution.bad_consolidation, 2);
    }

    #[tokio::test]
    async fn judge_findings_do_not_double_count_harness_findings() {
        // The probe finds nothing, so the harness records a FormationMiss for
        // the expectation; the judge reports the same kind (no expectation id)
        // plus a novel BadSynthesis. Only the novel finding may add.
        let verdict = json!({
            "memory_utility": 0.2,
            "forgetting_quality": 1.0,
            "uncertainty_calibration": 1.0,
            "satisfaction": 0.4,
            "reasoning": "memory missing",
            "findings": [
                {"kind": "formation_miss", "message": "graph never formed the fact"},
                {"kind": "bad_synthesis", "message": "answer ignored the query"}
            ]
        });
        let driver = FakeEvalDriver {
            recall_answer: "I do not know.".to_string(),
            completions: Mutex::new(vec![("strict evaluator".to_string(), verdict.to_string())]),
            ..Default::default()
        };
        let scenario = EvalScenario {
            id: "dedup".to_string(),
            timeline: vec![EvalTurn {
                turn: 1,
                turn_type: EvalTurnType::CheckpointOrganic,
                query: Some("What do I like?".to_string()),
                evaluation: Some(EvalRubric {
                    expected_memories: vec![ExpectedMemory {
                        id: "missing".to_string(),
                        probe: Some(Request {
                            command: "find_missing".to_string(),
                            readonly: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..empty_turn()
            }],
            ..empty_scenario()
        };
        let profile = EvalProfile {
            judge: EvalJudgeKind::Llm,
            ..Default::default()
        };

        let report = run_scenario(&driver, &scenario, &profile).await.unwrap();

        assert_eq!(report.attribution.formation_miss, 1);
        assert_eq!(report.attribution.bad_synthesis, 1);
    }

    #[test]
    fn recall_trace_extracts_tool_calls_and_outputs() {
        let call = ContentPart::ToolCall {
            name: "execute_kip_readonly".to_string(),
            args: json!({"command": "FIND(?x) WHERE { ?x {type: \"Preference\"} }"}),
            call_id: Some("call_1".to_string()),
        };
        let output = ToolOutput::new(json!([{"name": "prefers concise"}]));
        let output = ContentPart::ToolOutput {
            name: "execute_kip_readonly".to_string(),
            output: json!(output.output),
            is_error: None,
            call_id: Some("call_1".to_string()),
            remote_id: None,
        };
        let messages = vec![Message {
            role: "assistant".to_string(),
            content: vec![call, output],
            ..Default::default()
        }];

        let trace = RecallTrace::from_messages(&messages);

        assert_eq!(trace.tools.len(), 1);
        assert!(trace.contains_any_term(&["concise".to_string()]));
    }

    #[test]
    fn response_hit_count_handles_batch_responses() {
        let response = Response::ok(json!([
            {"result": [{"name": "a"}, {"name": "b"}]},
            {"result": []},
            {"error": {"code": "KIP_3002"}}
        ]));

        assert_eq!(response_hit_count(&response), 2);
    }

    #[test]
    fn suite_report_aggregates_scores_usage_and_attribution() {
        let reports = vec![
            EvalReport {
                scenario_id: "a".to_string(),
                score: EvalScore {
                    total: 0.5,
                    memory_utility: 0.4,
                    ..Default::default()
                },
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                attribution: AttributionSummary {
                    bad_grounding: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            EvalReport {
                scenario_id: "b".to_string(),
                score: EvalScore {
                    total: 1.0,
                    memory_utility: 0.8,
                    ..Default::default()
                },
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 7,
                    ..Default::default()
                },
                attribution: AttributionSummary {
                    overconfidence: 2,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];

        let suite = EvalSuiteReport::from_reports("suite".to_string(), reports);

        assert_eq!(suite.reports.len(), 2);
        assert_eq!(suite.usage.input_tokens, 30);
        assert_eq!(suite.usage.output_tokens, 12);
        assert_eq!(suite.attribution.bad_grounding, 1);
        assert_eq!(suite.attribution.overconfidence, 2);
        assert_eq!(suite.score.total, 0.75);
        assert!((suite.score.memory_utility - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn experiment_report_aggregates_suite_reports() {
        let suites = vec![
            EvalSuiteReport {
                suite_id: "a".to_string(),
                score: EvalScore {
                    total: 0.25,
                    graph_health: 0.5,
                    ..Default::default()
                },
                usage: Usage {
                    input_tokens: 3,
                    ..Default::default()
                },
                attribution: AttributionSummary {
                    formation_miss: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            EvalSuiteReport {
                suite_id: "b".to_string(),
                score: EvalScore {
                    total: 0.75,
                    graph_health: 1.0,
                    ..Default::default()
                },
                usage: Usage {
                    input_tokens: 7,
                    ..Default::default()
                },
                attribution: AttributionSummary {
                    bad_synthesis: 2,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];

        let experiment = EvalExperimentReport::from_suites("experiment".to_string(), suites);

        assert_eq!(experiment.suites.len(), 2);
        assert_eq!(experiment.usage.input_tokens, 10);
        assert_eq!(experiment.attribution.formation_miss, 1);
        assert_eq!(experiment.attribution.bad_synthesis, 2);
        assert_eq!(experiment.score.total, 0.5);
        assert_eq!(experiment.score.graph_health, 0.75);
        assert_eq!(experiment.best_suite_id.as_deref(), Some("b"));
        assert_eq!(experiment.comparisons.len(), 2);
        assert_eq!(experiment.comparisons[0].suite_id, "b");
        assert_eq!(experiment.comparisons[0].rank, 1);
        assert_eq!(experiment.comparisons[0].delta_from_best_total, 0.0);
        assert_eq!(experiment.comparisons[1].suite_id, "a");
        assert_eq!(experiment.comparisons[1].rank, 2);
        assert_eq!(experiment.comparisons[1].delta_from_best_total, -0.5);
        assert_eq!(experiment.comparisons[1].total_findings, 1);
        assert_eq!(experiment.comparisons[1].total_tokens, 3);
    }

    #[test]
    fn eval_gate_reports_score_and_finding_failures() {
        let gate = EvalGate {
            min_total_score: Some(0.9),
            max_total_findings: Some(1),
            confidence_z: None,
        };
        let report = gate.evaluate(
            &EvalScore {
                total: 0.8,
                ..Default::default()
            },
            &AttributionSummary {
                formation_miss: 1,
                bad_grounding: 1,
                ..Default::default()
            },
            None,
        );

        assert!(!report.passed);
        assert_eq!(report.criteria.min_total_score, Some(0.9));
        assert_eq!(report.criteria.max_total_findings, Some(1));
        assert_eq!(report.failures.len(), 2);
        assert!(report.failures[0].contains("below required minimum"));
        assert!(report.failures[1].contains("exceeds maximum"));
    }

    #[test]
    fn eval_gate_confidence_z_gates_on_lower_bound() {
        let gate = EvalGate {
            min_total_score: Some(0.75),
            max_total_findings: None,
            confidence_z: Some(2.0),
        };
        let score = EvalScore {
            total: 0.8,
            ..Default::default()
        };
        let attribution = AttributionSummary::default();

        // Mean passes, but the 2-sigma lower bound (0.8 - 2*0.05 = 0.7) fails.
        let report = gate.evaluate(&score, &attribution, Some(0.05));
        assert!(!report.passed);
        assert!(report.failures[0].contains("mean 0.8000"));

        // Without stddev the mean is used directly.
        let report = gate.evaluate(&score, &attribution, None);
        assert!(report.passed);

        // Tight variance passes the lower bound.
        let report = gate.evaluate(&score, &attribution, Some(0.01));
        assert!(report.passed);
    }

    #[test]
    fn validate_eval_plan_reports_offline_input_errors() {
        let scenario = EvalScenario {
            id: "scenario".to_string(),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    turn_type: EvalTurnType::Normal,
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 2,
                    turn_type: EvalTurnType::CheckpointSynthetic,
                    query: Some("What should I remember?".to_string()),
                    evaluation: Some(EvalRubric {
                        required_answer_terms: vec!["direct".to_string()],
                        forbidden_answer_terms: vec!["direct".to_string()],
                        expected_memories: vec![ExpectedMemory {
                            id: "pref".to_string(),
                            probe: Some(Request {
                                command: "SEARCH CONCEPT \"direct\" MODE \"semantic\" LIMIT 1"
                                    .to_string(),
                                readonly: false,
                                ..Default::default()
                            }),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };
        let profile = EvalProfile {
            id: Some("bad".to_string()),
            maintenance_every_n_turns: Some(0),
            ..Default::default()
        };

        let report = validate_eval_plan(&[scenario], &[profile]);

        assert!(!report.passed);
        assert_eq!(report.planned_runs, 1);
        assert_eq!(report.scenarios[0].normal_turns, 1);
        assert_eq!(report.scenarios[0].checkpoint_turns, 1);
        assert_eq!(report.scenarios[0].expected_memories, 1);
        assert_eq!(report.scenarios[0].probes, 1);
        assert_eq!(report.profiles[0].id, "bad");
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error && issue.message.contains("normal turn")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error && issue.message.contains("readonly")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error
                && issue.message.contains("maintenance_every_n_turns")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Warning
                && issue.message.contains("both required and forbidden")
        }));
    }

    #[test]
    fn validate_eval_plan_reports_duplicate_ids_and_warning_only_cases() {
        let no_checkpoint = EvalScenario {
            id: "duplicate".to_string(),
            timeline: vec![EvalTurn {
                turn: 1,
                turn_type: EvalTurnType::Normal,
                user: Some("Remember this setup note.".to_string()),
                ..empty_turn()
            }],
            ..empty_scenario()
        };
        let invalid_checkpoint = EvalScenario {
            id: "duplicate".to_string(),
            timeline: vec![
                EvalTurn {
                    turn: 2,
                    turn_type: EvalTurnType::CheckpointSynthetic,
                    query: Some(String::new()),
                    evaluation: Some(EvalRubric {
                        expected_memories: vec![
                            ExpectedMemory {
                                id: "memory".to_string(),
                                weight: 0.0,
                                ..Default::default()
                            },
                            ExpectedMemory {
                                id: "memory".to_string(),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 1,
                    turn_type: EvalTurnType::CheckpointSynthetic,
                    query: Some("Out of order?".to_string()),
                    evaluation: Some(EvalRubric::default()),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };
        let profile_a = EvalProfile {
            id: Some("same".to_string()),
            wait_timeout_ms: 0,
            poll_interval_ms: 0,
            ..Default::default()
        };
        let profile_b = EvalProfile {
            id: Some("same".to_string()),
            poll_interval_ms: 1_000,
            wait_timeout_ms: 10,
            max_checkpoint_latency_ms: Some(0),
            max_checkpoint_total_tokens: Some(0),
            ..Default::default()
        };

        let report = validate_eval_plan(
            &[no_checkpoint, invalid_checkpoint],
            &[profile_a, profile_b],
        );

        assert!(!report.passed);
        assert_eq!(report.planned_runs, 4);
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error
                && issue.message.contains("duplicate scenario id")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error
                && issue.message.contains("duplicate profile id")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error
                && issue.message.contains("non-empty `query`")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error
                && issue.message.contains("positive finite")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Error
                && issue.message.contains("duplicate expected memory id")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Warning
                && issue.message.contains("no checkpoint")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Warning
                && issue.message.contains("lower than previous")
        }));
        assert!(report.issues.iter().any(|issue| {
            issue.severity == EvalValidationSeverity::Warning
                && issue.message.contains("greater than `wait_timeout_ms`")
        }));
    }

    #[tokio::test]
    async fn checkpoint_samples_report_mean_stddev_and_majority_findings() {
        let driver = FakeEvalDriver {
            recall_answers: vec![
                "I will keep it concise.".to_string(),
                "Something vague.".to_string(),
                "I will keep it concise.".to_string(),
            ],
            ..Default::default()
        };
        driver.probes.lock().unwrap().insert(
            "find_style".to_string(),
            Response::ok(json!([{"name": "concise style"}])),
        );
        let scenario = EvalScenario {
            id: "sampling".to_string(),
            timeline: vec![EvalTurn {
                turn: 1,
                turn_type: EvalTurnType::CheckpointSynthetic,
                query: Some("Rewrite this like me?".to_string()),
                evaluation: Some(EvalRubric {
                    required_answer_terms: vec!["concise".to_string()],
                    expected_memories: vec![ExpectedMemory {
                        id: "style".to_string(),
                        probe: Some(Request {
                            command: "find_style".to_string(),
                            readonly: true,
                            ..Default::default()
                        }),
                        answer_terms: vec!["concise".to_string()],
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..empty_turn()
            }],
            ..empty_scenario()
        };
        let profile = EvalProfile {
            checkpoint_samples: 3,
            ..Default::default()
        };

        let report = run_scenario(&driver, &scenario, &profile).await.unwrap();

        let turn = &report.turns[0];
        assert_eq!(turn.samples.len(), 3);
        assert!(turn.score_stddev.unwrap() > 0.0);
        // The vague sample's findings appear in only 1/3 samples: dropped.
        assert!(turn.findings.is_empty());
        assert_eq!(report.attribution.total_findings(), 0);
        assert!(report.total_stddev.unwrap() > 0.0);
        assert!(turn.satisfaction.is_some());
        // Usage is the true cost of all samples.
        assert_eq!(turn.usage.input_tokens, 180);
    }

    #[test]
    fn noise_expansion_is_deterministic_and_marks_turns() {
        let scenario = EvalScenario {
            id: "noisy".to_string(),
            noise: Some(NoiseConfig {
                between_turns: 2,
                corpus: Vec::new(),
                seed: 7,
            }),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    user: Some("anchor one".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 2,
                    user: Some("anchor two".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 3,
                    user: Some("anchor three".to_string()),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };

        let first = effective_timeline(&scenario);
        let second = effective_timeline(&scenario);

        assert_eq!(first.len(), 3 + 2 * 2);
        assert_eq!(first.iter().filter(|turn| turn.noise).count(), 4);
        // Anchors keep order and content.
        let anchors: Vec<&str> = first
            .iter()
            .filter(|turn| !turn.noise)
            .map(|turn| turn.user.as_deref().unwrap())
            .collect();
        assert_eq!(anchors, vec!["anchor one", "anchor two", "anchor three"]);
        // Determinism: same seed, same expansion.
        let texts = |timeline: &[EvalTurn]| -> Vec<String> {
            timeline
                .iter()
                .map(|turn| turn.user.clone().unwrap_or_default())
                .collect()
        };
        assert_eq!(texts(&first), texts(&second));
    }

    #[tokio::test]
    async fn simulated_turn_uses_completion_and_forms_memory() {
        let driver = FakeEvalDriver {
            completions: Mutex::new(vec![(
                "simulate a real user".to_string(),
                "I switched to green tea recently.".to_string(),
            )]),
            ..Default::default()
        };
        let scenario = EvalScenario {
            id: "simulated".to_string(),
            hidden_profile: json!({"drink": "green tea"}),
            timeline: vec![EvalTurn {
                turn: 1,
                turn_type: EvalTurnType::Simulated,
                intent: Some("mention your new drink preference".to_string()),
                ..empty_turn()
            }],
            ..empty_scenario()
        };

        let report = run_scenario(&driver, &scenario, &EvalProfile::default())
            .await
            .unwrap();

        assert_eq!(report.turns[0].turn_type, EvalTurnTypeReport::Simulated);
        assert_eq!(
            report.turns[0].simulated_message.as_deref(),
            Some("I switched to green tea recently.")
        );
        let remembered = driver.remembered.lock().unwrap();
        assert_eq!(remembered.len(), 1);
        let encoded = serde_json::to_string(&remembered[0].messages).unwrap();
        assert!(encoded.contains("green tea"));
    }

    #[tokio::test]
    async fn llm_judge_scores_paraphrase_and_meta_reference_correctly() {
        // The answer paraphrases (no literal "concise") and meta-references
        // the forbidden term. Lexically this would be a double failure; the
        // judge scores it as correct behavior.
        let verdict = json!({
            "memory_utility": 1.0,
            "forgetting_quality": 1.0,
            "uncertainty_calibration": 0.9,
            "satisfaction": 0.95,
            "reasoning": "paraphrased the style; meta-reference is correct",
            "findings": []
        });
        let driver = FakeEvalDriver {
            recall_answer:
                "Unlike your old BBQ preference, I'd suggest vegetarian places; I'll keep it brief and to the point."
                    .to_string(),
            completions: Mutex::new(vec![(
                "strict evaluator".to_string(),
                verdict.to_string(),
            )]),
            ..Default::default()
        };
        driver.probes.lock().unwrap().insert(
            "find_style".to_string(),
            Response::ok(json!([{"name": "concise style"}])),
        );
        let scenario = EvalScenario {
            id: "judge".to_string(),
            timeline: vec![EvalTurn {
                turn: 1,
                turn_type: EvalTurnType::CheckpointOrganic,
                query: Some("Where should we eat?".to_string()),
                evaluation: Some(EvalRubric {
                    scoring_rubric: Some("honor the vegetarian preference".to_string()),
                    required_answer_terms: vec!["concise".to_string()],
                    forbidden_answer_terms: vec!["BBQ".to_string()],
                    expected_memories: vec![ExpectedMemory {
                        id: "style".to_string(),
                        probe: Some(Request {
                            command: "find_style".to_string(),
                            readonly: true,
                            ..Default::default()
                        }),
                        answer_terms: vec!["concise".to_string()],
                        ..Default::default()
                    }],
                }),
                ..empty_turn()
            }],
            ..empty_scenario()
        };
        let profile = EvalProfile {
            judge: EvalJudgeKind::Llm,
            ..Default::default()
        };

        let report = run_scenario(&driver, &scenario, &profile).await.unwrap();

        let turn = &report.turns[0];
        // No missing-term or forbidden-term findings under the judge.
        assert!(
            turn.findings.is_empty(),
            "unexpected findings: {:?}",
            turn.findings
        );
        let score = turn.score.as_ref().unwrap();
        // 0.7 * judge utility (1.0) + 0.3 * probe presence (1.0)
        assert!(score.memory_utility > 0.99);
        assert_eq!(turn.satisfaction, Some(0.95));
        assert!(turn.judge_reasoning.is_some());
        assert_eq!(turn.usage.input_tokens, 70);
        assert_eq!(turn.usage.output_tokens, 45);
    }

    #[tokio::test]
    async fn semantic_assertion_probe_uses_judge_verdict() {
        let driver = FakeEvalDriver {
            recall_answer: "Vegetarian spots it is.".to_string(),
            completions: Mutex::new(vec![
                (
                    "inspecting a knowledge graph".to_string(),
                    json!({"holds": false, "reason": "BBQ preference is superseded"}).to_string(),
                ),
                (
                    "strict evaluator".to_string(),
                    json!({
                        "memory_utility": 0.8,
                        "forgetting_quality": 1.0,
                        "uncertainty_calibration": 1.0,
                        "satisfaction": 0.9,
                        "reasoning": "ok",
                        "findings": []
                    })
                    .to_string(),
                ),
            ]),
            ..Default::default()
        };
        let scenario = EvalScenario {
            id: "assertion".to_string(),
            timeline: vec![EvalTurn {
                turn: 1,
                turn_type: EvalTurnType::CheckpointSynthetic,
                query: Some("Dinner suggestion?".to_string()),
                evaluation: Some(EvalRubric {
                    expected_memories: vec![ExpectedMemory {
                        id: "stale_bbq".to_string(),
                        mode: MemoryExpectationMode::ShouldNotExist,
                        assertion: Some(
                            "an active, non-superseded BBQ preference for user_042".to_string(),
                        ),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..empty_turn()
            }],
            ..empty_scenario()
        };
        let profile = EvalProfile {
            judge: EvalJudgeKind::Llm,
            ..Default::default()
        };

        let report = run_scenario(&driver, &scenario, &profile).await.unwrap();

        let probe = &report.turns[0].probes[0];
        assert!(probe.satisfied, "judge said it does not hold => satisfied");
        assert!(probe.assertion.is_some());
        assert_eq!(
            probe.judge_reason.as_deref(),
            Some("BBQ preference is superseded")
        );
        assert_eq!(report.attribution.bad_consolidation, 0);
        let score = report.turns[0].score.as_ref().unwrap();
        assert!(score.forgetting_quality > 0.99);
        assert_eq!(report.turns[0].usage.input_tokens, 80);
        assert_eq!(report.turns[0].usage.output_tokens, 50);
    }

    #[test]
    fn aggregate_scores_weights_late_checkpoints_and_measures_trajectory() {
        let make_turn = |turn: u64, total: f64| EvalTurnReport {
            turn,
            turn_type: EvalTurnTypeReport::Checkpoint,
            score: Some(EvalScore {
                total,
                memory_utility: total,
                ..Default::default()
            }),
            ..Default::default()
        };

        // Improving trajectory: early 0.4, late 0.8.
        let turns = vec![make_turn(1, 0.4), make_turn(2, 0.8)];
        let (score, stddev) = aggregate_scores(&turns);
        assert!(stddev.is_none());
        // Time weights 1 and 2: (0.4 + 1.6) / 3
        assert!((score.total - 2.0 / 3.0).abs() < 1e-9);
        assert!((score.evolution_quality - 0.7).abs() < 1e-9);

        // Regressing trajectory scores below 0.5.
        let turns = vec![make_turn(1, 0.8), make_turn(2, 0.4)];
        let (score, _) = aggregate_scores(&turns);
        assert!((score.evolution_quality - 0.3).abs() < 1e-9);

        // Early failure is cheaper than late failure.
        let early_fail = aggregate_scores(&[make_turn(1, 0.0), make_turn(2, 1.0)])
            .0
            .total;
        let late_fail = aggregate_scores(&[make_turn(1, 1.0), make_turn(2, 0.0)])
            .0
            .total;
        assert!(early_fail > late_fail);
    }

    #[test]
    fn propagate_stddev_combines_child_variances() {
        assert_eq!(propagate_stddev([None, None].into_iter()), None);
        let combined = propagate_stddev([Some(0.1), Some(0.2), None].into_iter()).unwrap();
        assert!((combined - (0.01f64 + 0.04).sqrt() / 3.0).abs() < 1e-12);
    }

    #[test]
    fn shared_formation_issues_flags_user_turns_after_checkpoint() {
        let ok = EvalScenario {
            id: "ok".to_string(),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    user: Some("hello".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 2,
                    turn_type: EvalTurnType::Maintenance,
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 3,
                    turn_type: EvalTurnType::CheckpointOrganic,
                    query: Some("q".to_string()),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };
        assert!(shared_formation_issues(std::slice::from_ref(&ok)).is_empty());

        let bad = EvalScenario {
            id: "bad".to_string(),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    turn_type: EvalTurnType::CheckpointOrganic,
                    query: Some("q".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 2,
                    user: Some("late fact".to_string()),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };
        let issues = shared_formation_issues(&[ok, bad]);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("shared-formation"));

        let noisy_after_checkpoint = EvalScenario {
            id: "noisy_after_checkpoint".to_string(),
            noise: Some(NoiseConfig {
                between_turns: 1,
                corpus: Vec::new(),
                seed: 1,
            }),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    turn_type: EvalTurnType::CheckpointOrganic,
                    query: Some("q".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 2,
                    turn_type: EvalTurnType::Maintenance,
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };
        let issues = shared_formation_issues(&[noisy_after_checkpoint]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "scenarios[0].noise");
        assert!(issues[0].message.contains("noise user turns"));
    }

    #[tokio::test]
    async fn formation_and_policy_phases_split_the_timeline() {
        let scenario = EvalScenario {
            id: "phased".to_string(),
            timeline: vec![
                EvalTurn {
                    turn: 1,
                    user: Some("I love climbing.".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 2,
                    user: Some("My budget is 300.".to_string()),
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 3,
                    turn_type: EvalTurnType::Maintenance,
                    ..empty_turn()
                },
                EvalTurn {
                    turn: 4,
                    turn_type: EvalTurnType::CheckpointSynthetic,
                    query: Some("What's my budget?".to_string()),
                    evaluation: Some(EvalRubric {
                        required_answer_terms: vec!["300".to_string()],
                        ..Default::default()
                    }),
                    ..empty_turn()
                },
            ],
            ..empty_scenario()
        };
        let profile = EvalProfile {
            maintenance_every_n_turns: Some(2),
            ..Default::default()
        };

        // Formation phase: only user turns, no maintenance, no checkpoints.
        let formation_driver = FakeEvalDriver::default();
        let report = run_formation_phase(&formation_driver, &scenario, &profile)
            .await
            .unwrap();
        assert_eq!(formation_driver.remembered.lock().unwrap().len(), 2);
        assert_eq!(formation_driver.maintained.lock().unwrap().len(), 0);
        assert!(report.turns.iter().all(|turn| turn.score.is_none()));

        // Policy phase: no formation, but cadence + explicit maintenance and
        // the checkpoint run.
        let policy_driver = FakeEvalDriver {
            recall_answer: "Your budget is 300.".to_string(),
            ..Default::default()
        };
        let report = run_policy_phase(&policy_driver, &scenario, &profile)
            .await
            .unwrap();
        assert_eq!(policy_driver.remembered.lock().unwrap().len(), 0);
        // One auto (after 2 user turns) + one explicit.
        assert_eq!(policy_driver.maintained.lock().unwrap().len(), 2);
        let checkpoint = report
            .turns
            .iter()
            .find(|turn| turn.turn_type == EvalTurnTypeReport::Checkpoint)
            .unwrap();
        assert!(checkpoint.score.as_ref().unwrap().total > 0.9);
    }

    #[test]
    fn first_integer_digs_into_kip_count_results() {
        assert_eq!(first_integer(&json!([{"result": [{"count": 7}]}])), Some(7));
        assert_eq!(first_integer(&json!("nope")), None);
        assert_eq!(first_integer(&json!(3)), Some(3));
    }

    fn empty_turn() -> EvalTurn {
        EvalTurn {
            turn: 0,
            turn_type: EvalTurnType::Normal,
            timestamp: None,
            context: None,
            user: None,
            messages: Vec::new(),
            query: None,
            intent: None,
            evaluation: None,
            maintenance: None,
            noise: false,
        }
    }

    fn empty_scenario() -> EvalScenario {
        EvalScenario {
            id: String::new(),
            description: None,
            hidden_profile: Json::Null,
            default_context: None,
            noise: None,
            timeline: Vec::new(),
        }
    }
}
