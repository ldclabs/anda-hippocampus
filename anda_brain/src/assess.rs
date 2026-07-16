//! Shared assessment instruments (memory evolution plan, module M0).
//!
//! These are the pieces of the eval harness that are also useful outside it:
//! the semantic-assertion judge, recall trace extraction, and read-only KIP
//! probe helpers. The offline eval harness (`crate::eval`) and the online
//! maintenance self-test (plan module M7) consume the same implementations,
//! so "what CI measures" and "what the brain checks about itself" cannot
//! drift apart.

use anda_core::{
    AgentOutput, BoxError, CompletionRequest, ContentPart, Json, Message, ModelEffort, Usage,
};
use anda_kip::{Request, Response};
use serde::{Deserialize, Serialize};

use crate::space::Space;
use crate::types::MemoryCitation;

/// Default similarity threshold for semantic assertion probes.
pub const DEFAULT_ASSERTION_SEARCH_THRESHOLD: f64 = 0.35;

/// Default result limit for semantic assertion probes.
pub const DEFAULT_ASSERTION_SEARCH_LIMIT: usize = 8;

/// Upper bound applied to serialized evidence blobs fed to a judge.
pub(crate) const MAX_EVIDENCE_CHARS: usize = 6_000;

/// Maintenance-backlog count: concepts still in the `Unsorted` domain.
/// The same assessment query the Maintenance prompt prescribes.
pub const UNSORTED_COUNT_KQL: &str =
    "FIND(COUNT(?n)) WHERE { (?n, \"belongs_to_domain\", {type: \"Domain\", name: \"Unsorted\"}) }";

/// Concepts without any `belongs_to_domain` proposition. Intentionally
/// matches every concept type: the Maintenance prompt requires schema/meta
/// concepts to be attached to the CoreSchema domain on creation, so a fully
/// maintained graph reaches zero orphans.
pub const ORPHAN_COUNT_KQL: &str =
    "FIND(COUNT(?n)) WHERE { ?n {} NOT { (?n, \"belongs_to_domain\", ?d) } }";

/// Registered `$PropositionType` count — the schema-sprawl indicator
/// (plan module M8).
pub const PREDICATE_TYPES_COUNT_KQL: &str =
    "FIND(COUNT(?t)) WHERE { ?t {type: \"$PropositionType\"} }";

/// Minimal capabilities the assessment instruments need from their host:
/// one-shot LLM completions (for judges and simulators) and read-only KIP
/// access (for graph probes). `Space` implements it directly; eval drivers
/// inherit it as a supertrait of `EvalDriver`.
#[async_trait::async_trait]
pub trait AssessContext: Send + Sync {
    /// One-shot LLM completion. Hosts without a model can leave the default.
    async fn complete(&self, _req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        Err("assess context does not support LLM completions".into())
    }

    /// Completion used by judges. Defaults to [`Self::complete`]; hosts with
    /// an independent judge model override this (plan M9), so judge scores
    /// stop sharing the evaluated system's blind spots.
    async fn judge_complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        self.complete(req).await
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError>;
}

#[async_trait::async_trait]
impl AssessContext for Space {
    async fn complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        self.eval_complete(req).await
    }

    async fn judge_complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        match self.judge_model() {
            Some(model) => model.completion(req).await,
            None => self.eval_complete(req).await,
        }
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError> {
        // Inherent method (space.rs); takes priority over this trait method.
        self.execute_kip_readonly(request).await
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

    /// Checks whether any term appears in a tool *output*. Tool names and
    /// args are deliberately excluded: recall echoes the user's query into
    /// search args, so matching them would misread "searched for it" as
    /// "retrieved it" and flip grounding failures into synthesis failures.
    pub fn contains_any_term(&self, terms: &[String]) -> bool {
        if terms.is_empty() {
            return false;
        }

        let haystack = self
            .tools
            .iter()
            .filter_map(|tool| tool.output.as_ref())
            .map(|output| serde_json::to_string(output).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
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

/// A judge invocation's verdict plus its token usage.
#[derive(Debug, Clone)]
pub struct JudgeCall<T> {
    pub verdict: T,
    pub usage: Usage,
}

/// Verdict for one semantic graph probe.
#[derive(Debug, Clone, Deserialize)]
pub struct AssertionVerdict {
    pub holds: bool,

    #[serde(default)]
    pub reason: String,
}

const ASSERTION_INSTRUCTIONS: &str = r#"You are inspecting a knowledge graph for an AI memory system. You will receive a statement about what the graph may currently assert, plus raw evidence returned by a semantic graph search.

Decide whether the evidence shows the statement currently holds in the graph. Superseded, archived, expired, or explicitly deactivated memories do NOT count as holding. Absence of any matching evidence means the statement does not hold. Do not assume facts beyond the evidence.

Respond with ONLY a JSON object: {"holds": true, "reason": "..."}"#;

/// Asks the judge whether `evidence` shows that `assertion` currently holds
/// in the graph.
pub async fn judge_assertion<C>(
    ctx: &C,
    assertion: &str,
    evidence: &Json,
) -> Result<JudgeCall<AssertionVerdict>, BoxError>
where
    C: AssessContext + ?Sized,
{
    let prompt = format!(
        "# Statement to verify\n{}\n\n# Graph search evidence\n{}",
        assertion,
        truncate_chars(
            &serde_json::to_string(evidence).unwrap_or_default(),
            MAX_EVIDENCE_CHARS
        ),
    );

    let output = ctx
        .judge_complete(CompletionRequest {
            instructions: ASSERTION_INSTRUCTIONS.to_string(),
            prompt,
            effort: Some(ModelEffort::Low),
            ..Default::default()
        })
        .await?;

    Ok(JudgeCall {
        verdict: parse_json_payload(&output.content)?,
        usage: output.usage,
    })
}

/// Builds the semantic search command for an assertion probe. The search text
/// is embedded in a KQL string literal, so backslashes must be escaped before
/// quotes to keep the command parseable for any input.
pub fn assertion_search_command(search: &str, threshold: f64, limit: usize) -> String {
    format!(
        "SEARCH CONCEPT \"{}\" MODE \"semantic\" THRESHOLD {threshold} LIMIT {limit}",
        search.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// Extracts the first JSON object from model output, tolerating code fences
/// and prose around it.
pub fn parse_json_payload<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, BoxError> {
    let start = text
        .find('{')
        .ok_or_else(|| format!("no JSON object in judge output: {text:.120}"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| format!("unterminated JSON object in judge output: {text:.120}"))?;
    if end < start {
        return Err(format!("malformed JSON object in judge output: {text:.120}").into());
    }
    Ok(serde_json::from_str(&text[start..=end])?)
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("…(truncated)");
    out
}

/// True for concept ids of the form `"C:<u64>"`.
pub fn is_concept_entity_id(value: &str) -> bool {
    value
        .strip_prefix("C:")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// True for proposition ids of the form `"P:<u64>:<predicate>"`.
pub fn is_proposition_entity_id(value: &str) -> bool {
    value
        .strip_prefix("P:")
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(id, predicate)| {
            !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) && !predicate.is_empty()
        })
}

/// True for any graph entity id (`C:*` concept or `P:*` proposition).
pub fn is_entity_id(value: &str) -> bool {
    is_concept_entity_id(value) || is_proposition_entity_id(value)
}

impl RecallTrace {
    /// Graph entity ids (`C:*` / `P:*`) surfaced in successful tool outputs.
    /// This is the usage-ledger signal (plan module M1): which memories a
    /// recall actually retrieved.
    pub fn entity_ids(&self) -> std::collections::BTreeSet<String> {
        let mut ids = std::collections::BTreeSet::new();
        for tool in &self.tools {
            if tool.is_error == Some(true) {
                continue;
            }
            if let Some(output) = &tool.output {
                collect_entity_objects(output, &mut |id, _| {
                    ids.insert(id.to_string());
                });
            }
        }
        ids
    }
}

/// Deterministic memory citations for a recall answer (plan module M4):
/// the entities the trace shows were retrieved, with type/name/confidence
/// harvested from the tool outputs when present. Never trusts the model to
/// self-report which ids it used.
pub fn extract_memory_citations(trace: &RecallTrace) -> Vec<MemoryCitation> {
    let mut seen = std::collections::BTreeSet::new();
    let mut citations = Vec::new();
    for tool in &trace.tools {
        if tool.is_error == Some(true) {
            continue;
        }
        let Some(output) = &tool.output else { continue };
        collect_citations(output, &mut seen, &mut citations);
    }
    citations
}

/// Memory citations found anywhere in one KIP result JSON (plan module M5:
/// the metamemory probe reports its hits in the same shape recall does).
pub fn citations_from_json(value: &Json) -> Vec<MemoryCitation> {
    let mut seen = std::collections::BTreeSet::new();
    let mut citations = Vec::new();
    collect_citations(value, &mut seen, &mut citations);
    citations
}

fn collect_citations(
    value: &Json,
    seen: &mut std::collections::BTreeSet<String>,
    citations: &mut Vec<MemoryCitation>,
) {
    collect_entity_objects(value, &mut |id, object| {
        if !seen.insert(id.to_string()) {
            return;
        }
        let r#type = object
            .get("type")
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| {
                // Propositions carry their predicate in the id itself.
                id.strip_prefix("P:")
                    .and_then(|rest| rest.split_once(':'))
                    .map(|(_, predicate)| predicate.to_string())
            });
        let metadata = object.get("metadata");
        citations.push(MemoryCitation {
            entity: id.to_string(),
            r#type,
            name: object
                .get("name")
                .and_then(Json::as_str)
                .map(str::to_string),
            confidence: metadata
                .and_then(|metadata| metadata.get("confidence"))
                .and_then(Json::as_f64),
            source: metadata
                .and_then(|metadata| metadata.get("source"))
                .and_then(|source| match source {
                    Json::String(text) => Some(text.clone()),
                    // Multi-source facts cite their first origin.
                    Json::Array(items) => items.iter().find_map(Json::as_str).map(str::to_string),
                    _ => None,
                }),
            created_at: metadata
                .and_then(|metadata| metadata.get("created_at"))
                .and_then(Json::as_str)
                .map(str::to_string),
        });
    });
}

/// Walks a KIP result recursively, visiting every object that carries a
/// graph entity `id` (concept or proposition).
pub(crate) fn collect_entity_objects(
    value: &Json,
    visit: &mut impl FnMut(&str, &serde_json::Map<String, Json>),
) {
    match value {
        Json::Array(items) => {
            for item in items {
                collect_entity_objects(item, visit);
            }
        }
        Json::Object(map) => {
            if let Some(Json::String(id)) = map.get("id")
                && is_entity_id(id)
            {
                visit(id, map);
            }
            for nested in map.values() {
                collect_entity_objects(nested, visit);
            }
        }
        _ => {}
    }
}

/// Opening tag of the recall self-report footer (plan module M4).
pub const RECALL_META_TAG_OPEN: &str = "<memory_meta>";

/// Closing tag of the recall self-report footer.
pub const RECALL_META_TAG_CLOSE: &str = "</memory_meta>";

/// The recall model's structured self-report, appended to its final answer.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct RecallMeta {
    /// Whether the graph held relevant memory for the query.
    #[serde(default)]
    pub found: Option<bool>,

    /// Self-assessed uncertainty of the answer, 0 (certain) ..= 1 (guessing).
    #[serde(default)]
    pub uncertainty: Option<f64>,
}

/// Splits the `<memory_meta>{...}</memory_meta>` self-report off a recall
/// answer, tail-anchored: the LAST closing tag and its nearest preceding
/// opening tag delimit the one block that is stripped and parsed; trailing
/// prose after the block is joined back onto the answer. Everything else —
/// earlier echoed example blocks, prose mentions, unclosed opens — stays in
/// the answer as literal text: marker leakage is accepted, content loss
/// never is. An absent or malformed payload degrades to `None` — the footer
/// is an enhancement, never a failure mode.
pub fn split_recall_meta(content: &str) -> (String, Option<RecallMeta>) {
    let Some(close) = content.rfind(RECALL_META_TAG_CLOSE) else {
        return (content.trim_end().to_string(), None);
    };
    let Some(open) = content[..close].rfind(RECALL_META_TAG_OPEN) else {
        return (content.trim_end().to_string(), None);
    };

    let meta = parse_json_payload::<RecallMeta>(&content[open + RECALL_META_TAG_OPEN.len()..close])
        .ok()
        .map(|meta| RecallMeta {
            uncertainty: meta
                .uncertainty
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 1.0)),
            ..meta
        });

    let before = content[..open].trim();
    let after = content[close + RECALL_META_TAG_CLOSE.len()..].trim();
    let answer = match (before.is_empty(), after.is_empty()) {
        (false, false) => format!("{before}\n{after}"),
        (false, true) => before.to_string(),
        (true, _) => after.to_string(),
    };
    (answer, meta)
}

/// Runs a read-only KIP count query and digs out the first integer in the
/// result. Returns `None` on error so callers degrade gracefully.
pub async fn kip_count<C>(ctx: &C, command: &str) -> Option<u64>
where
    C: AssessContext + ?Sized,
{
    let request = Request {
        command: command.to_string(),
        readonly: true,
        ..Default::default()
    };
    match ctx.execute_kip_readonly(request).await {
        Ok(Response::Ok { result, .. }) => first_integer(&result),
        _ => None,
    }
}

pub fn first_integer(value: &Json) -> Option<u64> {
    match value {
        Json::Number(number) => number.as_u64(),
        Json::Array(items) => items.iter().find_map(first_integer),
        Json::Object(map) => map.values().find_map(first_integer),
        _ => None,
    }
}

pub fn response_hit_count(response: &Response) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;
    use anda_core::ToolOutput;
    use serde_json::json;

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
        // Terms that only appear in args must not count as evidence.
        assert!(!trace.contains_any_term(&["Preference".to_string()]));
    }

    #[test]
    fn entity_id_matchers_accept_graph_ids_only() {
        assert!(is_concept_entity_id("C:7"));
        assert!(is_proposition_entity_id("P:11:likes"));
        assert!(is_entity_id("P:0:belongs_to_domain"));
        for bad in ["C:", "C:x", "P:11", "P::likes", "call_1", "wiki://x", ""] {
            assert!(!is_entity_id(bad), "{bad} must not match");
        }
    }

    #[test]
    fn entity_ids_and_citations_come_from_successful_outputs_only() {
        let trace = RecallTrace {
            tools: vec![
                ToolTrace {
                    name: "execute_kip_readonly".to_string(),
                    args: json!({"command": "mentions C:50 in args only"}),
                    call_id: None,
                    output: Some(json!({"result": [
                        {"id": "C:7", "type": "Preference", "name": "oolong",
                         "metadata": {"confidence": 0.9, "source": "chat:42",
                                      "created_at": "2026-07-01T00:00:00.000Z"}},
                        {"id": "P:3:prefers", "metadata": {"confidence": 0.8,
                                                           "source": ["a", "b"]}},
                        {"id": "not-an-entity"},
                        {"nested": [{"id": "C:7"}]}
                    ]})),
                    is_error: None,
                },
                ToolTrace {
                    name: "execute_kip_readonly".to_string(),
                    args: Json::Null,
                    call_id: None,
                    output: Some(json!([{"id": "C:99"}])),
                    is_error: Some(true),
                },
            ],
        };

        let ids = trace.entity_ids();
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            vec!["C:7".to_string(), "P:3:prefers".to_string()]
        );

        let citations = extract_memory_citations(&trace);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].entity, "C:7");
        assert_eq!(citations[0].r#type.as_deref(), Some("Preference"));
        assert_eq!(citations[0].name.as_deref(), Some("oolong"));
        assert_eq!(citations[0].confidence, Some(0.9));
        assert_eq!(citations[0].source.as_deref(), Some("chat:42"));
        assert_eq!(
            citations[0].created_at.as_deref(),
            Some("2026-07-01T00:00:00.000Z")
        );
        assert_eq!(citations[1].entity, "P:3:prefers");
        // Propositions derive their type from the id's predicate segment.
        assert_eq!(citations[1].r#type.as_deref(), Some("prefers"));
        assert_eq!(citations[1].confidence, Some(0.8));
        // Multi-source facts cite their first origin.
        assert_eq!(citations[1].source.as_deref(), Some("a"));
    }

    #[test]
    fn split_recall_meta_strips_footer_and_degrades_gracefully() {
        let (answer, meta) = split_recall_meta(
            "Answer.\n<memory_meta>{\"found\": true, \"uncertainty\": 0.25}</memory_meta>",
        );
        assert_eq!(answer, "Answer.");
        let meta = meta.unwrap();
        assert_eq!(meta.found, Some(true));
        assert_eq!(meta.uncertainty, Some(0.25));

        // Malformed payload: the tag block is still stripped.
        let (answer, meta) = split_recall_meta("Answer.\n<memory_meta>oops</memory_meta>");
        assert_eq!(answer, "Answer.");
        assert!(meta.is_none());

        // No footer at all.
        let (answer, meta) = split_recall_meta("Plain answer.  ");
        assert_eq!(answer, "Plain answer.");
        assert!(meta.is_none());

        // Out-of-range uncertainty clamps; prose after the footer survives.
        let (answer, meta) =
            split_recall_meta("Answer.\n<memory_meta>{\"uncertainty\": 7.0}</memory_meta>\ntail");
        assert_eq!(answer, "Answer.\ntail");
        assert_eq!(meta.unwrap().uncertainty, Some(1.0));

        // Tail-anchored: only the LAST closed block is stripped. An earlier
        // echoed example block stays in the answer as literal text — marker
        // leakage is accepted, content loss never is.
        let (answer, meta) = split_recall_meta(
            "<memory_meta>{\"uncertainty\": 0.9}</memory_meta>\nAnswer.\n\
             <memory_meta>{\"found\": true, \"uncertainty\": 0.1}</memory_meta>",
        );
        assert_eq!(
            answer,
            "<memory_meta>{\"uncertainty\": 0.9}</memory_meta>\nAnswer."
        );
        let meta = meta.unwrap();
        assert_eq!(meta.found, Some(true));
        assert_eq!(meta.uncertainty, Some(0.1));

        // No closing tag anywhere: nothing is stripped and nothing is
        // salvaged — an unclosed open (prose mention or truncated footer)
        // stays in the answer verbatim.
        let (answer, meta) = split_recall_meta("The <memory_meta> tag marks the footer, see docs.");
        assert_eq!(answer, "The <memory_meta> tag marks the footer, see docs.");
        assert!(meta.is_none());
        let (answer, meta) = split_recall_meta("Answer.\n<memory_meta>{\"found\": false}");
        assert_eq!(answer, "Answer.\n<memory_meta>{\"found\": false}");
        assert!(meta.is_none());

        // A prose mention followed by the real footer: the close pairs with
        // its NEAREST preceding open, so all answer content survives — the
        // mentioned tag is kept as literal text.
        let (answer, meta) = split_recall_meta(
            "I found it. As instructed, the <memory_meta> footer follows.\n\
             Your meeting is on Friday at 3pm.\n\
             <memory_meta>{\"found\": true, \"uncertainty\": 0.1}</memory_meta>",
        );
        assert_eq!(
            answer,
            "I found it. As instructed, the <memory_meta> footer follows.\n\
             Your meeting is on Friday at 3pm."
        );
        let meta = meta.unwrap();
        assert_eq!(meta.found, Some(true));
        assert_eq!(meta.uncertainty, Some(0.1));
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
    fn first_integer_digs_into_kip_count_results() {
        assert_eq!(first_integer(&json!([{"result": [{"count": 7}]}])), Some(7));
        assert_eq!(first_integer(&json!("nope")), None);
        assert_eq!(first_integer(&json!(3)), Some(3));
    }

    #[test]
    fn assertion_search_command_escapes_backslashes_and_quotes() {
        let command = assertion_search_command("say \"hi\" \\ bye", 0.5, 3);
        assert_eq!(
            command,
            "SEARCH CONCEPT \"say \\\"hi\\\" \\\\ bye\" MODE \"semantic\" THRESHOLD 0.5 LIMIT 3"
        );
    }

    #[test]
    fn parse_json_payload_rejects_non_json() {
        assert!(parse_json_payload::<AssertionVerdict>("no json here").is_err());
    }

    #[test]
    fn parse_assertion_verdict() {
        let verdict: AssertionVerdict =
            parse_json_payload("{\"holds\": false, \"reason\": \"superseded\"}").unwrap();
        assert!(!verdict.holds);
        assert_eq!(verdict.reason, "superseded");
    }

    #[test]
    fn truncate_chars_bounds_output() {
        let text = "x".repeat(100);
        let out = truncate_chars(&text, 10);
        assert!(out.starts_with("xxxxxxxxxx"));
        assert!(out.ends_with("(truncated)"));
        assert_eq!(truncate_chars("short", 10), "short");
    }
}
