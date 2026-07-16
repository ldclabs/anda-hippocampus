//! Evolutionary prompt optimization: the eval suite as a fitness function.
//!
//! The three agent system prompts are the evolvable genome. Each generation:
//! attributed failures from the last suite run are fed to an optimizer LLM,
//! which proposes surgical find/replace edits to the responsible prompt; the
//! candidate is installed via `agents::prompts::set_override`, the suite is
//! re-run, and the edit is kept only when fitness improves beyond the noise
//! band measured by `checkpoint_samples`. Everything is offline and
//! human-reviewable: the report carries every edit, score, and decision.

use anda_core::{BoxError, CompletionRequest, ModelEffort};
use serde::{Deserialize, Serialize};

use futures::future::BoxFuture;

use super::{AttributionSummary, EvalSuiteReport};
use crate::agents::prompts::{self, PromptTarget};
use crate::assess::{AssessContext, parse_json_payload, truncate_chars};
use crate::types::MemoryPolicy;

const MAX_FAILURE_SUMMARY_CHARS: usize = 8_000;

/// One surgical edit proposed by the optimizer.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptEdit {
    pub target: PromptTarget,
    pub find: String,
    pub replace: String,

    #[serde(default)]
    pub rationale: String,
}

/// Applies a find/replace edit; the needle must occur exactly once so edits
/// stay reviewable and cannot silently rewrite unrelated sections.
pub fn apply_edit(text: &str, find: &str, replace: &str) -> Result<String, BoxError> {
    if find.trim().is_empty() {
        return Err("edit `find` must not be empty".into());
    }
    let occurrences = text.matches(find).count();
    match occurrences {
        0 => Err("edit `find` text not present in prompt".into()),
        1 => Ok(text.replacen(find, replace, 1)),
        n => Err(format!("edit `find` text occurs {n} times; it must be unique").into()),
    }
}

/// Maps aggregate failure attribution to the prompt most responsible:
/// formation misses → Formation; grounding/synthesis/overconfidence →
/// Recall; consolidation debt → Maintenance.
pub fn pick_target(attribution: &AttributionSummary) -> PromptTarget {
    let formation = attribution.formation_miss;
    let recall = attribution
        .bad_grounding
        .saturating_add(attribution.bad_synthesis)
        .saturating_add(attribution.overconfidence);
    let maintenance = attribution.bad_consolidation;
    if formation >= recall && formation >= maintenance {
        PromptTarget::Formation
    } else if recall >= maintenance {
        PromptTarget::Recall
    } else {
        PromptTarget::Maintenance
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GenerationDecision {
    pub accepted: bool,
    pub reason: String,
}

/// Accepts a candidate only when it beats the baseline beyond the combined
/// noise band, so LLM variance cannot masquerade as progress.
pub fn decide(
    baseline_total: f64,
    baseline_stddev: Option<f64>,
    candidate_total: f64,
    candidate_stddev: Option<f64>,
    confidence_z: f64,
    min_delta: f64,
) -> GenerationDecision {
    let base_var = baseline_stddev.map(|s| s * s).unwrap_or_default();
    let cand_var = candidate_stddev.map(|s| s * s).unwrap_or_default();
    let noise_band = confidence_z * (base_var + cand_var).sqrt();
    let required = noise_band.max(min_delta);
    let delta = candidate_total - baseline_total;
    if delta > required {
        GenerationDecision {
            accepted: true,
            reason: format!("candidate improved total by {delta:.4} (> required {required:.4})"),
        }
    } else {
        GenerationDecision {
            accepted: false,
            reason: format!(
                "candidate delta {delta:.4} did not clear the noise band {required:.4}"
            ),
        }
    }
}

/// Compact, bounded description of what went wrong, for the optimizer LLM.
pub fn summarize_failures(suite: &EvalSuiteReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "suite total={:.4} attribution={}\n",
        suite.score.total,
        serde_json::to_string(&suite.attribution).unwrap_or_default()
    ));
    for report in &suite.reports {
        for turn in &report.turns {
            if turn.findings.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "\nscenario={} turn={} score={:.4}\n",
                report.scenario_id,
                turn.turn,
                turn.score.as_ref().map(|score| score.total).unwrap_or(0.0)
            ));
            if let Some(answer) = &turn.answer {
                out.push_str(&format!("answer: {}\n", truncate_chars(answer, 400)));
            }
            if let Some(reasoning) = &turn.judge_reasoning {
                out.push_str(&format!("judge: {}\n", truncate_chars(reasoning, 400)));
            }
            for finding in &turn.findings {
                out.push_str(&format!(
                    "- {}: {}\n",
                    serde_json::to_string(&finding.kind)
                        .unwrap_or_default()
                        .trim_matches('"'),
                    finding.message
                ));
            }
        }
    }
    truncate_chars(&out, MAX_FAILURE_SUMMARY_CHARS)
}

const OPTIMIZER_INSTRUCTIONS: &str = r#"You are optimizing the system prompt of one agent inside an AI memory system (Formation encodes conversations into a knowledge graph, Recall answers queries from it, Maintenance consolidates and prunes it). You will receive the full current prompt of the TARGET agent and a summary of attributed eval failures.

Propose 1 to 3 minimal, surgical edits to the TARGET prompt that address the observed failure modes. Rules:
- `find` must be an EXACT substring copied verbatim from the current prompt, unique within it, and at most ~15 lines.
- `replace` is the full replacement for that substring.
- Preserve the prompt's markdown structure, tone, and existing guarantees; do not delete safety rules.
- Prefer sharpening instructions over adding length.

Respond with ONLY a JSON object:
{"edits": [{"target": "formation|recall|maintenance", "find": "...", "replace": "...", "rationale": "..."}]}
If no edit is likely to help, respond {"edits": []}."#;

#[derive(Debug, Default, Deserialize)]
struct ProposedEdits {
    #[serde(default)]
    edits: Vec<PromptEdit>,
}

/// Asks the optimizer LLM for targeted edits to one prompt.
pub async fn propose_edits<C>(
    driver: &C,
    target: PromptTarget,
    current_prompt: &str,
    failure_summary: &str,
) -> Result<Vec<PromptEdit>, BoxError>
where
    C: AssessContext + ?Sized,
{
    let prompt = format!(
        "# Target agent\n{}\n\n# Attributed eval failures\n{}\n\n# Current prompt of the target agent\n{}",
        target.as_str(),
        failure_summary,
        current_prompt,
    );
    let output = driver
        .complete(CompletionRequest {
            instructions: OPTIMIZER_INSTRUCTIONS.to_string(),
            prompt,
            effort: Some(ModelEffort::High),
            ..Default::default()
        })
        .await?;
    let proposed: ProposedEdits = parse_json_payload(&output.content)?;
    // Edits for other prompts than the target are optimizer confusion; drop.
    Ok(proposed
        .edits
        .into_iter()
        .filter(|edit| edit.target == target)
        .collect())
}

/// Which genome the optimizer evolves (plan M10): agent prompt text, or the
/// numeric `MemoryPolicy` knobs. Policy mutations are cheaper to evaluate
/// and safer to apply than prompt edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GenomeKind {
    #[default]
    Prompt,
    Policy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OptimizeConfig {
    /// Number of propose→evaluate→select generations.
    pub generations: usize,

    /// Which genome to evolve (default: prompt).
    #[serde(default)]
    pub genome: GenomeKind,

    /// Fixed target prompt (prompt genome only); `None` picks per
    /// generation from attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PromptTarget>,

    /// Z multiplier for the noise band (default 1.0).
    #[serde(default = "default_confidence_z")]
    pub confidence_z: f64,

    /// Minimum absolute improvement even when no variance is measured.
    #[serde(default = "default_min_delta")]
    pub min_delta: f64,

    /// Max allowed holdout regression when train accepts (plan M9): a
    /// candidate that improves train but drops the holdout total more than
    /// this below the holdout baseline is rejected as overfitting.
    #[serde(default = "default_holdout_epsilon")]
    pub holdout_epsilon: f64,
}

// `Default` must agree with the serde field defaults: a config built with
// `..Default::default()` (the CLI path) gets the same noise-band floors as
// one deserialized from JSON.
impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            generations: 0,
            genome: GenomeKind::default(),
            target: None,
            confidence_z: default_confidence_z(),
            min_delta: default_min_delta(),
            holdout_epsilon: default_holdout_epsilon(),
        }
    }
}

fn default_confidence_z() -> f64 {
    1.0
}

fn default_min_delta() -> f64 {
    0.005
}

fn default_holdout_epsilon() -> f64 {
    0.01
}

/// One bounded numeric mutation of the memory policy (plan M10).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyPatch {
    pub field: String,
    pub value: f64,

    #[serde(default)]
    pub rationale: String,
}

/// Policy fields the optimizer may mutate: only knobs the runtime actually
/// consumes. `version`, structural knobs, and the declared-but-unwired
/// fields (`recall_reinforcement`, `correction_penalty`,
/// `recall_search_threshold`, `recall_max_rounds`) stay out of the genome —
/// mutating a knob nothing reads measures pure sampling noise, which the
/// accept gate can mistake for an improvement. Wire a field into the
/// runtime first, then add it here.
pub const POLICY_PATCH_FIELDS: &[&str] = &[
    "confidence_decay_factor",
    "decay_floor",
    "stale_event_threshold_days",
    "unsorted_max_backlog",
    "orphan_max_count",
    "self_test_queries_per_cycle",
];

/// Mutation bound for fractional fields: at most ±50% of the current value
/// (±0.05 absolute floor for values near zero), keeping evolution gradual
/// and reviewable.
fn bounded_f64(field: &str, old: f64, new: f64) -> Result<f64, BoxError> {
    if !new.is_finite() {
        return Err(format!("`{field}` mutation must be a finite number").into());
    }
    let delta = (new - old).abs();
    if delta > (old.abs() * 0.5).max(0.05) {
        return Err(
            format!("`{field}` mutation from {old} to {new} exceeds the ±50% step bound").into(),
        );
    }
    Ok(new)
}

/// Mutation bound for integer fields: at most ±50% (±1 absolute floor so
/// zero-valued knobs can still move).
fn bounded_u32(field: &str, old: u32, new: f64) -> Result<u32, BoxError> {
    if !new.is_finite() || new < 0.0 {
        return Err(format!("`{field}` mutation must be a non-negative number").into());
    }
    let rounded = new.round();
    let delta = (rounded - f64::from(old)).abs();
    if delta > (f64::from(old) * 0.5).max(1.0) {
        return Err(format!(
            "`{field}` mutation from {old} to {rounded} exceeds the ±50% step bound"
        )
        .into());
    }
    Ok(rounded as u32)
}

/// Applies one patch with the step bound and full policy validation; a bad
/// patch can never install a bad policy.
pub fn apply_policy_patch(
    policy: &MemoryPolicy,
    patch: &PolicyPatch,
) -> Result<MemoryPolicy, BoxError> {
    let mut next = policy.clone();
    let field = patch.field.as_str();
    match field {
        "confidence_decay_factor" => {
            next.confidence_decay_factor =
                bounded_f64(field, policy.confidence_decay_factor, patch.value)?;
        }
        "decay_floor" => {
            next.decay_floor = bounded_f64(field, policy.decay_floor, patch.value)?;
        }
        "stale_event_threshold_days" => {
            next.stale_event_threshold_days =
                bounded_u32(field, policy.stale_event_threshold_days, patch.value)?;
        }
        "unsorted_max_backlog" => {
            next.unsorted_max_backlog =
                bounded_u32(field, policy.unsorted_max_backlog, patch.value)?;
        }
        "orphan_max_count" => {
            next.orphan_max_count = bounded_u32(field, policy.orphan_max_count, patch.value)?;
        }
        "self_test_queries_per_cycle" => {
            next.self_test_queries_per_cycle =
                bounded_u32(field, policy.self_test_queries_per_cycle, patch.value)?;
        }
        other => {
            return Err(format!(
                "`{other}` is not a tunable policy field; expected one of {POLICY_PATCH_FIELDS:?}"
            )
            .into());
        }
    }
    next.validate()?;
    Ok(next)
}

const POLICY_OPTIMIZER_INSTRUCTIONS: &str = r#"You tune the numeric memory-policy knobs of an AI memory system (decay, reinforcement, backlog targets, self-test and recall budgets). You will receive the current policy as JSON, the list of tunable fields, and a summary of attributed eval failures.

Propose 1 to 3 minimal mutations that address the observed failure modes. Rules:
- `field` must be one of the tunable fields.
- `value` is the new numeric value; it must stay within ±50% of the current value and within each field's documented range.
- Prefer one decisive knob over many timid ones; explain the causal link in `rationale`.

Respond with ONLY a JSON object:
{"patches": [{"field": "confidence_decay_factor", "value": 0.9, "rationale": "..."}]}
If no mutation is likely to help, respond {"patches": []}."#;

#[derive(Debug, Default, Deserialize)]
struct ProposedPatches {
    #[serde(default)]
    patches: Vec<PolicyPatch>,
}

/// Asks the optimizer LLM for bounded policy mutations.
pub async fn propose_policy_patches<C>(
    proposer: &C,
    current: &MemoryPolicy,
    failure_summary: &str,
) -> Result<Vec<PolicyPatch>, BoxError>
where
    C: AssessContext + ?Sized,
{
    let prompt = format!(
        "# Current memory policy\n{}\n\n# Tunable fields\n{POLICY_PATCH_FIELDS:?}\n\n# Attributed eval failures\n{failure_summary}",
        serde_json::to_string_pretty(current).unwrap_or_default(),
    );
    let output = proposer
        .complete(CompletionRequest {
            instructions: POLICY_OPTIMIZER_INSTRUCTIONS.to_string(),
            prompt,
            effort: Some(ModelEffort::High),
            ..Default::default()
        })
        .await?;
    let proposed: ProposedPatches = parse_json_payload(&output.content)?;
    Ok(proposed.patches)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GenerationReport {
    pub generation: usize,

    /// Prompt-genome generations only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PromptTarget>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<PromptEdit>,

    /// Policy-genome generations only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<PolicyPatch>,

    pub baseline_total: f64,
    pub candidate_total: Option<f64>,

    /// Holdout suite total, when a holdout gate ran for this generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holdout_total: Option<f64>,

    pub decision: GenerationDecision,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OptimizeReport {
    pub baseline_total: f64,
    pub final_total: f64,
    pub accepted_generations: usize,
    pub generations: Vec<GenerationReport>,

    /// Final accepted prompt text per target, only for targets that changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_prompts: Vec<AcceptedPrompt>,

    /// Final accepted memory policy (policy genome only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_policy: Option<MemoryPolicy>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcceptedPrompt {
    pub target: PromptTarget,
    pub text: String,
}

/// A boxed fitness function for the optional holdout suite.
pub type BoxedFitness =
    Box<dyn FnMut(usize) -> BoxFuture<'static, Result<EvalSuiteReport, BoxError>> + Send>;

/// Warns when harness-error findings (judge/probe failures the suite runner
/// degraded to findings) swamp a suite: the environment, not the genome, is
/// being measured, so accept/reject decisions against it are suspect.
fn warn_if_degraded(stage: &str, suite: &EvalSuiteReport) {
    let turns: usize = suite.reports.iter().map(|report| report.turns.len()).sum();
    if turns == 0 {
        log::warn!(
            target: "eval",
            "{stage} suite {:?} produced no turns to score",
            suite.suite_id
        );
        return;
    }
    let errors = suite
        .attribution
        .judge_error
        .saturating_add(suite.attribution.graph_probe_error);
    let share = errors as f64 / turns as f64;
    if share > 0.25 {
        log::warn!(
            target: "eval",
            "{stage} suite {:?} looks harness-degraded: {errors} judge/probe error findings across {turns} turns ({:.0}%)",
            suite.suite_id,
            share * 100.0,
        );
    }
}

/// The candidate genome one generation proposes.
enum Candidate {
    Prompt { target: PromptTarget, text: String },
    Policy { policy: MemoryPolicy },
}

/// The generation loop. `fitness(generation)` must run the train eval suite
/// against whatever genome overrides are currently installed and return its
/// report; generation 0 is the baseline. Candidates install through
/// `agents::prompts::set_override` (prompt genome) or
/// `MemoryPolicy::set_eval_override` (policy genome); a rejected candidate
/// restores the last accepted genome (or clears the override back to the
/// compiled defaults when nothing was accepted yet). When `holdout` is
/// given, a train win must also keep the held-out suite within
/// `holdout_epsilon` of its baseline — the anti-overfitting gate (plan M9).
///
/// Error handling: a proposal or parse failure only costs its own
/// generation — it is recorded as rejected and the loop continues. A failed
/// fitness or holdout evaluation aborts the run with the error.
pub async fn run_optimize<C, F, Fut>(
    proposer: &C,
    config: &OptimizeConfig,
    mut fitness: F,
    mut holdout: Option<BoxedFitness>,
) -> Result<OptimizeReport, BoxError>
where
    C: AssessContext + ?Sized,
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<EvalSuiteReport, BoxError>>,
{
    // Drop-time clear: an early return or panic anywhere below must not leak
    // a candidate policy into later evals through the process-wide override.
    let _policy_override_guard = matches!(config.genome, GenomeKind::Policy)
        .then(crate::types::EvalPolicyOverrideGuard::arm);

    let mut baseline = fitness(0).await?;
    warn_if_degraded("baseline", &baseline);
    let baseline_total = baseline.score.total;
    if baseline.total_stddev.is_none() {
        // Without variance data the noise band collapses to `min_delta`
        // alone, and ordinary LLM run-to-run variance can be "accepted" as
        // an improvement — especially poisonous for numeric policy patches.
        log::warn!(
            target: "eval",
            "optimizer running without variance data (checkpoint_samples=1): \
             the accept gate is only min_delta={:.4}; use --checkpoint-samples > 1",
            config.min_delta
        );
    }
    let mut report = OptimizeReport {
        baseline_total,
        final_total: baseline_total,
        ..Default::default()
    };
    // Track locally-accepted genomes so rejection restores the last good state.
    let mut accepted_prompts: Vec<(PromptTarget, String)> = Vec::new();
    let mut accepted_policy: Option<MemoryPolicy> = None;

    let mut holdout_baseline = match holdout.as_mut() {
        Some(holdout) => {
            let suite = holdout(0).await?;
            warn_if_degraded("holdout baseline", &suite);
            Some(suite.score.total)
        }
        None => None,
    };

    for generation in 1..=config.generations {
        let failure_summary = summarize_failures(&baseline);
        let mut record = GenerationReport {
            generation,
            baseline_total: baseline.score.total,
            ..Default::default()
        };

        // Propose and build the candidate genome. A proposal or parse
        // failure costs nothing that must be preserved, so it is recorded as
        // this generation's rejection and the loop continues.
        let candidate = match config.genome {
            GenomeKind::Prompt => {
                let target = config
                    .target
                    .unwrap_or_else(|| pick_target(&baseline.attribution));
                record.target = Some(target);
                let current_prompt = prompts::active_prompt(target);
                let edits = match propose_edits(proposer, target, &current_prompt, &failure_summary)
                    .await
                {
                    Ok(edits) => edits,
                    Err(err) => {
                        record.decision = GenerationDecision {
                            accepted: false,
                            reason: format!("optimizer proposal failed: {err}"),
                        };
                        report.generations.push(record);
                        continue;
                    }
                };
                if edits.is_empty() {
                    record.decision = GenerationDecision {
                        accepted: false,
                        reason: "optimizer proposed no edits".to_string(),
                    };
                    report.generations.push(record);
                    continue;
                }
                record.edits = edits.clone();
                let mut text = current_prompt.to_string();
                let mut apply_error = None;
                for edit in &edits {
                    match apply_edit(&text, &edit.find, &edit.replace) {
                        Ok(next) => text = next,
                        Err(err) => {
                            apply_error = Some(err.to_string());
                            break;
                        }
                    }
                }
                if let Some(err) = apply_error {
                    record.decision = GenerationDecision {
                        accepted: false,
                        reason: format!("edit could not be applied: {err}"),
                    };
                    report.generations.push(record);
                    continue;
                }
                Candidate::Prompt { target, text }
            }
            GenomeKind::Policy => {
                let current = accepted_policy.clone().unwrap_or_default();
                let patches =
                    match propose_policy_patches(proposer, &current, &failure_summary).await {
                        Ok(patches) => patches,
                        Err(err) => {
                            record.decision = GenerationDecision {
                                accepted: false,
                                reason: format!("optimizer proposal failed: {err}"),
                            };
                            report.generations.push(record);
                            continue;
                        }
                    };
                if patches.is_empty() {
                    record.decision = GenerationDecision {
                        accepted: false,
                        reason: "optimizer proposed no patches".to_string(),
                    };
                    report.generations.push(record);
                    continue;
                }
                record.patches = patches.clone();
                let mut policy = current;
                let mut apply_error = None;
                let mut touched_fields = std::collections::BTreeSet::new();
                for patch in &patches {
                    // One patch per field per generation: chaining patches on
                    // the same field would compound past the ±50% step bound
                    // (three patches ≈ ±2.25×).
                    if !touched_fields.insert(patch.field.clone()) {
                        apply_error = Some(format!("duplicate patch for field `{}`", patch.field));
                        break;
                    }
                    match apply_policy_patch(&policy, patch) {
                        Ok(next) => policy = next,
                        Err(err) => {
                            apply_error = Some(err.to_string());
                            break;
                        }
                    }
                }
                if let Some(err) = apply_error {
                    record.decision = GenerationDecision {
                        accepted: false,
                        reason: format!("patch could not be applied: {err}"),
                    };
                    report.generations.push(record);
                    continue;
                }
                Candidate::Policy { policy }
            }
        };

        // Install the candidate genome.
        match &candidate {
            Candidate::Prompt { target, text } => {
                prompts::set_override(*target, Some(text.clone()));
            }
            Candidate::Policy { policy } => {
                MemoryPolicy::set_eval_override(Some(policy.clone()));
            }
        }

        let candidate_suite = fitness(generation).await?;
        warn_if_degraded("candidate", &candidate_suite);
        record.candidate_total = Some(candidate_suite.score.total);
        let mut decision = decide(
            baseline.score.total,
            baseline.total_stddev,
            candidate_suite.score.total,
            candidate_suite.total_stddev,
            config.confidence_z,
            config.min_delta,
        );

        // Holdout gate: a train win that regresses held-out scenarios is
        // overfitting, not progress.
        if decision.accepted
            && let Some(holdout) = holdout.as_mut()
        {
            let hold_suite = holdout(generation).await?;
            warn_if_degraded("holdout", &hold_suite);
            let hold_total = hold_suite.score.total;
            record.holdout_total = Some(hold_total);
            let base = holdout_baseline.unwrap_or_default();
            if hold_total < base - config.holdout_epsilon {
                decision = GenerationDecision {
                    accepted: false,
                    reason: format!(
                        "train improved but holdout regressed: {hold_total:.4} < {:.4} (baseline {base:.4} − epsilon {})",
                        base - config.holdout_epsilon,
                        config.holdout_epsilon
                    ),
                };
            } else {
                // Monotone: the bar only rises. Re-baselining downward would
                // let N accepted generations ratchet holdout down by N×ε,
                // each step individually "within epsilon".
                holdout_baseline = Some(base.max(hold_total));
            }
        }

        if decision.accepted {
            match candidate {
                Candidate::Prompt { target, text } => {
                    accepted_prompts.retain(|(kept, _)| *kept != target);
                    accepted_prompts.push((target, text));
                }
                Candidate::Policy { policy } => {
                    accepted_policy = Some(policy);
                }
            }
            report.final_total = candidate_suite.score.total;
            report.accepted_generations += 1;
            baseline = candidate_suite;
        } else {
            // Restore the last accepted genome state; `None` clears the
            // override back to the compiled defaults.
            match &candidate {
                Candidate::Prompt { target, .. } => {
                    let restore = accepted_prompts
                        .iter()
                        .find(|(kept, _)| kept == target)
                        .map(|(_, text)| text.clone());
                    prompts::set_override(*target, restore);
                }
                Candidate::Policy { .. } => {
                    MemoryPolicy::set_eval_override(accepted_policy.clone());
                }
            }
        }
        record.decision = decision;
        report.generations.push(record);
    }

    report.accepted_prompts = accepted_prompts
        .into_iter()
        .map(|(target, text)| AcceptedPrompt { target, text })
        .collect();
    report.accepted_policy = accepted_policy;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::EvalScore;
    use anda_core::AgentOutput;
    use anda_kip::{Request, Response};
    use std::sync::Mutex;

    /// Minimal assess context: only `complete` matters for the optimizer.
    #[derive(Default)]
    struct FakeProposer {
        responses: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl AssessContext for FakeProposer {
        async fn complete(
            &self,
            _req: anda_core::CompletionRequest,
        ) -> Result<AgentOutput, anda_core::BoxError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err("no canned response".into());
            }
            Ok(AgentOutput {
                content: responses.remove(0),
                ..Default::default()
            })
        }

        async fn execute_kip_readonly(
            &self,
            _request: Request,
        ) -> Result<Response, anda_core::BoxError> {
            Err("not used".into())
        }
    }

    fn suite_with_total(total: f64, stddev: Option<f64>) -> EvalSuiteReport {
        EvalSuiteReport {
            suite_id: "fitness".to_string(),
            score: EvalScore {
                total,
                ..Default::default()
            },
            total_stddev: stddev,
            attribution: AttributionSummary {
                bad_grounding: 1,
                ..Default::default()
            },
            // One healthy scored turn keeps the harness-degradation warning
            // quiet in tests.
            reports: vec![crate::eval::EvalReport {
                scenario_id: "s1".to_string(),
                turns: vec![crate::eval::EvalTurnReport::default()],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    // The guard intentionally spans awaits: it serializes tests that mutate
    // the process-wide prompt overrides, and the test runtime is single-file.
    #[allow(clippy::await_holding_lock)]
    async fn run_optimize_accepts_improvements_and_reverts_regressions() {
        let _guard = prompts::TEST_GUARD
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        prompts::clear_overrides();
        // Give the recall prompt a unique needle to edit.
        prompts::set_override(
            PromptTarget::Recall,
            Some("BASE PROMPT with UNIQUE_NEEDLE inside".to_string()),
        );

        let edit = |replace: &str| {
            serde_json::json!({
                "edits": [{
                    "target": "recall",
                    "find": "UNIQUE_NEEDLE",
                    "replace": replace,
                    "rationale": "test"
                }]
            })
            .to_string()
        };
        // Gen 1 keeps the needle so gen 2's edit still applies; gen 2 removes
        // it, which must be reverted on rejection.
        let proposer = FakeProposer {
            responses: Mutex::new(vec![edit("UNIQUE_NEEDLE IMPROVED"), edit("REGRESSED")]),
        };

        // Fitness: baseline 0.5, gen1 0.9 (accept), gen2 0.3 (reject).
        let totals = std::sync::Arc::new(Mutex::new(vec![0.5, 0.9, 0.3]));
        let totals_ref = totals.clone();
        let config = OptimizeConfig {
            generations: 2,
            target: Some(PromptTarget::Recall),
            ..Default::default()
        };

        let report = run_optimize(
            &proposer,
            &config,
            move |_generation| {
                let totals = totals_ref.clone();
                async move {
                    let total = totals.lock().unwrap().remove(0);
                    Ok(suite_with_total(total, None))
                }
            },
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.baseline_total, 0.5);
        assert_eq!(report.final_total, 0.9);
        assert_eq!(report.accepted_generations, 1);
        assert_eq!(report.generations.len(), 2);
        assert!(report.generations[0].decision.accepted);
        assert!(!report.generations[1].decision.accepted);

        // The accepted edit is installed; the regression was reverted to it.
        let active = prompts::active_prompt(PromptTarget::Recall);
        assert!(active.contains("IMPROVED"));
        assert!(!active.contains("REGRESSED"));
        assert_eq!(report.accepted_prompts.len(), 1);
        assert!(report.accepted_prompts[0].text.contains("IMPROVED"));

        prompts::clear_overrides();
    }

    #[test]
    fn apply_edit_requires_unique_needle() {
        assert_eq!(apply_edit("a b c", "b", "x").unwrap(), "a x c");
        assert!(apply_edit("a b b", "b", "x").is_err());
        assert!(apply_edit("a b c", "z", "x").is_err());
        assert!(apply_edit("a b c", "  ", "x").is_err());
    }

    #[test]
    fn pick_target_follows_attribution_majority() {
        assert_eq!(
            pick_target(&AttributionSummary {
                formation_miss: 3,
                bad_grounding: 1,
                ..Default::default()
            }),
            PromptTarget::Formation
        );
        assert_eq!(
            pick_target(&AttributionSummary {
                bad_grounding: 2,
                bad_synthesis: 2,
                bad_consolidation: 3,
                ..Default::default()
            }),
            PromptTarget::Recall
        );
        assert_eq!(
            pick_target(&AttributionSummary {
                bad_consolidation: 5,
                overconfidence: 1,
                ..Default::default()
            }),
            PromptTarget::Maintenance
        );
    }

    #[test]
    fn decide_requires_clearing_the_noise_band() {
        // Improvement below the noise band is rejected.
        let decision = decide(0.70, Some(0.05), 0.73, Some(0.05), 1.0, 0.005);
        assert!(!decision.accepted);
        // Improvement beyond the band is accepted.
        let decision = decide(0.70, Some(0.01), 0.75, Some(0.01), 1.0, 0.005);
        assert!(decision.accepted);
        // Without variance data the min_delta floor applies.
        let decision = decide(0.70, None, 0.703, None, 1.0, 0.005);
        assert!(!decision.accepted);
        let decision = decide(0.70, None, 0.72, None, 1.0, 0.005);
        assert!(decision.accepted);
    }

    #[test]
    fn summarize_failures_is_bounded_and_mentions_findings() {
        use crate::eval::{EvalFinding, EvalFindingKind, EvalReport, EvalTurnReport};
        let suite = EvalSuiteReport {
            suite_id: "s".to_string(),
            score: EvalScore {
                total: 0.5,
                ..Default::default()
            },
            reports: vec![EvalReport {
                scenario_id: "sc".to_string(),
                turns: vec![EvalTurnReport {
                    turn: 4,
                    answer: Some("bad answer".to_string()),
                    findings: vec![EvalFinding {
                        kind: EvalFindingKind::BadGrounding,
                        expectation_id: None,
                        message: "memory not retrieved".to_string(),
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let summary = summarize_failures(&suite);
        assert!(summary.contains("bad_grounding"));
        assert!(summary.contains("memory not retrieved"));
        assert!(summary.len() <= MAX_FAILURE_SUMMARY_CHARS + 64);
    }

    #[test]
    fn apply_policy_patch_bounds_and_validates() {
        let policy = MemoryPolicy::default();

        // Bounded fractional change applies.
        let next = apply_policy_patch(
            &policy,
            &PolicyPatch {
                field: "confidence_decay_factor".to_string(),
                value: 0.9,
                rationale: String::new(),
            },
        )
        .unwrap();
        assert_eq!(next.confidence_decay_factor, 0.9);

        // Integer fields round.
        let next = apply_policy_patch(
            &policy,
            &PolicyPatch {
                field: "unsorted_max_backlog".to_string(),
                value: 25.4,
                rationale: String::new(),
            },
        )
        .unwrap();
        assert_eq!(next.unsorted_max_backlog, 25);

        // The ±50% step bound rejects jumps.
        for (field, value) in [
            ("unsorted_max_backlog", 100.0),
            ("confidence_decay_factor", 0.4),
        ] {
            let err = apply_policy_patch(
                &policy,
                &PolicyPatch {
                    field: field.to_string(),
                    value,
                    rationale: String::new(),
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains("step bound"), "{field}: {err}");
        }

        // A zero-valued integer knob can still take its first step.
        let zeroed = MemoryPolicy {
            self_test_queries_per_cycle: 0,
            ..Default::default()
        };
        let next = apply_policy_patch(
            &zeroed,
            &PolicyPatch {
                field: "self_test_queries_per_cycle".to_string(),
                value: 1.0,
                rationale: String::new(),
            },
        )
        .unwrap();
        assert_eq!(next.self_test_queries_per_cycle, 1);

        // Unknown fields are rejected: the genome is a closed set.
        assert!(
            apply_policy_patch(
                &policy,
                &PolicyPatch {
                    field: "version".to_string(),
                    value: 2.0,
                    rationale: String::new(),
                },
            )
            .is_err()
        );
    }

    #[tokio::test]
    // Serializes tests that mutate process-wide override state.
    #[allow(clippy::await_holding_lock)]
    async fn run_optimize_policy_genome_installs_and_reverts_policy() {
        let _guard = prompts::TEST_GUARD
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        MemoryPolicy::set_eval_override(None);

        let patch = |value: f64| {
            serde_json::json!({
                "patches": [{
                    "field": "confidence_decay_factor",
                    "value": value,
                    "rationale": "test"
                }]
            })
            .to_string()
        };
        // Gen 1 improves (accept 0.9); gen 2 regresses (reject 0.85 →
        // override restored to the accepted 0.9).
        let proposer = FakeProposer {
            responses: Mutex::new(vec![patch(0.9), patch(0.85)]),
        };
        let totals = std::sync::Arc::new(Mutex::new(vec![0.5, 0.9, 0.3]));
        let totals_ref = totals.clone();
        let config = OptimizeConfig {
            generations: 2,
            genome: GenomeKind::Policy,
            ..Default::default()
        };

        let report = run_optimize(
            &proposer,
            &config,
            move |_generation| {
                let totals = totals_ref.clone();
                async move {
                    let total = totals.lock().unwrap().remove(0);
                    Ok(suite_with_total(total, None))
                }
            },
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.accepted_generations, 1);
        assert_eq!(
            report
                .accepted_policy
                .as_ref()
                .unwrap()
                .confidence_decay_factor,
            0.9
        );
        assert!(report.generations[0].target.is_none());
        assert_eq!(report.generations[0].patches.len(), 1);
        assert!(!report.generations[1].decision.accepted);
        // The RAII guard cleared the process-wide override on return: the
        // accepted policy lives in the report, not in a leaked global.
        assert!(MemoryPolicy::eval_override().is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn run_optimize_holdout_gate_rejects_overfitting() {
        let _guard = prompts::TEST_GUARD
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        prompts::clear_overrides();
        prompts::set_override(
            PromptTarget::Recall,
            Some("BASE PROMPT with UNIQUE_NEEDLE inside".to_string()),
        );

        let edit = serde_json::json!({
            "edits": [{
                "target": "recall",
                "find": "UNIQUE_NEEDLE",
                "replace": "OVERFIT",
                "rationale": "test"
            }]
        })
        .to_string();
        let proposer = FakeProposer {
            responses: Mutex::new(vec![edit]),
        };
        // Train improves (0.5 → 0.9) but holdout regresses (0.6 → 0.4).
        let train_totals = std::sync::Arc::new(Mutex::new(vec![0.5, 0.9]));
        let train_ref = train_totals.clone();
        let holdout_totals = std::sync::Arc::new(Mutex::new(vec![0.6, 0.4]));
        let holdout_ref = holdout_totals.clone();
        let holdout: BoxedFitness = Box::new(move |_generation| {
            let totals = holdout_ref.clone();
            Box::pin(async move {
                let total = totals.lock().unwrap().remove(0);
                Ok(suite_with_total(total, None))
            })
        });
        let config = OptimizeConfig {
            generations: 1,
            target: Some(PromptTarget::Recall),
            ..Default::default()
        };

        let report = run_optimize(
            &proposer,
            &config,
            move |_generation| {
                let totals = train_ref.clone();
                async move {
                    let total = totals.lock().unwrap().remove(0);
                    Ok(suite_with_total(total, None))
                }
            },
            Some(holdout),
        )
        .await
        .unwrap();

        assert_eq!(report.accepted_generations, 0);
        let generation = &report.generations[0];
        assert_eq!(generation.candidate_total, Some(0.9));
        assert_eq!(generation.holdout_total, Some(0.4));
        assert!(!generation.decision.accepted);
        assert!(generation.decision.reason.contains("holdout regressed"));
        // The overfitting edit was reverted: nothing was accepted, so the
        // override is cleared and the compiled default is active again.
        let active = prompts::active_prompt(PromptTarget::Recall);
        assert!(!active.contains("OVERFIT"));

        prompts::clear_overrides();
    }

    #[tokio::test]
    // Serializes tests that mutate process-wide override state.
    #[allow(clippy::await_holding_lock)]
    async fn run_optimize_records_proposal_failure_as_rejected_generation() {
        let _guard = prompts::TEST_GUARD
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        prompts::clear_overrides();
        prompts::set_override(
            PromptTarget::Recall,
            Some("BASE PROMPT with UNIQUE_NEEDLE inside".to_string()),
        );

        // Gen 1: unparseable proposal output. Gen 2: a valid improving edit.
        // The parse failure must cost only its own generation.
        let good_edit = serde_json::json!({
            "edits": [{
                "target": "recall",
                "find": "UNIQUE_NEEDLE",
                "replace": "UNIQUE_NEEDLE IMPROVED",
                "rationale": "test"
            }]
        })
        .to_string();
        let proposer = FakeProposer {
            responses: Mutex::new(vec!["not json at all".to_string(), good_edit]),
        };
        // Baseline 0.5; gen 1 runs no fitness; gen 2 candidate 0.9.
        let totals = std::sync::Arc::new(Mutex::new(vec![0.5, 0.9]));
        let totals_ref = totals.clone();
        let config = OptimizeConfig {
            generations: 2,
            target: Some(PromptTarget::Recall),
            ..Default::default()
        };

        let report = run_optimize(
            &proposer,
            &config,
            move |_generation| {
                let totals = totals_ref.clone();
                async move {
                    let total = totals.lock().unwrap().remove(0);
                    Ok(suite_with_total(total, None))
                }
            },
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.generations.len(), 2);
        assert!(!report.generations[0].decision.accepted);
        assert!(
            report.generations[0]
                .decision
                .reason
                .contains("proposal failed")
        );
        assert!(report.generations[1].decision.accepted);
        assert_eq!(report.accepted_generations, 1);
        assert_eq!(report.final_total, 0.9);

        prompts::clear_overrides();
    }
}
