//! LLM-as-judge for checkpoint scoring and semantic graph probes.
//!
//! The lexical scorer stays as a deterministic smoke gate; this module layers
//! rubric-driven judgment on top so paraphrases are not false negatives and
//! correct meta-references to superseded facts are not false positives.

use anda_core::{BoxError, CompletionRequest, Json, ModelEffort};
use serde::{Deserialize, Serialize};

use super::{EvalFinding, EvalFindingKind, MemoryExpectationMode, MemoryProbeReport};
use crate::assess::{
    AssessContext, JudgeCall, MAX_EVIDENCE_CHARS, parse_json_payload, truncate_chars,
};

/// Judge scores for one checkpoint answer sample. All values are clamped to
/// `0..=1` after parsing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct JudgeVerdict {
    /// Did the answer actually use the relevant memories to help the user?
    pub memory_utility: f64,

    /// Are superseded/expired facts absent as *assertions*? Meta-references
    /// ("unlike your old BBQ preference…") are correct and must not lower this.
    pub forgetting_quality: f64,

    /// Confidence hygiene: no stale facts asserted as current, uncertainty
    /// admitted when evidence is thin.
    pub uncertainty_calibration: f64,

    /// How satisfied a real user with the hidden profile would be (0..1).
    pub satisfaction: f64,

    pub reasoning: String,

    pub findings: Vec<EvalFinding>,
}

/// Wire shape for the judge output: findings are parsed leniently so a single
/// unknown kind does not discard an otherwise valid verdict.
#[derive(Debug, Default, Deserialize)]
struct RawVerdict {
    #[serde(default)]
    memory_utility: f64,

    #[serde(default)]
    forgetting_quality: f64,

    #[serde(default)]
    uncertainty_calibration: f64,

    #[serde(default)]
    satisfaction: f64,

    #[serde(default)]
    reasoning: String,

    #[serde(default)]
    findings: Vec<Json>,
}

impl From<RawVerdict> for JudgeVerdict {
    fn from(raw: RawVerdict) -> Self {
        let findings = raw
            .findings
            .into_iter()
            .filter_map(|value| serde_json::from_value::<EvalFinding>(value).ok())
            .filter(|finding| is_answer_finding(finding.kind))
            .collect();
        Self {
            memory_utility: raw.memory_utility.clamp(0.0, 1.0),
            forgetting_quality: raw.forgetting_quality.clamp(0.0, 1.0),
            uncertainty_calibration: raw.uncertainty_calibration.clamp(0.0, 1.0),
            satisfaction: raw.satisfaction.clamp(0.0, 1.0),
            reasoning: raw.reasoning,
            findings,
        }
    }
}

/// Everything the judge sees for one checkpoint sample.
pub(crate) struct JudgeCheckpointInput<'a> {
    pub query: &'a str,
    pub answer: &'a str,
    pub scoring_rubric: Option<&'a str>,
    pub hidden_profile: &'a Json,
    pub required_terms: &'a [String],
    pub forbidden_terms: &'a [String],
    pub probes: &'a [MemoryProbeReport],
    pub expectations: Vec<JudgeExpectation>,
    pub trace_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JudgeExpectation {
    pub id: String,
    pub mode: MemoryExpectationMode,
    pub description: Option<String>,
    /// Whether the pre-recall graph probe found this memory state satisfied.
    pub probe_satisfied: Option<bool>,
}

const JUDGE_INSTRUCTIONS: &str = r#"You are a strict evaluator of an AI memory system. You will receive a user query, the memory system's answer, a scoring rubric, the hidden ground-truth user profile, memory expectations with pre-answer graph probe outcomes, and a summary of the retrieval trace.

Score the answer on these dimensions, each 0.0..1.0:
- memory_utility: did the answer use the relevant remembered facts to genuinely help this user? Paraphrases count fully; literal keyword overlap is NOT required. Dumping remembered facts irrelevant to the query lowers this score.
- forgetting_quality: superseded or expired facts must not be asserted as currently true. IMPORTANT: meta-references are correct behavior — e.g. "unlike your old BBQ preference, here are vegetarian options" correctly acknowledges the superseded fact and must NOT be penalized. Only penalize stale facts presented as still valid.
- uncertainty_calibration: confident claims must be supported by the probes/trace; when evidence is thin the answer should hedge or ask.
- satisfaction: how satisfied would the real user (see hidden profile) be with this answer, 0..1.

Also report findings, attributing each failure to the responsible stage:
- "formation_miss": the probes show the graph never formed a needed memory.
- "bad_consolidation": the graph still holds a stale/conflicting memory as active.
- "bad_grounding": the graph has the memory (probe satisfied) but the retrieval trace shows it was never retrieved.
- "bad_synthesis": the memory was retrieved (visible in trace) but the answer failed to use it.
- "overconfidence": the answer asserts stale or unsupported facts as current truth.

Respond with ONLY a JSON object:
{"memory_utility": 0.0, "forgetting_quality": 0.0, "uncertainty_calibration": 0.0, "satisfaction": 0.0, "reasoning": "...", "findings": [{"kind": "bad_synthesis", "message": "...", "expectation_id": null}]}"#;

pub(crate) async fn judge_checkpoint<C>(
    driver: &C,
    input: JudgeCheckpointInput<'_>,
) -> Result<JudgeCall<JudgeVerdict>, BoxError>
where
    C: AssessContext + ?Sized,
{
    let probes: Vec<Json> = input
        .probes
        .iter()
        .map(|probe| {
            serde_json::json!({
                "expectation_id": probe.expectation_id,
                "mode": probe.mode,
                "satisfied": probe.satisfied,
                "hit_count": probe.hit_count,
                "assertion": probe.assertion,
            })
        })
        .collect();

    let prompt = format!(
        "# User query\n{}\n\n# Memory system answer\n{}\n\n# Scoring rubric\n{}\n\n# Hidden ground-truth user profile\n{}\n\n# Memory expectations\n{}\n\n# Pre-answer graph probes\n{}\n\n# Retrieval trace summary\n{}\n\n# Term hints (advisory only; paraphrase counts)\nrequired: {:?}\nforbidden-as-current-assertion: {:?}",
        input.query,
        input.answer,
        input.scoring_rubric.unwrap_or("(none provided)"),
        truncate_chars(
            &serde_json::to_string(input.hidden_profile).unwrap_or_default(),
            MAX_EVIDENCE_CHARS
        ),
        truncate_chars(
            &serde_json::to_string(&input.expectations).unwrap_or_default(),
            MAX_EVIDENCE_CHARS
        ),
        truncate_chars(
            &serde_json::to_string(&probes).unwrap_or_default(),
            MAX_EVIDENCE_CHARS
        ),
        input
            .trace_summary
            .as_deref()
            .unwrap_or("(no trace available)"),
        input.required_terms,
        input.forbidden_terms,
    );

    let output = driver
        .judge_complete(CompletionRequest {
            instructions: JUDGE_INSTRUCTIONS.to_string(),
            prompt,
            effort: Some(ModelEffort::Low),
            ..Default::default()
        })
        .await?;

    let raw: RawVerdict = parse_json_payload(&output.content)?;
    Ok(JudgeCall {
        verdict: raw.into(),
        usage: output.usage,
    })
}

/// Maps a judge finding to the attribution kind used by the harness; unknown
/// kinds are dropped by the deserializer, so this only sanity-checks bounds.
pub(crate) fn is_answer_finding(kind: EvalFindingKind) -> bool {
    matches!(
        kind,
        EvalFindingKind::FormationMiss
            | EvalFindingKind::BadConsolidation
            | EvalFindingKind::BadGrounding
            | EvalFindingKind::BadSynthesis
            | EvalFindingKind::Overconfidence
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_payload_tolerates_fences_and_prose() {
        let text = "Here is my verdict:\n```json\n{\"memory_utility\": 0.9, \"forgetting_quality\": 1.2, \"uncertainty_calibration\": -0.5, \"satisfaction\": 0.8, \"reasoning\": \"good\", \"findings\": [{\"kind\": \"bad_synthesis\", \"message\": \"m\"}, {\"kind\": \"weird_unknown\", \"message\": \"skip me\"}, {\"kind\": \"latency_cost\", \"message\": \"not a judge finding\"}]}\n```\nDone.";
        let raw: RawVerdict = parse_json_payload(text).unwrap();
        let verdict = JudgeVerdict::from(raw);
        assert_eq!(verdict.memory_utility, 0.9);
        assert_eq!(verdict.forgetting_quality, 1.0);
        assert_eq!(verdict.uncertainty_calibration, 0.0);
        assert_eq!(verdict.reasoning, "good");
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.findings[0].kind, EvalFindingKind::BadSynthesis);
    }
}
