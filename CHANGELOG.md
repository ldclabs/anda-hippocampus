# Changelog

All notable changes to the Anda Brain project.

## [Unreleased]

## [0.11.0] — 2026-08-07

### Changed
- **Release version advanced to `0.11.0`.** This release narrows the crate's public Rust API (breaking for library consumers); the HTTP and MCP wire contracts are unchanged except for the error-semantics fixes below.
- **Crate interface narrowed to what callers actually use.** The `wiki`, `ledger`, and `authz` modules are now crate-internal; `Space`'s handles (`db`, `formation`, `recall`, `memory`, `wiki`, `wiki_digest`) and 15 internal-only methods are no longer public; `payload` drops its unused RPC-request type and internal helpers; dead constants and the unwired retrieval-eval fixtures (now test-only) were removed or gated.
- **Space forking has a single entry point.** `AppState::fork_space` owns the whole fork protocol (object copy, state fork, load without background autostart), and `Space::close` is the public way to close throwaway spaces — the eval CLI no longer reaches into the DB handle.
- **Eval orchestration moved into the library (`eval::run`).** `EvalRunEnv`, run-scoped space hosting, suite and shared-formation runs, and the zero-score failure boundary now live in `eval/run.rs` with tests; `bin/main.rs` only parses flags and writes reports. The three hand-copied load→run→close sequences were unified behind one `with_eval_space` helper.
- **Shared test fixtures.** A `testkit` module replaces six verbatim copies of the test `AppState` wiring and space bootstrap across the unit-test suites.

### Fixed
- **Shared-formation eval forks no longer resume background work.** Profile forks previously loaded with background autostart, letting the inherited formation cursor and wiki-digest backlog burn model tokens and mutate forks mid-replay; forks now load through `fork_space` (autostart off), keeping A/B comparisons reproducible.
- **MCP errors mirror HTTP classification.** Caller-fixable failures (invalid input, guard rejections) now surface as JSON-RPC `invalid_params`/`invalid_request` instead of internal errors, and a wiki commit conflict carries the same `current_version`/`current_checksum` retry payload as the HTTP `409` body, so MCP agents can follow the documented re-read → merge → retry protocol.
- **`update_space_tier` reports an unknown space as `404`** (previously `400`), matching every other space endpoint.
- **Recall conversation snapshots no longer swallow serialization failures silently**; they are logged like the formation and maintenance paths.

## [0.10.2] — 2026-07-31

### Changed
- **Release version advanced to `0.10.2`.** `anda_brain` now reports `0.10.2`, and the lockfile was refreshed for the release.
- **AndaDB and Cognitive Nexus dependencies upgraded to `0.11`.** `anda_object_store`, `anda_db`, `anda_db_tfs`, `anda_cognitive_nexus`, and `anda_kip` now use the new release line.
- **MCP and HTTP dependencies upgraded.** The service now uses `rmcp` 3.0 and `tower-http` 0.7.

### Fixed
- **Database pagination now follows the AndaDB 0.11 query API.** Conversation and wiki listing, housekeeping, and ledger scans use ordered ID queries followed by targeted record reads, retaining bounded pages and stable cursors under the new API semantics.
- **Graph statistics remain compatible with Cognitive Nexus 0.11.** Space status and graph-counter fallbacks now use the public concept and proposition accessors.
- **Streamable MCP HTTP retains configured session behavior.** The migration maps the existing stateful-session setting to rmcp 3.0's legacy session mode, and KIP parameterized commands use the updated protocol representation.

## [0.10.1] — 2026-07-21

### Fixed
- **Wiki extraction now normalizes schema identifiers.** Extracted concept types are converted to `UpperCamelCase` and predicates to `snake_case` before KML is rendered, preventing malformed or duplicate KIP schema entries.
- **Orphan metrics now use a valid per-type census.** The graph-health sweep inventories registered concept types and totals their unassigned concepts instead of issuing an unsupported all-concepts query; failed legs return an unknown metric rather than a partial count.
- **KIP metric failures are visible.** Read-only count and orphan-census failures now emit warnings, and KIP string-literal escaping is centralized so search commands cannot drift.

### Changed
- **Release version advanced to `0.10.1`.** `anda_brain` now reports `0.10.1`, and the lockfile was refreshed for the release.

## [0.10.0] — 2026-07-16

### Changed
- **Release version advanced to `0.10.0`.** Upgraded `anda_db`, `anda_db_tfs`, `anda_object_store`, `anda_cognitive_nexus`, and `anda_kip` to `0.10`; migrated the MCP integration to `rmcp` 2.2 (`ContentBlock`); and refreshed the lockfile.

### Fixed (full-crate review, 8 P1 + 15 P2 + 30 P3 — see CODE_REVIEW.md)
- **Label ACL closed on the conversations channel.** `get/list_conversation` (HTTP and MCP) reject label-restricted tokens with 403 via a shared guard — recall conversations persist the unrestricted runner history, which bypassed the token's wiki ACL.
- **Token management hardened.** Minting a `*`-scope space token now requires a `*`-scope CWT; `list_space_tokens` redacts values to a display prefix; token `name` is required/unique and `revoke_space_token` accepts it as the revocation handle; mints are serialized against the count/uniqueness checks.
- **Read-only KIP errors are "unknown", never "absent".** `forget_memory` existence checks report an error entry instead of a clean `existed:false`; dream self-test skips (without stamping) candidates whose grounding search or concept lookup errored; eval assertion probes stop feeding errored searches to the judge as `satisfied:false`.
- **Formation/maintenance mutual exclusion has no settlement window.** `Space::maintenance` claims the processing slot *before* deterministic settlement and `MaintenanceAgent::run` inherits the claim, so a formation cycle can no longer start mid-settlement and write the graph concurrently.
- **Runner loops are bounded.** Formation/maintenance agent loops carry a 200-turn / 30-minute guardrail that fails the conversation into the existing retry path instead of holding the processing slot forever on a tool loop.
- **The optimizer no longer loses paid generations.** Proposal/parse failures record a rejected generation and continue; evaluation failures return (and `main` writes out) the partial report with all accepted genes; a prompt-genome guard mirrors the policy guard so unvalidated candidates cannot leak into the process-wide override. Rejections restore the run-start genome, not compiled defaults.
- **Eval harness degrades instead of aborting.** Transport-level recall/simulator/trace failures become findings; a failed scenario yields a zero-score placeholder report instead of discarding the suite; shape-mismatched judge output errors into the lexical fallback instead of scoring all-zero; blank rubric terms fail validation; empty-evidence probes short-circuit without a judge call.
- **HTTP layer: concurrency limits and honest errors.** Global tower load-shed cap (503) plus a stricter cap on LLM-billed routes (429); error bodies follow `Accept: application/cbor`; `/recall` honors `Accept: text/markdown`; a dead server task now cancels the process instead of leaving a zombie; nonexistent spaces map to 404 with internals kept out of response bodies; invalid `Shard-Id`/`X-Shard`, unknown `collection`, half-open ranges, and empty formation/recall inputs are 400s; `Bearer` parsing is case-insensitive and single-strip.
- **Wiki correctness and bounds.** `expand_hits` actually merges overlapping expanded hits (the bridge pass was dead code); `sweep_doc` reconciliation paginates past 1000 chunks; digest no longer marks a whole document superseded when racing a concurrent commit; atomic chunk units cap at 32 KiB; caller checksum mismatches are citation errors, not forged corruption events; restored documents re-enter the digest queue; OKF re-imports propagate tag deletion, imports are race-free under the write lock, exports are size-capped.
- **Assorted races and accounting.** `record_miss` checks the clear stamp under the write lock; `mark_flushed` tolerates rows removed by a forget cascade; `Space::update` performs fallible I/O before any in-memory mutation; per-run `MaintenanceParameters` validate against the policy bounds; the first `memory_status` census is single-flighted; failed recalls persist their real token usage; PII scrubbing preserves fractional-second timestamps; MCP auto-created spaces record the verified caller as owner; the MCP channel accepts JSON-string `context` and rejects unknown `wiki_search` modes.
- **Performance.** Context assembly, checkpoint samples/probes, shadow-eval forks/replays, wiki doc+TOC reads, and the schema census fan out concurrently (bounded); runner conversations persist every 5 turns instead of every turn (O(turns²) → O(turns)); wiki commit builds chunks outside the global write lock; `verify_recent` groups version loads; `prune_events` batch-derives digest ledger heads.

### Fixed (pre-launch review, memory evolution)
- **Usage counts can no longer be lost by settlement.** The reinforcement flush scans a `dirty` flag on ledger rows (schema v2) instead of a time-window watermark: rows whose KIP write fails, arrive past a batch limit, or are recorded concurrently with a settlement stay dirty and are retried by every later settlement. `mark_flushed` re-checks the row under the write lock so a recall racing the flush re-dirties it.
- **Correction discovery no longer starves behind its scan window.** Processed superseded links get a `correction_settled` graph marker and leave the result set — the marker is the cursor, so backlogs larger than one `LIMIT 500` batch drain across cycles instead of hiding all newer corrections forever.
- **Dream self-test coverage now slides across the whole graph.** Sampling excludes links already tested (`self_tested_at` stamp, 30-day retest horizon) or already recall-reinforced *in the query*, so every pass reaches new rows; previously the fixed lexicographic prefix was re-read until coverage stalled after ~4 cycles. Unresolvable candidates are stamped too. The self-test token budget now shrinks the candidate batch before the LLM call instead of warning after it.
- **Full-scan engine-cap failures are loud and partial, not silent and total.** A failing bulk-decay pass (e.g. KIP_4002 past the engine's 65,536-row full-scan cap) degrades with `log::error` and a `decay_error` report field while corrections and the schema census still run; the correction scan and self-test sampling log the same way. The ceiling and the pending predicate-sharding work are documented in README and the plan.
- **Decay can no longer push confidence through the floor.** The bulk-decay `CLAMP` lower bound is the policy `decay_floor` (was 0.0), so a 0.31-confidence link stops at 0.30 instead of landing permanently below the floor.
- **`forget` cascade is complete and cannot race maintenance.** Deleting a concept now enumerates its propositions first and removes their usage-ledger rows (their ids embed predicate names — usage traces of the forgotten memory); a successful forget clears the negative-knowledge cache (rows carry raw query text); and forget is rejected while a maintenance cycle runs, since the LLM's context could re-materialize the deleted entities.
- **Mined-scenario PII scrubbing covers every string field.** `scrub_scenario` round-trips the whole scenario JSON, reaching `required_answer_terms`/`forbidden_answer_terms`/`assertion`/`messages`/`scoring_rubric` — exactly the fields the miner LLM is told to put corrected facts in; a scenario that cannot be scrubbed is dropped. Emails are masked before digit runs, so `alice12345678@example.com` no longer half-leaks.
- **`MemoryPolicy` integer knobs are capped.** `self_test_queries_per_cycle` ≤ 100, token budget ≤ 1M, and bounds on the remaining integer fields: the policy is settable over HTTP, and an unbounded budget was a per-cycle cost bomb that only the optimizer path guarded against.
- **The negative-knowledge cache is bounded and fully invalidated.** Hard cap of 1024 rows with expired-row purge at the cap and a 512-char query limit (anonymous probes on public spaces can no longer grow it without bound); wiki-digest and maintenance graph writes now clear it like formation always did, so fresh memory stops being masked for up to an hour.
- **Shadow evaluation is isolated and serialized.** Forks open in no-autostart mode — they no longer resume the live space's formation backlog or wiki digest (double LLM spend, drifting A/B state); a per-space `shadow_lock` rejects concurrent runs (each holds two full in-memory space copies); the source space loads unpinned.
- **Dead knobs left the policy genome.** `recall_reinforcement`, `correction_penalty`, `recall_search_threshold`, and `recall_max_rounds` have no runtime consumers yet and were removed from `POLICY_PATCH_FIELDS` — mutating them measured pure sampling noise that the accept gate could bless as an "improvement". The optimizer also warns when running without variance data (`checkpoint_samples = 1`).

### Fixed (second review pass)
- **The recall footer cannot leak through side channels.** `split_recall_meta` strips *every* `<memory_meta>` occurrence (an echoed prompt example no longer reaches plain `/recall` clients) and keeps prose after an unclosed tag instead of discarding it (a truncated JSON footer is still dropped and salvaged); plain `/recall` also strips the footer from `chat_history` and `failed_reason`, which previously carried the raw model output.
- **Citations carry the provenance the plan promised.** `MemoryCitation` gains `source` and `created_at`, harvested deterministically from tool-output metadata like `confidence` already was.
- **Uncertainty calibration counts real traffic.** Plain `/recall` self-reports now feed `avg_uncertainty` (previously only `recall_structured` — a blind spot over most production traffic), and failed recalls no longer pollute the sample on either path.
- **`memory_status` no longer runs full scans per request.** Graph counters (orphans/unsorted/predicate types) are censused at settlement time into the `memory_graph_counters` extension (with `as_of`); the anonymous-reachable endpoint reads the cache. A failed per-predicate census count is now *omitted* from `schema_audit` instead of recorded as 0, which pointed the merge guidance at the busiest predicate.
- **Probe stops counting graph plumbing as memory.** Hits on meta-schema, domains, sleep tasks, and `$`-identities are filtered before `found` is decided — the engine's keyword fallback has no relevance threshold. Negative-cache keys are normalized (whitespace/case), and a `last_cleared_ms` guard drops misses whose search started before a concurrent cache clear (the miss could be answerable by the memory that just formed).
- **Self-test SleepTasks join the `System` domain** at creation, so each dream pass no longer inflates the orphan metric it reports on.
- **Shadow evaluation can actually measure decay knobs.** Fork settlements bypass the weekly decay rate limit (forks inherit the live `decay_applied_at` stamps, which made every decay comparison a systematic tie); the replay sample defaults to the policy's `shadow_replay_sample`; and the `JUDGE_MODEL_*` variables now configure an independent judge in *service* mode too (`AppState` installs it on every loaded space), so on-line verdicts stop falling back to the evaluated space's own model.
- **Optimizer gates hardened.** The holdout baseline is monotone (it was re-baselined downward on accept, letting N generations ratchet holdout down by N×ε); duplicate-field patch sets are rejected (chaining three patches on one field compounded past the ±50% step bound); and a drop-guard clears the process-wide policy override on any exit path, so a panic or early return cannot leak a candidate policy into later evals.
- **Mined scenarios never overwrite pending reviews.** Same-slug output files get a numeric suffix instead of silently replacing an earlier mined scenario awaiting human review.
- **Correction rates gain a denominator.** Full-scope settlements census per-source total link counts into `source_reliability.total_links`, turning raw correction counts into rates for encode-time source discounting (P3).
- **Auth-matrix tests for the six evolution endpoints** (`probe`, `recall_structured`, `memory_status`, `memory/pin`, `memory/forget`, `management/shadow_eval`): 401 when private, public-space bypass for reads only, and write/management gates verified end to end. BrainMaintenance.md §3.2 no longer showcases a bare bulk-decay `UPDATE` that contradicted Phase 7's "runtime-settled" contract, and the README documents the single-writer-per-space deployment assumption.

### Added
- **Shadow evaluation (evolution plan M11).** `POST /v1/{space_id}/management/shadow_eval` compares a candidate `MemoryPolicy` against the current one on the production distribution: the space forks twice into isolated in-memory stores, both forks settle under their policies, recent real recall queries replay on each, and the judge blind-compares answers with deterministic A/B alternation. The live space is only read (fork replays cannot touch its ledger/metrics — guardrail 4); the report persists in the `shadow_report` extension and promotion stays human via `update_space`.
- **Memory observability (`memory_status`, evolution plan M12).** `GET /v1/{space_id}/memory_status` returns incrementally-maintained counters (every evolution module bumps its own at write time — reads never run heavy queries), derived rates (probe hit rate, correction rate, mean self-reported uncertainty, maintenance-tokens-per-recall ROI proxy), graph counts, and the latest settlement/self-test/shadow reports.
- **Schema-metabolism census (evolution plan M8).** Full-scope settlements record a per-predicate link census into the `schema_audit` extension; `GraphStats`/`memory_status` gain a `predicate_types` schema-sprawl indicator, and BrainMaintenance.md Phase 6 now carries guarded predicate-merge guidance (bounded batches, core predicates untouchable, "unsure → review SleepTask, a wrong merge is worse than sprawl").
- **Policy genome optimization (evolution plan M10).** `anda_brain eval --optimize policy` evolves the numeric `MemoryPolicy` knobs: the optimizer LLM proposes 1–3 bounded mutations per generation (±50% steps, range-validated in code), candidates install through a process-wide eval policy override that run-scoped spaces pick up, and the accepted policy is written to `--optimize-out/memory_policy.json` for human review. `OptimizeConfig::default()` now also carries the documented noise-band floors (`min_delta` was silently 0 on the CLI path before).
- **Holdout gate for the optimizer (evolution plan M9).** `--holdout-scenario` runs a held-out suite whenever train accepts: candidates that improve train but regress holdout beyond `holdout_epsilon` are rejected as overfitting and reverted. Per-generation holdout totals are recorded in the optimize report.
- **Independent judge model (evolution plan M9).** `JUDGE_MODEL_*` env/CLI args route judge completions (checkpoint scoring, semantic assertion probes) through a separate model via the new `AssessContext::judge_complete`, installed on every run-scoped eval space including shared-formation forks. Judge scores stop sharing the evaluated system's blind spots.
- **Scenario mining (evolution plan M9).** `anda_brain eval --mine` distills an existing space's correction ledger into eval scenarios: superseded memories plus their source-conversation excerpts feed an LLM that writes correction-replay scenarios, strictly parsed, validated like hand-written fixtures, and PII-scrubbed (emails/long numbers masked on both LLM input and output). Mined files land in a review directory (`--mine-out`) outside the auto-validated fixture glob.
- **Dream self-test (evolution plan M7).** After each maintenance cycle completes, the runtime samples recent unused memories, generates one natural probe query per memory (single budgeted LLM call), and deterministically checks whether search surfaces them. Unfindable memories become pending `review` SleepTasks (source `memory_self_test`) with re-encode guidance for the next full cycle; BrainMaintenance.md Phase 2 documents how to process them. Self-test retrievals count only into the ledger's isolated `self_test_count`, the pass report persists in the `memory_self_test` extension, and the groundability rate surfaces as a new optional `GraphStats.groundability`.
- **Metamemory probe with negative-knowledge cache (evolution plan M5).** `POST /v1/{space_id}/probe` is an LLM-free existence check (hybrid/keyword search) returning `found` plus citation-shaped hits. Empty results are cached in the new `recall_misses` collection and answered from cache until formation completes (which clears the cache) or a 1h TTL expires — repeated dead-end queries stop costing graph work.
- **Pin and privacy-grade forget (evolution plan M6).** `POST /v1/{space_id}/memory/pin` marks entities `pinned` (exempt from confidence decay, already enforced by the M2 settlement); `POST /v1/{space_id}/memory/forget` physically deletes entities (concepts detach with their propositions; KIP_3004 keeps protecting system nodes) with `dry_run` preview and per-entity error reporting, cascading to usage-ledger rows. Archive does not satisfy forget.
- **Memory usage ledger (evolution plan M1).** Every completed recall records the graph entities its trace actually surfaced into the new `memory_usage` collection (`ledger::UsageLedger`): recall counts, last-recalled timestamps, and correction counts, with self-test counters reserved and isolated so the brain testing itself never counts as usage. Real usage is now the selection-pressure signal for memory evolution.
- **Deterministic usage-modulated metabolism (evolution plan M2).** Before each maintenance cycle the runtime settles memory metabolism in code (`Space::settle_memory_metabolism`): ledger counters flush onto graph metadata (`last_recalled_at`/`recall_count`), full cycles run the bulk confidence decay as a code-built KIP `UPDATE` (recently recalled, pinned, superseded, and system-truth links exempt; policy factor/floor; weekly rate-limited via `decay_applied_at`), and the report persists in the `memory_settlement` extension. BrainMaintenance.md Phase 7 now instructs the agent *not* to bulk-decay — only the semantic residue (re-confirmation, review flags) stays with the LLM.
- **Correction ledger and source reliability (evolution plan M3).** Settlement discovers newly superseded links, records them as corrections in the usage ledger, and aggregates correction counts per `metadata.source` into the `source_reliability` space extension — the raw material for encode-time source discounting.
- **Structured recall output (evolution plan M4).** New `POST /v1/{space_id}/recall_structured` returns `RecallOutput`: the answer plus trace-derived memory citations (entity id, type, name, confidence — never model-claimed), and the model's self-reported `found`/`uncertainty` from a new `<memory_meta>` footer contract in BrainRecall.md. The footer is stripped from all plain `recall` responses, so existing clients are unaffected.
- **Shared assessment instruments (`anda_brain::assess`, evolution plan M0).** The semantic-assertion judge, recall trace extraction, KIP probe helpers, and JSON-payload parsing moved out of the eval harness into a shared `assess` module behind a minimal `AssessContext` trait (implemented by `Space`, supertrait of `EvalDriver`). The offline harness and the upcoming maintenance self-test (plan M7) now consume identical instruments. Pure refactor: `eval::RecallTrace`/`ToolTrace` re-export from their new home.
- **Per-space `MemoryPolicy` (evolution plan M-P).** A versioned, validated policy object collects the numeric knobs of memory behavior (decay factor, stale-event threshold, backlog targets, plus fields reserved for reinforcement/self-test/recall/shadow phases). Stored in the `memory_policy` space extension, settable via `update_space`, readable via `Space::memory_policy()`. Maintenance cycles without explicit `parameters` now run under the space policy; defaults equal the values documented in BrainMaintenance.md, so an unset policy is not a behavior change.
- **Configurable semantic probe search.** Memory expectations accept `search_threshold` (default 0.35) and `search_limit` (default 8) for assertion probes; search text is now escaped for backslashes as well as quotes, and both fields are validated offline.
- **Eval report provenance.** `EvalReport` now records `profile_id`, the checkpoint `model`, and `started_at`, so reports can be compared across runs without external bookkeeping; checkpoint turns also carry the representative sample's model.
- **Fixture globbing in CI and `make eval-validate`.** Both now validate every `anda_brain/evals/*.json` fixture automatically (`*_profile.json` as profiles, the rest as scenarios), and a unit test (`bundled_eval_fixtures_parse_and_validate`) parses and validates the same set in `cargo test`. The wiki retrieval fixture moved to `anda_brain/evals/wiki/retrieval.json` to keep the top-level directory harness-only.
- **Checkpoint sampling with variance-aware gates.** Eval profiles accept `checkpoint_samples: N` (or `--checkpoint-samples`) to run Recall N times per checkpoint, reporting mean scores plus a propagated `total_stddev`; findings only count with majority support across samples, and `--confidence-z` makes `--min-score` gate on the lower confidence bound instead of a single noisy roll.
- **Shared-formation experiments.** `anda_brain eval --shared-formation` replays formation once per scenario, snapshots the space objects, and forks the snapshot into an isolated in-memory store per profile (`space::copy_space_objects` + `AppState::fork_with_store`), so maintenance policies are compared on identical encoded memory without formation LLM variance — and the most expensive phase runs once instead of once per profile.
- **LLM-as-judge scoring.** Profiles with `"judge": "llm"` score checkpoint answers against the rubric's previously unused `scoring_rubric` and the scenario `hidden_profile`: paraphrases count fully, correct meta-references to superseded facts are no longer penalized as stale, and the judge emits attributed findings plus a per-checkpoint satisfaction signal. Lexical scoring remains the deterministic default.
- **Semantic graph probes.** Memory expectations accept a natural-language `assertion` (with optional `search` text) instead of hand-written KQL; the harness runs a semantic search and lets the judge decide whether the evidence shows the asserted memory state, staying correct across valid graph-encoding variations.
- **Noise pressure and simulated users.** Scenarios accept a deterministic `noise` config (seeded corpus injection between anchors) and `"type": "simulated"` turns whose messages are written by an eval-only user simulator from the hidden profile, transcript, and satisfaction trail; reports carry a `satisfaction_trajectory`.
- **Trajectory metrics and real graph health.** Aggregate scores weight later checkpoints more, `evolution_quality` now measures late-vs-early checkpoint improvement instead of re-averaging other components, and `graph_health` reads real metabolism counters (unsorted backlog, orphans) through read-only KIP.
- **Prompt optimization loop.** `anda_brain eval --optimize formation|recall|maintenance|auto` treats the three agent prompts as an evolvable genome: attributed failures drive an optimizer LLM that proposes surgical find/replace edits, candidates are re-evaluated on fresh spaces, and edits are kept only when they beat the baseline beyond the sampling noise band. Accepted prompts and the decision log are written to `--optimize-out` for human review; agents read prompts through a new `agents::prompts` override layer.
- **Longitudinal memory eval harness.** `anda_brain::eval` can replay user timelines through Formation, optional Maintenance, and Recall checkpoints; score memory utility, forgetting quality, graph health, uncertainty, latency, and token cost; and attribute failures to Formation, Recall, Maintenance, grounding, synthesis, or overconfidence.
- **`anda_brain eval` CLI command.** Local eval runs now support single scenarios, scenario suites, profile comparisons, JSON report output, score/finding gates for CI, and starter scenarios/profiles under `anda_brain/evals/`.
- **Eval gate artifacts.** Gated `anda_brain eval` runs now embed the gate criteria, pass/fail state, and failure messages in the JSON report before returning a non-zero CI exit.
- **Eval validate-only mode.** `anda_brain eval --validate-only` now checks scenario/profile inputs offline, emits an `EvalValidationReport`, and fails before model or storage initialization when inputs are unsafe.
- **Eval fixture CI and summaries.** CI now runs the starter eval fixtures through offline validation, `anda_brain eval --summary-only` prints compact human-readable summaries, and the starter suite includes additional fact-correction, counterparty-boundary, travel-logistics, and expiring-discount scenarios.
- **Hermetic eval runs with automatic cleanup.** Every eval path (including single scenario + single profile) now runs in a freshly created, run-scoped space (`{space_id}_{profile}_{scenario}_{run_id}`), so reruns never score against memory left over from a previous run; run-scoped spaces are deleted from the object store after their report is collected unless `--keep-spaces` is passed (`space::delete_space_objects` + `AppState::evict_space`).
- **Strict eval fixture parsing.** Scenario and profile JSON now rejects unknown fields, turning rubric typos (e.g. `forbidden_terms` for `forbidden_answer_terms`) into load errors instead of silently weakened rubrics that still pass validation.

### Fixed
- **Agent failures are attributed to the stage that ran them.** A Formation agent failure now counts as `formation_miss` and a Maintenance failure as `bad_consolidation` (previously both were recorded as `bad_synthesis`), keeping attribution and the prompt optimizer's target selection honest.
- **Probe transport errors degrade instead of aborting.** A failed read-only KIP request during memory probing becomes a `graph_probe_error` finding and the run continues; previously it aborted the scenario and discarded every completed report in the suite.
- **Errored probes are no longer double-counted as memory failures.** An expectation whose probe errored (transport or KIP `Response::Err`) is scored as unknown — excluded from presence/forgetting weights — instead of also producing a `formation_miss`/`bad_consolidation` finding.
- **Unused expectation `answer_terms` now lower the lexical score.** Lexical utility averages probe-verified presence, required-term coverage, and expectation answer-term coverage, so a memory that exists but never reaches the answer costs points, not just findings.
- **Symmetric judge/harness finding dedup.** A judge finding with an expectation id no longer double-counts a harness finding of the same kind recorded without one.
- **Run-scoped space ids always satisfy AndaDB naming rules.** Composed eval space ids are lowercased to `[a-z0-9_]` and capped at 64 chars with a hash suffix, so uppercase/hyphenated scenario or profile ids and long `--space-id` values no longer fail at space creation.
- **Aborted eval runs no longer leak spaces.** Run-scoped spaces are closed and deleted even when a scenario or phase fails, across the suite, shared-formation, and optimizer paths.
- **Eval logs to stderr.** The eval command now initializes a stderr logger (stdout stays reserved for reports), so judge fallbacks and space setup are no longer silently dropped.
- **Trace grounding attribution matches tool outputs only.** Term evidence is no longer searched in tool names/args, where recall echoes the user's query, misclassifying grounding failures as synthesis failures.
- **Removed the ineffective `--auto-create-space` eval flag.** Run-scoped spaces are always freshly created; the boolean flag could not be disabled from the CLI anyway.
- **Eval turns no longer race in-flight maintenance.** Maintenance turns wait for the processing flag even when a cycle was already running (the agent returns no conversation id in that case), and checkpoints wait for maintenance to go idle before probing, so hook-triggered auto-maintenance can no longer let probes read a graph mid-consolidation.
- **Stuck background stages degrade to findings instead of aborting the suite.** Formation/maintenance wait timeouts are recorded as `formation_miss` / `bad_consolidation` findings and the run continues; failure attribution now also counts findings from non-checkpoint turns.
- **Judge findings no longer double-count harness findings.** Under the LLM judge, a judge finding duplicating an already-recorded probe/term finding of the same kind (and expectation) is dropped, keeping `--max-findings` gates honest.
- **Checkpoint token budgets no longer double-count cached tokens.** `max_checkpoint_total_tokens` now budgets input + output tokens only, since the OpenAI adapter already includes cached tokens in `input_tokens` while the Anthropic adapter reports cache reads separately.
- **`--optimize` now rejects `--min-score`/`--max-findings`** instead of silently ignoring them, and `--confidence-z` feeds the optimizer's accept/reject noise band.

### Changed
- **Shared-formation policy phases run concurrently.** Each profile's fork lives in its own in-memory store, so the policy replays now run in parallel per scenario.
- **JSON fixture load errors include the file path.**

## [0.9.2] — 2026-06-28

### Changed
- **Release version advanced to `0.9.2`.** `anda_brain` now reports `0.9.2`, and the lockfile was refreshed for the release.
- **Recall requests now use medium model effort.** Brain Recall raises its model effort from low to medium to improve answer quality while keeping the bounded runtime guardrails introduced in `0.9.1`.

## [0.9.1] — 2026-06-27

### Changed
- **Release version advanced to `0.9.1`.** `anda_brain` now reports `0.9.1`, and the lockfile was refreshed for the release.
- **Recall requests now use a leaner bounded runtime context.** Brain Recall limits carried history to the latest completed conversation, caches primer context briefly, loads counterparty/profile data concurrently, and no longer exposes the notes tool as a model dependency while still injecting notes into the prompt.

### Fixed
- **Recall execution now has explicit time and turn guardrails.** Recall runs enforce a total timeout and model turn limit, persist failed conversations consistently, and return the last available output with failure details when guardrails trip.

## [0.9.0] — 2026-06-27

### Added
- **Built-in MCP server support.** `anda_brain` now exposes memory operations through MCP over both stdio (`anda_brain mcp --space-id <spaceId> ...`) and Streamable HTTP (`/mcp/<spaceId>`), enabling MCP-capable agents to connect directly to Brain spaces.
- **MCP memory tools.** The MCP server provides tools for remembering conversations, recalling memory, running maintenance, and executing readonly KIP queries.
- **Remote MCP configuration controls.** HTTP service mode now includes MCP path prefix, host/origin allowlists, optional space auto-creation, stateful sessions, and keep-alive configuration.

### Changed
- **Release version advanced to `0.9.0`.** `anda_brain` and `anda-cli` now report `0.9.0`, and the lockfile was refreshed for the release.
- **Documentation and agent skill guidance now cover MCP integration.** English and Chinese READMEs, API docs, and SKILL files describe stdio and Streamable HTTP MCP setup plus the exposed memory tools.
- **Dependencies updated for MCP support.** The workspace now depends on `rmcp` and `schemars`, and `object_store` was refreshed to the `0.14` line.

### Fixed
- **Memory agents compact long-running contexts before continuing.** Formation, Maintenance, and Recall now perform runner handoffs when context windows fill, preventing large review prompts or extended tool loops from overrunning model limits while preserving conversation progress.

## [0.8.1] — 2026-06-20

### Changed
- **Release version advanced to `0.8.1`.** `anda_brain` now reports `0.8.1`, and the lockfile was refreshed for the release.
- **COSE/CWT test fixtures now use `cose2`.** The remaining direct `coset` dev dependency has been replaced with `cose2`, keeping token and COSE key fixtures aligned with the current COSE stack.
- **`make fix` now formats before applying clippy fixes.** The fix target runs `cargo fmt --all` before `cargo clippy --fix --workspace --tests`.
- **Anda ecosystem dependencies were refreshed to current patch releases.** The lockfile now resolves the latest compatible 0.13/0.8 runtime, database, KIP, and object-store crates.

## [0.8.0] — 2026-06-13

### Changed
- **Release version advanced to `0.8.0`.** `anda_brain` and `anda-cli` now report `0.8.0`, and the lockfile was refreshed for the release.
- **Anda ecosystem dependencies moved to the 0.13/0.8 line.** Brain now depends on `anda_core`, `anda_engine`, `anda_engine_server`, and `anda_web3_client` 0.13, plus the latest `anda_object_store` and `anda_db_tfs` 0.8 releases.
- **CBOR handling now uses `cbor2`.** Request parsing, CBOR responses, and payload tests now use canonical CBOR encoding/decoding through `cbor2` instead of `ciborium`.
- **Space token generation no longer depends on `ic_cose`.** Token entropy now comes from `rand`, allowing the direct `ic_cose` dependency to be removed while retaining 20-byte random token material.

## [0.7.2] — 2026-06-12

### Added
- **Maintenance status now exposes the latest task start time.** `MaintenanceAt` includes `start_at`, persisted when a maintenance cycle begins and surfaced through the Rust API, generated TypeScript API docs, Chinese API docs, and Go CLI client types.

### Changed
- **Release version advanced to `0.7.2`.** `anda_brain` and `anda-cli` now report `0.7.2`, and the lockfile was refreshed for the release.

## [0.7.1] — 2026-06-12

### Changed
- **Release version advanced to `0.7.1`.** `anda_brain` now reports `0.7.1`, and the lockfile was refreshed for the release.
- **Dependencies updated for the latest engine runtime fixes.** `anda_engine` 0.12.36 → 0.12.37, with transitive patch updates for `block-buffer`, `memchr`, and `smallvec`.
- **KIP prompt syntax summaries now match the expanded 0.8 grammar.** Brain Formation, Maintenance, Recall, and shared KIP syntax guidance show comma-separated multi-key `ORDER BY` and proposition-level `EXPECT VERSION` guards in their compact syntax blocks.

## [0.7.0] — 2026-06-11

### Added
- **KIP prompt assets now document the 0.8 protocol surface.** Brain Formation, Maintenance, Recall, and shared KIP syntax guidance cover reserved `_` metadata, `EXPECT VERSION` optimistic concurrency, predicate variables, multi-key `ORDER BY`, semantic/hybrid `SEARCH`, `UPDATE`, `MERGE`, and `EXPORT` patterns.
- **Regression coverage was added for new runtime guardrails.** Formation high-water processed markers, Maintenance history ordering and input validation, Recall empty-output preservation, token revoke safety, and conversation list limit clamping now have focused tests.

### Changed
- **Release version advanced to `0.7.0`.** `anda_brain` and `anda-cli` now report `0.7.0`, and the lockfile was refreshed for the release.
- **Anda ecosystem dependencies moved to the 0.8 line.** `anda_db`, `anda_cognitive_nexus`, and `anda_kip` now use the `0.8` series.
- **Brain service lifecycle handling is more robust.** Shutdown closes loaded spaces concurrently, idle eviction closes databases before removing entries, and scheduled maintenance trigger construction is simplified while preserving the existing cadence.
- **Shared request handling has been consolidated.** API handlers use a common sharding validator, and the Chinese website response is pre-rendered with the correct `zh-CN` document language.
- **`anda-cli` now targets the local Brain service by default and exposes deployment controls.** The CLI default base URL is `http://127.0.0.1:8042`, and new `--shard`/`ANDA_SHARD` plus `--timeout`/`ANDA_TIMEOUT` options send `Shard-Id` headers and tune HTTP request timeouts.
- **`anda-cli` documentation now reflects the current command surface.** The README documents `status` for service metadata, `info` for space details, `formation-status`, `get-or-init-user`, BYOK retrieval, `daydream` maintenance scope, batch formation exclusions, and single-command KIP readonly requests.
- **Documentation now reflects self-hosted deployment.** READMEs, website copy, and skill files remove discontinued hosted-service guidance, point users to self-hosted setup, link Anda Bot as a ready-to-run agent, and fix current CLI/API examples and model defaults.
- **Brain agent instructions are stricter and more operational.** Prompt assets emphasize empty-write discipline, bounded extraction, KIP error recovery, read-modify-write version guards, bulk update patterns, memory portability, and the absence of read-access statistics.

### Fixed
- **Formation resumes correctly from an empty processed marker.** Spaces restart formation from the beginning when no processed marker exists, so conversations queued before the first successful formation pass are not left stuck.
- **Formation processed markers are monotonic high-water marks.** Reprocessing an older conversation can no longer rewind `brain_processed`.
- **Agent records preserve original input on anomalous empty rounds.** Formation, Maintenance, and Recall no longer clear a conversation's original messages when a cancelled or empty round returns no chat history.
- **Agent context history now keeps completed conversations in runtime queue order.** Maintenance and Recall initialization filter out in-progress conversations and restore completed history oldest-to-newest, avoiding transient conversations leaking into later context while preserving newest entries correctly.
- **Maintenance startup and input handling are safer.** Malformed maintenance input is rejected before claiming the processing slot or creating a conversation.
- **Idle probes for unknown spaces no longer grow the space map indefinitely.** Uninitialized placeholder entries are evicted once idle, while initialized spaces are still protected against concurrent users and processing work.
- **Space token lookup and revocation are restricted to token-prefixed credentials.** Token verification and revocation now reject non-`ST` keys, keeping platform extensions such as tier and BYOK out of the token path.
- **Conversation listing clamps zero limits to safe bounds.** `limit=0` no longer allows an empty-page panic or unbounded scan.
- **`anda-cli formation` now rejects malformed JSON message payloads instead of storing them as plain text.** Valid JSON arrays and objects must decode to messages with role and content, while non-JSON log-like text still submits as a user message.
- **`anda-cli formation` batch mode avoids submitting bookkeeping and hidden files.** Recursive batch scans skip dot-prefixed entries, the checklist file, and temporary report files while preserving user/agent/topic context and only filling per-file `source` when it was not already provided.
- **`anda-cli execute-kip-readonly` accepts single-command requests cleanly.** Requests may now use either `command` or `commands`, object command `parameters` are optional and omitted when empty, and the two forms are validated as mutually exclusive.
- **`anda-cli conversations` supports 64-bit conversation IDs.** Conversation detail and delta commands parse IDs as unsigned 64-bit values to match server-side identifiers.

## [0.6.11] — 2026-06-10

### Changed
- **Dependencies updated for the latest engine runtime fixes.** `anda_engine` 0.12.32 → 0.12.35, bringing follow-up delivery, structured subagent arguments, and HTTP response decoding fixes into Brain.

### Fixed
- **Brain agents now read notes through the current engine note extension shape.** Formation, Maintenance, and Recall use `items` from the current notes payload while falling back to legacy notes storage when needed, preserving existing note context during the engine upgrade.

## [0.6.10] — 2026-06-07

### Changed
- **CI now validates the workspace on Linux, Windows, and macOS.** The GitHub Actions test job now uses an OS matrix, installs `protoc` per runner, and runs clippy plus workspace tests on all three platforms.
- **Dependencies updated for cross-platform runtime fixes.** `anda_core` 0.12.7 → 0.12.8 and `anda_engine` 0.12.30 → 0.12.32, picking up the latest platform-aware runtime support; transitive `bitflags` updated to 2.13.0.

## [0.6.9] — 2026-06-06

### Changed
- **KIP syntax guidance updated for RC7-compatible value handling.** Brain prompt assets now document JSON-compatible KIP values, unquoted identifier object keys, parameter placeholders in complete KIP value positions, `SEARCH` parameter forms, optional proposition handles, and the registered `belongs_to_class` predicate.
- **Brain Formation and Maintenance metadata discipline tightened.** Write templates now consistently include `created_at` alongside `source`, `author`, `confidence`, and `observed_at` where applicable.
- **Contradiction and decay workflows now update matched proposition IDs.** Formation and Maintenance examples first retrieve existing proposition IDs, then use `(id: :link_id)` updates to avoid accidentally creating missing historical links while marking facts superseded or decayed.
- **Brain Maintenance append patterns clarified.** Maintenance logs now use read-merge-write arrays instead of overwriting with a single-entry array, and confidence decay queries/updates are aligned with current KIP semantics.
- **Brain Recall ranking guidance aligned with current KIP ordering.** Contextual briefing now uses a single `ORDER BY` expression and instructs Recall to synthesize strongest-first ranking from returned evidence fields.
- **Dependencies updated.** `anda_cognitive_nexus` 0.7.19 → 0.7.20, `anda_core` 0.12.6 → 0.12.7, `anda_engine` 0.12.28 → 0.12.30, `anda_db` family patch releases, `anda_kip` 0.7.13 → 0.7.14, `anda_object_store` 0.3.3 → 0.3.4, plus minor `chrono` and `log` bumps.
- **Service startup and shutdown paths split into testable units.** CLI parsing, model configuration, object-store selection, CORS setup, router construction, and cancellation-driven service shutdown now have focused coverage without changing the public command-line surface.
- **Repository agent guidance added.** `AGENTS.md` now documents workspace layout, verification commands, Brain invariants, and API/doc synchronization expectations for future coding agents.

### Fixed
- **Space creation now persists metadata before returning.** Newly created spaces close the initialized database after saving metadata, ensuring owner and tier extensions are durable for subsequent opens. Idle eviction now closes spaces instead of only flushing them so resources are released consistently.
- **Formation and Maintenance history retention now records completed conversations only.** Shared history buffering ignores in-progress conversations and caps retained context deterministically, avoiding transient conversations leaking into later agent context.
- **Formation retries now clear stale failure reasons after success.** A conversation that previously failed but later completes now persists a null `failed_reason`, preventing old error text from lingering on successful runs.
- **BYOK updates now validate model configuration before persistence.** Invalid model settings fail before replacing stored BYOK configuration or mutating the runtime model registry.
- **External cancellation now participates in graceful shutdown.** Service shutdown can be driven by the cancellation token as well as OS signals, making runtime shutdown deterministic in tests and embedded callers.

## [0.6.8] — 2026-06-04

### Added
- **Test coverage for core Brain modules.** Added unit tests across 9 modules: Formation/Maintenance `ProcessingGuard` lifecycle, Recall KIP function definition and timeout, `AnyHost` matching, ED25519 public key parsing (trim, validation, comma-separated), `markdown_to_html` GFM tables and raw-HTML preservation, `StringOr` and `HeaderVals` X-Shard extractors, `SpaceEntry` initialization and `touch`, `ModelConfig` compact alias deserialization and engine conversion, compact ref serialization, double-encoded `InputContext` JSON strings, `MaintenanceScope` `FromStr`/`Display` roundtrip.

### Changed
- **Dependencies updated.** `anda_core` 0.12.4 → 0.12.6, `anda_engine` 0.12.24 → 0.12.28, plus minor bumps (bitflags, hyper, uuid, zerocopy, etc.).

## [0.6.7] — 2026-05-30

### Added
- **`PayloadFormat` struct** separating request `ContentType` detection from response serialization format. Request format now respects `Content-Type` header only; response format honors `Accept` header independently.
- **Conversation delta endpoint.** `GET /v1/{space_id}/conversations/{conversation_id}/delta` route for incremental conversation sync.
- **`daydream` maintenance scope.** New `MaintenanceScope::Daydream` variant for lightweight background processing.

### Fixed
- **KIP readonly auth scope corrected.** `execute_kip_readonly` was incorrectly requiring `Write` scope; changed to `Read` with `is_public` guard for space token verification.
- **`SpaceTier::allow_nodes` overflow prevention.** Replaced unchecked `pow(2, tier-1)` with `checked_pow` saturating to `MAX`.
- **`MaintenanceInput.timestamp` now optional.** Added `#[serde(default)]` so callers can omit the field.

### Changed
- **Default content type changed** from `Markdown(false)` to `Json` for both missing `Content-Type` and missing `Accept` headers.
- **API docs, SKILL.md, READMEs** updated with new endpoints, `daydream` scope, and anda-bot usage example.

## [0.6.6] — 2026-05-29

### Changed
- **Formation now defers to active Maintenance.** `FormationAgent::process` and the idle-path both early-return when `BrainHook::is_maintenance_processing()` is true, letting Maintenance finish before Formation resumes.
- **Shutdown path now explicitly flushes all open spaces.** Cancellation collects entries first, avoiding iterator-invalidation while holding the read lock.
- **Idle eviction guard tightened.** `try_remove_idle_space` checks `Arc::strong_count` on both the `SpaceEntry` (≤2) and `Space` (≤1) before evicting, preventing races where a request is mid-flight.
- **Space idle timeout tightened** from 20 minutes to 9 minutes for faster resource reclamation.

### Added
- **`is_maintenance_processing` hook.** New `BrainHook` trait method; `Hooks` implementation delegates to `space.maintenance.is_processing()`. Formation uses it to queue safely during Maintenance runs.
- **`TimedMemoryReadonly` read-only wrapper.** A `Tool` implementation wrapping `MemoryReadonly` with a 15-second `READONLY_KIP_TIMEOUT`; on timeout it returns a `KipErrorCode::ExecutionTimeout` response instead of hanging.
- **Recall read timeout.** `Space::kip_readonly` now wraps KIP execution in `tokio::time::timeout(15s)`, converting hangs into structured timeout errors.
- **Async `MaintenanceAgent::set_processed_at`.** Switched from synchronous extension write to `save_extension_from(...).await`, matching the engine's async persistence layer.

### Fixed
- **User init routed through Formation.** `get_or_init_user` now calls `space.formation.get_or_init_counterparty()` instead of `space.memory.get_or_init_caller()`, aligning user identity with the Formation pipeline.
- **`Space.formation` visibility.** Changed from private to `pub` so external callers can reach it without going through `memory`.
- **Maintenance history retention.** In-memory history buffer now keeps the latest 2 entries (was 3), reducing transient memory footprint during long maintenance runs.

## [0.6.5] — 2026-05-29

### Changed
- **Dropped "(大脑)" Chinese annotations from Brain identity.** All three KIP prompts (`BrainFormation`, `BrainMaintenance`, `BrainRecall`) now refer to "Brain" without the parenthetical Chinese label — the name is self-sufficient.
- **Default `memory_tier` changed from `episodic` to `short-term`** in Formation's event encoding template. New events start as short-term and graduate to episodic only after Maintenance validates them.

### Added
- **Flashbulb salience encoding in Formation.** Phase 2 now supports setting an initial `salience_score` (60–100) for emotionally charged moments (corrections, breakthroughs, strong commitments) so they resist decay from the start.
- **Reinforcement (spacing effect) in Formation.** Phase 3 ("Deduplicate & Reinforce") now strengthens re-confirmed facts — bump `evidence_count`, refresh `last_observed`, nudge `confidence` upward (cap 0.99). The counter-force to Maintenance's decay.
- **Associative encoding in Formation.** Phase 5b now links new concepts to already-grounded related concepts via existing predicates, forming a connected web for better recall.
- **Flashbulb salience protection in Maintenance.** Scoring now refines existing `salience_score` rather than blindly overwriting — flashbulb memories are preserved.
- **`resolve_contradiction` task action in Maintenance.** New action for reconciling conflicting facts (supersede the older, strengthen the current).
- **Strength-aware (asymmetric) decay in Maintenance.** Reinforced memories (high `evidence_count`, recent `last_observed`, high `salience_score`) decay slowly; low-salience/unreinforced facts fade faster — "use it or lose it" pruning.
- **Pattern K — Contextual Briefing in Recall.** Assembles identity + preferences + recent Events + commitments + Insights into a single composite briefing for the common "what should I know before I respond?" query.
- **Memory strength ranking in Recall.** Reinforced facts (high `evidence_count` + recent `last_observed`) now sort first; tie-break by recency then confidence.
- **`ModelEffort` wiring.** `ModelConfig` and `ModelConfigRef` now support an `effort` field (`serde` alias `e`), wired through to the engine. `main.rs` defaults to `ModelEffort::High`.

### Removed
- Redundant KIP `SPECIFICATION.md` links from all three prompts — the runtime auto-injects the primer.
- `Keep the response short` instruction from Formation's output format section — unnecessary constraint on the model's response style.

### Dependencies
- `anda_core` 0.12.3 → 0.12.4.
- `anda_engine` 0.12.23 → 0.12.24.
- `anda_kip` 0.7.12 → 0.7.13.
- `anda_cognitive_nexus` 0.7.18 → 0.7.19.
- `hyper` 1.9.0 → 1.10.0.
- `candid` 0.10.28 → 0.10.29.
- `zerocopy` 0.8.48 → 0.8.49.
- `displaydoc` 0.2.5 → 0.2.6.
- `socket2` 0.6.3 → 0.6.4.
- `mio` 1.2.0 → 1.2.1.
- `cmov` 0.5.3 → 0.5.4.

## [0.6.4] — 2026-05-27

### Changed
- **SKILL.md relocated from `anda_brain/` to `skills/anda-brain/`.** The skill file now lives in the top-level skills directory alongside other agent skills. Updated `handler.rs` `include_str!` path and `README.md` link accordingly.
- **`MODEL_CONTEXT_WINDOW` default reduced** from 1,000,000 to 400,000 in `main.rs` — reflects the typical context window of currently used models.

### Fixed
- ASCII art box alignment across all docs (`README.md`, `README_cn.md`, `anda_brain/README.md`, `WEBSITE.md`, `WEBSITE_cn.md`).

### Dependencies
- `anda_engine` 0.12.21 → 0.12.23.
- `reqwest` 0.13.3 → 0.13.4.
- `http` 1.4.0 → 1.4.1.
- `log` 0.4.29 → 0.4.30.
- `memchr` 2.8.0 → 2.8.1.
- `serde-saphyr` 0.0.26 → 0.0.27.
- `sval` family 2.19.0 → 2.20.0.
- `granit-parser` 0.0.2 → 0.0.3.

## [0.6.0] — 2026-05-21

### Changed
- **Project renamed from `anda-hippocampus` to `anda-brain`.** All crate names, directory names, asset files, OpenClaw plugin, CI workflows, Docker images, systemd service, Cargo/pnpm workspaces, Go module paths, and documentation updated accordingly.

## [0.5.4] — 2026-05-17

### Dependencies
- `anda_engine` 0.12.8 → 0.12.12.

**Engine changelog (cumulative 0.12.9–0.12.12):**

| Version     | Summary                                                                                                                                                                                                                                                                                                                                                              |
| ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **0.12.9**  | `steering_message` / `follow_up_message` upgraded from `Vec<String>` to `Vec<ContentPart>` — multimodal passthrough for steer/follow-up content.                                                                                                                                                                                                                     |
| **0.12.10** | `implicit_context` — injectable one-shot context that doesn't persist in message history. Fixed prompt ordering (system messages now consistently first) across all 4 providers (Anthropic, Gemini, OpenAI, OpenAIv2).                                                                                                                                               |
| **0.12.11** | Prevent `implicit_context` injection on tool-call turns (only injects when assistant actually responds). **DeepSeek compatibility**: skip `tool_choice` parameter for DeepSeek models (API doesn't support it).                                                                                                                                                      |
| **0.12.12** | **Tool output splitting**: multi-tool-output `Message`s now split into separate tool-role `MessageInput`s, each with its own `tool_call_id` (fixes protocol violation). **Message round-trip rewrite**: image/audio/file/video/refusal content parts preserved during `MessageOutput → Message` conversion (were silently lost). `msg.name` now survives round-trip. |

## [0.5.3] — 2026-05-16

### Dependencies
- `anda_engine` 0.12.6 → 0.12.8.

**Engine changelog (0.12.8):** Major release — Anthropic/Gemini types, OpenAI Responses API support, `TryFrom` MIME detection, SubAgent enhancements. Paired with `anda_core` v0.12.1.

## [0.5.2] — 2026-05-12

### Changed
- **User init routed through RecallAgent.** `get_or_init_user` now calls `space.recall.get_or_init_counterparty()` instead of `space.memory.get_or_init_caller()`, aligning user identity management with the recall pipeline.
- **`GetOrInitUserInput.user` type relaxed.** `user` field changed from `Principal` to `String` for broader caller compatibility.
- **`Space.recall` now `pub`.** RecallAgent is publicly accessible for user initialization and other external callers.

### Improved
- **Human-readable datetime in agent prompts.** Replaced `rfc3339_datetime()` with `local_date_hour()` across Formation, Maintenance, and Recall agents — `YYYY-MM-DD HH(AM/PM) ±TZ` format is more compact and readable for LLM context.
- **Prompt section labels consistently capitalized.** ("Your Notes", "Counterparty Profile", "Current Datetime").

### Removed
- **`SYSTEM_PROMPT_DYNAMIC_BOUNDARY`** from Formation, Maintenance, and Recall agent instruction prompts — simplifies prompt structure without loss of context.

### Dependencies
- `anda_engine` 0.12.2 → 0.12.6.

## [0.5.0] — 2026-05-07

### Features
- **Robust InputContext deserialization.** `InputContext` now accepts both a JSON object and a JSON string (1–2 levels of nesting), so clients that serialize context as a string work correctly. The `user` field is accepted as a legacy alias for `counterparty`. The OpenClaw plugin mirrors this behavior with a `normalizeInputContext()` helper.
- **Invocation Discipline for recall_memory.** Formation and Recall agent instructions now explicitly state that `recall_memory` is for long-term memory only — agents should answer from local context for facts already present in the active conversation. Formation runs asynchronously and fresh memories may take a minute or more to become searchable.
- **ConversationDelta HTTP endpoint and CLI support.** Incremental conversation fetching via delta tokens, enabling efficient long-running agent conversations without re-fetching the full history.
- **Dynamic token limits.** The model's context window is now read at runtime and used to compute the output budget, replacing hard-coded constants.
- **Conditional review trigger.** Formation review now obeys KIP spec alignment — only fires when meaningful change is detected in the knowledge graph.

### Refactors
- **Full model output budget.** Recall agent now uses the complete output budget available from the model, with the minimum floor raised to 32k tokens.
- **Remove deprecated `prune_raw_history_if`.** Cleaned up obsolete pipeline calls from the engine migration.

### Fixes
- **Note tool extension key.** Fixed incorrect extension key reference in the note tool.

### Internal
- Upgrade `anda_engine` dependency path from 0.11.22 → 0.12.0.
- Migrate `EngineModelConfig` from `label` to `labels` field.
- Bump all components to 0.5.0: `anda_brain`, `anda-cli`, `anda-brain-openclaw`.
