//! Wire types and error semantics for the wiki subsystem.

use anda_db::{error::DBError, schema::Json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::model::WikiDocRecord;

/// Default namespace for documents committed without one.
pub const DEFAULT_NAMESPACE: &str = "default";

/// Namespace the retrieval-eval fixture corpus is imported into, keeping
/// eval documents isolated from real space content (digest and listings
/// exclude it).
pub const EVAL_NAMESPACE: &str = "wiki_eval";

/// Caps enforced by [`WikiCommitInput::validate`] on every write path.
pub const MAX_TAGS: usize = 64;
pub const MAX_TAG_CHARS: usize = 120;
pub const MAX_SLUG_CHARS: usize = 256;
pub const MAX_METADATA_KEYS: usize = 64;
pub const MAX_METADATA_BYTES: usize = 64 * 1024;

/// Wiki errors carry enough structure for HTTP status mapping (409/413/404)
/// and for agents to self-correct (a conflict names the current version).
#[derive(Debug, Clone, Serialize)]
pub enum WikiError {
    /// CAS failure: the document moved past `parent_version`. Re-read,
    /// merge, and retry with the returned `current_version`.
    Conflict {
        current_version: u64,
        /// Checksum of the current content (PRD §13): lets the caller detect
        /// "same content, different version" without an extra read.
        current_checksum: String,
        updated_by: String,
        updated_at: u64,
    },
    /// Content exceeds the per-document byte limit.
    TooLarge {
        size: usize,
        max: usize,
    },
    NotFound(String),
    /// Invalid input or state (e.g. committing to an archived document).
    Invalid(String),
    Db(String),
}

impl std::fmt::Display for WikiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict {
                current_version,
                current_checksum,
                updated_by,
                updated_at,
            } => write!(
                f,
                "commit conflict: document is at version {current_version} (checksum {current_checksum}, updated by {updated_by} at {updated_at}); re-read, merge, and retry with parent_version={current_version}"
            ),
            Self::TooLarge { size, max } => write!(
                f,
                "content too large: {size} bytes exceeds the {max} byte limit; split the document"
            ),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::Invalid(msg) => write!(f, "invalid: {msg}"),
            Self::Db(msg) => write!(f, "db error: {msg}"),
        }
    }
}

impl WikiError {
    /// Structured payload a caller needs to recover from this error: `Some`
    /// only for [`WikiError::Conflict`], carrying the CAS retry protocol
    /// fields (re-read, merge, retry with `current_version`). Both the HTTP
    /// and MCP error renderers must attach it, so agents on either channel
    /// can follow the commit instructions.
    pub fn retry_data(&self) -> Option<Json> {
        match self {
            Self::Conflict {
                current_version,
                current_checksum,
                updated_by,
                updated_at,
            } => Some(serde_json::json!({
                "current_version": current_version,
                "current_checksum": current_checksum,
                "updated_by": updated_by,
                "updated_at": updated_at,
            })),
            _ => None,
        }
    }
}

impl std::error::Error for WikiError {}

impl From<DBError> for WikiError {
    fn from(err: DBError) -> Self {
        match err {
            DBError::NotFound { name, path, .. } => Self::NotFound(format!("{path}/{name}")),
            err => Self::Db(format!("{err:?}")),
        }
    }
}

/// One write primitive: commit. `doc_id: None` creates; `Some` updates and
/// then requires `parent_version` for CAS.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiCommitInput {
    #[serde(default)]
    pub doc_id: Option<u64>,
    #[serde(default)]
    pub parent_version: Option<u64>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
    /// `None` keeps the stored tags on update; `Some` replaces them.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// ACL label. `None` keeps the stored label (or inherits the namespace
    /// default on create); `Some("")` clears it.
    #[serde(default)]
    pub acl_label: Option<String>,
    /// `None` keeps the stored value on update; `Some("")` clears it (a
    /// deleted `resource:` key must propagate on OKF re-import, mirroring
    /// tags).
    #[serde(default)]
    pub source_uri: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    /// `None` keeps the stored metadata on update; `Some` replaces it.
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, Json>>,
}

impl WikiCommitInput {
    /// Builds a create-commit from bare Markdown, deriving the title from
    /// the first heading.
    pub fn from_markdown(content: String) -> Self {
        let title = markdown_title(&content).unwrap_or_else(|| "Untitled".to_string());
        Self {
            title,
            content,
            ..Default::default()
        }
    }

    pub fn normalize(&mut self) {
        self.title = self.title.trim().to_string();
        if self.title.is_empty()
            && let Some(title) = markdown_title(&self.content)
        {
            self.title = title;
        }
        normalize_opt(&mut self.namespace);
        normalize_opt(&mut self.slug);
        normalize_opt(&mut self.message);
        // Trim only — `Some("")` is the explicit "clear" marker (like
        // `acl_label`), so it must survive normalization instead of
        // collapsing into "keep the stored value".
        if let Some(uri) = &mut self.source_uri {
            *uri = uri.trim().to_string();
        }
        if let Some(tags) = &self.tags {
            self.tags = Some(normalize_tags(tags));
        }
    }

    /// Bounds checks beyond normalization (call after [`Self::normalize`]):
    /// tag count/length, slug length and metadata size are otherwise
    /// unbounded caller input persisted verbatim.
    pub fn validate(&self) -> Result<(), WikiError> {
        if let Some(tags) = &self.tags {
            if tags.len() > MAX_TAGS {
                return Err(WikiError::Invalid(format!(
                    "too many tags: {} exceeds the {MAX_TAGS} limit",
                    tags.len()
                )));
            }
            if let Some(tag) = tags.iter().find(|t| t.chars().count() > MAX_TAG_CHARS) {
                return Err(WikiError::Invalid(format!(
                    "tag {tag:?} exceeds {MAX_TAG_CHARS} characters"
                )));
            }
        }
        if let Some(slug) = &self.slug
            && slug.chars().count() > MAX_SLUG_CHARS
        {
            return Err(WikiError::Invalid(format!(
                "slug exceeds {MAX_SLUG_CHARS} characters"
            )));
        }
        if let Some(metadata) = &self.metadata {
            if metadata.len() > MAX_METADATA_KEYS {
                return Err(WikiError::Invalid(format!(
                    "too many metadata keys: {} exceeds the {MAX_METADATA_KEYS} limit",
                    metadata.len()
                )));
            }
            let size: usize = metadata
                .iter()
                .map(|(key, value)| key.len() + value.to_string().len())
                .sum();
            if size > MAX_METADATA_BYTES {
                return Err(WikiError::Invalid(format!(
                    "metadata too large: ~{size} bytes exceeds the {MAX_METADATA_BYTES} byte limit"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiCommitOutput {
    pub doc: WikiDocInfo,
    pub version: WikiVersionInfo,
    pub chunks: usize,
    pub created: bool,
    /// True when the commit was a no-op because nothing changed.
    pub idempotent: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiDocInfo {
    pub id: u64,
    pub namespace: String,
    pub slug: String,
    pub title: String,
    pub status: String,
    pub current_version: u64,
    pub current_checksum: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub acl_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Json>,
    pub created_by: String,
    pub updated_by: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl From<WikiDocRecord> for WikiDocInfo {
    fn from(doc: WikiDocRecord) -> Self {
        Self {
            id: doc._id,
            namespace: doc.namespace,
            slug: doc.slug,
            title: doc.title,
            status: doc.status,
            current_version: doc.current_version,
            current_checksum: doc.current_checksum,
            tags: doc.tags,
            acl_label: doc.acl_label,
            source_uri: doc.source_uri,
            metadata: doc.metadata,
            created_by: doc.created_by,
            updated_by: doc.updated_by,
            created_at: doc.created_at,
            updated_at: doc.updated_at,
        }
    }
}

/// Version metadata without content (content is read via `read`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiVersionInfo {
    pub id: u64,
    pub doc_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_version: Option<u64>,
    pub checksum: String,
    pub size: u64,
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub created_at: u64,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WikiSearchMode {
    #[default]
    Chunks,
    Docs,
}

/// The tool schema declares these fields nullable ("null searches all") and
/// strict mode forces the model to send every key, but `#[serde(default)]`
/// only covers a *missing* key — an explicit `null` fails on plain
/// `Vec`/enum fields. Map `null` to the default so schema-conforming tool
/// calls deserialize.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiSearchInput {
    pub query: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub namespaces: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub doc_ids: Vec<u64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub mode: WikiSearchMode,
    /// Neighbor expansion: widen each hit by up to N adjacent chunks on both
    /// sides (0–2, default 0). Hits expand independently (nearby hits may
    /// repeat overlapping context); each widened citation range stays
    /// verifiable.
    #[serde(default)]
    pub expand: Option<u8>,
}

impl WikiSearchInput {
    pub fn from_query(query: String) -> Self {
        Self {
            query,
            ..Default::default()
        }
    }

    pub fn normalize(&mut self) {
        self.query = self.query.trim().to_string();
        self.namespaces = normalize_tags(&self.namespaces);
        self.tags = normalize_tags(&self.tags);
        self.doc_ids.sort_unstable();
        self.doc_ids.dedup();
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiCitation {
    /// `wiki://{space}/{doc_id}@{version_id}#{start}-{end}`
    pub uri: String,
    pub doc_id: u64,
    pub version_id: u64,
    pub chunk_id: u64,
    pub heading_path: Vec<String>,
    pub anchor: String,
    pub byte_range: (u64, u64),
    pub checksum: String,
    pub quote: String,
}

/// One retrieval hit. PRD §5.1 sketches a `score` field; AndaDB's search
/// API returns relevance-ordered ids without exposing BM25 scores, so hits
/// carry rank order only.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiHit {
    pub text: String,
    pub doc_title: String,
    pub heading_path: Vec<String>,
    pub citation: WikiCitation,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiSearchOutput {
    pub hits: Vec<WikiHit>,
    pub total_docs_matched: usize,
}

/// Progressive disclosure selector: browse the TOC, read one section, slice
/// a byte range, or read the whole document (bounded).
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WikiSelector {
    Toc,
    Section {
        anchor: String,
    },
    Range {
        start: u64,
        end: u64,
    },
    #[default]
    Full,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiReadInput {
    pub doc_id: u64,
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub selector: WikiSelector,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiTocEntry {
    pub anchor: String,
    pub heading_path: Vec<String>,
    pub byte_start: u64,
    pub byte_end: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiReadOutput {
    pub doc_id: u64,
    pub version_id: u64,
    pub is_current: bool,
    pub title: String,
    pub status: String,
    pub checksum: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toc: Option<Vec<WikiTocEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Byte range of `content` within the version, when content is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_range: Option<(u64, u64)>,
    pub truncated: bool,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiVerifyInput {
    /// `wiki://{space}/{doc_id}@{version_id}#{start}-{end}`; explicit fields
    /// below are used when absent.
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub doc_id: Option<u64>,
    #[serde(default)]
    pub version_id: Option<u64>,
    #[serde(default)]
    pub byte_range: Option<(u64, u64)>,
    /// When provided, compared against the recomputed chunk checksum.
    #[serde(default)]
    pub checksum: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WikiVerifyStatus {
    /// Checksum matches and the cited version is current.
    Valid,
    /// Checksum matches but a newer version exists.
    Superseded,
    /// The citation does not match stored content: storage corruption signal.
    Invalid,
    NotFound,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiVerifyOutput {
    pub status: WikiVerifyStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<u64>,
    /// Recomputed checksum for the cited range, when resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiListDocsInput {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiDocListOutput {
    pub docs: Vec<WikiDocInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiVersionListOutput {
    pub versions: Vec<WikiVersionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiEventInfo {
    pub id: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<u64>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: BTreeMap<String, Json>,
    pub created_at: u64,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiEventListOutput {
    pub events: Vec<WikiEventInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One file of an OKF bundle: a bundle-relative path and its full content.
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct WikiBundleEntry {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiImportInput {
    pub entries: Vec<WikiBundleEntry>,
    /// Target namespace; defaults to "default". Bundles round-trip per
    /// namespace: export paths carry no namespace prefix.
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WikiImportStatus {
    Created,
    Updated,
    Unchanged,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiImportedDoc {
    pub path: String,
    pub doc_id: u64,
    pub version_id: u64,
    pub status: WikiImportStatus,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WikiImportSkip {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiImportOutput {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub docs: Vec<WikiImportedDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<WikiImportSkip>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiExportOutput {
    pub namespace: String,
    /// Concept `.md` files plus generated `index.md` and `manifest.json`.
    pub entries: Vec<WikiBundleEntry>,
    pub docs: usize,
}

/// Caller identity for scoped (HTTP/MCP) wiki reads. `labels: None` means
/// unrestricted (CWT holders and legacy space tokens); `Some(list)` grants
/// unlabeled content plus the listed labels. The filter runs inside the
/// same AndaDB query as retrieval, so over-broad results are structurally
/// impossible.
#[derive(Debug, Clone, Default)]
pub struct WikiAccess {
    pub actor: String,
    pub labels: Option<Vec<String>>,
}

impl WikiAccess {
    pub fn allows(&self, label: &str) -> bool {
        match &self.labels {
            None => true,
            Some(labels) => label.is_empty() || labels.iter().any(|l| l == label),
        }
    }
}

/// Last housekeeping stale scan, persisted for `SpaceInfo`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WikiStaleReport {
    pub stale_docs: u64,
    pub checked_docs: u64,
    pub checked_at: u64,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WikiSweepReport {
    pub docs_removed: usize,
    pub versions_removed: usize,
    pub chunks_removed: usize,
    pub chunks_repaired: usize,
}

impl WikiSweepReport {
    pub fn is_empty(&self) -> bool {
        self.docs_removed == 0
            && self.versions_removed == 0
            && self.chunks_removed == 0
            && self.chunks_repaired == 0
    }
}

/// Builds the canonical citation URI.
pub fn citation_uri(space_id: &str, doc_id: u64, version_id: u64, start: u64, end: u64) -> String {
    format!("wiki://{space_id}/{doc_id}@{version_id}#{start}-{end}")
}

/// Parses a `wiki://{space}/{doc_id}@{version_id}#{start}-{end}` URI into
/// `(space, doc_id, version_id, start, end)`.
pub fn parse_citation_uri(uri: &str) -> Option<(String, u64, u64, u64, u64)> {
    let rest = uri.strip_prefix("wiki://")?;
    let (space, rest) = rest.split_once('/')?;
    let (doc, rest) = rest.split_once('@')?;
    let (version, range) = rest.split_once('#')?;
    let (start, end) = range.split_once('-')?;
    Some((
        space.to_string(),
        doc.parse().ok()?,
        version.parse().ok()?,
        start.parse().ok()?,
        end.parse().ok()?,
    ))
}

/// Title derivation from the first ATX heading. Fence-aware: a `# comment`
/// inside a code block never becomes the title (the v1 regression PRD §4.2
/// calls out), matching the chunker's fence rules exactly.
pub(crate) fn markdown_title(content: &str) -> Option<String> {
    super::chunk::first_heading_title(content)
}

fn normalize_opt(value: &mut Option<String>) {
    if let Some(inner) = value {
        let trimmed = inner.trim();
        if trimmed.is_empty() {
            *value = None;
        } else if trimmed != inner {
            *inner = trimmed.to_string();
        }
    }
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    // Order-preserving global dedup: `Vec::dedup` only removes adjacent
    // duplicates, so ["a", "b", "a"] used to keep the second "a".
    let mut seen = std::collections::BTreeSet::new();
    tags.iter()
        .map(|tag| tag.trim())
        .filter(|tag| !tag.is_empty())
        .filter(|tag| seen.insert(tag.to_string()))
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citation_uri_round_trips() {
        let uri = citation_uri("my_space", 7, 42, 100, 260);
        assert_eq!(uri, "wiki://my_space/7@42#100-260");
        let (space, doc, ver, start, end) = parse_citation_uri(&uri).unwrap();
        assert_eq!(space, "my_space");
        assert_eq!((doc, ver, start, end), (7, 42, 100, 260));
        assert!(parse_citation_uri("wiki://bad").is_none());
        assert!(parse_citation_uri("http://x/1@2#3-4").is_none());
    }

    #[test]
    fn markdown_title_finds_first_heading() {
        assert_eq!(
            markdown_title("intro\n\n## 部署指南 ##\nbody").as_deref(),
            Some("部署指南")
        );
        assert_eq!(markdown_title("no headings"), None);
    }

    #[test]
    fn tags_dedupe_globally_and_commit_input_caps_hold() {
        let mut input = WikiCommitInput {
            title: "t".to_string(),
            content: "# t\n\nbody\n".to_string(),
            tags: Some(vec![
                "a".to_string(),
                " b ".to_string(),
                "a".to_string(), // non-adjacent duplicate
                "b".to_string(),
                String::new(),
            ]),
            ..Default::default()
        };
        input.normalize();
        assert_eq!(input.tags, Some(vec!["a".to_string(), "b".to_string()]));
        assert!(input.validate().is_ok());

        let too_many = WikiCommitInput {
            tags: Some((0..MAX_TAGS + 1).map(|i| format!("t{i}")).collect()),
            ..Default::default()
        };
        assert!(matches!(too_many.validate(), Err(WikiError::Invalid(_))));

        let long_tag = WikiCommitInput {
            tags: Some(vec!["长".repeat(MAX_TAG_CHARS + 1)]),
            ..Default::default()
        };
        assert!(matches!(long_tag.validate(), Err(WikiError::Invalid(_))));

        let long_slug = WikiCommitInput {
            slug: Some("s/".repeat(MAX_SLUG_CHARS)),
            ..Default::default()
        };
        assert!(matches!(long_slug.validate(), Err(WikiError::Invalid(_))));

        let fat_metadata = WikiCommitInput {
            metadata: Some(BTreeMap::from([(
                "k".to_string(),
                Json::from("x".repeat(MAX_METADATA_BYTES)),
            )])),
            ..Default::default()
        };
        assert!(matches!(
            fat_metadata.validate(),
            Err(WikiError::Invalid(_))
        ));
    }
}
