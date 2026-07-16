//! Engine tools exposing the wiki to agents (and through them to MCP).
//!
//! Tool descriptions instruct the model to cite `wiki://` URIs when using
//! wiki evidence — the whole point of the subsystem is answers that can
//! name their source.

use anda_core::{BoxError, FunctionDefinition, Resource, Tool, ToolOutput};
use anda_engine::{context::BaseCtx, unix_ms};
use serde_json::json;
use std::sync::Arc;

use super::{
    WikiCommitInput, WikiCommitOutput, WikiReadInput, WikiReadOutput, WikiSearchInput,
    WikiSearchOutput, WikiService,
};

/// Label view for agent wiki reads, evaluated per call: `None` =
/// unrestricted (private space, where every recall caller holds an
/// unrestricted credential), `Some(vec![])` = unlabeled content only
/// (public space, where the recall endpoint is world-reachable and its
/// tools must not surface labeled documents — PRD §8.2).
pub type WikiToolScope = Arc<dyn Fn() -> Option<Vec<String>> + Send + Sync>;

#[derive(Clone)]
pub struct WikiSearchTool {
    wiki: Arc<WikiService>,
    scope: WikiToolScope,
}

impl WikiSearchTool {
    pub const NAME: &'static str = "wiki_search";

    pub fn new(wiki: Arc<WikiService>, scope: WikiToolScope) -> Self {
        Self { wiki, scope }
    }
}

impl Tool<BaseCtx> for WikiSearchTool {
    type Args = WikiSearchInput;
    type Output = WikiSearchOutput;

    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn description(&self) -> String {
        "Searches the space wiki (versioned reference documents: policies, manuals, SOPs, API \
         docs, FAQs) with keyword BM25 retrieval and returns text snippets with verifiable \
         citations. Each citation has a wiki:// URI — include it when you use the evidence in an \
         answer. If results are poor, retry with reformulated keywords (synonyms, error codes, \
         exact product terms). Use wiki_read with a citation's doc_id/anchor to read the full \
         section."
            .to_string()
    }

    fn definition(&self) -> FunctionDefinition {
        FunctionDefinition {
            name: self.name(),
            description: self.description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword query. BM25 matching: prefer exact terms, product names, error codes over full sentences."
                    },
                    "namespaces": {
                        "type": ["array", "null"],
                        "items": {"type": "string"},
                        "description": "Restrict to these namespaces; null searches all."
                    },
                    "doc_ids": {
                        "type": ["array", "null"],
                        "items": {"type": "integer"},
                        "description": "Restrict to these document ids; null searches all."
                    },
                    "tags": {
                        "type": ["array", "null"],
                        "items": {"type": "string"},
                        "description": "Restrict to documents carrying any of these tags."
                    },
                    "top_k": {
                        "type": ["integer", "null"],
                        "description": "Max hits, 1-50. Default 8."
                    },
                    "mode": {
                        "type": ["string", "null"],
                        "enum": ["chunks", "docs", null],
                        "description": "chunks (default): best matching passages. docs: one best passage per document, for 'which document covers X'."
                    },
                    "expand": {
                        "type": ["integer", "null"],
                        "description": "Neighbor expansion 0-2 (default 0): widen each hit with adjacent passages for more context; citations widen accordingly."
                    }
                },
                "required": ["query", "namespaces", "doc_ids", "tags", "top_k", "mode", "expand"],
                "additionalProperties": false
            }),
            strict: Some(true),
        }
    }

    async fn call(
        &self,
        _ctx: BaseCtx,
        args: Self::Args,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<Self::Output>, BoxError> {
        let view = (self.scope)();
        let output = self.wiki.search_view(args, view.as_deref()).await?;
        Ok(ToolOutput::new(output))
    }
}

#[derive(Clone)]
pub struct WikiReadTool {
    wiki: Arc<WikiService>,
    scope: WikiToolScope,
}

impl WikiReadTool {
    pub const NAME: &'static str = "wiki_read";

    pub fn new(wiki: Arc<WikiService>, scope: WikiToolScope) -> Self {
        Self { wiki, scope }
    }
}

impl Tool<BaseCtx> for WikiReadTool {
    type Args = WikiReadInput;
    type Output = WikiReadOutput;

    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn description(&self) -> String {
        "Reads a wiki document progressively: 'toc' lists sections with anchors, 'section' \
         returns one section's full text, 'range' slices exact bytes, 'full' returns the whole \
         document (bounded). Prefer toc → section over full for long documents. Pass a version \
         id to read history."
            .to_string()
    }

    fn definition(&self) -> FunctionDefinition {
        FunctionDefinition {
            name: self.name(),
            description: self.description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "doc_id": {"type": "integer", "description": "Document id."},
                    "version": {
                        "type": ["integer", "null"],
                        "description": "Version id to read; null reads the current version."
                    },
                    "selector": {
                        "type": "object",
                        "description": "What to read. {type:'toc'} | {type:'section',anchor} | {type:'range',start,end} | {type:'full'}",
                        "properties": {
                            "type": {"type": "string", "enum": ["toc", "section", "range", "full"]},
                            "anchor": {"type": ["string", "null"], "description": "Section anchor (from toc or a citation)."},
                            "start": {"type": ["integer", "null"], "description": "Range start byte."},
                            "end": {"type": ["integer", "null"], "description": "Range end byte."}
                        },
                        "required": ["type", "anchor", "start", "end"],
                        "additionalProperties": false
                    }
                },
                "required": ["doc_id", "version", "selector"],
                "additionalProperties": false
            }),
            strict: Some(true),
        }
    }

    async fn call(
        &self,
        _ctx: BaseCtx,
        args: Self::Args,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<Self::Output>, BoxError> {
        let view = (self.scope)();
        let output = self.wiki.read_view(args, view.as_deref()).await?;
        Ok(ToolOutput::new(output))
    }
}

#[derive(Clone)]
pub struct WikiCommitTool {
    wiki: Arc<WikiService>,
}

impl WikiCommitTool {
    pub const NAME: &'static str = "wiki_commit";

    pub fn new(wiki: Arc<WikiService>) -> Self {
        Self { wiki }
    }
}

impl Tool<BaseCtx> for WikiCommitTool {
    type Args = WikiCommitInput;
    type Output = WikiCommitOutput;

    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn description(&self) -> String {
        "Commits a wiki document as an immutable new version (git-like). Create: omit doc_id. \
         Update: pass doc_id AND parent_version (the version you read). On a conflict error the \
         document moved — re-read it, merge your change, and retry with the reported current \
         version as parent_version. Identical content is a no-op. Markdown only; keep documents \
         focused and under 1 MiB."
            .to_string()
    }

    fn definition(&self) -> FunctionDefinition {
        FunctionDefinition {
            name: self.name(),
            description: self.description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "doc_id": {"type": ["integer", "null"], "description": "Existing document id to update; null creates."},
                    "parent_version": {"type": ["integer", "null"], "description": "Required for updates: the current_version you based this edit on (CAS)."},
                    "namespace": {"type": ["string", "null"], "description": "Logical partition, e.g. 'policy'. Default 'default'."},
                    "slug": {"type": ["string", "null"], "description": "Display slug; auto-derived from title when null."},
                    "title": {"type": "string", "description": "Document title."},
                    "content": {"type": "string", "description": "Full Markdown content (whole document, not a diff)."},
                    "tags": {"type": ["array", "null"], "items": {"type": "string"}, "description": "Tags for filtering."},
                    "source_uri": {"type": ["string", "null"], "description": "External origin, when imported."},
                    "message": {"type": ["string", "null"], "description": "Commit message: why this change."},
                    "acl_label": {"type": ["string", "null"], "description": "ACL label; null keeps/inherits, empty string clears."}
                },
                "required": ["doc_id", "parent_version", "namespace", "slug", "title", "content", "tags", "source_uri", "message", "acl_label"],
                "additionalProperties": false
            }),
            strict: Some(true),
        }
    }

    async fn call(
        &self,
        ctx: BaseCtx,
        args: Self::Args,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<Self::Output>, BoxError> {
        use anda_core::StateFeatures;

        let actor = ctx.caller().to_string();
        let output = self.wiki.commit(actor, args, unix_ms()).await?;
        Ok(ToolOutput::new(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::{WikiSearchMode, WikiSelector};

    // The strict tool schemas force the model to send every key and declare
    // the optional ones nullable, so each Args type must accept explicit
    // `null` for every nullable field — `Tool::call_raw` feeds the raw JSON
    // straight into serde with no null preprocessing.

    #[test]
    fn wiki_search_args_accept_schema_conforming_nulls() {
        let args: WikiSearchInput = serde_json::from_value(json!({
            "query": "error code 42",
            "namespaces": null,
            "doc_ids": null,
            "tags": null,
            "top_k": null,
            "mode": null,
            "expand": null
        }))
        .unwrap();
        assert_eq!(args.query, "error code 42");
        assert!(args.namespaces.is_empty());
        assert!(args.doc_ids.is_empty());
        assert!(args.tags.is_empty());
        assert_eq!(args.top_k, None);
        assert_eq!(args.mode, WikiSearchMode::Chunks);
        assert_eq!(args.expand, None);

        // Non-null values still deserialize as before.
        let args: WikiSearchInput = serde_json::from_value(json!({
            "query": "q",
            "namespaces": ["policy"],
            "doc_ids": [7],
            "tags": ["faq"],
            "top_k": 3,
            "mode": "docs",
            "expand": 1
        }))
        .unwrap();
        assert_eq!(args.namespaces, vec!["policy".to_string()]);
        assert_eq!(args.doc_ids, vec![7]);
        assert_eq!(args.mode, WikiSearchMode::Docs);
    }

    #[test]
    fn wiki_read_args_accept_schema_conforming_nulls() {
        for (selector_json, expected) in [
            (
                json!({"type": "toc", "anchor": null, "start": null, "end": null}),
                WikiSelector::Toc,
            ),
            (
                json!({"type": "section", "anchor": "h-intro", "start": null, "end": null}),
                WikiSelector::Section {
                    anchor: "h-intro".to_string(),
                },
            ),
            (
                json!({"type": "range", "anchor": null, "start": 0, "end": 128}),
                WikiSelector::Range { start: 0, end: 128 },
            ),
            (
                json!({"type": "full", "anchor": null, "start": null, "end": null}),
                WikiSelector::Full,
            ),
        ] {
            let args: WikiReadInput = serde_json::from_value(json!({
                "doc_id": 1,
                "version": null,
                "selector": selector_json
            }))
            .unwrap();
            assert_eq!(args.doc_id, 1);
            assert_eq!(args.version, None);
            assert_eq!(args.selector, expected);
        }
    }

    #[test]
    fn wiki_commit_args_accept_schema_conforming_nulls() {
        let args: WikiCommitInput = serde_json::from_value(json!({
            "doc_id": null,
            "parent_version": null,
            "namespace": null,
            "slug": null,
            "title": "Refund policy",
            "content": "# Refund policy",
            "tags": null,
            "source_uri": null,
            "message": null,
            "acl_label": null
        }))
        .unwrap();
        assert_eq!(args.title, "Refund policy");
        assert!(args.doc_id.is_none());
        assert!(args.tags.is_none());
    }
}
