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

use super::{
    AttributionSummary, EvalDriver, EvalSuiteReport,
    judge::{parse_json_payload, truncate_chars},
};
use crate::agents::prompts::{self, PromptTarget};

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
pub async fn propose_edits<D>(
    driver: &D,
    target: PromptTarget,
    current_prompt: &str,
    failure_summary: &str,
) -> Result<Vec<PromptEdit>, BoxError>
where
    D: EvalDriver + ?Sized,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OptimizeConfig {
    /// Number of propose→evaluate→select generations.
    pub generations: usize,

    /// Fixed target prompt; `None` picks per generation from attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PromptTarget>,

    /// Z multiplier for the noise band (default 1.0).
    #[serde(default = "default_confidence_z")]
    pub confidence_z: f64,

    /// Minimum absolute improvement even when no variance is measured.
    #[serde(default = "default_min_delta")]
    pub min_delta: f64,
}

fn default_confidence_z() -> f64 {
    1.0
}

fn default_min_delta() -> f64 {
    0.005
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GenerationReport {
    pub generation: usize,
    pub target: PromptTarget,
    pub edits: Vec<PromptEdit>,
    pub baseline_total: f64,
    pub candidate_total: Option<f64>,
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
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcceptedPrompt {
    pub target: PromptTarget,
    pub text: String,
}

/// The generation loop. `fitness(generation)` must run the eval suite against
/// whatever prompt overrides are currently installed and return its report;
/// generation 0 is the baseline. The loop installs candidate prompts through
/// `agents::prompts::set_override` and restores them on rejection.
pub async fn run_optimize<D, F, Fut>(
    proposer: &D,
    config: &OptimizeConfig,
    mut fitness: F,
) -> Result<OptimizeReport, BoxError>
where
    D: EvalDriver + ?Sized,
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<EvalSuiteReport, BoxError>>,
{
    let mut baseline = fitness(0).await?;
    let baseline_total = baseline.score.total;
    let mut report = OptimizeReport {
        baseline_total,
        final_total: baseline_total,
        ..Default::default()
    };
    // Track locally-accepted texts so rejection restores the last good state.
    let mut accepted: Vec<(PromptTarget, String)> = Vec::new();

    for generation in 1..=config.generations {
        let target = config
            .target
            .unwrap_or_else(|| pick_target(&baseline.attribution));
        let current_prompt = prompts::active_prompt(target);
        let failure_summary = summarize_failures(&baseline);
        let edits = propose_edits(proposer, target, &current_prompt, &failure_summary).await?;

        if edits.is_empty() {
            report.generations.push(GenerationReport {
                generation,
                target,
                edits,
                baseline_total: baseline.score.total,
                candidate_total: None,
                decision: GenerationDecision {
                    accepted: false,
                    reason: "optimizer proposed no edits".to_string(),
                },
            });
            continue;
        }

        let mut candidate_text = current_prompt.to_string();
        let mut apply_error = None;
        for edit in &edits {
            match apply_edit(&candidate_text, &edit.find, &edit.replace) {
                Ok(next) => candidate_text = next,
                Err(err) => {
                    apply_error = Some(err.to_string());
                    break;
                }
            }
        }
        if let Some(err) = apply_error {
            report.generations.push(GenerationReport {
                generation,
                target,
                edits,
                baseline_total: baseline.score.total,
                candidate_total: None,
                decision: GenerationDecision {
                    accepted: false,
                    reason: format!("edit could not be applied: {err}"),
                },
            });
            continue;
        }

        prompts::set_override(target, Some(candidate_text.clone()));
        let candidate = fitness(generation).await?;
        let decision = decide(
            baseline.score.total,
            baseline.total_stddev,
            candidate.score.total,
            candidate.total_stddev,
            config.confidence_z,
            config.min_delta,
        );

        if decision.accepted {
            accepted.retain(|(kept, _)| *kept != target);
            accepted.push((target, candidate_text));
            report.final_total = candidate.score.total;
            report.accepted_generations += 1;
            report.generations.push(GenerationReport {
                generation,
                target,
                edits,
                baseline_total: baseline.score.total,
                candidate_total: Some(candidate.score.total),
                decision,
            });
            baseline = candidate;
        } else {
            // Revert to the last accepted text for this target (or default).
            let restore = accepted
                .iter()
                .find(|(kept, _)| *kept == target)
                .map(|(_, text)| text.clone());
            prompts::set_override(target, restore);
            report.generations.push(GenerationReport {
                generation,
                target,
                edits,
                baseline_total: baseline.score.total,
                candidate_total: Some(candidate.score.total),
                decision,
            });
        }
    }

    report.accepted_prompts = accepted
        .into_iter()
        .map(|(target, text)| AcceptedPrompt { target, text })
        .collect();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{EvalAgentResult, EvalScore};
    use crate::types::{FormationInput, MaintenanceInput, RecallInput};
    use anda_core::AgentOutput;
    use anda_kip::{Request, Response};
    use std::sync::Mutex;

    /// Minimal driver: only `complete` matters for the optimizer.
    #[derive(Default)]
    struct FakeProposer {
        responses: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl super::super::EvalDriver for FakeProposer {
        async fn remember(
            &self,
            _input: FormationInput,
        ) -> Result<EvalAgentResult, anda_core::BoxError> {
            Err("not used".into())
        }

        async fn recall(
            &self,
            _input: RecallInput,
        ) -> Result<EvalAgentResult, anda_core::BoxError> {
            Err("not used".into())
        }

        async fn maintain(
            &self,
            _input: MaintenanceInput,
        ) -> Result<EvalAgentResult, anda_core::BoxError> {
            Err("not used".into())
        }

        async fn execute_kip_readonly(
            &self,
            _request: Request,
        ) -> Result<Response, anda_core::BoxError> {
            Err("not used".into())
        }

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
            confidence_z: 1.0,
            min_delta: 0.005,
        };

        let report = run_optimize(&proposer, &config, move |_generation| {
            let totals = totals_ref.clone();
            async move {
                let total = totals.lock().unwrap().remove(0);
                Ok(suite_with_total(total, None))
            }
        })
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
}
