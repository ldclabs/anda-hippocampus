//! Scenario mining (memory evolution plan, module M9): turns real
//! correction events — memories the user superseded — into eval scenarios,
//! so the optimizer's fitness function grows toward the production failure
//! distribution instead of staying frozen on hand-written fixtures.
//!
//! Mined scenarios land in a review directory and must pass human review
//! before joining the train/holdout suites; the miner scrubs obvious PII
//! from both the LLM's input excerpts and the produced scenario, but the
//! human reviewer is the real gate.

use anda_core::{BoxError, CompletionRequest, Json, ModelEffort, Usage};
use serde::Deserialize;

use super::{EvalProfile, EvalScenario, validate_eval_plan};
use crate::{
    assess::{self, AssessContext},
    space::Space,
};

/// Per-conversation excerpt budget fed to the miner LLM.
const MINE_EXCERPT_CHARS: usize = 2_000;

/// Max source conversations pulled per correction signal.
const MINE_MAX_CONVERSATIONS: usize = 3;

#[derive(Debug, Clone)]
pub struct MineConfig {
    /// Only corrections observed after this Unix-ms timestamp are mined.
    pub since_ms: u64,

    /// Upper bound of scenarios produced per run.
    pub max_scenarios: usize,
}

/// One mined scenario plus its provenance.
#[derive(Debug, Clone)]
pub struct MinedScenario {
    pub scenario: EvalScenario,

    /// The correction signal it was distilled from (entity id).
    pub signal: String,
}

const MINE_INSTRUCTIONS: &str = r#"You distill real memory-correction events into longitudinal eval scenarios for an AI memory system. You will receive a superseded knowledge-graph link (a fact the user later corrected) and excerpts of the conversations it came from.

Write ONE eval scenario that replays this class of failure: the user states the original fact, later corrects it, maintenance runs, and a checkpoint verifies the correction won. Requirements:

- `id`: short snake_case slug starting with "mined_".
- `hidden_profile`: the ground truth about the user AFTER the correction.
- `timeline`: 2-4 `normal` turns (original statement, then the correction, paraphrased naturally), one `maintenance` turn ({"trigger":"on_demand","scope":"quick"}), and one final `checkpoint_synthetic` turn whose `query` a real user would ask and whose `evaluation` carries: `scoring_rubric`, `required_answer_terms` for the corrected fact, `forbidden_answer_terms` for the stale fact, and `expected_memories` with a `should_exist` semantic `assertion` for the corrected fact plus a `should_not_exist` assertion for the stale one.
- Give every turn a `turn` number and an RFC3339 `timestamp`; corrections happen days after the original.
- PRIVACY: replace real names with role placeholders (e.g. "user_042", "colleague_A"), emails with "[email]", phone/account numbers with "[number]". Never copy personal identifiers verbatim.

Respond with ONLY a JSON object: {"scenario": { ... EvalScenario ... }}"#;

#[derive(Debug, Deserialize)]
struct MinedWire {
    scenario: EvalScenario,
}

/// Mines eval scenarios from the space's correction ledger. Returns the
/// validated scenarios and the LLM usage spent.
pub async fn mine_scenarios(
    space: &Space,
    config: &MineConfig,
) -> Result<(Vec<MinedScenario>, Usage), BoxError> {
    let corrected = space
        .corrected_entities(
            config.since_ms,
            config.max_scenarios.saturating_mul(3).max(8),
        )
        .await?;
    let mut mined: Vec<MinedScenario> = Vec::new();
    let mut usage = Usage::default();

    for entity in corrected {
        if mined.len() >= config.max_scenarios {
            break;
        }
        let Some(link) = fetch_link(space, &entity).await else {
            continue;
        };
        let excerpts = fetch_source_excerpts(space, &link).await;
        let prompt = format!(
            "# Corrected memory (superseded link)\n{}\n\n# Related conversation excerpts\n{}",
            scrub_pii(&assess::truncate_chars(
                &serde_json::to_string_pretty(&link).unwrap_or_default(),
                MINE_EXCERPT_CHARS,
            )),
            if excerpts.is_empty() {
                "(none available)".to_string()
            } else {
                scrub_pii(&excerpts.join("\n---\n"))
            },
        );

        let output = match space
            .complete(CompletionRequest {
                instructions: MINE_INSTRUCTIONS.to_string(),
                prompt,
                effort: Some(ModelEffort::Medium),
                ..Default::default()
            })
            .await
        {
            Ok(output) => output,
            Err(err) => {
                log::warn!(target: "eval", "scenario mining completion failed for {entity}: {err}", entity = entity);
                continue;
            }
        };
        usage.accumulate(&output.usage);

        let wire: MinedWire = match assess::parse_json_payload(&output.content) {
            Ok(wire) => wire,
            Err(err) => {
                log::warn!(target: "eval", "mined scenario for {entity} did not parse: {err}");
                continue;
            }
        };
        let mut scenario = wire.scenario;
        if !scenario.id.starts_with("mined_") {
            scenario.id = format!("mined_{}", scenario.id);
        }
        scenario.description = Some(format!(
            "[mined from correction of {entity}] {} — review before adding to train/holdout suites",
            scenario.description.unwrap_or_default()
        ));
        // Fail closed: a scenario that cannot be scrubbed is dropped rather
        // than written to disk unscrubbed.
        if let Err(err) = scrub_scenario(&mut scenario) {
            log::warn!(
                target: "eval",
                "mined scenario `{}` failed PII scrubbing and was dropped: {err}",
                scenario.id
            );
            continue;
        }

        // A mined scenario must survive the same strict validation as
        // hand-written fixtures before it is even offered for review.
        let report = validate_eval_plan(std::slice::from_ref(&scenario), &[EvalProfile::default()]);
        if report.has_errors() {
            log::warn!(
                target: "eval",
                "mined scenario `{}` failed validation and was dropped: {:?}",
                scenario.id,
                report.issues
            );
            continue;
        }

        mined.push(MinedScenario {
            scenario,
            signal: entity,
        });
    }

    Ok((mined, usage))
}

/// Fetches a link by id; `None` when it no longer exists (e.g. forgotten).
async fn fetch_link(space: &Space, entity: &str) -> Option<Json> {
    if !assess::is_proposition_entity_id(entity) {
        return None;
    }
    let response = space
        .execute_kip_readonly(anda_kip::Request {
            command: format!(
                "FIND(?link) WHERE {{ ?link (id: \"{}\") }} LIMIT 1",
                entity.replace('\\', "\\\\").replace('"', "\\\"")
            ),
            readonly: true,
            ..Default::default()
        })
        .await
        .ok()?;
    let mut found = None;
    if let anda_kip::Response::Ok { result, .. } = &response {
        assess::collect_entity_objects(result, &mut |id, object| {
            if id == entity && found.is_none() {
                found = Some(Json::Object(object.clone()));
            }
        });
    }
    found
}

/// Pulls bounded excerpts of the conversations the link's `metadata.source`
/// points at (formation writes conversation ids there).
async fn fetch_source_excerpts(space: &Space, link: &Json) -> Vec<String> {
    let mut ids: Vec<u64> = Vec::new();
    let mut push_source = |value: &Json| {
        let id = match value {
            Json::Number(number) => number.as_u64(),
            Json::String(text) => text.trim().parse::<u64>().ok(),
            _ => None,
        };
        if let Some(id) = id {
            ids.push(id);
        }
    };
    match link.get("metadata").and_then(|meta| meta.get("source")) {
        Some(Json::Array(items)) => items.iter().for_each(&mut push_source),
        Some(value) => push_source(value),
        None => {}
    }
    ids.truncate(MINE_MAX_CONVERSATIONS);

    let mut excerpts = Vec::new();
    for id in ids {
        if let Ok(conversation) = space.get_conversation(None, id).await {
            let text = conversation
                .messages
                .iter()
                .filter_map(|message| {
                    let role = message.get("role")?.as_str()?;
                    let content = serde_json::to_string(message.get("content")?).ok()?;
                    Some(format!("{role}: {content}"))
                })
                .collect::<Vec<_>>()
                .join("\n");
            excerpts.push(assess::truncate_chars(&text, MINE_EXCERPT_CHARS));
        }
    }
    excerpts
}

/// Masks obvious PII: email-like tokens and digit runs of 7+ characters.
/// Belt-and-braces on top of the prompt's placeholder rules — the human
/// review directory remains the real gate.
///
/// Emails are masked *before* digit runs: the other order would first turn
/// `alice12345678@example.com` into `alice[number]@example.com`, whose `@`
/// no longer borders token characters, leaking the name and domain.
pub fn scrub_pii(text: &str) -> String {
    mask_digit_runs(&mask_emails(text))
}

/// Masks email-like tokens (expand around '@' over token characters).
fn mask_emails(text: &str) -> String {
    let is_token_char =
        |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+');
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@'
            && index > 0
            && is_token_char(chars[index - 1])
            && index + 1 < chars.len()
            && is_token_char(chars[index + 1])
        {
            // Rewind the local part already emitted.
            let mut start = out.chars().count();
            for previous in out.chars().rev() {
                if is_token_char(previous) {
                    start -= 1;
                } else {
                    break;
                }
            }
            out = out.chars().take(start).collect();
            let mut end = index + 1;
            while end < chars.len() && is_token_char(chars[end]) {
                end += 1;
            }
            out.push_str("[email]");
            index = end;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Masks digit runs of 7+ characters (phone/account numbers).
fn mask_digit_runs(text: &str) -> String {
    let mut masked = String::with_capacity(text.len());
    let mut digits = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if digits.len() >= 7 {
            masked.push_str("[number]");
        } else {
            masked.push_str(&digits);
        }
        digits.clear();
        masked.push(ch);
    }
    if digits.len() >= 7 {
        masked.push_str("[number]");
    } else {
        masked.push_str(&digits);
    }
    masked
}

/// Scrubs every string field of a scenario in place by round-tripping it
/// through JSON. Whole-value scrubbing is deliberate: the miner LLM is
/// *instructed* to put the corrected fact into `required_answer_terms` /
/// `forbidden_answer_terms` / `expected_memories[].assertion`, so any
/// field-by-field allowlist that misses one of them ships raw PII to disk.
/// (RFC3339 timestamps survive: their digit runs are ≤4 chars.)
fn scrub_scenario(scenario: &mut EvalScenario) -> Result<(), BoxError> {
    let mut value = serde_json::to_value(&*scenario)?;
    scrub_json_strings(&mut value);
    *scenario = serde_json::from_value(value)?;
    Ok(())
}

fn scrub_json_strings(value: &mut Json) {
    match value {
        Json::String(text) => *text = scrub_pii(text),
        Json::Array(items) => items.iter_mut().for_each(scrub_json_strings),
        Json::Object(map) => map.values_mut().for_each(scrub_json_strings),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_pii_masks_emails_and_long_numbers() {
        assert_eq!(
            scrub_pii("mail alice.w+x@example.com or call 13800138000 ok"),
            "mail [email] or call [number] ok"
        );
        // Short numbers and plain words survive.
        assert_eq!(scrub_pii("room 42, budget 300"), "room 42, budget 300");
        // '@' without token context is untouched.
        assert_eq!(scrub_pii("a @ b"), "a @ b");
        // Emails whose local part contains a long digit run must still be
        // recognized as emails (email pass runs before the digit pass).
        assert_eq!(scrub_pii("alice12345678@example.com"), "[email]");
    }

    #[test]
    fn scrub_scenario_covers_evaluation_and_assertion_fields() {
        // The miner is instructed to put the corrected fact into rubric
        // fields; the scrub must reach them, not just user/query text.
        let mut scenario: EvalScenario = serde_json::from_value(serde_json::json!({
            "id": "mined_pii_probe",
            "hidden_profile": {"email": "bob@corp.example"},
            "timeline": [
                {
                    "turn": 1,
                    "type": "checkpoint_synthetic",
                    "query": "what is the contact?",
                    "evaluation": {
                        "scoring_rubric": "must cite carol@corp.example",
                        "required_answer_terms": ["dave@corp.example", "13800138000"],
                        "forbidden_answer_terms": ["old.bob@corp.example"],
                        "expected_memories": [{
                            "id": "m1",
                            "assertion": "user email is erin@corp.example",
                            "mode": "should_exist"
                        }]
                    }
                }
            ]
        }))
        .expect("scenario parses");
        scrub_scenario(&mut scenario).expect("scrub succeeds");
        let flat = serde_json::to_string(&scenario).expect("serializes");
        assert!(!flat.contains("corp.example"), "email leaked: {flat}");
        assert!(!flat.contains("13800138000"), "number leaked: {flat}");
        assert!(flat.contains("[email]"));
    }
}
