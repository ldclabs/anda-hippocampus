//! Memory usage ledger (memory evolution plan, module M1).
//!
//! Off-graph counters of how memories are actually used: which graph
//! entities each completed recall surfaced, and which were later corrected
//! (superseded). The ledger — not the graph — absorbs the high-frequency
//! writes; maintenance settles it into graph metadata once per cycle
//! (module M2), so a recall never mutates the graph it reads.

use anda_db::{
    collection::{Collection, CollectionConfig},
    database::AndaDB,
    error::DBError,
    query::{Filter, Fv, Query, RangeQuery},
    schema::AndaDBSchema,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Usage counters for one graph entity.
#[derive(Debug, Clone, Default, Serialize, Deserialize, AndaDBSchema)]
pub struct MemoryUsage {
    pub _id: u64,

    /// Graph entity id: `"C:<id>"` for concepts, `"P:<id>:<predicate>"`
    /// for propositions.
    pub entity: String,

    /// Completed production recalls that surfaced this entity.
    pub recall_count: u64,

    /// Recalls issued by the maintenance self-test (plan module M7). Tracked
    /// separately and never merged into `recall_count`: the brain testing
    /// itself must not count as usage (plan guardrail 1).
    pub self_test_count: u64,

    /// Unix ms of the newest production recall that surfaced this entity.
    pub last_recalled_at: u64,

    /// 1 once the entity has been observed superseded/corrected.
    /// `record_correction` deliberately deduplicates — repeat observations of
    /// the same supersede event must not compound the correction penalty —
    /// so despite the name this is a flag, kept as a count for schema
    /// compatibility.
    pub correction_count: u64,

    /// Unix ms when the newest correction was observed.
    pub last_corrected_at: u64,

    /// `recall_count` value already settled onto graph metadata; settlement
    /// only flushes rows where `recall_count > flushed_recall_count`.
    pub flushed_recall_count: u64,

    /// 1 while the row carries recall counts not yet settled onto graph
    /// metadata (schema v2). Settlement scans this flag instead of a
    /// time-window watermark, so a row whose KIP flush failed — or that
    /// arrived past a batch limit — is retried forever instead of silently
    /// falling out of the scan window. (u64 because AndaDB BTree indexes do
    /// not support Bool.)
    pub dirty: u64,

    pub updated_at: u64,
}

/// Wrapper around the `memory_usage` collection. All mutations serialize on
/// an internal lock so concurrent recall writebacks cannot race one entity
/// into duplicate rows.
pub struct UsageLedger {
    collection: Arc<Collection>,
    write_lock: tokio::sync::Mutex<()>,
}

impl UsageLedger {
    pub async fn connect(db: &Arc<AndaDB>) -> Result<Self, DBError> {
        // v2 adds the `dirty` flush flag.
        let mut schema = MemoryUsage::schema()?;
        schema.with_version(2);
        let collection = db
            .open_or_create_collection(
                schema,
                CollectionConfig {
                    name: "memory_usage".to_string(),
                    description: "Memory usage ledger (recall/correction counters)".to_string(),
                },
                async |collection| {
                    collection.create_btree_index_nx(&["entity"]).await?;
                    collection.create_btree_index_nx(&["dirty"]).await?;
                    collection
                        .create_btree_index_nx(&["last_recalled_at"])
                        .await?;
                    collection
                        .create_btree_index_nx(&["last_corrected_at"])
                        .await?;
                    Ok(())
                },
            )
            .await?;
        Ok(Self {
            collection,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn get(&self, entity: &str) -> Result<Option<MemoryUsage>, DBError> {
        let rows: Vec<MemoryUsage> = self
            .collection
            .search_as(Query {
                search: None,
                filter: Some(Filter::Field((
                    "entity".to_string(),
                    RangeQuery::Eq(Fv::Text(entity.to_string())),
                ))),
                limit: Some(1),
            })
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Records one completed recall having surfaced `entities`. Returns the
    /// number of ledger rows touched.
    pub async fn record_recall(
        &self,
        entities: &BTreeSet<String>,
        now_ms: u64,
    ) -> Result<u64, DBError> {
        let _guard = self.write_lock.lock().await;
        let mut touched = 0u64;
        for entity in entities {
            match self.get(entity).await? {
                Some(row) => {
                    self.collection
                        .update(
                            row._id,
                            BTreeMap::from([
                                ("recall_count".to_string(), Fv::U64(row.recall_count + 1)),
                                ("last_recalled_at".to_string(), Fv::U64(now_ms)),
                                ("dirty".to_string(), Fv::U64(1)),
                                ("updated_at".to_string(), Fv::U64(now_ms)),
                            ]),
                        )
                        .await?;
                }
                None => {
                    self.collection
                        .add_from(&MemoryUsage {
                            entity: entity.clone(),
                            recall_count: 1,
                            last_recalled_at: now_ms,
                            dirty: 1,
                            updated_at: now_ms,
                            ..Default::default()
                        })
                        .await?;
                }
            }
            touched += 1;
        }
        Ok(touched)
    }

    /// Records a correction (superseded memory) observation. Returns `true`
    /// when this is the first correction seen for the entity, so settlement
    /// can diff "newly corrected since last cycle" out of a full scan.
    pub async fn record_correction(&self, entity: &str, now_ms: u64) -> Result<bool, DBError> {
        let _guard = self.write_lock.lock().await;
        match self.get(entity).await? {
            Some(row) => {
                if row.correction_count > 0 {
                    return Ok(false);
                }
                self.collection
                    .update(
                        row._id,
                        BTreeMap::from([
                            ("correction_count".to_string(), Fv::U64(1)),
                            ("last_corrected_at".to_string(), Fv::U64(now_ms)),
                            ("updated_at".to_string(), Fv::U64(now_ms)),
                        ]),
                    )
                    .await?;
                Ok(true)
            }
            None => {
                self.collection
                    .add_from(&MemoryUsage {
                        entity: entity.to_string(),
                        correction_count: 1,
                        last_corrected_at: now_ms,
                        updated_at: now_ms,
                        ..Default::default()
                    })
                    .await?;
                Ok(true)
            }
        }
    }

    /// Rows carrying recall counts not yet settled onto graph metadata.
    /// Scans the `dirty` flag — not a time window — so a row whose flush
    /// failed, or that arrived past a batch limit, keeps showing up until it
    /// actually settles.
    ///
    /// Pages by `_id > after_id` so a prefix of persistently-failing rows
    /// cannot occupy every batch window and starve the dirty rows behind
    /// them. The id-set intersection is index-only; ids are sorted here
    /// because `Filter::And` yields an unordered set, and only the page's
    /// documents are fetched. Returns the page plus `Some(last_scanned_id)`
    /// when more dirty rows remain past it (`None` = scan exhausted).
    pub async fn unflushed_recalls(
        &self,
        after_id: u64,
        limit: usize,
    ) -> Result<(Vec<MemoryUsage>, Option<u64>), DBError> {
        let mut ids = self
            .collection
            .query_ids(
                Filter::And(vec![
                    Box::new(Filter::Field((
                        "dirty".to_string(),
                        RangeQuery::Eq(Fv::U64(1)),
                    ))),
                    Box::new(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(after_id)),
                    ))),
                ]),
                None,
            )
            .await?;
        ids.sort_unstable();
        let next_cursor = if ids.len() > limit {
            ids.get(limit.saturating_sub(1)).copied()
        } else {
            None
        };
        ids.truncate(limit);

        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            match self.collection.get_as::<MemoryUsage>(id).await {
                Ok(row) if row.recall_count > row.flushed_recall_count => rows.push(row),
                // Dirty without a pending delta, or removed while paging
                // (forget cascade): nothing to settle.
                Ok(_) | Err(_) => {}
            }
        }
        Ok((rows, next_cursor))
    }

    /// Marks a row's recall counter as settled onto graph metadata. Re-reads
    /// the row under the write lock: a recall recorded between the
    /// settlement's scan and this call keeps the row dirty, so its delta is
    /// picked up by the next settlement instead of being lost.
    pub async fn mark_flushed(
        &self,
        id: u64,
        recall_count: u64,
        now_ms: u64,
    ) -> Result<(), DBError> {
        let _guard = self.write_lock.lock().await;
        let current = match self.collection.get_as::<MemoryUsage>(id).await {
            Ok(row) => row.recall_count,
            // Row already removed (forget cascade racing the settlement
            // scan): there is nothing left to settle for this entity, and
            // erroring here would abort the whole reinforcement pass.
            Err(_) => return Ok(()),
        };
        let dirty = if current > recall_count { 1 } else { 0 };
        self.collection
            .update(
                id,
                BTreeMap::from([
                    ("flushed_recall_count".to_string(), Fv::U64(recall_count)),
                    ("dirty".to_string(), Fv::U64(dirty)),
                    ("updated_at".to_string(), Fv::U64(now_ms)),
                ]),
            )
            .await?;
        Ok(())
    }

    /// Records self-test retrievals (plan M7). Deliberately touches only
    /// `self_test_count` — never `recall_count`/`last_recalled_at` — so the
    /// brain testing itself can never reinforce its own memories
    /// (plan guardrail 1).
    pub async fn record_self_test(
        &self,
        entities: &BTreeSet<String>,
        now_ms: u64,
    ) -> Result<(), DBError> {
        let _guard = self.write_lock.lock().await;
        for entity in entities {
            match self.get(entity).await? {
                Some(row) => {
                    self.collection
                        .update(
                            row._id,
                            BTreeMap::from([
                                (
                                    "self_test_count".to_string(),
                                    Fv::U64(row.self_test_count + 1),
                                ),
                                ("updated_at".to_string(), Fv::U64(now_ms)),
                            ]),
                        )
                        .await?;
                }
                None => {
                    self.collection
                        .add_from(&MemoryUsage {
                            entity: entity.clone(),
                            self_test_count: 1,
                            updated_at: now_ms,
                            ..Default::default()
                        })
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Rows corrected after `since_ms`, newest signals for scenario mining
    /// (plan M9).
    pub async fn corrected_since(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<MemoryUsage>, DBError> {
        let rows: Vec<MemoryUsage> = self
            .collection
            .search_as(Query {
                search: None,
                filter: Some(Filter::Field((
                    "last_corrected_at".to_string(),
                    RangeQuery::Gt(Fv::U64(since_ms)),
                ))),
                limit: Some(limit),
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter(|row| row.correction_count > 0)
            .collect())
    }

    /// Removes an entity's ledger row entirely (plan M6 forget cascade).
    pub async fn forget_entity(&self, entity: &str) -> Result<bool, DBError> {
        let _guard = self.write_lock.lock().await;
        match self.get(entity).await? {
            Some(row) => {
                self.collection.remove(row._id).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// One negative-knowledge entry: a query the graph provably had nothing for.
#[derive(Debug, Clone, Default, Serialize, Deserialize, AndaDBSchema)]
pub struct RecallMiss {
    pub _id: u64,
    pub query: String,
    pub created_at: u64,
}

/// Negative-knowledge cache (plan M5): remembers probe queries that found
/// nothing, so agents stop paying to hit the same wall. Invalidation is
/// deliberately coarse — any completed formation clears the whole cache
/// (new memory could answer any past miss) — with a TTL as backstop.
///
/// Keys are normalized (whitespace-folded, lowercased): the cache exists to
/// absorb repeats, and users re-ask the same question with different casing
/// and spacing.
pub struct MissCache {
    collection: Arc<Collection>,
    write_lock: tokio::sync::Mutex<()>,
}

/// Backstop TTL for negative-knowledge entries.
pub const RECALL_MISS_TTL_MS: u64 = 3_600_000;

/// Hard row cap: unauthenticated probes on public spaces must not be able to
/// grow this collection without bound. At the cap, expired rows are purged;
/// if the cache is still full the new miss simply is not cached (a full
/// cache only costs performance, never correctness).
const MISS_CACHE_MAX_ROWS: usize = 1024;

/// Queries longer than this are never cached (they are unlikely to repeat
/// verbatim, and unbounded query text is a disk-write amplifier).
const MISS_QUERY_MAX_CHARS: usize = 512;

impl MissCache {
    pub async fn connect(db: &Arc<AndaDB>) -> Result<Self, DBError> {
        let collection = db
            .open_or_create_collection(
                RecallMiss::schema()?,
                CollectionConfig {
                    name: "recall_misses".to_string(),
                    description: "Negative-knowledge cache (queries with no memory)".to_string(),
                },
                async |collection| {
                    collection.create_btree_index_nx(&["query"]).await?;
                    Ok(())
                },
            )
            .await?;
        Ok(Self {
            collection,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Cache key: whitespace-folded, lowercased query text.
    fn cache_key(query: &str) -> String {
        query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    async fn get(&self, key: &str) -> Result<Option<RecallMiss>, DBError> {
        let rows: Vec<RecallMiss> = self
            .collection
            .search_as(Query {
                search: None,
                filter: Some(Filter::Field((
                    "query".to_string(),
                    RangeQuery::Eq(Fv::Text(key.to_string())),
                ))),
                limit: Some(1),
            })
            .await?;
        Ok(rows.into_iter().next())
    }

    /// True when a fresh (unexpired) miss is cached for this query. Expired
    /// rows are pruned lazily.
    pub async fn is_fresh_miss(&self, query: &str, now_ms: u64) -> Result<bool, DBError> {
        match self.get(&Self::cache_key(query)).await? {
            Some(row) if now_ms.saturating_sub(row.created_at) <= RECALL_MISS_TTL_MS => Ok(true),
            Some(row) => {
                let _guard = self.write_lock.lock().await;
                let _ = self.collection.remove(row._id).await;
                Ok(false)
            }
            None => Ok(false),
        }
    }

    /// Records a miss observed at `now_ms`. A cache clear racing an
    /// in-flight probe may re-cache a query that just-formed memory can now
    /// answer; that staleness only affects the probe channel and self-heals
    /// on the next formation clear or the TTL.
    pub async fn record_miss(&self, query: &str, now_ms: u64) -> Result<(), DBError> {
        let key = Self::cache_key(query);
        if key.is_empty() || key.chars().count() > MISS_QUERY_MAX_CHARS {
            return Ok(());
        }
        let _guard = self.write_lock.lock().await;
        match self.get(&key).await? {
            Some(row) => {
                self.collection
                    .update(
                        row._id,
                        BTreeMap::from([("created_at".to_string(), Fv::U64(now_ms))]),
                    )
                    .await?;
            }
            None => {
                if self.collection.len() >= MISS_CACHE_MAX_ROWS {
                    self.purge_expired_locked(now_ms).await?;
                    if self.collection.len() >= MISS_CACHE_MAX_ROWS {
                        return Ok(());
                    }
                }
                self.collection
                    .add_from(&RecallMiss {
                        query: key,
                        created_at: now_ms,
                        ..Default::default()
                    })
                    .await?;
            }
        }
        Ok(())
    }

    /// Removes every expired row. Caller must hold `write_lock`.
    async fn purge_expired_locked(&self, now_ms: u64) -> Result<(), DBError> {
        let mut cursor = 0u64;
        loop {
            let rows: Vec<RecallMiss> = self
                .collection
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(cursor)),
                    ))),
                    limit: Some(100),
                })
                .await?;
            let Some(max_id) = rows.iter().map(|row| row._id).max() else {
                break;
            };
            cursor = cursor.max(max_id);
            for row in rows {
                if now_ms.saturating_sub(row.created_at) > RECALL_MISS_TTL_MS {
                    let _ = self.collection.remove(row._id).await;
                }
            }
        }
        Ok(())
    }

    /// Drops every cached miss. Called when formation completes: any new
    /// memory could answer any past miss, and clearing is cheaper than being
    /// wrong.
    pub async fn clear(&self) -> Result<u64, DBError> {
        let _guard = self.write_lock.lock().await;
        let mut cleared = 0u64;
        let mut cursor = 0u64;
        loop {
            // Filterless queries return nothing in AndaDB; scan by `_id`.
            // The cursor advances past rows whose remove failed (same shape
            // as purge_expired_locked), so a persistent storage error cannot
            // spin this loop forever — clear() runs inline in the
            // conversation-end hook. Skipped rows expire via the TTL purge.
            let rows: Vec<RecallMiss> = self
                .collection
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(cursor)),
                    ))),
                    limit: Some(100),
                })
                .await?;
            let Some(max_id) = rows.iter().map(|row| row._id).max() else {
                break;
            };
            cursor = cursor.max(max_id);
            for row in rows {
                if self.collection.remove(row._id).await.is_ok() {
                    cleared += 1;
                }
            }
        }
        Ok(cleared)
    }
}
