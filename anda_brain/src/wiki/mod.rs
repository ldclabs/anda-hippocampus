//! Anda Wiki: the semantic reference-memory layer of a Space.
//!
//! Git-model document store over AndaDB: immutable version commits with CAS
//! concurrency control, a chunk retrieval plane queried by native BM25
//! (jieba tokenizer), verifiable byte-range citations, and an append-only
//! audit log. The query path is deterministic and LLM-free; answer
//! composition belongs to the calling agent.

mod chunk;
mod digest;
pub mod evalset;
mod model;
mod okf;
mod tool;
mod types;

pub use chunk::{CHUNKER_VERSION, chunk_markdown, normalize_content, slugify, slugify_path};
pub use digest::{DigestedFact, WIKI_DIGEST_EXTRACTOR, WikiDigest, WikiDigestReport};
pub use model::*;
pub use okf::OKF_VERSION;
pub use tool::{WikiCommitTool, WikiReadTool, WikiSearchTool};
pub use types::*;

use anda_db::{
    collection::{Collection, CollectionConfig},
    database::AndaDB,
    error::DBError,
    query::{Filter, Fv, Query, RangeQuery, Search},
    schema::Json,
};
use anda_db_tfs::jieba_tokenizer;
use std::{collections::BTreeMap, sync::Arc};

use chunk::{checksum_for, chunk_checksum, floor_char_boundary, quote_excerpt};

/// Reject documents larger than this after normalization (1 MiB).
pub const MAX_DOC_BYTES: usize = 1024 * 1024;
/// `Full` reads return at most this many bytes (truncated on a char boundary).
pub const MAX_READ_BYTES: usize = 256 * 1024;
/// Initializing documents (`current_version == 0`) older than this are
/// reclaimed by the orphan sweep.
const SENTINEL_TTL_MS: u64 = 10 * 60 * 1000;
/// Page size for internal full-collection scans.
const SCAN_PAGE: usize = 200;
/// Cap for `Include` filter key lists (AndaDB rejects larger ones).
const MAX_INCLUDE_KEYS: usize = 4096;

#[derive(Clone)]
pub struct WikiService {
    space_id: String,
    docs: Arc<Collection>,
    versions: Arc<Collection>,
    chunks: Arc<Collection>,
    events: Arc<Collection>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl WikiService {
    pub async fn connect(space_id: String, db: Arc<AndaDB>) -> Result<Self, DBError> {
        let docs = db
            .open_or_create_collection(
                WikiDocRecord::schema()?,
                CollectionConfig {
                    name: "wiki_docs".to_string(),
                    description: "Wiki document registry".to_string(),
                },
                async |c| init_wiki_docs(c).await,
            )
            .await?;
        let versions = db
            .open_or_create_collection(
                WikiVersionRecord::schema()?,
                CollectionConfig {
                    name: "wiki_versions".to_string(),
                    description: "Immutable wiki version commits".to_string(),
                },
                async |c| init_wiki_versions(c).await,
            )
            .await?;
        let chunks = db
            .open_or_create_collection(
                WikiChunkRecord::schema()?,
                CollectionConfig {
                    name: "wiki_chunks".to_string(),
                    description: "Wiki retrieval chunks".to_string(),
                },
                async |c| init_wiki_chunks(c).await,
            )
            .await?;
        let events = db
            .open_or_create_collection(
                WikiEventRecord::schema()?,
                CollectionConfig {
                    name: "wiki_events".to_string(),
                    description: "Wiki audit events".to_string(),
                },
                async |c| init_wiki_events(c).await,
            )
            .await?;

        Ok(Self {
            space_id,
            docs,
            versions,
            chunks,
            events,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn docs_count(&self) -> usize {
        self.docs.metadata().stats.num_documents as usize
    }

    pub fn chunks_count(&self) -> usize {
        self.chunks.metadata().stats.num_documents as usize
    }

    // ─── Write path ─────────────────────────────────────────────────────────

    /// The single write primitive: an immutable commit (see PRD §4.1).
    ///
    /// Ordering is crash-safe: version row → inactive chunks → doc flip (the
    /// activation point) → chunk activation → superseded-chunk removal →
    /// event. A crash at any step leaves either the old version fully
    /// visible or the new one; leftovers are invisible and reclaimed by
    /// [`WikiService::orphan_sweep`].
    pub async fn commit(
        &self,
        actor: String,
        mut input: WikiCommitInput,
        now_ms: u64,
    ) -> Result<WikiCommitOutput, WikiError> {
        input.normalize();
        if input.title.is_empty() {
            return Err(WikiError::Invalid(
                "title is required (or provide a markdown heading)".into(),
            ));
        }
        let content = normalize_content(&input.content);
        if content.is_empty() {
            return Err(WikiError::Invalid("content cannot be empty".into()));
        }
        if content.len() > MAX_DOC_BYTES {
            return Err(WikiError::TooLarge {
                size: content.len(),
                max: MAX_DOC_BYTES,
            });
        }
        let checksum = checksum_for([content.as_bytes()]);

        let _guard = self.write_lock.lock().await;

        let existing = match input.doc_id {
            Some(id) => {
                let doc = self.doc_record(id).await?;
                if doc.status == DOC_STATUS_ARCHIVED {
                    return Err(WikiError::Invalid(format!(
                        "document {id} is archived; restore it before committing"
                    )));
                }
                let parent = input.parent_version.ok_or_else(|| {
                    WikiError::Invalid("parent_version is required when updating".into())
                })?;
                if parent != doc.current_version {
                    return Err(WikiError::Conflict {
                        current_version: doc.current_version,
                        updated_by: doc.updated_by.clone(),
                        updated_at: doc.updated_at,
                    });
                }
                Some(doc)
            }
            None => None,
        };

        // Idempotent short-circuit: nothing changed, nothing written.
        if let Some(doc) = &existing
            && doc.current_checksum == checksum
            && doc.title == input.title
            && input.tags.as_ref().is_none_or(|tags| *tags == doc.tags)
            && input
                .namespace
                .as_ref()
                .is_none_or(|ns| *ns == doc.namespace)
            && input.slug.as_ref().is_none_or(|s| *s == doc.slug)
            && input
                .source_uri
                .as_ref()
                .is_none_or(|s| Some(s) == doc.source_uri.as_ref())
            && input.metadata.as_ref().is_none_or(|m| *m == doc.metadata)
        {
            let version = self.version_record(doc.current_version).await?;
            let chunks = self.chunk_ids_of(doc._id, doc.current_version).await?.len();
            return Ok(WikiCommitOutput {
                doc: doc.clone().into(),
                version: version_info(&version, doc.current_version),
                chunks,
                created: false,
                idempotent: true,
            });
        }

        // None keeps stored tags on update; empty documents start with none.
        let tags = match (&input.tags, &existing) {
            (Some(tags), _) => tags.clone(),
            (None, Some(doc)) => doc.tags.clone(),
            (None, None) => Vec::new(),
        };

        let (doc_id, created, namespace, slug, prev_version) = match &existing {
            Some(doc) => {
                let namespace = input
                    .namespace
                    .clone()
                    .unwrap_or_else(|| doc.namespace.clone());
                let want_slug = input.slug.clone().unwrap_or_else(|| doc.slug.clone());
                let slug = if want_slug != doc.slug || namespace != doc.namespace {
                    self.unique_slug(&namespace, &want_slug, Some(doc._id), now_ms)
                        .await?
                } else {
                    doc.slug.clone()
                };
                (doc._id, false, namespace, slug, doc.current_version)
            }
            None => {
                let namespace = input
                    .namespace
                    .clone()
                    .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
                let base = input.slug.clone().unwrap_or_else(|| slugify(&input.title));
                let slug = self.unique_slug(&namespace, &base, None, now_ms).await?;
                let doc = WikiDocRecord {
                    _id: 0,
                    namespace: namespace.clone(),
                    slug: slug.clone(),
                    title: input.title.clone(),
                    status: DOC_STATUS_ACTIVE.to_string(),
                    current_version: 0,
                    current_checksum: String::new(),
                    tags: tags.clone(),
                    source_uri: input.source_uri.clone(),
                    metadata: input.metadata.clone().unwrap_or_default(),
                    created_by: actor.clone(),
                    updated_by: actor.clone(),
                    created_at: now_ms,
                    updated_at: now_ms,
                };
                let id = self.docs.add_from(&doc).await?;
                (id, true, namespace, slug, 0)
            }
        };

        // 1) Immutable version row.
        let version = WikiVersionRecord {
            _id: 0,
            doc_id,
            parent_version: (!created).then_some(prev_version),
            checksum: checksum.clone(),
            content: content.clone(),
            size: content.len() as u64,
            author: actor.clone(),
            message: input.message.clone(),
            created_at: now_ms,
        };
        let version_id = self.versions.add_from(&version).await?;

        // 2) Chunks, inactive until the doc flips.
        let plan = chunk_markdown(&content);
        let mut chunk_ids = Vec::with_capacity(plan.drafts.len());
        for (idx, draft) in plan.drafts.iter().enumerate() {
            let text = &content[draft.byte_start..draft.byte_end];
            let record = WikiChunkRecord {
                _id: 0,
                doc_id,
                version_id,
                namespace: namespace.clone(),
                current: 0,
                title: input.title.clone(),
                heading_path: draft.heading_path.clone(),
                anchor: draft.anchor.clone(),
                ordinal: idx as u64,
                text: text.to_string(),
                byte_start: draft.byte_start as u64,
                byte_end: draft.byte_end as u64,
                checksum: chunk_checksum(&checksum, draft.byte_start, draft.byte_end, text),
                chunker_version: CHUNKER_VERSION as u64,
                acl_label: None,
            };
            chunk_ids.push(self.chunks.add_from(&record).await?);
        }

        // 3) Activation point: one doc update makes the commit effective.
        let mut fields = BTreeMap::from([
            ("namespace".to_string(), Fv::Text(namespace.clone())),
            ("slug".to_string(), Fv::Text(slug)),
            ("title".to_string(), Fv::Text(input.title.clone())),
            (
                "status".to_string(),
                Fv::Text(DOC_STATUS_ACTIVE.to_string()),
            ),
            ("current_version".to_string(), Fv::U64(version_id)),
            ("current_checksum".to_string(), Fv::Text(checksum.clone())),
            (
                "tags".to_string(),
                Fv::Array(tags.iter().cloned().map(Fv::Text).collect()),
            ),
            ("updated_by".to_string(), Fv::Text(actor.clone())),
            ("updated_at".to_string(), Fv::U64(now_ms)),
        ]);
        // None keeps the stored source_uri/metadata on update.
        if let Some(source_uri) = &input.source_uri {
            fields.insert("source_uri".to_string(), Fv::Text(source_uri.clone()));
        }
        if let Some(metadata) = &input.metadata {
            fields.insert("metadata".to_string(), Fv::from(metadata.clone()));
        }
        self.docs.update(doc_id, fields).await?;

        // 4) Activate the new chunk set.
        for id in &chunk_ids {
            self.chunks
                .update(*id, BTreeMap::from([("current".to_string(), Fv::U64(1))]))
                .await?;
        }

        // 5) Remove the superseded chunk set.
        if !created {
            for id in self.chunk_ids_of(doc_id, prev_version).await? {
                self.chunks.remove(id).await?;
            }
        }

        // 6) Audit event.
        let mut detail = BTreeMap::from([
            ("chunks".to_string(), Json::from(plan.drafts.len() as u64)),
            ("checksum".to_string(), Json::from(checksum)),
            ("size".to_string(), Json::from(content.len() as u64)),
        ]);
        if plan.forced_splits > 0 {
            detail.insert(
                "forced_splits".to_string(),
                Json::from(plan.forced_splits as u64),
            );
        }
        if plan.capped {
            detail.insert("chunks_capped".to_string(), Json::from(true));
        }
        if let Some(message) = &input.message {
            detail.insert("message".to_string(), Json::from(message.clone()));
        }
        self.write_event(
            if created {
                EVENT_DOC_CREATED
            } else {
                EVENT_VERSION_COMMITTED
            },
            Some(doc_id),
            Some(version_id),
            actor,
            detail,
            now_ms,
        )
        .await?;

        let doc = self.doc_record(doc_id).await?;
        let stored = self.version_record(version_id).await?;
        Ok(WikiCommitOutput {
            doc: doc.into(),
            version: version_info(&stored, version_id),
            chunks: plan.drafts.len(),
            created,
            idempotent: false,
        })
    }

    pub async fn archive(
        &self,
        actor: String,
        doc_id: u64,
        now_ms: u64,
    ) -> Result<WikiDocInfo, WikiError> {
        let _guard = self.write_lock.lock().await;
        let doc = self.doc_record(doc_id).await?;
        if doc.status == DOC_STATUS_ARCHIVED {
            return Err(WikiError::Invalid(format!(
                "document {doc_id} is already archived"
            )));
        }
        self.docs
            .update(
                doc_id,
                BTreeMap::from([
                    (
                        "status".to_string(),
                        Fv::Text(DOC_STATUS_ARCHIVED.to_string()),
                    ),
                    ("updated_by".to_string(), Fv::Text(actor.clone())),
                    ("updated_at".to_string(), Fv::U64(now_ms)),
                ]),
            )
            .await?;
        self.set_chunks_current(doc_id, doc.current_version, false)
            .await?;
        self.write_event(
            EVENT_DOC_ARCHIVED,
            Some(doc_id),
            Some(doc.current_version),
            actor,
            BTreeMap::new(),
            now_ms,
        )
        .await?;
        Ok(self.doc_record(doc_id).await?.into())
    }

    pub async fn restore(
        &self,
        actor: String,
        doc_id: u64,
        now_ms: u64,
    ) -> Result<WikiDocInfo, WikiError> {
        let _guard = self.write_lock.lock().await;
        let doc = self.doc_record(doc_id).await?;
        if doc.status != DOC_STATUS_ARCHIVED {
            return Err(WikiError::Invalid(format!(
                "document {doc_id} is not archived"
            )));
        }
        self.docs
            .update(
                doc_id,
                BTreeMap::from([
                    (
                        "status".to_string(),
                        Fv::Text(DOC_STATUS_ACTIVE.to_string()),
                    ),
                    ("updated_by".to_string(), Fv::Text(actor.clone())),
                    ("updated_at".to_string(), Fv::U64(now_ms)),
                ]),
            )
            .await?;
        self.set_chunks_current(doc_id, doc.current_version, true)
            .await?;
        self.write_event(
            EVENT_DOC_RESTORED,
            Some(doc_id),
            Some(doc.current_version),
            actor,
            BTreeMap::new(),
            now_ms,
        )
        .await?;
        Ok(self.doc_record(doc_id).await?.into())
    }

    // ─── Read path ──────────────────────────────────────────────────────────

    /// One-call BM25 retrieval over the chunk plane. Visibility (current
    /// version, namespace, archive state) is entirely in the filter, which
    /// AndaDB applies in the same query while preserving relevance order.
    pub async fn search(&self, mut input: WikiSearchInput) -> Result<WikiSearchOutput, WikiError> {
        input.normalize();
        if input.query.is_empty() {
            return Err(WikiError::Invalid("query cannot be empty".into()));
        }
        let top_k = input.top_k.unwrap_or(8).clamp(1, 50);

        let mut doc_ids = input.doc_ids.clone();
        if !input.tags.is_empty() {
            let tagged = self.doc_ids_by_tags(&input.tags).await?;
            doc_ids = if doc_ids.is_empty() {
                tagged
            } else {
                doc_ids.retain(|id| tagged.contains(id));
                doc_ids
            };
            if doc_ids.is_empty() {
                return Ok(WikiSearchOutput::default());
            }
        }

        let mut filters: Vec<Box<Filter>> = vec![Box::new(Filter::Field((
            "current".to_string(),
            RangeQuery::Eq(Fv::U64(1)),
        )))];
        if !input.namespaces.is_empty() {
            filters.push(Box::new(Filter::Field((
                "namespace".to_string(),
                RangeQuery::Include(
                    input
                        .namespaces
                        .iter()
                        .take(MAX_INCLUDE_KEYS)
                        .cloned()
                        .map(Fv::Text)
                        .collect(),
                ),
            ))));
        }
        if !doc_ids.is_empty() {
            filters.push(Box::new(Filter::Field((
                "doc_id".to_string(),
                RangeQuery::Include(
                    doc_ids
                        .iter()
                        .take(MAX_INCLUDE_KEYS)
                        .copied()
                        .map(Fv::U64)
                        .collect(),
                ),
            ))));
        }

        let fetch = match input.mode {
            WikiSearchMode::Chunks => top_k,
            WikiSearchMode::Docs => (top_k * 8).clamp(top_k, 200),
        };
        let rows: Vec<WikiChunkRecord> = self
            .chunks
            .search_as(Query {
                search: Some(Search {
                    text: Some(input.query.clone()),
                    ..Default::default()
                }),
                filter: Some(Filter::And(filters)),
                limit: Some(fetch),
            })
            .await?;

        let mut seen_docs = std::collections::BTreeSet::new();
        for row in &rows {
            seen_docs.insert(row.doc_id);
        }
        let total_docs_matched = seen_docs.len();

        let core: Vec<WikiChunkRecord> = match input.mode {
            WikiSearchMode::Chunks => rows,
            WikiSearchMode::Docs => {
                let mut picked = std::collections::BTreeSet::new();
                rows.into_iter()
                    .filter(|row| picked.insert(row.doc_id))
                    .take(top_k)
                    .collect()
            }
        };

        let expand = input.expand.unwrap_or(0).min(2) as usize;
        let hits = if expand == 0 {
            core.iter().map(|row| self.hit_from(row)).collect()
        } else {
            self.expand_hits(core, expand).await?
        };

        Ok(WikiSearchOutput {
            hits,
            total_docs_matched,
        })
    }

    /// Neighbor expansion (PRD §5.3): widens each hit by up to `expand`
    /// adjacent chunks. Chunks tile their version, so concatenating
    /// neighbors equals the exact content slice and the widened citation is
    /// recomputed over that range — still verifiable. Overlapping
    /// expansions within one document merge into a single hit at the
    /// best-ranked position.
    async fn expand_hits(
        &self,
        core: Vec<WikiChunkRecord>,
        expand: usize,
    ) -> Result<Vec<WikiHit>, WikiError> {
        struct DocLayout {
            rows: Vec<WikiChunkRecord>,
            version_checksum: String,
        }
        let mut layouts: BTreeMap<(u64, u64), DocLayout> = BTreeMap::new();
        for row in &core {
            let key = (row.doc_id, row.version_id);
            if layouts.contains_key(&key) {
                continue;
            }
            let mut rows: Vec<WikiChunkRecord> = self
                .chunks
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::And(vec![
                        Box::new(Filter::Field((
                            "doc_id".to_string(),
                            RangeQuery::Eq(Fv::U64(row.doc_id)),
                        ))),
                        Box::new(Filter::Field((
                            "version_id".to_string(),
                            RangeQuery::Eq(Fv::U64(row.version_id)),
                        ))),
                    ])),
                    limit: Some(Collection::MAX_SEARCH_LIMIT),
                })
                .await?;
            rows.sort_by_key(|r| r.ordinal);
            let version_checksum = self.version_record(row.version_id).await?.checksum;
            layouts.insert(
                key,
                DocLayout {
                    rows,
                    version_checksum,
                },
            );
        }

        struct Interval {
            key: (u64, u64),
            lo: usize,
            hi: usize,
            core_idx: usize, // index into `core`: the best-ranked seed
        }
        let mut intervals: Vec<Interval> = Vec::new();
        for (rank, row) in core.iter().enumerate() {
            let key = (row.doc_id, row.version_id);
            let layout = &layouts[&key];
            let Some(pos) = layout.rows.iter().position(|r| r._id == row._id) else {
                // Layout raced away (e.g. concurrent commit): keep unexpanded.
                intervals.push(Interval {
                    key,
                    lo: usize::MAX,
                    hi: usize::MAX,
                    core_idx: rank,
                });
                continue;
            };
            let mut lo = pos.saturating_sub(expand);
            let mut hi = (pos + expand).min(layout.rows.len() - 1);
            // Merge into the best-ranked overlapping/adjacent interval.
            if let Some(existing) = intervals.iter_mut().find(|iv| {
                iv.key == key && iv.lo != usize::MAX && iv.lo <= hi + 1 && lo <= iv.hi + 1
            }) {
                existing.lo = existing.lo.min(lo);
                existing.hi = existing.hi.max(hi);
                continue;
            }
            // A later merge may bridge two earlier intervals; one
            // stabilization pass is enough because merging only grows ranges.
            loop {
                let bridged = intervals.iter().position(|iv| {
                    iv.key == key && iv.lo != usize::MAX && iv.lo <= hi + 1 && lo <= iv.hi + 1
                });
                match bridged {
                    Some(idx) => {
                        lo = lo.min(intervals[idx].lo);
                        hi = hi.max(intervals[idx].hi);
                        intervals.remove(idx);
                    }
                    None => break,
                }
            }
            intervals.push(Interval {
                key,
                lo,
                hi,
                core_idx: rank,
            });
        }
        intervals.sort_by_key(|iv| iv.core_idx);

        let mut hits = Vec::with_capacity(intervals.len());
        for iv in intervals {
            let row = &core[iv.core_idx];
            if iv.lo == usize::MAX {
                hits.push(self.hit_from(row));
                continue;
            }
            let layout = &layouts[&iv.key];
            let slice = &layout.rows[iv.lo..=iv.hi];
            let text: String = slice.iter().map(|r| r.text.as_str()).collect();
            let start = slice
                .first()
                .map(|r| r.byte_start)
                .unwrap_or(row.byte_start);
            let end = slice.last().map(|r| r.byte_end).unwrap_or(row.byte_end);
            let checksum = chunk_checksum(
                &layout.version_checksum,
                start as usize,
                end as usize,
                &text,
            );
            hits.push(WikiHit {
                text,
                doc_title: row.title.clone(),
                heading_path: row.heading_path.clone(),
                score: None,
                citation: WikiCitation {
                    uri: citation_uri(&self.space_id, row.doc_id, row.version_id, start, end),
                    doc_id: row.doc_id,
                    version_id: row.version_id,
                    chunk_id: row._id,
                    heading_path: row.heading_path.clone(),
                    anchor: row.anchor.clone(),
                    byte_range: (start, end),
                    checksum,
                    quote: quote_excerpt(&row.text),
                },
            });
        }
        Ok(hits)
    }

    /// Progressive disclosure over one version: TOC, a section, a byte
    /// range, or the bounded full text. Historical versions are re-chunked
    /// in memory (the chunker is deterministic), so time-travel reads work
    /// without keeping historical chunk rows.
    pub async fn read(&self, input: WikiReadInput) -> Result<WikiReadOutput, WikiError> {
        let doc = self.doc_record(input.doc_id).await?;
        let version_id = input.version.unwrap_or(doc.current_version);
        let version = self.version_record(version_id).await?;
        if version.doc_id != doc._id {
            return Err(WikiError::NotFound(format!(
                "version {version_id} does not belong to document {}",
                doc._id
            )));
        }
        let is_current = version_id == doc.current_version;
        let content = &version.content;

        let mut output = WikiReadOutput {
            doc_id: doc._id,
            version_id,
            is_current,
            title: doc.title.clone(),
            status: doc.status.clone(),
            checksum: version.checksum.clone(),
            size: version.size,
            toc: None,
            content: None,
            byte_range: None,
            truncated: false,
        };

        match input.selector {
            WikiSelector::Toc => {
                let layout = self.layout_of(&doc, &version, is_current).await?;
                output.toc = Some(
                    layout
                        .into_iter()
                        .map(|entry| WikiTocEntry {
                            anchor: entry.anchor,
                            heading_path: entry.heading_path,
                            byte_start: entry.byte_start,
                            byte_end: entry.byte_end,
                        })
                        .collect(),
                );
            }
            WikiSelector::Section { anchor } => {
                let layout = self.layout_of(&doc, &version, is_current).await?;
                let idx = layout
                    .iter()
                    .position(|entry| entry.anchor == anchor)
                    .ok_or_else(|| {
                        WikiError::NotFound(format!("section anchor {anchor} not found"))
                    })?;
                let path = layout[idx].heading_path.clone();
                let mut start_idx = idx;
                while start_idx > 0 && layout[start_idx - 1].heading_path == path {
                    start_idx -= 1;
                }
                let mut end_idx = idx;
                while end_idx + 1 < layout.len() && layout[end_idx + 1].heading_path == path {
                    end_idx += 1;
                }
                let start = layout[start_idx].byte_start;
                let end = layout[end_idx].byte_end;
                output.content = Some(content[start as usize..end as usize].to_string());
                output.byte_range = Some((start, end));
            }
            WikiSelector::Range { start, end } => {
                if start > end {
                    return Err(WikiError::Invalid("range start exceeds end".into()));
                }
                let start = floor_char_boundary(content, start as usize);
                let end = floor_char_boundary(content, end as usize);
                output.content = Some(content[start..end].to_string());
                output.byte_range = Some((start as u64, end as u64));
            }
            WikiSelector::Full => {
                let end = floor_char_boundary(content, MAX_READ_BYTES);
                output.truncated = end < content.len();
                output.content = Some(content[..end].to_string());
                output.byte_range = Some((0, end as u64));
            }
        }
        Ok(output)
    }

    /// Citation verification: recomputes the chunk checksum from the
    /// immutable version content. `Invalid` means storage corruption and is
    /// evented; `Superseded` reports the version that replaced the cited one.
    pub async fn verify(
        &self,
        actor: String,
        input: WikiVerifyInput,
        now_ms: u64,
    ) -> Result<WikiVerifyOutput, WikiError> {
        let (doc_id, version_id, start, end) = match &input.uri {
            Some(uri) => {
                let (space, doc_id, version_id, start, end) = parse_citation_uri(uri)
                    .ok_or_else(|| WikiError::Invalid(format!("malformed citation uri: {uri}")))?;
                if space != self.space_id {
                    return Err(WikiError::Invalid(format!(
                        "citation belongs to space {space}, not {}",
                        self.space_id
                    )));
                }
                (doc_id, version_id, start, end)
            }
            None => {
                let (Some(doc_id), Some(version_id), Some((start, end))) =
                    (input.doc_id, input.version_id, input.byte_range)
                else {
                    return Err(WikiError::Invalid(
                        "either uri or doc_id+version_id+byte_range is required".into(),
                    ));
                };
                (doc_id, version_id, start, end)
            }
        };

        let not_found = WikiVerifyOutput {
            status: WikiVerifyStatus::NotFound,
            current_version: None,
            checksum: None,
            quote: None,
        };
        let Ok(doc) = self.doc_record(doc_id).await else {
            return Ok(not_found);
        };
        let Ok(version) = self.version_record(version_id).await else {
            return Ok(not_found);
        };
        if version.doc_id != doc._id {
            return Ok(not_found);
        }
        let Some(text) = version.content.get(start as usize..end as usize) else {
            return Ok(WikiVerifyOutput {
                status: WikiVerifyStatus::Invalid,
                current_version: Some(doc.current_version),
                checksum: None,
                quote: None,
            });
        };

        let computed = chunk_checksum(&version.checksum, start as usize, end as usize, text);
        if let Some(expected) = &input.checksum
            && *expected != computed
        {
            self.write_event(
                EVENT_CITATION_VERIFY_FAILED,
                Some(doc_id),
                Some(version_id),
                actor,
                BTreeMap::from([
                    ("expected".to_string(), Json::from(expected.clone())),
                    ("computed".to_string(), Json::from(computed.clone())),
                    ("byte_start".to_string(), Json::from(start)),
                    ("byte_end".to_string(), Json::from(end)),
                ]),
                now_ms,
            )
            .await?;
            return Ok(WikiVerifyOutput {
                status: WikiVerifyStatus::Invalid,
                current_version: Some(doc.current_version),
                checksum: Some(computed),
                quote: None,
            });
        }

        Ok(WikiVerifyOutput {
            status: if version_id == doc.current_version {
                WikiVerifyStatus::Valid
            } else {
                WikiVerifyStatus::Superseded
            },
            current_version: Some(doc.current_version),
            checksum: Some(computed),
            quote: Some(quote_excerpt(text)),
        })
    }

    pub async fn get_doc(&self, doc_id: u64) -> Result<WikiDocInfo, WikiError> {
        Ok(self.doc_record(doc_id).await?.into())
    }

    pub async fn list_docs(
        &self,
        input: WikiListDocsInput,
    ) -> Result<WikiDocListOutput, WikiError> {
        let limit = input.limit.unwrap_or(20).clamp(1, 100);
        let cursor = self.cursor_or_max(&self.docs, &input.cursor)?;

        let mut filters: Vec<Box<Filter>> = vec![
            Box::new(Filter::Field((
                "_id".to_string(),
                RangeQuery::Lt(Fv::U64(cursor)),
            ))),
            // Hide initializing sentinels (crashed creates awaiting sweep).
            Box::new(Filter::Field((
                "current_version".to_string(),
                RangeQuery::Gt(Fv::U64(0)),
            ))),
        ];
        if let Some(namespace) = &input.namespace {
            filters.push(Box::new(Filter::Field((
                "namespace".to_string(),
                RangeQuery::Eq(Fv::Text(namespace.clone())),
            ))));
        }
        if let Some(status) = &input.status {
            filters.push(Box::new(Filter::Field((
                "status".to_string(),
                RangeQuery::Eq(Fv::Text(status.clone())),
            ))));
        }
        if let Some(tag) = &input.tag {
            filters.push(Box::new(Filter::Field((
                "tags".to_string(),
                RangeQuery::Eq(Fv::Text(tag.clone())),
            ))));
        }

        let rows: Vec<WikiDocRecord> = self
            .docs
            .search_as(Query {
                search: None,
                filter: Some(Filter::And(filters)),
                limit: Some(limit),
            })
            .await?;
        let next_cursor = page_cursor(&rows, limit, |doc| doc._id);
        Ok(WikiDocListOutput {
            docs: rows.into_iter().map(Into::into).collect(),
            next_cursor,
        })
    }

    pub async fn list_versions(
        &self,
        doc_id: u64,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<WikiVersionListOutput, WikiError> {
        let doc = self.doc_record(doc_id).await?;
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let cursor = self.cursor_or_max(&self.versions, &cursor)?;
        let rows: Vec<WikiVersionRecord> = self
            .versions
            .search_as(Query {
                search: None,
                filter: Some(Filter::And(vec![
                    Box::new(Filter::Field((
                        "doc_id".to_string(),
                        RangeQuery::Eq(Fv::U64(doc._id)),
                    ))),
                    Box::new(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Lt(Fv::U64(cursor)),
                    ))),
                    // Orphans from crashed commits are not history.
                    Box::new(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Le(Fv::U64(doc.current_version)),
                    ))),
                ])),
                limit: Some(limit),
            })
            .await?;
        let next_cursor = page_cursor(&rows, limit, |v| v._id);
        Ok(WikiVersionListOutput {
            versions: rows.iter().map(|v| version_info(v, v._id)).collect(),
            next_cursor,
        })
    }

    pub async fn list_events(
        &self,
        kind: Option<String>,
        doc_id: Option<u64>,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<WikiEventListOutput, WikiError> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let cursor = self.cursor_or_max(&self.events, &cursor)?;
        let mut filters: Vec<Box<Filter>> = vec![Box::new(Filter::Field((
            "_id".to_string(),
            RangeQuery::Lt(Fv::U64(cursor)),
        )))];
        if let Some(kind) = kind {
            filters.push(Box::new(Filter::Field((
                "kind".to_string(),
                RangeQuery::Eq(Fv::Text(kind)),
            ))));
        }
        if let Some(doc_id) = doc_id {
            filters.push(Box::new(Filter::Field((
                "doc_id".to_string(),
                RangeQuery::Eq(Fv::U64(doc_id)),
            ))));
        }
        let rows: Vec<WikiEventRecord> = self
            .events
            .search_as(Query {
                search: None,
                filter: Some(Filter::And(filters)),
                limit: Some(limit),
            })
            .await?;
        let next_cursor = page_cursor(&rows, limit, |e| e._id);
        Ok(WikiEventListOutput {
            events: rows
                .into_iter()
                .map(|e| WikiEventInfo {
                    id: e._id,
                    kind: e.kind,
                    doc_id: e.doc_id,
                    version_id: e.version_id,
                    actor: e.actor,
                    detail: e.detail,
                    created_at: e.created_at,
                })
                .collect(),
            next_cursor,
        })
    }

    // ─── Maintenance ────────────────────────────────────────────────────────

    /// Reclaims commit-crash leftovers and repairs chunk visibility. Runs
    /// under the write lock so in-flight commits are never mistaken for
    /// orphans. Safe to run at any time; called on space startup.
    pub async fn orphan_sweep(&self, now_ms: u64) -> Result<WikiSweepReport, WikiError> {
        let _guard = self.write_lock.lock().await;
        let mut report = WikiSweepReport::default();

        let mut cursor = self.docs.max_document_id() + 1;
        loop {
            let docs: Vec<WikiDocRecord> = self
                .docs
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Lt(Fv::U64(cursor)),
                    ))),
                    limit: Some(SCAN_PAGE),
                })
                .await?;
            let Some(min_id) = docs.iter().map(|d| d._id).min() else {
                break;
            };
            cursor = min_id;
            let page_len = docs.len();
            for doc in docs {
                self.sweep_doc(doc, now_ms, &mut report).await?;
            }
            if page_len < SCAN_PAGE {
                break;
            }
        }

        if !report.is_empty() {
            self.write_event(
                EVENT_ORPHAN_SWEPT,
                None,
                None,
                "system".to_string(),
                BTreeMap::from([
                    (
                        "docs_removed".to_string(),
                        Json::from(report.docs_removed as u64),
                    ),
                    (
                        "versions_removed".to_string(),
                        Json::from(report.versions_removed as u64),
                    ),
                    (
                        "chunks_removed".to_string(),
                        Json::from(report.chunks_removed as u64),
                    ),
                    (
                        "chunks_repaired".to_string(),
                        Json::from(report.chunks_repaired as u64),
                    ),
                ]),
                now_ms,
            )
            .await?;
        }
        Ok(report)
    }

    async fn sweep_doc(
        &self,
        doc: WikiDocRecord,
        now_ms: u64,
        report: &mut WikiSweepReport,
    ) -> Result<(), WikiError> {
        // A create that never activated: reclaim the whole document once the
        // in-flight window has clearly passed.
        if doc.current_version == 0 {
            if now_ms.saturating_sub(doc.created_at) > SENTINEL_TTL_MS {
                for version_id in self.version_ids_of(doc._id).await? {
                    self.versions.remove(version_id).await?;
                    report.versions_removed += 1;
                }
                for chunk_id in self.all_chunk_ids_of(doc._id).await? {
                    self.chunks.remove(chunk_id).await?;
                    report.chunks_removed += 1;
                }
                self.docs.remove(doc._id).await?;
                report.docs_removed += 1;
            }
            return Ok(());
        }

        // Versions newer than the活跃 one never activated (commit crashed
        // between the version write and the doc flip).
        let orphan_versions: Vec<u64> = self
            .version_ids_of(doc._id)
            .await?
            .into_iter()
            .filter(|id| *id > doc.current_version)
            .collect();
        for version_id in orphan_versions {
            for chunk_id in self.chunk_ids_of(doc._id, version_id).await? {
                self.chunks.remove(chunk_id).await?;
                report.chunks_removed += 1;
            }
            self.versions.remove(version_id).await?;
            report.versions_removed += 1;
        }

        // Reconcile chunk visibility with the doc row.
        let rows: Vec<WikiChunkRecord> = self
            .chunks
            .search_as(Query {
                search: None,
                filter: Some(Filter::Field((
                    "doc_id".to_string(),
                    RangeQuery::Eq(Fv::U64(doc._id)),
                ))),
                limit: Some(Collection::MAX_SEARCH_LIMIT),
            })
            .await?;
        let want_current: u64 = (doc.status == DOC_STATUS_ACTIVE) as u64;
        for row in rows {
            if row.version_id != doc.current_version {
                self.chunks.remove(row._id).await?;
                report.chunks_removed += 1;
            } else if row.current != want_current {
                self.chunks
                    .update(
                        row._id,
                        BTreeMap::from([("current".to_string(), Fv::U64(want_current))]),
                    )
                    .await?;
                report.chunks_repaired += 1;
            }
        }
        Ok(())
    }

    // ─── Internals ──────────────────────────────────────────────────────────

    /// Loads a document, treating initializing sentinels as absent.
    async fn doc_record(&self, doc_id: u64) -> Result<WikiDocRecord, WikiError> {
        let doc: WikiDocRecord = self
            .docs
            .get_as(doc_id)
            .await
            .map_err(|_| WikiError::NotFound(format!("document {doc_id} not found")))?;
        if doc.current_version == 0 {
            return Err(WikiError::NotFound(format!("document {doc_id} not found")));
        }
        Ok(doc)
    }

    async fn version_record(&self, version_id: u64) -> Result<WikiVersionRecord, WikiError> {
        self.versions
            .get_as(version_id)
            .await
            .map_err(|_| WikiError::NotFound(format!("version {version_id} not found")))
    }

    fn hit_from(&self, row: &WikiChunkRecord) -> WikiHit {
        WikiHit {
            text: row.text.clone(),
            doc_title: row.title.clone(),
            heading_path: row.heading_path.clone(),
            score: None,
            citation: WikiCitation {
                uri: citation_uri(
                    &self.space_id,
                    row.doc_id,
                    row.version_id,
                    row.byte_start,
                    row.byte_end,
                ),
                doc_id: row.doc_id,
                version_id: row.version_id,
                chunk_id: row._id,
                heading_path: row.heading_path.clone(),
                anchor: row.anchor.clone(),
                byte_range: (row.byte_start, row.byte_end),
                checksum: row.checksum.clone(),
                quote: quote_excerpt(&row.text),
            },
        }
    }

    /// Section layout of a version: stored chunk rows for the current
    /// version, an in-memory re-chunk for historical ones.
    async fn layout_of(
        &self,
        doc: &WikiDocRecord,
        version: &WikiVersionRecord,
        is_current: bool,
    ) -> Result<Vec<LayoutEntry>, WikiError> {
        if is_current {
            let mut rows: Vec<WikiChunkRecord> = self
                .chunks
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::And(vec![
                        Box::new(Filter::Field((
                            "doc_id".to_string(),
                            RangeQuery::Eq(Fv::U64(doc._id)),
                        ))),
                        Box::new(Filter::Field((
                            "version_id".to_string(),
                            RangeQuery::Eq(Fv::U64(version._id)),
                        ))),
                    ])),
                    limit: Some(Collection::MAX_SEARCH_LIMIT),
                })
                .await?;
            if !rows.is_empty() {
                rows.sort_by_key(|row| row.ordinal);
                return Ok(rows
                    .into_iter()
                    .map(|row| LayoutEntry {
                        anchor: row.anchor,
                        heading_path: row.heading_path,
                        byte_start: row.byte_start,
                        byte_end: row.byte_end,
                    })
                    .collect());
            }
        }
        Ok(chunk_markdown(&version.content)
            .drafts
            .into_iter()
            .map(|draft| LayoutEntry {
                anchor: draft.anchor,
                heading_path: draft.heading_path,
                byte_start: draft.byte_start as u64,
                byte_end: draft.byte_end as u64,
            })
            .collect())
    }

    async fn find_doc_id_by_slug(
        &self,
        namespace: &str,
        slug: &str,
    ) -> Result<Option<u64>, WikiError> {
        let rows: Vec<WikiDocRecord> = self
            .docs
            .search_as(Query {
                search: None,
                filter: Some(Filter::And(vec![
                    Box::new(Filter::Field((
                        "namespace".to_string(),
                        RangeQuery::Eq(Fv::Text(namespace.to_string())),
                    ))),
                    Box::new(Filter::Field((
                        "slug".to_string(),
                        RangeQuery::Eq(Fv::Text(slug.to_string())),
                    ))),
                ])),
                limit: Some(1),
            })
            .await?;
        Ok(rows.first().map(|doc| doc._id))
    }

    /// Resolves a unique slug within a namespace by suffixing `-2`, `-3`, …
    /// on collision. Never merges two documents (the v1 failure mode).
    async fn unique_slug(
        &self,
        namespace: &str,
        base: &str,
        exclude: Option<u64>,
        now_ms: u64,
    ) -> Result<String, WikiError> {
        // Path form preserves `/` hierarchy (OKF concept ids); titles are
        // already flat after slugify, so plain slugs pass through unchanged.
        let base = slugify_path(base);
        for attempt in 0..100u32 {
            let candidate = if attempt == 0 {
                base.clone()
            } else {
                format!("{base}-{}", attempt + 1)
            };
            match self.find_doc_id_by_slug(namespace, &candidate).await? {
                None => return Ok(candidate),
                Some(id) if Some(id) == exclude => return Ok(candidate),
                Some(_) => {}
            }
        }
        Ok(format!("{base}-{now_ms}"))
    }

    async fn doc_ids_by_tags(&self, tags: &[String]) -> Result<Vec<u64>, WikiError> {
        let ids = self
            .docs
            .search_ids(Query {
                search: None,
                filter: Some(Filter::Field((
                    "tags".to_string(),
                    RangeQuery::Include(
                        tags.iter()
                            .take(MAX_INCLUDE_KEYS)
                            .cloned()
                            .map(Fv::Text)
                            .collect(),
                    ),
                ))),
                limit: Some(Collection::MAX_SEARCH_LIMIT),
            })
            .await?;
        Ok(ids)
    }

    async fn chunk_ids_of(&self, doc_id: u64, version_id: u64) -> Result<Vec<u64>, WikiError> {
        Ok(self
            .chunks
            .search_ids(Query {
                search: None,
                filter: Some(Filter::And(vec![
                    Box::new(Filter::Field((
                        "doc_id".to_string(),
                        RangeQuery::Eq(Fv::U64(doc_id)),
                    ))),
                    Box::new(Filter::Field((
                        "version_id".to_string(),
                        RangeQuery::Eq(Fv::U64(version_id)),
                    ))),
                ])),
                limit: Some(Collection::MAX_SEARCH_LIMIT),
            })
            .await?)
    }

    async fn all_chunk_ids_of(&self, doc_id: u64) -> Result<Vec<u64>, WikiError> {
        Ok(self
            .chunks
            .search_ids(Query {
                search: None,
                filter: Some(Filter::Field((
                    "doc_id".to_string(),
                    RangeQuery::Eq(Fv::U64(doc_id)),
                ))),
                limit: Some(Collection::MAX_SEARCH_LIMIT),
            })
            .await?)
    }

    async fn version_ids_of(&self, doc_id: u64) -> Result<Vec<u64>, WikiError> {
        Ok(self
            .versions
            .search_ids(Query {
                search: None,
                filter: Some(Filter::Field((
                    "doc_id".to_string(),
                    RangeQuery::Eq(Fv::U64(doc_id)),
                ))),
                limit: Some(Collection::MAX_SEARCH_LIMIT),
            })
            .await?)
    }

    async fn set_chunks_current(
        &self,
        doc_id: u64,
        version_id: u64,
        current: bool,
    ) -> Result<(), WikiError> {
        for id in self.chunk_ids_of(doc_id, version_id).await? {
            self.chunks
                .update(
                    id,
                    BTreeMap::from([("current".to_string(), Fv::U64(current as u64))]),
                )
                .await?;
        }
        Ok(())
    }

    async fn write_event(
        &self,
        kind: &str,
        doc_id: Option<u64>,
        version_id: Option<u64>,
        actor: String,
        detail: BTreeMap<String, Json>,
        now_ms: u64,
    ) -> Result<u64, WikiError> {
        Ok(self
            .events
            .add_from(&WikiEventRecord {
                _id: 0,
                kind: kind.to_string(),
                doc_id,
                version_id,
                actor,
                detail,
                created_at: now_ms,
            })
            .await?)
    }

    fn cursor_or_max(
        &self,
        collection: &Arc<Collection>,
        cursor: &Option<String>,
    ) -> Result<u64, WikiError> {
        use anda_db::index::BTree;
        match BTree::from_cursor::<u64>(cursor)
            .map_err(|err| WikiError::Invalid(format!("invalid cursor: {err:?}")))?
        {
            Some(cursor) => Ok(cursor),
            None => Ok(collection.max_document_id() + 1),
        }
    }
}

struct LayoutEntry {
    anchor: String,
    heading_path: Vec<String>,
    byte_start: u64,
    byte_end: u64,
}

fn version_info(version: &WikiVersionRecord, id: u64) -> WikiVersionInfo {
    WikiVersionInfo {
        id,
        doc_id: version.doc_id,
        parent_version: version.parent_version,
        checksum: version.checksum.clone(),
        size: version.size,
        author: version.author.clone(),
        message: version.message.clone(),
        created_at: version.created_at,
    }
}

/// Pages are ascending "newest N below cursor"; the next cursor is the
/// smallest id in a full page (same convention as conversation listing).
fn page_cursor<T>(rows: &[T], limit: usize, id_of: impl Fn(&T) -> u64) -> Option<String> {
    use anda_db::index::BTree;
    if rows.len() >= limit {
        rows.iter()
            .map(&id_of)
            .min()
            .and_then(|id| BTree::to_cursor(&id))
    } else {
        None
    }
}

async fn init_wiki_docs(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["namespace"]).await?;
    collection.create_btree_index_nx(&["slug"]).await?;
    collection.create_btree_index_nx(&["status"]).await?;
    collection.create_btree_index_nx(&["tags"]).await?;
    collection
        .create_btree_index_nx(&["current_version"])
        .await?;
    collection.create_bm25_index_nx(&["title", "tags"]).await?;
    Ok(())
}

async fn init_wiki_versions(collection: &mut Collection) -> Result<(), DBError> {
    collection.create_btree_index_nx(&["doc_id"]).await?;
    collection.create_btree_index_nx(&["checksum"]).await?;
    Ok(())
}

async fn init_wiki_chunks(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["doc_id"]).await?;
    collection.create_btree_index_nx(&["version_id"]).await?;
    collection.create_btree_index_nx(&["namespace"]).await?;
    collection.create_btree_index_nx(&["current"]).await?;
    collection
        .create_bm25_index_nx(&["title", "heading_path", "text"])
        .await?;
    Ok(())
}

async fn init_wiki_events(collection: &mut Collection) -> Result<(), DBError> {
    collection.create_btree_index_nx(&["kind"]).await?;
    collection.create_btree_index_nx(&["doc_id"]).await?;
    collection.create_btree_index_nx(&["actor"]).await?;
    collection.create_btree_index_nx(&["created_at"]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_db::{database::DBConfig, storage::StorageConfig};
    use object_store::memory::InMemory;

    pub(super) async fn test_wiki(name: &str) -> WikiService {
        let db = Arc::new(
            AndaDB::create(
                Arc::new(InMemory::new()),
                DBConfig {
                    name: name.to_string(),
                    description: "wiki test db".to_string(),
                    storage: StorageConfig::default(),
                    lock: None,
                },
            )
            .await
            .unwrap(),
        );
        WikiService::connect("test_space".to_string(), db)
            .await
            .unwrap()
    }

    pub(super) fn commit_input(title: &str, content: &str) -> WikiCommitInput {
        WikiCommitInput {
            title: title.to_string(),
            content: content.to_string(),
            ..Default::default()
        }
    }

    const CN_DOC: &str = "# 部署指南\n\n本指南描述生产环境的部署步骤与回滚策略。\n\n## 前置条件\n\n需要配置对象存储与访问令牌，并确认分片参数一致。\n\n```bash\n# 这行注释不是标题\nexport ANDA_TOKEN=secret\n```\n\n## 回滚策略\n\n出现故障时使用上一版本快照回滚，并验证引用校验和。\n";

    #[tokio::test]
    async fn commit_search_read_verify_roundtrip() {
        let wiki = test_wiki("wiki_roundtrip").await;
        let out = wiki
            .commit(
                "user:alice".to_string(),
                commit_input("部署指南", CN_DOC),
                1000,
            )
            .await
            .unwrap();
        assert!(out.created);
        assert!(!out.idempotent);
        assert!(out.chunks >= 1);
        assert_eq!(out.doc.created_by, "user:alice");
        assert_eq!(out.version.author, "user:alice");

        // Search hits with precise citations.
        let rt = wiki
            .search(WikiSearchInput::from_query("回滚策略".to_string()))
            .await
            .unwrap();
        assert!(!rt.hits.is_empty());
        let hit = &rt.hits[0];
        assert_eq!(hit.doc_title, "部署指南");
        assert!(hit.citation.uri.starts_with("wiki://test_space/"));

        // The citation byte range slices the normalized content exactly.
        let read = wiki
            .read(WikiReadInput {
                doc_id: out.doc.id,
                version: None,
                selector: WikiSelector::Full,
            })
            .await
            .unwrap();
        let content = read.content.unwrap();
        let (start, end) = hit.citation.byte_range;
        assert_eq!(&content[start as usize..end as usize], hit.text);
        // The code-fence comment must not become a heading/section.
        assert!(
            !hit.citation
                .heading_path
                .iter()
                .any(|h| h.contains("这行注释"))
        );

        // TOC and section reads.
        let toc = wiki
            .read(WikiReadInput {
                doc_id: out.doc.id,
                version: None,
                selector: WikiSelector::Toc,
            })
            .await
            .unwrap()
            .toc
            .unwrap();
        assert!(!toc.is_empty());
        let anchor = &hit.citation.anchor;
        let section = wiki
            .read(WikiReadInput {
                doc_id: out.doc.id,
                version: None,
                selector: WikiSelector::Section {
                    anchor: anchor.clone(),
                },
            })
            .await
            .unwrap();
        assert!(section.content.unwrap().contains(&hit.text));

        // Verify: valid via uri + checksum.
        let verified = wiki
            .verify(
                "user:alice".to_string(),
                WikiVerifyInput {
                    uri: Some(hit.citation.uri.clone()),
                    checksum: Some(hit.citation.checksum.clone()),
                    ..Default::default()
                },
                2000,
            )
            .await
            .unwrap();
        assert_eq!(verified.status, WikiVerifyStatus::Valid);
    }

    #[tokio::test]
    async fn idempotent_commits_do_not_grow_versions() {
        let wiki = test_wiki("wiki_idempotent").await;
        let first = wiki
            .commit(
                "a".to_string(),
                commit_input("政策", "# 政策\n\n条款内容。\n"),
                1000,
            )
            .await
            .unwrap();

        for i in 0..100u64 {
            let mut input = commit_input("政策", "# 政策\n\n条款内容。\n");
            input.doc_id = Some(first.doc.id);
            input.parent_version = Some(first.version.id);
            let out = wiki.commit("a".to_string(), input, 2000 + i).await.unwrap();
            assert!(out.idempotent);
            assert_eq!(out.version.id, first.version.id);
        }

        let versions = wiki
            .list_versions(first.doc.id, None, Some(100))
            .await
            .unwrap();
        assert_eq!(versions.versions.len(), 1);
        assert_eq!(wiki.chunks_count(), first.chunks);
    }

    #[tokio::test]
    async fn cas_conflict_version_chain_and_chunk_replacement() {
        let wiki = test_wiki("wiki_cas").await;
        let v1 = wiki
            .commit(
                "a".to_string(),
                commit_input("手册", "# 手册\n\n第一版内容：旧的错误码说明。\n"),
                1000,
            )
            .await
            .unwrap();
        let chunks_after_v1 = wiki.chunks_count();

        // Update without parent_version fails.
        let mut missing = commit_input("手册", "# 手册\n\n新内容。\n");
        missing.doc_id = Some(v1.doc.id);
        assert!(matches!(
            wiki.commit("b".to_string(), missing, 1500).await,
            Err(WikiError::Invalid(_))
        ));

        // Correct CAS succeeds and links the version chain.
        let mut update = commit_input("手册", "# 手册\n\n第二版内容：新的重试策略说明。\n");
        update.doc_id = Some(v1.doc.id);
        update.parent_version = Some(v1.version.id);
        let v2 = wiki.commit("b".to_string(), update, 2000).await.unwrap();
        assert!(!v2.created);
        assert_eq!(v2.version.parent_version, Some(v1.version.id));

        // Stale CAS now conflicts, reporting the current version.
        let mut stale = commit_input("手册", "# 手册\n\n第三版内容。\n");
        stale.doc_id = Some(v1.doc.id);
        stale.parent_version = Some(v1.version.id);
        match wiki.commit("c".to_string(), stale, 3000).await {
            Err(WikiError::Conflict {
                current_version,
                updated_by,
                ..
            }) => {
                assert_eq!(current_version, v2.version.id);
                assert_eq!(updated_by, "b");
            }
            other => panic!("expected conflict, got {other:?}"),
        }

        // Old chunks replaced, not accumulated; search sees only v2.
        assert_eq!(wiki.chunks_count(), chunks_after_v1 - v1.chunks + v2.chunks);
        let rt = wiki
            .search(WikiSearchInput::from_query("错误码".to_string()))
            .await
            .unwrap();
        assert!(rt.hits.is_empty());
        let rt = wiki
            .search(WikiSearchInput::from_query("重试策略".to_string()))
            .await
            .unwrap();
        assert_eq!(rt.hits[0].citation.version_id, v2.version.id);

        // Historical version still readable (re-chunked layout) and verify
        // reports it superseded.
        let old = wiki
            .read(WikiReadInput {
                doc_id: v1.doc.id,
                version: Some(v1.version.id),
                selector: WikiSelector::Toc,
            })
            .await
            .unwrap();
        assert!(!old.is_current);
        assert!(old.toc.unwrap().iter().any(|t| !t.anchor.is_empty()));
        let verified = wiki
            .verify(
                "a".to_string(),
                WikiVerifyInput {
                    doc_id: Some(v1.doc.id),
                    version_id: Some(v1.version.id),
                    byte_range: Some((0, 2)),
                    ..Default::default()
                },
                4000,
            )
            .await
            .unwrap();
        assert_eq!(verified.status, WikiVerifyStatus::Superseded);
        assert_eq!(verified.current_version, Some(v2.version.id));
    }

    #[tokio::test]
    async fn chinese_titles_never_collide() {
        let wiki = test_wiki("wiki_slug_cn").await;
        let a = wiki
            .commit(
                "a".to_string(),
                commit_input("产品手册", "# 产品手册\n\n产品功能介绍。\n"),
                1000,
            )
            .await
            .unwrap();
        let b = wiki
            .commit(
                "a".to_string(),
                commit_input("安全政策", "# 安全政策\n\n安全合规要求。\n"),
                1100,
            )
            .await
            .unwrap();
        // v1 regression: both would slugify to "untitled" and silently merge.
        assert_ne!(a.doc.id, b.doc.id);
        assert_ne!(a.doc.slug, b.doc.slug);
        assert!(a.doc.slug.contains("产品手册"));

        // Same title twice: suffixing, never merging.
        let c = wiki
            .commit(
                "a".to_string(),
                commit_input("产品手册", "# 产品手册\n\n另一篇同名文档。\n"),
                1200,
            )
            .await
            .unwrap();
        assert_ne!(c.doc.id, a.doc.id);
        assert_ne!(c.doc.slug, a.doc.slug);

        let docs = wiki.list_docs(WikiListDocsInput::default()).await.unwrap();
        assert_eq!(docs.docs.len(), 3);
    }

    #[tokio::test]
    async fn archive_hides_from_search_and_restore_recovers() {
        let wiki = test_wiki("wiki_archive").await;
        let out = wiki
            .commit(
                "a".to_string(),
                commit_input("旧规范", "# 旧规范\n\n历史合规要求文本。\n"),
                1000,
            )
            .await
            .unwrap();

        let archived = wiki
            .archive("b".to_string(), out.doc.id, 2000)
            .await
            .unwrap();
        assert_eq!(archived.status, DOC_STATUS_ARCHIVED);
        let rt = wiki
            .search(WikiSearchInput::from_query("合规要求".to_string()))
            .await
            .unwrap();
        assert!(rt.hits.is_empty());

        // Still readable by id; commit to archived doc rejected.
        let read = wiki
            .read(WikiReadInput {
                doc_id: out.doc.id,
                version: None,
                selector: WikiSelector::Full,
            })
            .await
            .unwrap();
        assert!(read.content.unwrap().contains("历史合规要求"));
        let mut update = commit_input("旧规范", "# 旧规范\n\n修改。\n");
        update.doc_id = Some(out.doc.id);
        update.parent_version = Some(out.version.id);
        assert!(matches!(
            wiki.commit("b".to_string(), update, 2500).await,
            Err(WikiError::Invalid(_))
        ));

        wiki.restore("b".to_string(), out.doc.id, 3000)
            .await
            .unwrap();
        let rt = wiki
            .search(WikiSearchInput::from_query("合规要求".to_string()))
            .await
            .unwrap();
        assert_eq!(rt.hits.len(), 1);

        // Audit trail recorded real actors.
        let events = wiki
            .list_events(None, Some(out.doc.id), None, Some(10))
            .await
            .unwrap();
        let kinds: Vec<_> = events.events.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&EVENT_DOC_CREATED));
        assert!(kinds.contains(&EVENT_DOC_ARCHIVED));
        assert!(kinds.contains(&EVENT_DOC_RESTORED));
        assert!(
            events
                .events
                .iter()
                .all(|e| e.actor == "a" || e.actor == "b")
        );
    }

    #[tokio::test]
    async fn size_and_validity_limits() {
        let wiki = test_wiki("wiki_limits").await;
        let huge = "字".repeat(MAX_DOC_BYTES / 3 + 1);
        assert!(matches!(
            wiki.commit("a".to_string(), commit_input("大文档", &huge), 1000)
                .await,
            Err(WikiError::TooLarge { .. })
        ));
        assert!(matches!(
            wiki.commit("a".to_string(), commit_input("空", "   \n  \n"), 1000)
                .await,
            Err(WikiError::Invalid(_))
        ));
        assert!(matches!(
            wiki.commit(
                "a".to_string(),
                commit_input("", "no heading content"),
                1000
            )
            .await,
            Err(WikiError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_updates_exactly_one_wins() {
        let wiki = test_wiki("wiki_concurrent").await;
        let v1 = wiki
            .commit(
                "a".to_string(),
                commit_input("竞争文档", "# 竞争文档\n\n初始内容。\n"),
                1000,
            )
            .await
            .unwrap();

        let mk = |text: &str| {
            let mut input = commit_input("竞争文档", text);
            input.doc_id = Some(v1.doc.id);
            input.parent_version = Some(v1.version.id);
            input
        };
        let w1 = wiki.clone();
        let w2 = wiki.clone();
        let i1 = mk("# 竞争文档\n\n写者甲的修改。\n");
        let i2 = mk("# 竞争文档\n\n写者乙的修改。\n");
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { w1.commit("甲".to_string(), i1, 2000).await }),
            tokio::spawn(async move { w2.commit("乙".to_string(), i2, 2001).await }),
        );
        let results = [r1.unwrap(), r2.unwrap()];
        let oks = results.iter().filter(|r| r.is_ok()).count();
        let conflicts = results
            .iter()
            .filter(|r| matches!(r, Err(WikiError::Conflict { .. })))
            .count();
        assert_eq!((oks, conflicts), (1, 1));
    }

    #[tokio::test]
    async fn orphan_sweep_reclaims_crash_leftovers() {
        let wiki = test_wiki("wiki_sweep").await;
        let out = wiki
            .commit(
                "a".to_string(),
                commit_input("正常文档", "# 正常文档\n\n正常内容。\n"),
                1000,
            )
            .await
            .unwrap();

        // Simulate a crash between the version write and the doc flip: an
        // orphan version with inactive chunks.
        let orphan_version = wiki
            .versions
            .add_from(&WikiVersionRecord {
                _id: 0,
                doc_id: out.doc.id,
                parent_version: Some(out.version.id),
                checksum: "sha3-256:dead".to_string(),
                content: "# 孤儿版本\n".to_string(),
                size: 12,
                author: "a".to_string(),
                message: None,
                created_at: 2000,
            })
            .await
            .unwrap();
        wiki.chunks
            .add_from(&WikiChunkRecord {
                _id: 0,
                doc_id: out.doc.id,
                version_id: orphan_version,
                namespace: "default".to_string(),
                current: 0,
                title: "正常文档".to_string(),
                heading_path: vec![],
                anchor: "section-0".to_string(),
                ordinal: 0,
                text: "孤儿".to_string(),
                byte_start: 0,
                byte_end: 6,
                checksum: "sha3-256:dead".to_string(),
                chunker_version: CHUNKER_VERSION as u64,
                acl_label: None,
            })
            .await
            .unwrap();

        // Simulate a crashed create: a sentinel doc past the TTL.
        wiki.docs
            .add_from(&WikiDocRecord {
                _id: 0,
                namespace: "default".to_string(),
                slug: "sentinel".to_string(),
                title: "半成品".to_string(),
                status: DOC_STATUS_ACTIVE.to_string(),
                current_version: 0,
                current_checksum: String::new(),
                tags: vec![],
                source_uri: None,
                metadata: BTreeMap::new(),
                created_by: "a".to_string(),
                updated_by: "a".to_string(),
                created_at: 1000,
                updated_at: 1000,
            })
            .await
            .unwrap();

        // Orphans are invisible before the sweep.
        let versions = wiki
            .list_versions(out.doc.id, None, Some(50))
            .await
            .unwrap();
        assert_eq!(versions.versions.len(), 1);
        let docs = wiki.list_docs(WikiListDocsInput::default()).await.unwrap();
        assert_eq!(docs.docs.len(), 1);

        let report = wiki.orphan_sweep(1000 + SENTINEL_TTL_MS + 1).await.unwrap();
        assert_eq!(report.docs_removed, 1);
        assert_eq!(report.versions_removed, 1);
        assert_eq!(report.chunks_removed, 1);

        // The healthy document is untouched and searchable.
        let rt = wiki
            .search(WikiSearchInput::from_query("正常内容".to_string()))
            .await
            .unwrap();
        assert_eq!(rt.hits.len(), 1);
        let report = wiki.orphan_sweep(1000 + SENTINEL_TTL_MS + 2).await.unwrap();
        assert!(report.is_empty());
    }

    #[tokio::test]
    async fn search_filters_and_docs_mode() {
        let wiki = test_wiki("wiki_filters").await;
        let mut a = commit_input(
            "检索文档甲",
            "# 检索文档甲\n\n共享关键词：分布式检索测试。\n",
        );
        a.namespace = Some("engineering".to_string());
        a.tags = Some(vec!["api".to_string()]);
        let a = wiki.commit("u".to_string(), a, 1000).await.unwrap();

        let mut b = commit_input(
            "检索文档乙",
            "# 检索文档乙\n\n共享关键词：分布式检索测试。\n\n## 附录\n\n共享关键词：分布式检索测试补充。\n",
        );
        b.namespace = Some("policy".to_string());
        b.tags = Some(vec!["compliance".to_string()]);
        let b = wiki.commit("u".to_string(), b, 1100).await.unwrap();

        // Namespace filter.
        let mut q = WikiSearchInput::from_query("分布式检索测试".to_string());
        q.namespaces = vec!["engineering".to_string()];
        let rt = wiki.search(q).await.unwrap();
        assert!(!rt.hits.is_empty());
        assert!(rt.hits.iter().all(|h| h.citation.doc_id == a.doc.id));

        // Tag filter.
        let mut q = WikiSearchInput::from_query("分布式检索测试".to_string());
        q.tags = vec!["compliance".to_string()];
        let rt = wiki.search(q).await.unwrap();
        assert!(!rt.hits.is_empty());
        assert!(rt.hits.iter().all(|h| h.citation.doc_id == b.doc.id));

        // Docs mode dedupes to one hit per document.
        let mut q = WikiSearchInput::from_query("分布式检索测试".to_string());
        q.mode = WikiSearchMode::Docs;
        q.top_k = Some(10);
        let rt = wiki.search(q).await.unwrap();
        let doc_ids: Vec<_> = rt.hits.iter().map(|h| h.citation.doc_id).collect();
        let unique: std::collections::BTreeSet<_> = doc_ids.iter().collect();
        assert_eq!(doc_ids.len(), unique.len());
        assert_eq!(rt.total_docs_matched, 2);
    }

    #[tokio::test]
    async fn list_docs_paginates_without_gaps_or_dups() {
        let wiki = test_wiki("wiki_paging").await;
        for i in 0..5 {
            wiki.commit(
                "u".to_string(),
                commit_input(
                    &format!("分页文档{i}"),
                    &format!("# 分页文档{i}\n\n内容 {i}。\n"),
                ),
                1000 + i,
            )
            .await
            .unwrap();
        }

        let mut seen = std::collections::BTreeSet::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = wiki
                .list_docs(WikiListDocsInput {
                    cursor: cursor.clone(),
                    limit: Some(2),
                    ..Default::default()
                })
                .await
                .unwrap();
            for doc in &page.docs {
                assert!(seen.insert(doc.id), "duplicate doc {} across pages", doc.id);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen.len(), 5);
    }
}

#[cfg(test)]
mod m2_tests {
    use super::tests::{commit_input, test_wiki};
    use super::*;

    fn bundle_entry(path: &str, content: &str) -> WikiBundleEntry {
        WikiBundleEntry {
            path: path.to_string(),
            content: content.to_string(),
        }
    }

    #[tokio::test]
    async fn okf_import_maps_frontmatter_and_paths() {
        let wiki = test_wiki("wiki_okf_import").await;
        let out = wiki
            .import_bundle(
                "importer".to_string(),
                WikiImportInput {
                    entries: vec![
                        bundle_entry(
                            "guides/setup.md",
                            "---\ntype: Guide\ntitle: 安装指南\ntags: [setup, 中文]\nresource: https://example.com/setup\ncustom_field: 保留我\n---\n\n# 安装指南\n\n准备环境并执行安装脚本。\n",
                        ),
                        bundle_entry("index.md", "# listing"),
                        bundle_entry("notes/log.md", "history"),
                        bundle_entry("manifest.json", "{}"),
                        bundle_entry("../evil.md", "# nope"),
                    ],
                    namespace: Some("kb".to_string()),
                },
                1000,
            )
            .await
            .unwrap();

        assert_eq!(out.created, 1);
        assert_eq!(out.skipped.len(), 4);
        let doc = wiki.get_doc(out.docs[0].doc_id).await.unwrap();
        assert_eq!(doc.namespace, "kb");
        assert_eq!(doc.slug, "guides/setup");
        assert_eq!(doc.title, "安装指南");
        assert_eq!(doc.tags, vec!["setup".to_string(), "中文".to_string()]);
        assert_eq!(doc.source_uri.as_deref(), Some("https://example.com/setup"));
        assert!(
            doc.metadata
                .get("x_okf_frontmatter")
                .and_then(|v| v.as_str())
                .unwrap()
                .contains("custom_field: 保留我")
        );

        // Content excludes frontmatter: body starts at the heading.
        let read = wiki
            .read(WikiReadInput {
                doc_id: doc.id,
                version: None,
                selector: WikiSelector::Full,
            })
            .await
            .unwrap();
        assert!(read.content.unwrap().trim_start().starts_with("# 安装指南"));
    }

    #[tokio::test]
    async fn okf_round_trip_preserves_unknown_fields_with_zero_growth() {
        let wiki = test_wiki("wiki_okf_roundtrip").await;
        let entries = vec![
            bundle_entry(
                "policy/security.md",
                "---\ntype: Policy\ntitle: 安全政策\n# reviewer: alice — keep this comment\nunknown_key: 未知字段无损\ntags: [policy]\n---\n\n# 安全政策\n\n密钥必须存放在 KMS。\n",
            ),
            bundle_entry("faq.md", "# 常见问题\n\n没有 frontmatter 的文档。\n"),
        ];
        let input = WikiImportInput {
            entries: entries.clone(),
            namespace: Some("kb".to_string()),
        };

        let first = wiki
            .import_bundle("importer".to_string(), input.clone(), 1000)
            .await
            .unwrap();
        assert_eq!(first.created, 2);

        // Re-import: checksum-idempotent, zero version growth.
        let second = wiki
            .import_bundle("importer".to_string(), input.clone(), 2000)
            .await
            .unwrap();
        assert_eq!(second.unchanged, 2);
        assert_eq!(second.created + second.updated, 0);
        for doc in &first.docs {
            let versions = wiki
                .list_versions(doc.doc_id, None, Some(10))
                .await
                .unwrap();
            assert_eq!(versions.versions.len(), 1);
        }

        // Export: unknown fields and comments verbatim, x_anda_* appended.
        let export = wiki
            .export_bundle("exporter".to_string(), Some("kb".to_string()), 3000)
            .await
            .unwrap();
        assert_eq!(export.docs, 2);
        let sec = export
            .entries
            .iter()
            .find(|e| e.path == "policy/security.md")
            .unwrap();
        assert!(sec.content.contains("unknown_key: 未知字段无损"));
        assert!(
            sec.content
                .contains("# reviewer: alice — keep this comment")
        );
        assert!(sec.content.contains("x_anda_doc_id:"));
        assert!(sec.content.contains("x_anda_checksum: sha3-256:"));
        assert!(export.entries.iter().any(|e| e.path == "index.md"));
        let manifest = export
            .entries
            .iter()
            .find(|e| e.path == "manifest.json")
            .unwrap();
        assert!(manifest.content.contains("\"okf_version\": \"0.1\""));

        // Import the exported bundle back: still zero growth (x_anda_*
        // stripped; frontmatter-less doc gains synthesized metadata once).
        let reimport = wiki
            .import_bundle(
                "importer".to_string(),
                WikiImportInput {
                    entries: export.entries.clone(),
                    namespace: Some("kb".to_string()),
                },
                4000,
            )
            .await
            .unwrap();
        assert_eq!(reimport.created, 0);
        assert_eq!(reimport.unchanged + reimport.updated, 2);
        // The OKF-origin doc must be byte-stable across the full cycle.
        let sec_status = reimport
            .docs
            .iter()
            .find(|d| d.path == "policy/security.md")
            .unwrap();
        assert_eq!(sec_status.status, WikiImportStatus::Unchanged);

        // A second full cycle is completely stable for every doc.
        let export2 = wiki
            .export_bundle("exporter".to_string(), Some("kb".to_string()), 5000)
            .await
            .unwrap();
        let reimport2 = wiki
            .import_bundle(
                "importer".to_string(),
                WikiImportInput {
                    entries: export2.entries,
                    namespace: Some("kb".to_string()),
                },
                6000,
            )
            .await
            .unwrap();
        assert_eq!(reimport2.unchanged, 2);
        assert_eq!(reimport2.created + reimport2.updated, 0);
    }

    #[tokio::test]
    async fn okf_import_updates_changed_docs_in_place() {
        let wiki = test_wiki("wiki_okf_update").await;
        let v1 = WikiImportInput {
            entries: vec![bundle_entry("guide.md", "# 指南\n\n第一版内容。\n")],
            namespace: None,
        };
        let first = wiki.import_bundle("i".to_string(), v1, 1000).await.unwrap();
        assert_eq!(first.created, 1);

        let v2 = WikiImportInput {
            entries: vec![bundle_entry("guide.md", "# 指南\n\n第二版内容。\n")],
            namespace: None,
        };
        let second = wiki.import_bundle("i".to_string(), v2, 2000).await.unwrap();
        assert_eq!(second.updated, 1);
        assert_eq!(second.docs[0].doc_id, first.docs[0].doc_id);
        let versions = wiki
            .list_versions(first.docs[0].doc_id, None, Some(10))
            .await
            .unwrap();
        assert_eq!(versions.versions.len(), 2);
    }

    #[tokio::test]
    async fn neighbor_expansion_widens_and_merges_hits() {
        let wiki = test_wiki("wiki_expand").await;
        // Every section clears CHUNK_TARGET_MIN so the sibling-merge pass
        // keeps them as three separate chunks.
        let filler_a = "前置说明。".repeat(60);
        let filler_b = "核心细节。".repeat(60);
        let filler_c = "后续说明。".repeat(60);
        let content = format!(
            "# 邻域测试\n\n## 前言\n\n{filler_a}\n\n## 核心章节\n\n独特关键词：量子轨道谐振。\n\n{filler_b}\n\n## 附录\n\n{filler_c}\n"
        );
        let out = wiki
            .commit("u".to_string(), commit_input("邻域测试", &content), 1000)
            .await
            .unwrap();
        assert!(out.chunks >= 3, "fixture must span multiple chunks");

        // Baseline: the hit covers only the core section.
        let plain = wiki
            .search(WikiSearchInput::from_query("量子轨道谐振".to_string()))
            .await
            .unwrap();
        assert_eq!(plain.hits.len(), 1);
        assert!(!plain.hits[0].text.contains("前置说明"));

        // expand=1 pulls in both neighbors and the citation stays verifiable.
        let mut q = WikiSearchInput::from_query("量子轨道谐振".to_string());
        q.expand = Some(1);
        let expanded = wiki.search(q).await.unwrap();
        assert_eq!(expanded.hits.len(), 1);
        let hit = &expanded.hits[0];
        assert!(hit.text.contains("前置说明"));
        assert!(hit.text.contains("量子轨道谐振"));
        assert!(hit.text.contains("后续说明"));
        let (start, end) = hit.citation.byte_range;
        assert!(
            end - start > plain.hits[0].citation.byte_range.1 - plain.hits[0].citation.byte_range.0
        );
        let verified = wiki
            .verify(
                "u".to_string(),
                WikiVerifyInput {
                    uri: Some(hit.citation.uri.clone()),
                    checksum: Some(hit.citation.checksum.clone()),
                    ..Default::default()
                },
                2000,
            )
            .await
            .unwrap();
        assert_eq!(verified.status, WikiVerifyStatus::Valid);

        // Two adjacent hits merge into one expanded hit instead of
        // duplicating overlapping context.
        let mut q = WikiSearchInput::from_query("前置说明 后续说明".to_string());
        q.expand = Some(2);
        q.top_k = Some(10);
        let merged = wiki.search(q).await.unwrap();
        assert_eq!(merged.hits.len(), 1);
        assert!(merged.hits[0].text.contains("量子轨道谐振"));
    }
}
