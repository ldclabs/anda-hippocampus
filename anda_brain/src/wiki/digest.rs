//! WikiDigest: distills committed wiki versions into the Cognitive Nexus
//! (PRD §7.3), the graph half of the "graph understands, wiki proves" story.
//!
//! Provenance-by-construction: the LLM only proposes structured facts
//! (subject/predicate/object + section anchor); this module renders the KIP
//! KML itself, attaching `source/citation/checksum/extractor/confidence`
//! metadata to every proposition. A prompt can forget provenance — a
//! renderer cannot. Superseding works the same way: when a new version is
//! digested, facts the new extraction no longer asserts get their
//! propositions marked `superseded` (graph maintenance owns any deeper
//! contradiction resolution). Every digest is recorded as a
//! `DigestExtracted` wiki event whose fact list doubles as the citation
//! sample for verification.

use anda_core::{BoxError, CompletionFeatures, CompletionRequest, Usage};
use anda_db::{
    query::{Filter, Fv, Query, RangeQuery},
    schema::Json,
};
use anda_engine::{context::AgentCtx, memory::MemoryManagement, model::Models};
use anda_kip::{parse_kml, parse_kql};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    Collection, EVENT_DIGEST_EXTRACTED, EVENT_DIGEST_FAILED, WikiChunkRecord, WikiDocRecord,
    WikiError, WikiService, WikiVerifyInput, WikiVerifyStatus, WikiVersionRecord,
    chunk::chunk_checksum, citation_uri, evalset::EVAL_NAMESPACE,
};

/// Extractor fingerprint prefix written into proposition metadata; bump on
/// prompt or renderer changes so maintenance can bulk-invalidate old
/// extractions. The full fingerprint appends the model id (PRD §7.3):
/// `wiki_digest@v1/<model_id>`.
pub const WIKI_DIGEST_EXTRACTOR: &str = "wiki_digest@v1";
const DIGEST_PROMPT: &str = include_str!("../../assets/BrainWikiDigest.md");
/// Collection-extension key holding the digest high-water mark (version id).
const DIGEST_CURSOR_KEY: &str = "wiki_digested";
const DIGEST_USAGE_KEY: &str = "wiki_digest_usage";
/// Collection-extension key tracking consecutive failures of one version
/// (the poison-version fuse).
const DIGEST_FAILURE_KEY: &str = "wiki_digest_failure";
/// Collection-extension key holding doc ids queued for a digest catch-up
/// (documents restored from archive after the cursor passed their version).
pub(super) const DIGEST_PENDING_KEY: &str = "wiki_digest_pending";
/// After this many consecutive failures a version is skipped (with a
/// `DigestFailed` event) instead of wedging the pipeline and re-burning
/// tokens every run.
const MAX_VERSION_FAILURES: u64 = 3;
const MAX_FACTS_PER_VERSION: usize = 64;
const MAX_EXTRA_CONCEPTS: usize = 64;
const MAX_BATCH_BYTES: usize = 24 * 1024;
const MAX_VERSIONS_PER_RUN: usize = 20;
const MAX_IDENT_CHARS: usize = 120;
/// How many recent digests the post-run citation sample re-verifies.
const VERIFY_SAMPLE_EVENTS: usize = 5;

/// Resets the running flag on drop so a panicking digest never wedges.
struct RunningGuard(Arc<AtomicU64>);
impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct WikiDigest {
    wiki: Arc<WikiService>,
    memory: Arc<MemoryManagement>,
    /// For the extractor fingerprint: `wiki_digest@v1/<model_id>`.
    models: Arc<Models>,
    /// 0 = idle; otherwise the version id currently being digested.
    running: Arc<AtomicU64>,
}

/// Outcome of one version's digest attempt.
enum DigestOutcome {
    Digested,
    /// Permanently not digestible (superseded, archived, labeled, eval
    /// corpus, reclaimed): the cursor advances past it.
    Skipped,
    /// The version row exists but its document has not flipped to it yet
    /// (commit in flight, or a crash leftover awaiting the orphan sweep):
    /// the cursor must NOT advance, or the version would silently never be
    /// digested.
    NotReady,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WikiDigestReport {
    /// Versions digested into the graph this run.
    pub digested: usize,
    /// Propositions written (across all digested versions).
    pub facts: usize,
    /// Propositions from older versions marked superseded.
    pub superseded: usize,
    /// Pending versions skipped (already superseded, archived, eval corpus).
    pub skipped: usize,
    /// Post-run citation sample: how many were checked / found corrupt.
    pub citations_checked: usize,
    pub citations_invalid: usize,
    pub usage: Usage,
}

/// LLM output schema (see assets/BrainWikiDigest.md).
#[derive(Debug, Clone, Default, Deserialize)]
struct Extraction {
    #[serde(default)]
    concepts: Vec<ExtractedConcept>,
    #[serde(default)]
    facts: Vec<ExtractedFact>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExtractedConcept {
    r#type: String,
    name: String,
    #[serde(default)]
    attributes: serde_json::Map<String, Json>,
}

#[derive(Debug, Clone, Deserialize)]
struct ConceptRef {
    r#type: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ExtractedFact {
    subject: ConceptRef,
    predicate: String,
    object: ConceptRef,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    anchor: Option<String>,
}

/// A validated fact with its resolved citation, as persisted in the
/// `DigestExtracted` event (the digest ledger used for superseding and
/// citation sampling).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DigestedFact {
    pub subject_type: String,
    pub subject_name: String,
    pub predicate: String,
    pub object_type: String,
    pub object_name: String,
    pub confidence: f64,
    pub citation: String,
    pub checksum: String,
}

/// (subject_type, subject_name, predicate, object_type, object_name)
type TripleKey = (String, String, String, String, String);

impl DigestedFact {
    fn triple_key(&self) -> TripleKey {
        (
            self.subject_type.clone(),
            self.subject_name.clone(),
            self.predicate.clone(),
            self.object_type.clone(),
            self.object_name.clone(),
        )
    }
}

impl WikiDigest {
    pub fn new(wiki: Arc<WikiService>, memory: Arc<MemoryManagement>, models: Arc<Models>) -> Self {
        Self {
            wiki,
            memory,
            models,
            running: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn is_processing(&self) -> bool {
        self.running.load(Ordering::SeqCst) != 0
    }

    /// The extractor fingerprint, model id included, so §13's
    /// "bulk-invalidate by fingerprint" can target one model's extractions.
    fn extractor(&self) -> String {
        match self.models.get_model() {
            Some(model) => format!("{WIKI_DIGEST_EXTRACTOR}/{}", model.model_name()),
            None => WIKI_DIGEST_EXTRACTOR.to_string(),
        }
    }

    /// High-water mark: the largest version id already digested (or skipped).
    pub fn cursor(&self) -> u64 {
        self.wiki
            .docs
            .get_extension_as::<u64>(DIGEST_CURSOR_KEY)
            .unwrap_or_default()
    }

    /// Digests all pending versions (bounded per run), supersedes stale
    /// facts, then re-verifies a citation sample from recent digests.
    /// Single-flight per space; failures stop the run without advancing the
    /// cursor past the failed version, so the next run retries it.
    pub async fn run_pending(
        &self,
        ctx: AgentCtx,
        now_ms: u64,
    ) -> Result<WikiDigestReport, BoxError> {
        if self
            .running
            .compare_exchange(0, u64::MAX, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("wiki digest is already running".into());
        }
        let _guard = RunningGuard(self.running.clone());

        let mut report = WikiDigestReport::default();
        let mut cursor = self.cursor();
        let mut processed = 0usize;

        'run: while processed < MAX_VERSIONS_PER_RUN {
            let versions: Vec<WikiVersionRecord> = self
                .wiki
                .versions
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(cursor)),
                    ))),
                    limit: Some(MAX_VERSIONS_PER_RUN),
                })
                .await
                .map_err(WikiError::from)?;
            if versions.is_empty() {
                break;
            }

            for version in versions {
                if processed >= MAX_VERSIONS_PER_RUN {
                    break 'run;
                }
                processed += 1;
                self.running.store(version._id, Ordering::SeqCst);

                match self
                    .digest_version(&ctx, &version, now_ms, &mut report)
                    .await
                {
                    Ok(DigestOutcome::NotReady) => {
                        // Commit in flight: retry from here next run (the
                        // orphan sweep reclaims it if the commit crashed).
                        break 'run;
                    }
                    Ok(_) => {
                        cursor = version._id;
                        self.save_cursor(cursor).await;
                        self.clear_failure();
                    }
                    Err(err) => {
                        let failures = self.bump_failure(version._id);
                        if failures >= MAX_VERSION_FAILURES {
                            // Poison-version fuse: skip it after repeated
                            // failures so one bad document cannot wedge the
                            // pipeline and re-burn tokens forever.
                            log::error!(
                                target: "brain",
                                version_id = version._id,
                                doc_id = version.doc_id;
                                "wiki digest failed {failures} times, skipping version: {err:?}"
                            );
                            let _ = self
                                .wiki
                                .write_event(
                                    EVENT_DIGEST_FAILED,
                                    Some(version.doc_id),
                                    Some(version._id),
                                    "wiki_digest".to_string(),
                                    BTreeMap::from([
                                        ("error".to_string(), Json::from(err.to_string())),
                                        ("attempts".to_string(), Json::from(failures)),
                                        ("extractor".to_string(), Json::from(self.extractor())),
                                    ]),
                                    now_ms,
                                )
                                .await;
                            cursor = version._id;
                            self.save_cursor(cursor).await;
                            self.clear_failure();
                            report.skipped += 1;
                            continue;
                        }
                        // Leave the cursor before the failed version: the
                        // next run retries it instead of silently skipping.
                        log::error!(
                            target: "brain",
                            version_id = version._id,
                            doc_id = version.doc_id;
                            "wiki digest failed (attempt {failures}/{MAX_VERSION_FAILURES}): {err:?}"
                        );
                        self.save_usage(&report.usage).await;
                        return Err(err);
                    }
                }
            }
        }

        self.digest_pending_restores(&ctx, now_ms, cursor, &mut processed, &mut report)
            .await;

        let (checked, invalid) = self.verify_recent(now_ms).await?;
        report.citations_checked = checked;
        report.citations_invalid = invalid;
        self.save_usage(&report.usage).await;
        Ok(report)
    }

    /// Catch-up pass for documents restored from archive after the cursor
    /// passed their version: the main loop never revisits those ids, so
    /// without this their facts would never enter the graph. Each queued
    /// document's current version is digested unless the ledger shows it
    /// already was. Never fails the run; entries that error stay queued,
    /// with the poison fuse bounding retries.
    async fn digest_pending_restores(
        &self,
        ctx: &AgentCtx,
        now_ms: u64,
        cursor: u64,
        processed: &mut usize,
        report: &mut WikiDigestReport,
    ) {
        let pending = self
            .wiki
            .docs
            .get_extension_as::<BTreeSet<u64>>(DIGEST_PENDING_KEY)
            .unwrap_or_default();
        for doc_id in pending {
            if *processed >= MAX_VERSIONS_PER_RUN {
                break; // budget spent; the rest stays queued for the next run
            }
            let doc = match self.wiki.doc_record(doc_id).await {
                Ok(doc) => doc,
                Err(WikiError::NotFound(_)) => {
                    // Reclaimed or re-initializing: nothing to catch up.
                    self.clear_pending(doc_id);
                    continue;
                }
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        doc_id = doc_id;
                        "pending digest doc load failed, kept queued: {err:?}"
                    );
                    continue;
                }
            };
            if doc.current_version > cursor {
                // The main cursor loop reaches this version by itself; once
                // its DigestExtracted lands, the ledger check below clears
                // the entry on the next run.
                continue;
            }
            match version_digested(&self.wiki, doc_id, doc.current_version).await {
                Ok(true) => {
                    self.clear_pending(doc_id);
                    continue;
                }
                Ok(false) => {}
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        doc_id = doc_id;
                        "pending digest ledger check failed, kept queued: {err:?}"
                    );
                    continue;
                }
            }
            let version = match self.wiki.version_record(doc.current_version).await {
                Ok(version) => version,
                Err(err) => {
                    // The current version row is gone for good: unqueue.
                    log::warn!(
                        target: "brain",
                        doc_id = doc_id,
                        version_id = doc.current_version;
                        "pending digest version load failed, dropped: {err:?}"
                    );
                    self.clear_pending(doc_id);
                    continue;
                }
            };
            *processed += 1;
            self.running.store(version._id, Ordering::SeqCst);
            match self.digest_version(ctx, &version, now_ms, report).await {
                // Digested, or permanently skipped (re-archived, labeled,
                // eval corpus): either way the queue entry is done. A
                // later restore re-queues re-archived documents.
                Ok(_) => {
                    self.clear_pending(doc_id);
                    self.clear_failure();
                }
                Err(err) => {
                    let failures = self.bump_failure(version._id);
                    log::error!(
                        target: "brain",
                        version_id = version._id,
                        doc_id = doc_id;
                        "pending digest failed (attempt {failures}/{MAX_VERSION_FAILURES}): {err:?}"
                    );
                    if failures >= MAX_VERSION_FAILURES {
                        let _ = self
                            .wiki
                            .write_event(
                                EVENT_DIGEST_FAILED,
                                Some(doc_id),
                                Some(version._id),
                                "wiki_digest".to_string(),
                                BTreeMap::from([
                                    ("error".to_string(), Json::from(err.to_string())),
                                    ("attempts".to_string(), Json::from(failures)),
                                    ("extractor".to_string(), Json::from(self.extractor())),
                                ]),
                                now_ms,
                            )
                            .await;
                        self.clear_pending(doc_id);
                        self.clear_failure();
                        report.skipped += 1;
                    }
                }
            }
        }
    }

    fn clear_pending(&self, doc_id: u64) {
        let _ = self.wiki.docs.set_extension_from_with::<_, BTreeSet<u64>>(
            DIGEST_PENDING_KEY.to_string(),
            |v| {
                let mut set = v.unwrap_or_default();
                set.remove(&doc_id);
                Some(set)
            },
        );
    }

    async fn digest_version(
        &self,
        ctx: &AgentCtx,
        version: &WikiVersionRecord,
        now_ms: u64,
        report: &mut WikiDigestReport,
    ) -> Result<DigestOutcome, BoxError> {
        let doc = match self.wiki.doc_record(version.doc_id).await {
            Ok(doc) => doc,
            // Orphan or reclaimed document: nothing to digest.
            Err(WikiError::NotFound(_)) => {
                report.skipped += 1;
                return Ok(DigestOutcome::Skipped);
            }
            Err(err) => return Err(err.into()),
        };
        if version._id > doc.current_version {
            // Written but not flipped: a concurrent commit is between step 1
            // and its activation point. Advancing past it here would leave
            // the graph stale until the document's next commit.
            return Ok(DigestOutcome::NotReady);
        }
        if doc.current_version != version._id
            || doc.status != super::DOC_STATUS_ACTIVE
            || doc.namespace == EVAL_NAMESPACE
            // The Cognitive Nexus has no ACL: distilling a labeled document
            // would let any Read principal recall its facts (and citation
            // URIs) through the graph.
            || !doc.acl_label.is_empty()
        {
            report.skipped += 1;
            return Ok(DigestOutcome::Skipped);
        }

        let Some(chunks) = digest_chunks(&self.wiki, version).await? else {
            // The doc pointed at this version a moment ago but its chunks
            // are gone: a concurrent commit superseded it mid-digest. Skip —
            // running an empty extraction here would let `supersede_stale`
            // mark every fact of the previous digest superseded.
            report.skipped += 1;
            return Ok(DigestOutcome::Skipped);
        };
        let extraction = self.extract(ctx, &doc, version, &chunks, report).await?;
        let extractor = self.extractor();
        let (facts, alive) =
            normalize_facts(&self.wiki.space_id, &doc, version, &chunks, &extraction);

        let mut proposition_ids: Vec<String> = Vec::new();
        if !facts.is_empty() {
            let kml = render_digest_kml(
                &self.wiki.space_id,
                &doc,
                version,
                &extraction,
                &facts,
                &extractor,
            );
            let response = self
                .memory
                .nexus
                .execute_kml(parse_kml(&kml)?, false)
                .await?;
            proposition_ids = response
                .get("upsert_proposition_links")
                .and_then(|v| v.as_array())
                .map(|ids| {
                    ids.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
        }

        let superseded = self.supersede_stale(&doc, version, &alive, now_ms).await?;

        self.wiki
            .write_event(
                EVENT_DIGEST_EXTRACTED,
                Some(doc._id),
                Some(version._id),
                "wiki_digest".to_string(),
                BTreeMap::from([
                    (
                        "facts".to_string(),
                        serde_json::to_value(&facts).unwrap_or(Json::Null),
                    ),
                    (
                        "proposition_ids".to_string(),
                        Json::from(proposition_ids.clone()),
                    ),
                    ("superseded".to_string(), Json::from(superseded as u64)),
                    ("extractor".to_string(), Json::from(extractor)),
                ]),
                now_ms,
            )
            .await?;

        report.digested += 1;
        report.facts += facts.len();
        report.superseded += superseded;
        Ok(DigestOutcome::Digested)
    }

    /// Runs the extraction prompt over section batches and merges the
    /// results. One retry per batch on non-JSON replies.
    async fn extract(
        &self,
        ctx: &AgentCtx,
        doc: &WikiDocRecord,
        version: &WikiVersionRecord,
        chunks: &[WikiChunkRecord],
        report: &mut WikiDigestReport,
    ) -> Result<Extraction, BoxError> {
        let header = format!(
            "Document: {}\nURI: {}\nNamespace: {}\nTags: {}\n",
            doc.title,
            citation_uri(&self.wiki.space_id, doc._id, version._id, 0, version.size),
            doc.namespace,
            doc.tags.join(", "),
        );

        let mut batches: Vec<String> = Vec::new();
        let mut batch = String::new();
        for chunk in chunks {
            let section = format!(
                "\n[anchor: {}] {}\n{}\n",
                chunk.anchor,
                chunk.heading_path.join(" > "),
                chunk.text,
            );
            if !batch.is_empty() && batch.len() + section.len() > MAX_BATCH_BYTES {
                batches.push(std::mem::take(&mut batch));
            }
            batch.push_str(&section);
        }
        if !batch.is_empty() {
            batches.push(batch);
        }

        let mut merged = Extraction::default();
        for batch in batches {
            let prompt = format!("{header}{batch}");
            let extraction = self.extract_batch(ctx, prompt, report).await?;
            merged.concepts.extend(extraction.concepts);
            merged.facts.extend(extraction.facts);
        }
        Ok(merged)
    }

    async fn extract_batch(
        &self,
        ctx: &AgentCtx,
        prompt: String,
        report: &mut WikiDigestReport,
    ) -> Result<Extraction, BoxError> {
        let mut attempt_prompt = prompt.clone();
        for attempt in 0..2 {
            let res = ctx
                .completion(
                    CompletionRequest {
                        instructions: DIGEST_PROMPT.to_string(),
                        prompt: attempt_prompt.clone(),
                        ..Default::default()
                    },
                    Vec::new(),
                )
                .await?;
            report.usage.accumulate(&res.usage);
            if let Some(reason) = res.failed_reason {
                return Err(format!("digest completion failed: {reason}").into());
            }
            match parse_extraction(&res.content) {
                Ok(extraction) => return Ok(extraction),
                Err(err) if attempt == 0 => {
                    attempt_prompt = format!(
                        "{prompt}\n\nYour previous reply was not the required JSON object ({err}). Reply with ONLY the JSON object."
                    );
                }
                Err(err) => {
                    return Err(format!("digest extraction returned invalid JSON: {err}").into());
                }
            }
        }
        unreachable!("extract_batch loops at most twice")
    }

    /// Marks propositions asserted by this document's previous digest but
    /// absent from the new one as superseded. Per-fact capsules keep one
    /// missing endpoint (e.g. merged away by maintenance) from aborting the
    /// rest. `alive` covers every valid extracted triple (not just the
    /// persisted, capped prefix) so a large document never mis-supersedes
    /// facts that were merely truncated away.
    async fn supersede_stale(
        &self,
        doc: &WikiDocRecord,
        version: &WikiVersionRecord,
        alive: &BTreeSet<TripleKey>,
        _now_ms: u64,
    ) -> Result<usize, BoxError> {
        let previous = self.previous_digest_facts(doc._id, version._id).await?;
        if previous.is_empty() {
            return Ok(0);
        }
        let superseded_by =
            citation_uri(&self.wiki.space_id, doc._id, version._id, 0, version.size);

        let mut superseded = 0usize;
        for fact in previous {
            if alive.contains(&fact.triple_key()) {
                continue;
            }
            // Graph maintenance may have metabolized the proposition away; a
            // supersede UPSERT would then resurrect it as a tombstone. Only
            // touch propositions that still exist.
            if !self.proposition_exists(&fact).await {
                continue;
            }
            let kml = render_supersede_kml(&fact, &superseded_by);
            match parse_kml(&kml) {
                Ok(cmd) => match self.memory.nexus.execute_kml(cmd, false).await {
                    Ok(_) => superseded += 1,
                    Err(err) => {
                        log::warn!(
                            target: "brain",
                            doc_id = doc._id;
                            "supersede skipped for {:?}: {err:?}",
                            fact.predicate
                        );
                    }
                },
                Err(err) => {
                    log::warn!(target: "brain", doc_id = doc._id; "supersede kml parse failed: {err:?}");
                }
            }
        }
        Ok(superseded)
    }

    /// Existence probe for one (subject, predicate, object) proposition.
    /// Fails toward `true`: a redundant superseded marker is cheaper than a
    /// stale fact staying active because the probe errored.
    async fn proposition_exists(&self, fact: &DigestedFact) -> bool {
        let kql = format!(
            "FIND(?link) WHERE {{ ?link ({}, {}, {}) }} LIMIT 1",
            concept_literal(&fact.subject_type, &fact.subject_name),
            serde_json::to_string(&fact.predicate).unwrap_or_default(),
            concept_literal(&fact.object_type, &fact.object_name),
        );
        let query = match parse_kql(&kql) {
            Ok(query) => query,
            Err(err) => {
                log::warn!(target: "brain", "proposition existence kql parse failed: {err:?}");
                return true;
            }
        };
        match self.memory.nexus.execute_kql(query).await {
            Ok((result, _)) => kql_has_rows(&result),
            Err(err) => {
                log::warn!(target: "brain", "proposition existence probe failed: {err:?}");
                true
            }
        }
    }

    /// Facts recorded by the most recent digest of this document before the
    /// given version.
    async fn previous_digest_facts(
        &self,
        doc_id: u64,
        before_version: u64,
    ) -> Result<Vec<DigestedFact>, BoxError> {
        let events = self
            .wiki
            .list_events(
                Some(EVENT_DIGEST_EXTRACTED.to_string()),
                Some(doc_id),
                None,
                Some(20),
            )
            .await?;
        let latest = events
            .events
            .iter()
            .filter(|e| e.version_id.is_some_and(|v| v < before_version))
            .max_by_key(|e| e.id);
        let Some(event) = latest else {
            return Ok(Vec::new());
        };
        let facts = match event
            .detail
            .get("facts")
            .cloned()
            .map(serde_json::from_value::<Vec<DigestedFact>>)
        {
            Some(Ok(facts)) => facts,
            Some(Err(err)) => {
                // Schema drift would otherwise disable superseding silently.
                log::warn!(
                    target: "brain",
                    doc_id = doc_id,
                    event_id = event.id;
                    "digest ledger facts unreadable, superseding skipped for this pass: {err}"
                );
                Vec::new()
            }
            None => Vec::new(),
        };
        Ok(facts)
    }

    /// Re-verifies every citation recorded by recent digests; `Invalid`
    /// results are corruption signals (evented inside `verify` when the
    /// stored content itself is corrupt).
    pub async fn verify_recent(&self, now_ms: u64) -> Result<(usize, usize), BoxError> {
        verify_recent_citations(&self.wiki, now_ms).await
    }

    async fn save_cursor(&self, cursor: u64) {
        if let Err(err) = self
            .wiki
            .docs
            .save_extension(DIGEST_CURSOR_KEY.to_string(), cursor.into())
            .await
        {
            // Non-fatal: the next run re-digests from the stale cursor
            // (idempotent for the graph, but re-billed), so be loud.
            log::warn!(
                target: "brain",
                cursor = cursor;
                "wiki digest cursor save failed (next run will re-digest): {err:?}"
            );
        }
    }

    async fn save_usage(&self, usage: &Usage) {
        if usage.requests == 0 {
            return;
        }
        // In-memory metadata update, flushed with the collection; the return
        // value is the previous entry (None on first write), not an error.
        let _ =
            self.wiki
                .docs
                .set_extension_from_with::<_, Usage>(DIGEST_USAGE_KEY.to_string(), |v| {
                    let mut total: Usage = v.unwrap_or_default();
                    total.accumulate(usage);
                    Some(total)
                });
    }

    /// Consecutive-failure counter for the poison fuse; resets whenever a
    /// different version fails or any version succeeds.
    fn bump_failure(&self, version_id: u64) -> u64 {
        let mut count = 1u64;
        let _ = self.wiki.docs.set_extension_from_with::<_, (u64, u64)>(
            DIGEST_FAILURE_KEY.to_string(),
            |v| {
                if let Some((prev, prev_count)) = v
                    && prev == version_id
                {
                    count = prev_count + 1;
                }
                Some((version_id, count))
            },
        );
        count
    }

    fn clear_failure(&self) {
        let _ = self
            .wiki
            .docs
            .set_extension_from_with::<_, (u64, u64)>(DIGEST_FAILURE_KEY.to_string(), |_| {
                Some((0, 0))
            });
    }
}

/// Chunk rows for one version, or `None` when they raced away: a concurrent
/// commit can activate a newer version and delete this version's chunks
/// between the caller's doc check and this read. A committed version always
/// has at least one chunk (content is never empty), so an empty set is that
/// race, not a real state — callers must skip WITHOUT extracting or
/// superseding, or the previous digest's facts would all be marked stale.
async fn digest_chunks(
    wiki: &WikiService,
    version: &WikiVersionRecord,
) -> Result<Option<Vec<WikiChunkRecord>>, BoxError> {
    let mut rows: Vec<WikiChunkRecord> = wiki
        .chunks
        .search_as(Query {
            search: None,
            filter: Some(Filter::And(vec![
                Box::new(Filter::Field((
                    "doc_id".to_string(),
                    RangeQuery::Eq(Fv::U64(version.doc_id)),
                ))),
                Box::new(Filter::Field((
                    "version_id".to_string(),
                    RangeQuery::Eq(Fv::U64(version._id)),
                ))),
            ])),
            limit: Some(Collection::MAX_SEARCH_LIMIT),
        })
        .await
        .map_err(WikiError::from)?;
    if rows.is_empty() {
        match wiki.doc_record(version.doc_id).await {
            Ok(doc) if doc.current_version == version._id => {
                // Should be impossible (commits delete chunks only after
                // flipping the doc away): stay loud, but never fall through
                // to an empty extraction.
                log::warn!(
                    target: "brain",
                    doc_id = version.doc_id,
                    version_id = version._id;
                    "current version has no chunk rows; digest skipped without superseding"
                );
            }
            Ok(_) | Err(WikiError::NotFound(_)) => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(None);
    }
    rows.sort_by_key(|row| row.ordinal);
    Ok(Some(rows))
}

/// Whether the digest ledger already covers (doc, version): true when the
/// document's newest `DigestExtracted` event points at that version.
async fn version_digested(
    wiki: &WikiService,
    doc_id: u64,
    version_id: u64,
) -> Result<bool, BoxError> {
    let events = wiki
        .list_events(
            Some(EVENT_DIGEST_EXTRACTED.to_string()),
            Some(doc_id),
            None,
            Some(20),
        )
        .await?;
    Ok(events
        .events
        .iter()
        .max_by_key(|e| e.id)
        .is_some_and(|e| e.version_id == Some(version_id)))
}

/// Re-verifies the citations recorded by the most recent digests. Facts of
/// one digest cite the same version, so (doc, version) loads are cached
/// across facts instead of re-reading full version content per fact.
async fn verify_recent_citations(
    wiki: &WikiService,
    now_ms: u64,
) -> Result<(usize, usize), BoxError> {
    let events = wiki
        .list_events(
            Some(EVENT_DIGEST_EXTRACTED.to_string()),
            None,
            None,
            Some(VERIFY_SAMPLE_EVENTS),
        )
        .await?;
    type Loaded = Option<(WikiDocRecord, WikiVersionRecord)>;
    let mut cache: BTreeMap<(u64, u64), Loaded> = BTreeMap::new();
    let mut checked = 0usize;
    let mut invalid = 0usize;
    for event in events.events {
        let Some(facts) = event
            .detail
            .get("facts")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<DigestedFact>>(v).ok())
        else {
            continue;
        };
        for fact in facts {
            let input = WikiVerifyInput {
                uri: Some(fact.citation.clone()),
                checksum: Some(fact.checksum.clone()),
                ..Default::default()
            };
            let (doc_id, version_id, start, end) = wiki.verify_target(&input)?;
            let loaded = match cache.entry((doc_id, version_id)) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let loaded = match wiki.doc_record(doc_id).await {
                        Ok(doc) => match wiki.version_record(version_id).await {
                            Ok(version) => Some((doc, version)),
                            Err(_) => None,
                        },
                        Err(_) => None,
                    };
                    entry.insert(loaded)
                }
            };
            let status = match loaded {
                Some((doc, version)) => {
                    wiki.verify_resolved(
                        "wiki_digest".to_string(),
                        doc,
                        version,
                        (start, end),
                        Some(&fact.checksum),
                        now_ms,
                    )
                    .await?
                    .status
                }
                None => WikiVerifyStatus::NotFound,
            };
            checked += 1;
            if status == WikiVerifyStatus::Invalid {
                invalid += 1;
            }
        }
    }
    Ok((checked, invalid))
}

/// Whether a KQL FIND result contains any row.
fn kql_has_rows(result: &Json) -> bool {
    match result {
        Json::Array(rows) => !rows.is_empty(),
        Json::Object(map) => map.values().any(kql_has_rows),
        Json::Null => false,
        _ => true,
    }
}

/// Parses the extraction JSON, tolerating markdown fences and surrounding
/// prose (first `{` to last `}`).
fn parse_extraction(content: &str) -> Result<Extraction, String> {
    let trimmed = content.trim();
    if let Ok(extraction) = serde_json::from_str::<Extraction>(trimmed) {
        return Ok(extraction);
    }
    let start = trimmed.find('{').ok_or("no JSON object found")?;
    let end = trimmed.rfind('}').ok_or("no JSON object found")?;
    if start >= end {
        return Err("no JSON object found".to_string());
    }
    serde_json::from_str::<Extraction>(&trimmed[start..=end]).map_err(|err| err.to_string())
}

fn clean_ident(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_IDENT_CHARS
        || value.starts_with('$')
        || value.starts_with('_')
    {
        return None;
    }
    Some(value.to_string())
}

/// Validates extracted facts and resolves each anchor to its chunk's byte
/// range and checksum; facts with unknown anchors cite the whole version.
/// Returns the persisted facts (capped at [`MAX_FACTS_PER_VERSION`]) plus
/// the alive-set of ALL valid triples: superseding compares against the
/// uncapped set, so truncation never marks a still-asserted fact stale.
fn normalize_facts(
    space_id: &str,
    doc: &WikiDocRecord,
    version: &WikiVersionRecord,
    chunks: &[WikiChunkRecord],
    extraction: &Extraction,
) -> (Vec<DigestedFact>, BTreeSet<TripleKey>) {
    let by_anchor: BTreeMap<&str, &WikiChunkRecord> = chunks
        .iter()
        .map(|chunk| (chunk.anchor.as_str(), chunk))
        .collect();
    let whole_doc = (
        citation_uri(space_id, doc._id, version._id, 0, version.size),
        chunk_checksum(
            &version.checksum,
            0,
            version.size as usize,
            &version.content,
        ),
    );

    let mut alive = BTreeSet::new();
    let mut facts = Vec::new();
    for fact in &extraction.facts {
        let (Some(s_type), Some(s_name), Some(predicate), Some(o_type), Some(o_name)) = (
            clean_ident(&fact.subject.r#type),
            clean_ident(&fact.subject.name),
            clean_ident(&fact.predicate),
            clean_ident(&fact.object.r#type),
            clean_ident(&fact.object.name),
        ) else {
            continue;
        };
        let (citation, checksum) = fact
            .anchor
            .as_deref()
            .and_then(|anchor| by_anchor.get(anchor))
            .map(|chunk| {
                (
                    citation_uri(
                        space_id,
                        doc._id,
                        version._id,
                        chunk.byte_start,
                        chunk.byte_end,
                    ),
                    chunk.checksum.clone(),
                )
            })
            .unwrap_or_else(|| whole_doc.clone());
        let normalized = DigestedFact {
            subject_type: s_type,
            subject_name: s_name,
            predicate,
            object_type: o_type,
            object_name: o_name,
            confidence: fact.confidence.unwrap_or(0.7).clamp(0.0, 1.0),
            citation,
            checksum,
        };
        if alive.insert(normalized.triple_key()) && facts.len() < MAX_FACTS_PER_VERSION {
            facts.push(normalized);
        }
    }
    (facts, alive)
}

/// Renders a KIP object literal with unquoted identifier keys (the concept
/// matcher grammar requires them) and JSON-encoded values.
fn kip_object(pairs: &[(&str, Json)]) -> String {
    let body = pairs
        .iter()
        .filter(|(key, _)| is_kip_identifier(key))
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{body}}}")
}

fn is_kip_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn concept_literal(r#type: &str, name: &str) -> String {
    kip_object(&[("type", json!(r#type)), ("name", json!(name))])
}

/// Renders one atomic UPSERT capsule: schema registration for every type
/// and predicate used (KIP requires define-before-use), concept blocks,
/// then one proposition block per fact with its citation metadata. The
/// global metadata block carries the shared provenance envelope.
fn render_digest_kml(
    space_id: &str,
    doc: &WikiDocRecord,
    version: &WikiVersionRecord,
    extraction: &Extraction,
    facts: &[DigestedFact],
    extractor: &str,
) -> String {
    let mut concept_types = BTreeSet::new();
    let mut predicates = BTreeSet::new();
    let mut endpoints: Vec<(String, String)> = Vec::new();
    let mut endpoint_seen = BTreeSet::new();
    let mut push_endpoint = |t: &str, n: &str, endpoints: &mut Vec<(String, String)>| {
        if endpoint_seen.insert((t.to_string(), n.to_string())) {
            endpoints.push((t.to_string(), n.to_string()));
        }
    };
    for fact in facts {
        concept_types.insert(fact.subject_type.clone());
        concept_types.insert(fact.object_type.clone());
        predicates.insert(fact.predicate.clone());
        push_endpoint(&fact.subject_type, &fact.subject_name, &mut endpoints);
        push_endpoint(&fact.object_type, &fact.object_name, &mut endpoints);
    }

    // Optional descriptions from the extraction, only for endpoints in use.
    let mut attributes: BTreeMap<(String, String), serde_json::Map<String, Json>> = BTreeMap::new();
    for concept in extraction.concepts.iter().take(MAX_EXTRA_CONCEPTS) {
        let (Some(t), Some(n)) = (clean_ident(&concept.r#type), clean_ident(&concept.name)) else {
            continue;
        };
        if !concept.attributes.is_empty() {
            attributes.insert((t, n), concept.attributes.clone());
        }
    }

    let mut lines = vec!["UPSERT {".to_string()];
    for (idx, kind) in concept_types.iter().enumerate() {
        lines.push(format!(
            "  CONCEPT ?ct{idx} {{ {} }}",
            concept_literal("$ConceptType", kind)
        ));
    }
    for (idx, predicate) in predicates.iter().enumerate() {
        lines.push(format!(
            "  CONCEPT ?pt{idx} {{ {} }}",
            concept_literal("$PropositionType", predicate)
        ));
    }
    let mut handles: BTreeMap<(String, String), String> = BTreeMap::new();
    for (idx, (t, n)) in endpoints.iter().enumerate() {
        let handle = format!("?c{idx}");
        let mut block = format!("  CONCEPT {handle} {{ {}", concept_literal(t, n));
        if let Some(attrs) = attributes.get(&(t.clone(), n.clone())) {
            let pairs: Vec<(&str, Json)> =
                attrs.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            block.push_str(&format!(" SET ATTRIBUTES {}", kip_object(&pairs)));
        }
        block.push_str(" }");
        lines.push(block);
        handles.insert((t.clone(), n.clone()), handle);
    }
    for (idx, fact) in facts.iter().enumerate() {
        let subject = &handles[&(fact.subject_type.clone(), fact.subject_name.clone())];
        let object = &handles[&(fact.object_type.clone(), fact.object_name.clone())];
        lines.push(format!(
            "  PROPOSITION ?f{idx} {{ ({subject}, {}, {object}) }}",
            serde_json::to_string(&fact.predicate).unwrap_or_default()
        ));
        lines.push(format!(
            "  WITH METADATA {}",
            kip_object(&[
                ("confidence", json!(fact.confidence)),
                ("citation", json!(fact.citation)),
                ("checksum", json!(fact.checksum)),
                // A fact can vanish in one revision (marked superseded) and
                // return in a later one; nexus metadata merges are shallow
                // and keep stale keys, so the re-assertion must explicitly
                // clear the tombstone.
                ("status", json!("active")),
                ("superseded_by", Json::Null),
            ])
        ));
    }
    lines.push("}".to_string());
    lines.push(format!(
        "WITH METADATA {}",
        kip_object(&[
            ("source", json!("wiki")),
            ("author", json!("$self")),
            ("extractor", json!(extractor)),
            ("doc_id", json!(doc._id)),
            ("version_id", json!(version._id)),
            (
                "citation",
                json!(citation_uri(
                    space_id,
                    doc._id,
                    version._id,
                    0,
                    version.size
                )),
            ),
            ("confidence", json!(0.7)),
        ])
    ));
    lines.join("\n")
}

/// One capsule per stale fact: marks the proposition superseded without
/// touching its other metadata (shallow merge).
fn render_supersede_kml(fact: &DigestedFact, superseded_by: &str) -> String {
    format!(
        "UPSERT {{\n  PROPOSITION ?p {{ ({}, {}, {}) }}\n  WITH METADATA {}\n}}\nWITH METADATA {}",
        concept_literal(&fact.subject_type, &fact.subject_name),
        serde_json::to_string(&fact.predicate).unwrap_or_default(),
        concept_literal(&fact.object_type, &fact.object_name),
        kip_object(&[
            ("status", json!("superseded")),
            ("superseded_by", json!(superseded_by)),
        ]),
        kip_object(&[("source", json!("wiki")), ("author", json!("$self"))]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(s: (&str, &str), p: &str, o: (&str, &str)) -> DigestedFact {
        DigestedFact {
            subject_type: s.0.to_string(),
            subject_name: s.1.to_string(),
            predicate: p.to_string(),
            object_type: o.0.to_string(),
            object_name: o.1.to_string(),
            confidence: 0.9,
            citation: "wiki://sp/1@2#0-10".to_string(),
            checksum: "sha3-256:x".to_string(),
        }
    }

    #[test]
    fn parse_extraction_tolerates_fences_and_prose() {
        let strict = r#"{"facts": [{"subject": {"type": "A", "name": "a"}, "predicate": "p", "object": {"type": "B", "name": "b"}}]}"#;
        assert_eq!(parse_extraction(strict).unwrap().facts.len(), 1);

        let fenced = format!("Here you go:\n```json\n{strict}\n```\nDone.");
        assert_eq!(parse_extraction(&fenced).unwrap().facts.len(), 1);

        assert!(parse_extraction("no json here").is_err());
    }

    #[test]
    fn clean_ident_rejects_reserved_and_oversized() {
        assert_eq!(clean_ident(" Person "), Some("Person".to_string()));
        assert!(clean_ident("$ConceptType").is_none());
        assert!(clean_ident("_hidden").is_none());
        assert!(clean_ident("").is_none());
        assert!(clean_ident(&"x".repeat(200)).is_none());
    }

    #[test]
    fn render_digest_kml_registers_schema_and_attaches_provenance() {
        let doc = WikiDocRecord {
            _id: 3,
            namespace: "kb".to_string(),
            slug: "policy".to_string(),
            title: "安全政策".to_string(),
            status: super::super::DOC_STATUS_ACTIVE.to_string(),
            current_version: 7,
            current_checksum: "sha3-256:doc".to_string(),
            tags: vec![],
            acl_label: String::new(),
            source_uri: None,
            metadata: BTreeMap::new(),
            created_by: "a".to_string(),
            updated_by: "a".to_string(),
            created_at: 0,
            updated_at: 0,
        };
        let version = WikiVersionRecord {
            _id: 7,
            doc_id: 3,
            parent_version: None,
            checksum: "sha3-256:v".to_string(),
            content: "c".to_string(),
            size: 1,
            author: "a".to_string(),
            message: None,
            created_at: 0,
        };
        let facts = vec![fact(
            ("Organization", "Acme \"quoted\""),
            "publishes",
            ("Policy", "安全政策"),
        )];
        let kml = render_digest_kml(
            "sp",
            &doc,
            &version,
            &Extraction::default(),
            &facts,
            "wiki_digest@v1/test-model",
        );

        assert!(kml.contains(r#"{type: "$ConceptType", name: "Organization"}"#));
        assert!(kml.contains(r#"{type: "$ConceptType", name: "Policy"}"#));
        assert!(kml.contains(r#"{type: "$PropositionType", name: "publishes"}"#));
        assert!(kml.contains(r#"{type: "Organization", name: "Acme \"quoted\""}"#));
        assert!(kml.contains(r#"(?c0, "publishes", ?c1)"#));
        assert!(kml.contains(r#"citation: "wiki://sp/1@2#0-10""#));
        assert!(kml.contains(r#"extractor: "wiki_digest@v1/test-model""#));
        assert!(kml.contains(r#"source: "wiki""#));
        // A re-asserted fact must clear a stale superseded tombstone.
        assert!(kml.contains(r#"status: "active""#));
        assert!(kml.contains("superseded_by: null"));
        // Renderer output must parse as valid KML.
        assert!(parse_kml(&kml).is_ok());

        let supersede = render_supersede_kml(&facts[0], "wiki://sp/1@9#0-20");
        assert!(supersede.contains(r#"status: "superseded""#));
        assert!(supersede.contains(r#"superseded_by: "wiki://sp/1@9#0-20""#));
        assert!(parse_kml(&supersede).is_ok());
    }

    #[test]
    fn normalize_facts_resolves_anchors_and_dedupes() {
        let doc = WikiDocRecord {
            _id: 1,
            namespace: "kb".to_string(),
            slug: "d".to_string(),
            title: "t".to_string(),
            status: super::super::DOC_STATUS_ACTIVE.to_string(),
            current_version: 2,
            current_checksum: String::new(),
            tags: vec![],
            acl_label: String::new(),
            source_uri: None,
            metadata: BTreeMap::new(),
            created_by: String::new(),
            updated_by: String::new(),
            created_at: 0,
            updated_at: 0,
        };
        let version = WikiVersionRecord {
            _id: 2,
            doc_id: 1,
            parent_version: None,
            checksum: "sha3-256:v".to_string(),
            content: "0123456789".to_string(),
            size: 10,
            author: String::new(),
            message: None,
            created_at: 0,
        };
        let chunk = WikiChunkRecord {
            _id: 5,
            doc_id: 1,
            version_id: 2,
            namespace: "kb".to_string(),
            current: 1,
            title: "t".to_string(),
            heading_path: vec![],
            anchor: "sec-0".to_string(),
            ordinal: 0,
            text: "01234".to_string(),
            byte_start: 0,
            byte_end: 5,
            checksum: "sha3-256:chunk".to_string(),
            chunker_version: 1,
            acl_label: String::new(),
        };
        let extraction = Extraction {
            concepts: vec![],
            facts: vec![
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: "a".into(),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: Some(2.0),
                    anchor: Some("sec-0".into()),
                },
                // Duplicate triple: dropped.
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: "a".into(),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: None,
                },
                // Unknown anchor: cites the whole version.
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: "a".into(),
                    },
                    predicate: "q".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: Some("missing".into()),
                },
                // Reserved type: dropped.
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "$Evil".into(),
                        name: "x".into(),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: None,
                },
            ],
        };

        let (facts, alive) = normalize_facts("sp", &doc, &version, &[chunk], &extraction);
        assert_eq!(facts.len(), 2);
        assert_eq!(alive.len(), 2);
        assert_eq!(facts[0].confidence, 1.0);
        assert_eq!(facts[0].citation, "wiki://sp/1@2#0-5");
        assert_eq!(facts[0].checksum, "sha3-256:chunk");
        assert_eq!(facts[1].citation, "wiki://sp/1@2#0-10");
    }

    #[test]
    fn alive_set_is_not_capped_by_fact_truncation() {
        let doc = WikiDocRecord {
            _id: 1,
            namespace: "kb".to_string(),
            slug: "d".to_string(),
            title: "t".to_string(),
            status: super::super::DOC_STATUS_ACTIVE.to_string(),
            current_version: 2,
            current_checksum: String::new(),
            tags: vec![],
            acl_label: String::new(),
            source_uri: None,
            metadata: BTreeMap::new(),
            created_by: String::new(),
            updated_by: String::new(),
            created_at: 0,
            updated_at: 0,
        };
        let version = WikiVersionRecord {
            _id: 2,
            doc_id: 1,
            parent_version: None,
            checksum: "sha3-256:v".to_string(),
            content: "x".to_string(),
            size: 1,
            author: String::new(),
            message: None,
            created_at: 0,
        };
        let extraction = Extraction {
            concepts: vec![],
            facts: (0..MAX_FACTS_PER_VERSION + 10)
                .map(|i| ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: format!("a{i}"),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: None,
                })
                .collect(),
        };
        let (facts, alive) = normalize_facts("sp", &doc, &version, &[], &extraction);
        // Persisted facts are capped, but the alive set keeps every valid
        // triple so superseding never treats truncated facts as stale.
        assert_eq!(facts.len(), MAX_FACTS_PER_VERSION);
        assert_eq!(alive.len(), MAX_FACTS_PER_VERSION + 10);
        let truncated = &extraction.facts[MAX_FACTS_PER_VERSION + 5];
        let key = (
            "A".to_string(),
            truncated.subject.name.clone(),
            "p".to_string(),
            "B".to_string(),
            "b".to_string(),
        );
        assert!(alive.contains(&key));
    }

    #[test]
    fn kql_has_rows_detects_emptiness() {
        assert!(!kql_has_rows(&json!({"?link": []})));
        assert!(!kql_has_rows(&json!(null)));
        assert!(kql_has_rows(&json!({"?link": [{"id": "P1"}]})));
        assert!(kql_has_rows(&json!([{"id": "P1"}])));
    }

    use super::super::EVENT_CITATION_VERIFY_FAILED;
    use super::super::tests::{commit_input, test_wiki};

    #[tokio::test]
    async fn digest_chunks_skips_versions_raced_by_commits() {
        let wiki = test_wiki("wiki_digest_race").await;
        let v1 = wiki
            .commit(
                "a".to_string(),
                commit_input("竞态", "# 竞态\n\n第一版内容。\n"),
                1000,
            )
            .await
            .unwrap();
        let v1_record = wiki
            .versions
            .get_as::<WikiVersionRecord>(v1.version.id)
            .await
            .unwrap();

        // A concurrent commit lands v2 and removes v1's chunk set — exactly
        // what a digest can observe between its doc check and chunk read.
        let mut update = commit_input("竞态", "# 竞态\n\n第二版内容。\n");
        update.doc_id = Some(v1.doc.id);
        update.parent_version = Some(v1.version.id);
        let v2 = wiki.commit("a".to_string(), update, 2000).await.unwrap();

        // v1 resolves to "raced away" (None), never to an empty chunk list
        // that would supersede the previous digest's facts wholesale.
        assert!(digest_chunks(&wiki, &v1_record).await.unwrap().is_none());
        let v2_record = wiki
            .versions
            .get_as::<WikiVersionRecord>(v2.version.id)
            .await
            .unwrap();
        let chunks = digest_chunks(&wiki, &v2_record).await.unwrap().unwrap();
        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn restore_queues_doc_for_digest_catchup() {
        let wiki = test_wiki("wiki_digest_restore_pending").await;
        let out = wiki
            .commit(
                "a".to_string(),
                commit_input("归档文档", "# 归档文档\n\n内容。\n"),
                1000,
            )
            .await
            .unwrap();
        wiki.archive("a".to_string(), out.doc.id, 2000)
            .await
            .unwrap();
        assert!(
            wiki.docs
                .get_extension_as::<BTreeSet<u64>>(DIGEST_PENDING_KEY)
                .unwrap_or_default()
                .is_empty()
        );
        wiki.restore("a".to_string(), out.doc.id, 3000)
            .await
            .unwrap();
        let pending = wiki
            .docs
            .get_extension_as::<BTreeSet<u64>>(DIGEST_PENDING_KEY)
            .unwrap_or_default();
        assert!(pending.contains(&out.doc.id));

        // The ledger check driving the catch-up: false until a
        // DigestExtracted event covers the document's current version.
        assert!(
            !version_digested(&wiki, out.doc.id, out.doc.current_version)
                .await
                .unwrap()
        );
        wiki.write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(out.doc.id),
            Some(out.doc.current_version),
            "wiki_digest".to_string(),
            BTreeMap::new(),
            4000,
        )
        .await
        .unwrap();
        assert!(
            version_digested(&wiki, out.doc.id, out.doc.current_version)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn verify_recent_checks_ledger_citations_with_grouped_loads() {
        let wiki = test_wiki("wiki_digest_verify_recent").await;
        let out = wiki
            .commit(
                "a".to_string(),
                commit_input("样本", "# 样本\n\n引用样本内容。\n"),
                1000,
            )
            .await
            .unwrap();
        let version = wiki
            .versions
            .get_as::<WikiVersionRecord>(out.version.id)
            .await
            .unwrap();
        let ok = fact(("A", "a"), "p", ("B", "b"));
        let ok = DigestedFact {
            citation: citation_uri("test_space", out.doc.id, out.version.id, 0, version.size),
            checksum: chunk_checksum(
                &version.checksum,
                0,
                version.size as usize,
                &version.content,
            ),
            ..ok
        };
        let mut stale = ok.clone();
        stale.predicate = "q".into();
        stale.checksum = "sha3-256:wrong".to_string();
        wiki.write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(out.doc.id),
            Some(out.version.id),
            "wiki_digest".to_string(),
            BTreeMap::from([(
                "facts".to_string(),
                serde_json::to_value(vec![ok, stale]).unwrap(),
            )]),
            2000,
        )
        .await
        .unwrap();

        let (checked, invalid) = verify_recent_citations(&wiki, 3000).await.unwrap();
        assert_eq!((checked, invalid), (2, 1));
        // The mismatched recorded checksum sits over intact content: a
        // reference error, not corruption — no audit event.
        let events = wiki
            .list_events(
                Some(EVENT_CITATION_VERIFY_FAILED.to_string()),
                None,
                None,
                Some(10),
            )
            .await
            .unwrap();
        assert!(events.events.is_empty());
    }
}
