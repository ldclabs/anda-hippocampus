//! OKF (Open Knowledge Format) v0.1 bundle import/export.
//!
//! OKF is the exchange layer only; the internal model stays authoritative
//! (PRD §9). Fidelity strategy: the YAML frontmatter block is stored
//! verbatim in `doc.metadata["x_okf_frontmatter"]` and written back on
//! export — unknown keys, ordering and comments survive round-trips without
//! a YAML dependency. Known keys (`title`, `tags`, `resource`, `type`) are
//! additionally extracted by a line-level parser for mapping. `x_anda_*`
//! keys are ours: appended fresh on export, stripped on import.

use anda_db::schema::Json;
use std::collections::BTreeMap;

use super::{
    DEFAULT_NAMESPACE, DOC_STATUS_ACTIVE, EVENT_EXPORT_COMPLETED, EVENT_IMPORT_COMPLETED,
    WikiBundleEntry, WikiCommitInput, WikiDocInfo, WikiError, WikiExportOutput, WikiImportInput,
    WikiImportOutput, WikiImportSkip, WikiImportStatus, WikiImportedDoc, WikiListDocsInput,
    WikiService, markdown_title, prepare_commit, slugify_path,
};

pub const OKF_VERSION: &str = "0.1";
/// Metadata key holding the verbatim frontmatter block (no delimiters).
pub const FRONTMATTER_KEY: &str = "x_okf_frontmatter";
/// Metadata key holding the OKF `type` value.
pub const OKF_TYPE_KEY: &str = "okf_type";
/// Sized so a full export of a large namespace replays in one call (M4
/// acceptance); bundles beyond this must be split by the external toolchain.
const MAX_IMPORT_ENTRIES: usize = 65_536;
/// Export ceiling, aligned with the import side: a namespace beyond this
/// cannot round-trip anyway and must be reorganized first.
const MAX_EXPORT_DOCS: usize = MAX_IMPORT_ENTRIES;

impl WikiService {
    /// Imports an OKF bundle into one namespace. Concept paths become
    /// hierarchical slugs; existing documents (same namespace + slug) are
    /// updated; identical content is a checksum-idempotent no-op, so
    /// re-importing a bundle never grows the version chain.
    pub async fn import_bundle(
        &self,
        actor: String,
        input: WikiImportInput,
        now_ms: u64,
    ) -> Result<WikiImportOutput, WikiError> {
        if input.entries.is_empty() {
            return Err(WikiError::Invalid("bundle has no entries".into()));
        }
        if input.entries.len() > MAX_IMPORT_ENTRIES {
            return Err(WikiError::Invalid(format!(
                "bundle has {} entries, limit is {MAX_IMPORT_ENTRIES}",
                input.entries.len()
            )));
        }
        let namespace = input
            .namespace
            .as_deref()
            .map(str::trim)
            .filter(|ns| !ns.is_empty())
            .unwrap_or(DEFAULT_NAMESPACE)
            .to_string();

        let mut output = WikiImportOutput::default();
        // Distinct bundle paths that slugify to the same document ("A.md" vs
        // "a.md") converge last-write-wins into one document.
        for entry in &input.entries {
            match self.import_entry(&actor, &namespace, entry, now_ms).await {
                Ok(Some(doc)) => {
                    match doc.status {
                        WikiImportStatus::Created => output.created += 1,
                        WikiImportStatus::Updated => output.updated += 1,
                        WikiImportStatus::Unchanged => output.unchanged += 1,
                    }
                    output.docs.push(doc);
                }
                Ok(None) => output.skipped.push(WikiImportSkip {
                    path: entry.path.clone(),
                    reason: skip_reason(&entry.path),
                }),
                Err(err) => output.skipped.push(WikiImportSkip {
                    path: entry.path.clone(),
                    reason: err.to_string(),
                }),
            }
        }

        self.write_event(
            EVENT_IMPORT_COMPLETED,
            None,
            None,
            actor,
            BTreeMap::from([
                ("namespace".to_string(), Json::from(namespace)),
                ("created".to_string(), Json::from(output.created as u64)),
                ("updated".to_string(), Json::from(output.updated as u64)),
                ("unchanged".to_string(), Json::from(output.unchanged as u64)),
                (
                    "skipped".to_string(),
                    Json::from(output.skipped.len() as u64),
                ),
            ]),
            now_ms,
        )
        .await?;
        Ok(output)
    }

    async fn import_entry(
        &self,
        actor: &str,
        namespace: &str,
        entry: &WikiBundleEntry,
        now_ms: u64,
    ) -> Result<Option<WikiImportedDoc>, WikiError> {
        let Some(concept) = concept_path(&entry.path) else {
            return Ok(None);
        };
        let slug = slugify_path(&concept);

        let (frontmatter, body) = split_frontmatter(&entry.content);
        let parsed = frontmatter.as_deref().map(parse_frontmatter);
        let title = parsed
            .as_ref()
            .and_then(|fm| fm.title.clone())
            .or_else(|| markdown_title(body))
            .unwrap_or_else(|| concept.rsplit('/').next().unwrap_or(&concept).to_string());

        let mut metadata = BTreeMap::new();
        if let Some(raw) = &frontmatter {
            metadata.insert(FRONTMATTER_KEY.to_string(), Json::from(raw.clone()));
        }
        if let Some(kind) = parsed.as_ref().and_then(|fm| fm.r#type.clone()) {
            metadata.insert(OKF_TYPE_KEY.to_string(), Json::from(kind));
        }

        // Pure-CPU commit preparation (normalization, chunking, hashing)
        // stays outside the lock; document identity is resolved under it.
        let mut prepared = prepare_commit(WikiCommitInput {
            doc_id: None,
            parent_version: None,
            namespace: Some(namespace.to_string()),
            slug: Some(slug.clone()),
            title,
            content: body.to_string(),
            // A deleted `tags:` key must propagate on re-import: `None`
            // would read as "keep the stored tags" and the next export
            // would resurrect them.
            tags: Some(
                parsed
                    .as_ref()
                    .and_then(|fm| fm.tags.clone())
                    .unwrap_or_default(),
            ),
            // OKF cannot express enterprise ACLs (PRD §9): imported docs
            // keep their stored label or inherit the namespace default.
            acl_label: None,
            // `None` keeps the stored source_uri: deleting a `resource:` key
            // does not propagate through OKF (clear it via `wiki_commit`).
            source_uri: parsed.as_ref().and_then(|fm| fm.resource.clone()),
            message: Some(format!("okf import: {}", entry.path)),
            metadata: (!metadata.is_empty()).then_some(metadata),
        })?;

        // "Slug lookup + commit" runs under one write-lock acquisition:
        // concurrent imports of the same bundle would otherwise both miss
        // the lookup and duplicate the document under a suffixed slug
        // instead of converging on one create + idempotent replays. The
        // lock is per entry, so large bundles do not starve other writers.
        let _guard = self.write_lock.lock().await;
        if let Some(id) = self.find_doc_id_by_slug(namespace, &slug).await? {
            let doc = self.doc_record(id).await?;
            prepared.input.doc_id = Some(doc._id);
            prepared.input.parent_version = Some(doc.current_version);
        }
        let out = self
            .commit_prepared(actor.to_string(), prepared, now_ms)
            .await?;
        Ok(Some(WikiImportedDoc {
            path: entry.path.clone(),
            doc_id: out.doc.id,
            version_id: out.version.id,
            status: if out.created {
                WikiImportStatus::Created
            } else if out.idempotent {
                WikiImportStatus::Unchanged
            } else {
                WikiImportStatus::Updated
            },
        }))
    }

    /// Exports one namespace as an OKF bundle: concept `.md` files (verbatim
    /// frontmatter plus `x_anda_*` provenance keys), a root `index.md`, and
    /// a `manifest.json` with checksums so the bundle can be diffed and
    /// replayed.
    pub async fn export_bundle(
        &self,
        actor: String,
        namespace: Option<String>,
        now_ms: u64,
    ) -> Result<WikiExportOutput, WikiError> {
        let namespace = namespace
            .as_deref()
            .map(str::trim)
            .filter(|ns| !ns.is_empty())
            .unwrap_or(DEFAULT_NAMESPACE)
            .to_string();

        let mut docs: Vec<WikiDocInfo> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .list_docs(WikiListDocsInput {
                    namespace: Some(namespace.clone()),
                    status: Some(DOC_STATUS_ACTIVE.to_string()),
                    tag: None,
                    cursor: cursor.clone(),
                    limit: Some(100),
                })
                .await?;
            docs.extend(page.docs);
            if docs.len() > MAX_EXPORT_DOCS {
                // The whole namespace is buffered in memory and serialized
                // into a single response: refuse unbounded exports early.
                return Err(WikiError::Invalid(format!(
                    "namespace {namespace:?} has more than {MAX_EXPORT_DOCS} active documents; \
                     split content across namespaces and export them separately"
                )));
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        docs.sort_by(|a, b| a.slug.cmp(&b.slug));

        let mut entries = Vec::with_capacity(docs.len() + 2);
        let mut manifest_docs = Vec::with_capacity(docs.len());
        let mut index_lines = vec![
            "---".to_string(),
            format!("okf_version: \"{OKF_VERSION}\""),
            "---".to_string(),
            String::new(),
            format!("# {namespace}"),
            String::new(),
        ];

        for doc in &docs {
            let version = self.version_record(doc.current_version).await?;
            let raw = doc
                .metadata
                .get(FRONTMATTER_KEY)
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let okf_type = doc
                .metadata
                .get(OKF_TYPE_KEY)
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let frontmatter =
                render_frontmatter(raw.as_deref(), okf_type.as_deref(), doc, &version);
            let path = format!("{}.md", doc.slug);
            entries.push(WikiBundleEntry {
                path: path.clone(),
                content: format!("{frontmatter}{}", version.content),
            });
            index_lines.push(format!("- [{}]({})", doc.title, path));
            manifest_docs.push(serde_json::json!({
                "path": path,
                "title": doc.title,
                "doc_id": doc.id,
                "version_id": doc.current_version,
                "checksum": doc.current_checksum,
                "size": version.size,
            }));
        }

        index_lines.push(String::new());
        entries.push(WikiBundleEntry {
            path: "index.md".to_string(),
            content: index_lines.join("\n"),
        });
        let manifest = serde_json::json!({
            "okf_version": OKF_VERSION,
            "namespace": namespace,
            "exported_at": now_ms,
            "docs": manifest_docs,
        });
        entries.push(WikiBundleEntry {
            path: "manifest.json".to_string(),
            content: serde_json::to_string_pretty(&manifest)
                .map_err(|err| WikiError::Db(err.to_string()))?,
        });

        self.write_event(
            EVENT_EXPORT_COMPLETED,
            None,
            None,
            actor,
            BTreeMap::from([
                ("namespace".to_string(), Json::from(namespace.clone())),
                ("docs".to_string(), Json::from(docs.len() as u64)),
            ]),
            now_ms,
        )
        .await?;

        Ok(WikiExportOutput {
            namespace,
            entries,
            docs: docs.len(),
        })
    }
}

/// Bundle-relative concept path for an importable entry, or `None` for
/// reserved/non-markdown/invalid paths.
fn concept_path(path: &str) -> Option<String> {
    let path = path.trim().replace('\\', "/");
    let concept = path.strip_suffix(".md")?;
    if concept.is_empty() || path.starts_with('/') {
        return None;
    }
    let segments: Vec<&str> = concept.split('/').collect();
    if segments
        .iter()
        .any(|s| s.trim().is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    let base = segments.last()?.trim().to_ascii_lowercase();
    if base == "index" || base == "log" {
        return None; // reserved OKF files
    }
    Some(concept.to_string())
}

fn skip_reason(path: &str) -> String {
    let lower = path.trim().to_ascii_lowercase();
    if !lower.ends_with(".md") {
        return "not a markdown file".to_string();
    }
    if lower == "index.md"
        || lower == "log.md"
        || lower.ends_with("/index.md")
        || lower.ends_with("/log.md")
    {
        return "reserved OKF file".to_string();
    }
    "invalid path".to_string()
}

/// Splits an optional leading YAML frontmatter block. Returns the block
/// content without delimiters (LF-normalized, `x_anda_*` lines stripped)
/// and the body. Permissive: malformed frontmatter is treated as body.
pub(super) fn split_frontmatter(content: &str) -> (Option<String>, &str) {
    // The BOM is stripped in every branch so it never reaches the body (and
    // through it the checksum). `normalize_content` strips it again for
    // non-import commits.
    let stripped = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = match stripped.strip_prefix("---") {
        Some(rest) if rest.starts_with('\n') || rest.starts_with("\r\n") => rest,
        _ => return (None, stripped),
    };
    let block_start = if let Some(r) = rest.strip_prefix("\r\n") {
        r
    } else {
        &rest[1..]
    };

    let mut offset = 0usize;
    for line in block_start.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed == "---" || trimmed == "..." {
            let raw = &block_start[..offset];
            let body = &block_start[offset + line.len()..];
            let raw = raw
                .replace("\r\n", "\n")
                .lines()
                .filter(|l| !is_x_anda_line(l))
                .collect::<Vec<_>>()
                .join("\n");
            return (Some(raw), body);
        }
        offset += line.len();
    }
    (None, stripped)
}

fn is_x_anda_line(line: &str) -> bool {
    !line.starts_with(char::is_whitespace) && line.trim_start().starts_with("x_anda_")
}

#[derive(Debug, Default, Clone)]
pub(super) struct ParsedFrontmatter {
    pub title: Option<String>,
    pub tags: Option<Vec<String>>,
    pub resource: Option<String>,
    pub r#type: Option<String>,
}

/// Line-level extraction of the known OKF keys. Everything else stays in
/// the verbatim raw block; this parser never has to be complete.
pub(super) fn parse_frontmatter(raw: &str) -> ParsedFrontmatter {
    let mut out = ParsedFrontmatter::default();
    let lines: Vec<&str> = raw.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with(char::is_whitespace) || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        let scalar = |v: &str| -> Option<String> {
            // Folded/block scalars (`title: >` / `title: |`) are beyond the
            // line-level parser: fall back to the other title sources rather
            // than storing the indicator character as the value.
            if v.starts_with(['|', '>']) {
                return None;
            }
            let v = unquote(v);
            if v.is_empty() { None } else { Some(v) }
        };
        match key {
            "title" => out.title = scalar(value),
            "resource" => out.resource = scalar(value),
            "type" => out.r#type = scalar(value),
            "tags" => {
                if let Some(inline) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                    out.tags = Some(parse_list_items(inline.split(',')));
                } else if value.is_empty() {
                    let mut items = Vec::new();
                    while i < lines.len() {
                        let item = lines[i].trim_start();
                        let Some(item) = item.strip_prefix("- ") else {
                            break;
                        };
                        items.push(item);
                        i += 1;
                    }
                    out.tags = Some(parse_list_items(items.into_iter()));
                } else {
                    out.tags = Some(parse_list_items(std::iter::once(value)));
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_list_items<'a>(items: impl Iterator<Item = &'a str>) -> Vec<String> {
    items
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return value[1..value.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("''", "'");
    }
    value.to_string()
}

fn yaml_scalar(value: &str) -> String {
    let plain_safe = !value.is_empty()
        && !value.starts_with(char::is_whitespace)
        && !value.ends_with(char::is_whitespace)
        && !value.contains(['"', '\'', ':', '#', '[', ']', '{', '}', '\n'])
        && !value.starts_with(['-', '&', '*', '!', '|', '>', '%', '@']);
    if plain_safe {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn tags_line(tags: &[String]) -> String {
    format!(
        "tags: [{}]",
        tags.iter()
            .map(|t| yaml_scalar(t))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The frontmatter block (no delimiters) synthesized for documents that
/// never carried one, or whose stored block drifted (see
/// [`frontmatter_is_stale`]).
fn synthesized_frontmatter(
    okf_type: Option<&str>,
    title: &str,
    tags: &[String],
    source_uri: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("type: {}", yaml_scalar(okf_type.unwrap_or("Document"))),
        format!("title: {}", yaml_scalar(title)),
    ];
    if !tags.is_empty() {
        lines.push(tags_line(tags));
    }
    if let Some(resource) = source_uri {
        lines.push(format!("resource: {}", yaml_scalar(resource)));
    }
    lines.join("\n")
}

/// Documents edited through `wiki_commit` after an OKF import drift from
/// their stored frontmatter block. Exporting the stale block would make the
/// bundle disagree with its own manifest and silently revert title/tags on
/// replay, so a drifted block is discarded wholesale and export falls back
/// to the synthesized form (unknown keys and comments are lost on drift;
/// the no-drift path stays byte-stable so replays do not churn versions).
/// A missing `title:` key falls back to the body heading exactly like
/// import does, so such blocks are not spuriously treated as drifted.
fn frontmatter_is_stale(raw: &str, doc: &WikiDocInfo, body: &str) -> bool {
    let parsed = parse_frontmatter(raw);
    let title = parsed.title.or_else(|| markdown_title(body));
    title.as_deref() != Some(doc.title.as_str())
        || parsed.tags.unwrap_or_default() != doc.tags
        || parsed.resource.as_deref() != doc.source_uri.as_deref()
}

/// Assembles the exported frontmatter: the stored verbatim block (when no
/// known key drifted) or the synthesized one, plus fresh `x_anda_*`
/// provenance keys.
fn render_frontmatter(
    raw: Option<&str>,
    okf_type: Option<&str>,
    doc: &WikiDocInfo,
    version: &super::WikiVersionRecord,
) -> String {
    let block = match raw {
        Some(raw) if !frontmatter_is_stale(raw, doc, &version.content) => raw.to_string(),
        _ => synthesized_frontmatter(okf_type, &doc.title, &doc.tags, doc.source_uri.as_deref()),
    };
    let mut lines: Vec<String> = block
        .lines()
        .filter(|l| !is_x_anda_line(l))
        .map(str::to_string)
        .collect();
    lines.push(format!("x_anda_doc_id: {}", doc.id));
    lines.push(format!("x_anda_version_id: {}", doc.current_version));
    lines.push(format!("x_anda_checksum: {}", version.checksum));
    format!("---\n{}\n---\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_extracts_block_and_strips_x_anda() {
        let content = "---\ntype: SOP\n# a comment\ncustom_field: 保留我\nx_anda_doc_id: 7\n---\n\n# Body\ntext\n";
        let (raw, body) = split_frontmatter(content);
        let raw = raw.unwrap();
        assert!(raw.contains("custom_field: 保留我"));
        assert!(raw.contains("# a comment"));
        assert!(!raw.contains("x_anda_doc_id"));
        assert_eq!(body, "\n# Body\ntext\n");

        // Permissive: no frontmatter, unclosed frontmatter.
        assert_eq!(split_frontmatter("# Just body"), (None, "# Just body"));
        let unclosed = "---\ntype: X\nno closing";
        assert_eq!(split_frontmatter(unclosed), (None, unclosed));
    }

    #[test]
    fn parse_frontmatter_reads_known_keys() {
        let fm = parse_frontmatter(
            "type: API Endpoint\ntitle: \"Recall \\\"v1\\\"\"\nresource: anda://x\ntags: [api, recall]\nunknown: kept",
        );
        assert_eq!(fm.r#type.as_deref(), Some("API Endpoint"));
        assert_eq!(fm.title.as_deref(), Some("Recall \"v1\""));
        assert_eq!(fm.resource.as_deref(), Some("anda://x"));
        assert_eq!(fm.tags, Some(vec!["api".to_string(), "recall".to_string()]));

        let block = parse_frontmatter("tags:\n  - a\n  - 'b c'\ntitle: t");
        assert_eq!(block.tags, Some(vec!["a".to_string(), "b c".to_string()]));
        assert_eq!(block.title.as_deref(), Some("t"));

        // Folded/block scalars are unsupported: fall back instead of storing
        // the indicator character.
        let folded = parse_frontmatter("title: >\n  folded text\ntype: Doc");
        assert_eq!(folded.title, None);
        assert_eq!(folded.r#type.as_deref(), Some("Doc"));
    }

    fn doc_info(title: &str, tags: &[&str], source_uri: Option<&str>) -> WikiDocInfo {
        WikiDocInfo {
            id: 1,
            namespace: "kb".to_string(),
            slug: "d".to_string(),
            title: title.to_string(),
            status: "active".to_string(),
            current_version: 2,
            current_checksum: String::new(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            acl_label: String::new(),
            source_uri: source_uri.map(str::to_string),
            metadata: BTreeMap::new(),
            created_by: String::new(),
            updated_by: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn frontmatter_staleness_detects_known_key_drift() {
        let raw = "type: Guide\n# keep this comment\ntitle: 旧标题\ntags: [old]\nunknown: kept";

        // No drift: the stored block exports verbatim (byte-stable replays).
        assert!(!frontmatter_is_stale(
            raw,
            &doc_info("旧标题", &["old"], None),
            ""
        ));

        // Any known-key drift discards the block wholesale on export.
        assert!(frontmatter_is_stale(
            raw,
            &doc_info("新标题", &["old"], None),
            ""
        ));
        assert!(frontmatter_is_stale(
            raw,
            &doc_info("旧标题", &["new"], None),
            ""
        ));
        assert!(frontmatter_is_stale(
            raw,
            &doc_info("旧标题", &["old"], Some("anda://x")),
            ""
        ));

        // A block without `title:` falls back to the body heading (the same
        // derivation import uses): not drift.
        assert!(!frontmatter_is_stale(
            "type: Guide\nunknown: kept",
            &doc_info("正文标题", &[], None),
            "# 正文标题\n\n正文。\n"
        ));
    }

    #[test]
    fn synthesized_frontmatter_matches_export_shape() {
        let block = synthesized_frontmatter(None, "标题", &["a".to_string()], Some("anda://x"));
        assert_eq!(
            block,
            "type: Document\ntitle: 标题\ntags: [a]\nresource: \"anda://x\""
        );
    }

    #[test]
    fn yaml_scalar_quotes_only_when_needed() {
        assert_eq!(yaml_scalar("部署指南"), "部署指南");
        assert_eq!(yaml_scalar("a: b"), "\"a: b\"");
        assert_eq!(yaml_scalar("he said \"hi\""), "\"he said \\\"hi\\\"\"");
        assert_eq!(unquote(&yaml_scalar("he said \"hi\"")), "he said \"hi\"");
    }

    #[test]
    fn concept_path_filters_reserved_and_invalid() {
        assert_eq!(
            concept_path("guides/setup.md").as_deref(),
            Some("guides/setup")
        );
        assert_eq!(concept_path("指南.md").as_deref(), Some("指南"));
        assert!(concept_path("index.md").is_none());
        assert!(concept_path("a/log.md").is_none());
        assert!(concept_path("manifest.json").is_none());
        assert!(concept_path("../evil.md").is_none());
        assert!(concept_path("/abs.md").is_none());
        assert!(concept_path("a//b.md").is_none());
    }
}
