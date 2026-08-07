//! Retrieval-quality baseline (PRD §12 M2).
//!
//! A fixture-driven hit-rate harness over BM25 search. The built-in fixture
//! seeds a bilingual enterprise corpus through the OKF importer and measures
//! top-k document hit rates; the report is the measurement basis for the
//! §6 semantic-search restart condition (revisit embeddings only if hit@8
//! drops below 0.85 and query reformulation cannot compensate). Grow the
//! fixture with real query logs as they appear.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{
    EVAL_NAMESPACE, WikiBundleEntry, WikiError, WikiImportInput, WikiSearchInput, WikiService,
};

const EVAL_TOP_K: usize = 8;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetrievalCase {
    pub query: String,
    /// Slug of the document that should be retrieved.
    pub expect_slug: String,
    /// When set, the matching hit's text must also contain this substring
    /// (section-level correctness).
    #[serde(default)]
    pub expect_text: Option<String>,
    /// Paraphrase cases with little lexical overlap: expected headroom for
    /// BM25, kept in the metrics to keep the baseline honest.
    #[serde(default)]
    pub hard: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetrievalFixture {
    pub docs: Vec<WikiBundleEntry>,
    pub cases: Vec<RetrievalCase>,
}

/// The fixture shipped with the repo (`anda_brain/evals/wiki/retrieval.json`).
/// It lives in a subdirectory because the top-level `evals/*.json` files are
/// memory-eval-harness fixtures, globbed and validated as such by CI.
pub fn builtin_fixture() -> RetrievalFixture {
    serde_json::from_str(include_str!("../../evals/wiki/retrieval.json"))
        .expect("builtin wiki retrieval fixture must parse")
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrievalMiss {
    pub query: String,
    pub expect_slug: String,
    pub got_slugs: Vec<String>,
    pub hard: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RetrievalReport {
    pub total: usize,
    pub hits_at_1: usize,
    pub hits_at_k: usize,
    pub top_k: usize,
    pub misses: Vec<RetrievalMiss>,
}

impl RetrievalReport {
    pub fn hit_rate_at_k(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.hits_at_k as f64 / self.total as f64
    }

    pub fn hit_rate_at_1(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.hits_at_1 as f64 / self.total as f64
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "wiki retrieval baseline: {}/{} hit@{} ({:.1}%), {}/{} hit@1 ({:.1}%)\n",
            self.hits_at_k,
            self.total,
            self.top_k,
            self.hit_rate_at_k() * 100.0,
            self.hits_at_1,
            self.total,
            self.hit_rate_at_1() * 100.0,
        );
        for miss in &self.misses {
            out.push_str(&format!(
                "  miss{}: {:?} expected {} got [{}]\n",
                if miss.hard { " (hard)" } else { "" },
                miss.query,
                miss.expect_slug,
                miss.got_slugs.join(", "),
            ));
        }
        out
    }
}

/// Imports the fixture corpus into [`EVAL_NAMESPACE`]. Checksum-idempotent:
/// re-running never grows version chains.
pub async fn import_fixture(
    wiki: &WikiService,
    fixture: &RetrievalFixture,
    actor: &str,
    now_ms: u64,
) -> Result<super::WikiImportOutput, WikiError> {
    wiki.import_bundle(
        actor.to_string(),
        WikiImportInput {
            entries: fixture.docs.clone(),
            namespace: Some(EVAL_NAMESPACE.to_string()),
        },
        now_ms,
    )
    .await
}

/// Runs every case as a plain BM25 search restricted to the eval namespace
/// and scores document-level hits (plus optional section substrings).
pub async fn run_retrieval_eval(
    wiki: &WikiService,
    fixture: &RetrievalFixture,
) -> Result<RetrievalReport, WikiError> {
    let mut report = RetrievalReport {
        total: fixture.cases.len(),
        top_k: EVAL_TOP_K,
        ..Default::default()
    };
    let mut slug_cache: BTreeMap<u64, String> = BTreeMap::new();

    for case in &fixture.cases {
        let output = wiki
            .search(WikiSearchInput {
                query: case.query.clone(),
                namespaces: vec![EVAL_NAMESPACE.to_string()],
                top_k: Some(EVAL_TOP_K),
                ..Default::default()
            })
            .await?;

        let mut got_slugs = Vec::with_capacity(output.hits.len());
        let mut matched_rank: Option<usize> = None;
        for (rank, hit) in output.hits.iter().enumerate() {
            let slug = match slug_cache.get(&hit.citation.doc_id) {
                Some(slug) => slug.clone(),
                None => {
                    let slug = wiki.get_doc(hit.citation.doc_id).await?.slug;
                    slug_cache.insert(hit.citation.doc_id, slug.clone());
                    slug
                }
            };
            let text_ok = case
                .expect_text
                .as_ref()
                .is_none_or(|expect| hit.text.contains(expect));
            if matched_rank.is_none() && slug == case.expect_slug && text_ok {
                matched_rank = Some(rank);
            }
            got_slugs.push(slug);
        }

        match matched_rank {
            Some(0) => {
                report.hits_at_1 += 1;
                report.hits_at_k += 1;
            }
            Some(_) => report.hits_at_k += 1,
            None => report.misses.push(RetrievalMiss {
                query: case.query.clone(),
                expect_slug: case.expect_slug.clone(),
                got_slugs,
                hard: case.hard,
            }),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::WikiService;
    use anda_db::database::{AndaDB, DBConfig};
    use anda_db::storage::StorageConfig;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    #[tokio::test]
    async fn retrieval_baseline_meets_restart_threshold() {
        let db = Arc::new(
            AndaDB::create(
                Arc::new(InMemory::new()),
                DBConfig {
                    name: "wiki_evalset".to_string(),
                    description: "wiki retrieval eval".to_string(),
                    storage: StorageConfig::default(),
                    lock: None,
                },
            )
            .await
            .unwrap(),
        );
        let wiki = WikiService::connect("eval_space".to_string(), db)
            .await
            .unwrap();
        let fixture = builtin_fixture();

        let import = import_fixture(&wiki, &fixture, "eval", 1000).await.unwrap();
        assert_eq!(import.created, fixture.docs.len());
        assert!(import.skipped.is_empty());

        // Zero growth on re-import (the corpus doubles as the OKF
        // idempotency acceptance check).
        let again = import_fixture(&wiki, &fixture, "eval", 2000).await.unwrap();
        assert_eq!(again.unchanged, fixture.docs.len());

        let report = run_retrieval_eval(&wiki, &fixture).await.unwrap();
        println!("{}", report.render());
        assert_eq!(report.total, fixture.cases.len());
        // PRD §6 restart condition baseline: BM25 top-8 hit rate ≥ 85%.
        assert!(
            report.hit_rate_at_k() >= 0.85,
            "baseline regressed: {}",
            report.render()
        );
        // Every miss must be an expected-hard paraphrase case.
        assert!(
            report.misses.iter().all(|m| m.hard),
            "non-hard case missed: {}",
            report.render()
        );
    }
}
