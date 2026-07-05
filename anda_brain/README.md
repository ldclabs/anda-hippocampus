# Anda Brain — Technical Documentation

A dedicated LLM-powered memory management service that maintains a persistent **Cognitive Nexus** (Knowledge Graph) on behalf of business AI agents via [KIP (Knowledge Interaction Protocol)](https://github.com/ldclabs/KIP).

Business agents interact entirely through natural language and a REST API — no KIP knowledge required.

Anda Brain is designed to be **self-hosted** (the hosted cloud service has been discontinued). For a complete agent built on Anda Brain, see [Anda Bot](https://github.com/ldclabs/anda-bot).

## Architecture

```
┌─────────────────────┐
│   Business Agent    │  ← Focuses on business logic & user interaction
│  (No KIP knowledge) │    Only speaks natural language
└────────┬────────────┘
         │ Natural Language / REST API
         ▼
┌─────────────────────┐
│      Brain          │  ← The ONLY layer that understands KIP
│   (LLM + KIP)       │    Three agents: Formation / Recall / Maintenance
└────────┬────────────┘
         │ KIP (KQL / KML / META)
         ▼
┌─────────────────────┐
│  Cognitive Nexus    │  ← Persistent Knowledge Graph (backed by AndaDB)
│  (Knowledge Graph)  │
└─────────────────────┘
```

## Features

- **Zero KIP knowledge required** — Business agents interact through natural language and a simple REST API.
- **Persistent, structured memory** — Facts, preferences, relationships, events, and patterns encoded into a knowledge graph.
- **Three operational modes** — Formation (encoding), Recall (retrieval), and Maintenance (consolidation & pruning).
- **Multi-space isolation** — Each space has its own independent database, knowledge graph, and conversation history.
- **Triple serialization** — Supports JSON, CBOR, and Markdown for request/response payloads (negotiated via `Content-Type` / `Accept` headers).
- **Built-in MCP server** — MCP-capable agents can use Anda Brain through Streamable HTTP or stdio tools without writing REST glue code.
- **Pluggable storage backends** — Local filesystem, AWS S3, or in-memory (for development/testing).
- **Longitudinal eval harness** — Development/CI support for replaying user timelines, probing graph state, scoring checkpoints, and attributing memory failures to Formation, Recall, or Maintenance.

## Agents

### Formation — Memory Encoding (`formation_memory`)

Receives conversation messages and encodes them into structured memory within the Cognitive Nexus via KIP.

**System prompt:** [BrainFormation.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/BrainFormation.md)

**Processing pipeline:**
1. Receives `FormationInput` (messages + optional context + timestamp).
2. Creates a tracked `Conversation` record (status: `Submitted` → `Working` → `Completed` | `Failed`).
3. LLM analyzes messages, extracting three types of memory:
   - **Episodic memory** — Events with timestamps, participants, outcomes
   - **Semantic memory** — Stable facts, preferences, relationships, domain knowledge
   - **Cognitive memory** — Behavioral patterns, decision criteria, communication style
4. Deduplicates against existing knowledge (SEARCH before CREATE).
5. Encodes structured memory into the Cognitive Nexus via `execute_kip` tool.

**Key behaviors:**
- Sequential processing with automatic queue draining — new conversations are picked up after the current one completes.
- Atomic single-conversation processing via `processing_conversation` flag.
- Schema auto-evolution — defines new concept types/predicates when needed.

### Recall — Memory Retrieval (`recall_memory`)

Translates natural language queries into knowledge graph lookups and returns synthesized answers.

**System prompt:** [BrainRecall.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/BrainRecall.md)

**Processing pipeline:**
1. Receives `RecallInput` (query + optional context).
2. Analyzes query intent (entity lookup, relationship traversal, attribute query, event recall, pattern detection, etc.).
3. Grounds entities to actual graph nodes (resolves ambiguity).
4. Executes structured KQL queries via read-only memory tools + conversation search.
5. Iterative deepening — follows up with additional queries if needed (max 5 rounds).
6. Synthesizes results into a coherent natural language answer.

**Available tools:**
- `MemoryReadonly` — Read-only access to the knowledge graph

### Maintenance — Memory Metabolism (`maintenance_memory`)

Consolidates, prunes, and optimizes the knowledge graph during scheduled or on-demand cycles.

**System prompt:** [BrainMaintenance.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/BrainMaintenance.md)

**Processing phases (full scope):**
1. **Assessment** — Audit memory health (read-only): `DESCRIBE PRIMER`, pending SleepTasks, unsorted items, orphans, stale events.
2. **SleepTask Processing** — Handle queued actions: `consolidate_to_semantic`, `archive`, `merge_duplicates`, `reclassify`, `review`.
3. **Unsorted Inbox** — Reclassify items to appropriate topic domains.
4. **Stale Event Consolidation** — Extract semantic knowledge from old events (configurable threshold), create linked Preference/Fact nodes.
5. **Duplicate Merging** — Find and merge similar concepts, updating all propositions.
6. **Orphan Cleanup** — Assign domain-less concepts to appropriate domains.
7. **Confidence Decay** — Age facts by reducing confidence scores (`confidence * decay_factor`).

**Key behaviors:**
- Single-execution guard — only one maintenance cycle can run at a time per space.
- Non-destructive principle — archives before deleting, decays confidence rather than removing.
- Async execution — returns immediately with conversation ID; actual processing in background.

## Longitudinal Evaluation

Anda Brain includes an eval-first harness in `anda_brain::eval`. It drives the
same deep interface used by real callers:

1. Replay normal turns through Formation.
2. Optionally trigger Maintenance by explicit turns or every N normal turns.
3. Run checkpoint turns through Recall.
4. Execute read-only KIP probes before each checkpoint.
5. Score answer utility, forgetting quality, graph health, uncertainty, latency,
   and token cost.
6. Attribute failures to `formation_miss`, `bad_consolidation`,
   `bad_grounding`, `bad_synthesis`, or `overconfidence`.

The harness is intended for local experiments, regression tests, and CI
benchmarks. It is not a public HTTP endpoint. See
[`evals/style_preference.json`](evals/style_preference.json) for a minimal
scenario shape.

Beyond the basic replay loop, the harness supports:

- **Sampling & variance** — `checkpoint_samples: N` in a profile (or
  `--checkpoint-samples N`) runs Recall N times per checkpoint and reports the
  mean score plus `total_stddev` (propagated through suite and experiment
  aggregates). Findings only count when they appear in a majority of samples.
  `--confidence-z Z` makes `--min-score` gate on `total - Z * stddev`, so a
  lucky single roll cannot pass CI.
- **LLM judge** — `"judge": "llm"` in a profile scores each answer against the
  checkpoint's `scoring_rubric` and the scenario's `hidden_profile`.
  Paraphrases count fully, and correct meta-references to superseded facts
  ("unlike your old BBQ preference…") are not penalized as stale. The judge
  also emits attributed findings and a per-checkpoint `satisfaction` signal.
  The lexical scorer remains the default for deterministic smoke runs.
- **Semantic probes** — an expectation may state an `assertion` in natural
  language ("an active, non-superseded BBQ preference for user_042") instead
  of hand-written KQL. The harness runs a semantic graph search and asks the
  judge whether the evidence shows the statement, so probes stay correct
  across valid encoding variations. Raw `probe` KQL remains as fallback.
- **Noise pressure** — a scenario-level `noise` config deterministically
  inserts chit-chat turns between authored anchors (`between_turns`, `seed`,
  optional `corpus`), scaling a 6-turn script into a long timeline where
  Formation must keep the needle in a haystack.
- **Simulated users** — `"type": "simulated"` turns carry an `intent`; an
  eval-only user simulator writes the actual message from `hidden_profile`,
  the recent transcript, and the satisfaction trail, adapting its behavior
  when the memory system has been failing it. Reports include a
  `satisfaction_trajectory` — the survival-pressure signal.
- **Trajectory metrics** — aggregate scores weight later checkpoints more
  (an established memory failing late costs more than an early miss), and
  `evolution_quality` compares late-half vs early-half checkpoint scores:
  above 0.5 means the system improved over the timeline. `graph_health` reads
  real metabolism counters (unsorted backlog, orphans) via read-only KIP
  instead of probe execution success.
- **Shared-formation experiments** — `--shared-formation` (with multiple
  `--profile`) replays formation once per scenario, snapshots the space, and
  forks the snapshot into an isolated in-memory store per profile. Every
  maintenance policy is then measured on identical encoded memory, removing
  formation LLM variance as a confound — and the most expensive phase runs
  once instead of once per profile. Requires all user turns to precede the
  first checkpoint (validated); use the default interleaved mode otherwise.
- **Prompt optimization** — `--optimize formation|recall|maintenance|auto`
  runs an offline evolution loop with the eval suite as fitness: attributed
  failures are fed to an optimizer LLM that proposes surgical find/replace
  edits to the responsible agent prompt; the candidate suite must beat the
  baseline beyond the sampling noise band to be kept, otherwise it is
  reverted. Accepted prompts and the full decision log are written to
  `--optimize-out` (default `./eval_optimize`) for human review — nothing is
  written back to `assets/`. Two caveats to keep in mind when reading
  optimize results: the noise band only covers Recall sampling variance
  (`checkpoint_samples`) — each generation re-runs formation, whose LLM
  variance is *not* in the band, so prefer more scenarios and samples over
  trusting a single close call; and the judge runs on the same model that
  powers the agents, so judge scores share that model's blind spots. Treat
  accepted prompts as candidates for human review, not as ground truth.
- **Hermetic runs & cleanup** — every run executes in freshly created,
  run-scoped spaces named `{space_id}_{profile}_{scenario}_{run_id}`, so
  reruns never see memory left over from a previous run. These spaces are
  deleted from the object store once their report is collected; pass
  `--keep-spaces` to keep them for post-mortem inspection (e.g. poking the
  graph with read-only KIP).

Scenario and profile JSON is parsed strictly: an unknown field (usually a
typo like `forbidden_terms` for `forbidden_answer_terms`) fails the load
instead of silently weakening the rubric.

Validate scenario/profile inputs without running models:

```bash
cargo run -p anda_brain -- \
  eval \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --profile anda_brain/evals/default_profile.json \
  --validate-only \
  --summary-only
```

Run a scenario locally:

```bash
cargo run -p anda_brain -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id style_eval \
  --scenario anda_brain/evals/style_preference.json \
  --profile anda_brain/evals/default_profile.json \
  --output /tmp/style_eval_report.json \
  local --db /tmp/anda-brain-eval-db
```

Run a small suite by repeating `--scenario`. The suite writes one
`EvalSuiteReport` with per-scenario reports plus aggregate score, usage, and
failure attribution:

```bash
cargo run -p anda_brain -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id memory_suite \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --scenario anda_brain/evals/preference_reversal.json \
  --profile anda_brain/evals/default_profile.json \
  --output /tmp/anda_brain_eval_suite.json \
  local --db /tmp/anda-brain-eval-suite-db
```

Compare maintenance policies by repeating both `--scenario` and `--profile`.
The experiment writes one `EvalExperimentReport` with `best_suite_id` and
ranked `comparisons`, so profiles can be compared by quality, findings, and
token cost:

```bash
cargo run -p anda_brain -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id memory_experiment \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --scenario anda_brain/evals/preference_reversal.json \
  --profile anda_brain/evals/no_maintenance_profile.json \
  --profile anda_brain/evals/default_profile.json \
  --profile anda_brain/evals/quick_profile.json \
  --output /tmp/anda_brain_eval_experiment.json \
  local --db /tmp/anda-brain-eval-experiment-db
```

Add `--summary-only` to any eval command to print a compact human-readable
summary instead of JSON. Omit it for artifacts intended for CI or downstream
analysis.

Use gates in CI to fail when aggregate quality falls below a floor. The report
is written before the command exits non-zero, and gated runs include a top-level
`gate` object with the criteria, pass/fail state, and failure messages:

```bash
cargo run -p anda_brain -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id memory_ci \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --profile anda_brain/evals/default_profile.json \
  --output /tmp/anda_brain_ci_eval.json \
  --min-score 0.75 \
  --max-findings 3 \
  local --db /tmp/anda-brain-ci-eval-db
```

Report shapes:

- `EvalValidationReport`: emitted by `--validate-only`; contains `passed`, `planned_runs`, scenario/profile plans, and validation `issues` with `error` or `warning` severity.
- `EvalReport`: single scenario result; contains `scenario_id`, aggregate `score`, optional `total_stddev`, `attribution`, `usage`, `satisfaction_trajectory`, optional `gate`, and per-turn reports (with per-sample scores, probes, graph stats, and judge reasoning).
- `EvalSuiteReport`: one profile across multiple scenarios; contains `suite_id`, aggregate `score`, optional `total_stddev`, `attribution`, `usage`, optional `gate`, and child `reports`.
- `EvalExperimentReport`: multiple profiles across the same scenario set; contains `experiment_id`, aggregate `score`, optional `total_stddev`, `best_suite_id`, ranked `comparisons`, optional `shared_formation` reports, optional `gate`, and child `suites`.
- `EvalScore`: normalized `total`, `memory_utility`, `evolution_quality`, `uncertainty_calibration`, `forgetting_quality`, `graph_health`, `latency_penalty`, and `token_cost_penalty`.
- `AttributionSummary`: counts failures by `formation_miss`, `bad_consolidation`, `bad_grounding`, `bad_synthesis`, `overconfidence`, `graph_probe_error`, `latency_cost`, `token_cost`, and `judge_error`.
- `OptimizeReport`: emitted by `--optimize`; contains `baseline_total`, `final_total`, per-generation edits with accept/reject decisions, and `accepted_prompts`.

Included starter scenarios:

- [`evals/style_preference.json`](evals/style_preference.json) — long-term writing style preference.
- [`evals/project_budget.json`](evals/project_budget.json) — project context and rough budget recall.
- [`evals/preference_reversal.json`](evals/preference_reversal.json) — newer preference superseding stale preference.
- [`evals/fact_correction.json`](evals/fact_correction.json) — corrected fact superseding stale fact.
- [`evals/counterparty_boundary.json`](evals/counterparty_boundary.json) — preferences isolated by counterparty.
- [`evals/travel_logistics.json`](evals/travel_logistics.json) — durable travel logistics and preferences.
- [`evals/expiring_discount.json`](evals/expiring_discount.json) — expired time-sensitive facts not reused.
- [`evals/noisy_style_preference.json`](evals/noisy_style_preference.json) — longitudinal pressure: style preferences must survive injected noise and a simulated follow-up, measured at two checkpoints for trajectory.

Included starter profiles:

- [`evals/no_maintenance_profile.json`](evals/no_maintenance_profile.json) — Formation plus Recall without scheduled maintenance.
- [`evals/default_profile.json`](evals/default_profile.json) — daydream maintenance every two normal turns.
- [`evals/quick_profile.json`](evals/quick_profile.json) — quick maintenance every two normal turns.
- [`evals/llm_judge_profile.json`](evals/llm_judge_profile.json) — LLM judge with three recall samples per checkpoint for mean±stddev scoring.

## API Endpoints

Detailed API docs (with TypeScript request/response types):
- English: [API.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/API.md)
- 中文: [API_cn.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/API_cn.md)
- Agent Skill: [SKILL.md](https://github.com/ldclabs/anda-brain/blob/main/skills/anda-brain/SKILL.md)

| Method  | Path                                                   | Description                                                                   | Auth Scope                   |
| ------- | ------------------------------------------------------ | ----------------------------------------------------------------------------- | ---------------------------- |
| `GET`   | `/`                                                    | Anda Brain website                                                            | —                            |
| `GET`   | `/favicon.ico`                                         | Favicon                                                                       | —                            |
| `GET`   | `/apple-touch-icon.webp`                               | Apple touch icon                                                              | —                            |
| `GET`   | `/info`                                                | Service info (name, version, sharding)                                        | —                            |
| `GET`   | `/SKILL.md`                                            | Skill description (Markdown)                                                  | —                            |
| `GET`   | `/v1/{space_id}/info`                                  | Get space status & statistics                                                 | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/formation_status`                      | Get formation status (lightweight endpoint for monitoring formation progress) | `read` (CWT or space token)  |
| `POST`  | `/v1/{space_id}/formation`                             | Submit messages for memory encoding                                           | `write` (CWT or space token) |
| `POST`  | `/v1/{space_id}/recall`                                | Query memory with natural language                                            | `read` (CWT or space token)  |
| `POST`  | `/v1/{space_id}/maintenance`                           | Trigger maintenance cycle                                                     | `write` (CWT or space token) |
| `POST`  | `/v1/{space_id}/execute_kip_readonly`                  | Execute a KIP request (read-only mode, suitable for queries)                  | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/conversations/{conversation_id}`       | Get one conversation detail                                                   | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/conversations/{conversation_id}/delta` | Get incremental conversation updates                                          | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/conversations`                         | List conversations (cursor pagination)                                        | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/management/space_tokens`               | List space tokens                                                             | `write` (CWT)                |
| `POST`  | `/v1/{space_id}/management/add_space_token`            | Add a space token                                                             | `write` (CWT)                |
| `POST`  | `/v1/{space_id}/management/revoke_space_token`         | Revoke a space token                                                          | `write` (CWT)                |
| `PATCH` | `/v1/{space_id}/management/update_space`               | Update space information (name, description, public/private)                  | `write` (CWT)                |
| `PATCH` | `/v1/{space_id}/management/restart_formation`          | Restart a formation task                                                      | `write` (CWT)                |
| `GET`   | `/v1/{space_id}/management/space_byok`                 | Get BYOK (Bring Your Own Key) configuration                                   | `write` (CWT)                |
| `PATCH` | `/v1/{space_id}/management/space_byok`                 | Update BYOK (Bring Your Own Key) configuration                                | `write` (CWT)                |
| `POST`  | `/admin/{space_id}/update_space_tier`                  | Update a space tier (manager only)                                            | `write` (CWT)                |
| `POST`  | `/admin/create_space`                                  | Create a new space (manager only)                                             | `write` (CWT)                |

### MCP Server

When the HTTP service starts, Anda Brain also exposes a Streamable HTTP MCP endpoint:

```text
https://your-brain-host/mcp/{space_id}
```

Use this for multi-user deployments where each employee or agent team receives a dedicated Brain space. MCP clients should send the same CWT or space token used by REST as `Authorization: Bearer <token>`. Read-only tools can access public spaces without a token.

For local desktop or development clients, Anda Brain can also run as a stdio MCP server:

```bash
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  cargo run -p anda_brain -- mcp --space-id my_space_001 local --db ./data
```

Both MCP modes use the same storage/model configuration as the HTTP service and expose these tools:

| Tool | Purpose | Scope |
| ---- | ------- | ----- |
| `anda_brain_remember_conversation` | Encode conversation messages into memory | `write` |
| `anda_brain_recall_memory` | Ask natural-language questions against memory | `read` |
| `anda_brain_run_maintenance` | Trigger memory consolidation/pruning | `write` |
| `anda_brain_get_space_info` | Read space statistics and metadata | `read` |
| `anda_brain_get_formation_status` | Read formation/maintenance progress | `read` |
| `anda_brain_execute_kip_readonly` | Run read-only KIP for advanced graph inspection | `read` |
| `anda_brain_get_or_init_user` | Get or create a counterparty concept | `write` |
| `anda_brain_list_conversations` | Page through tracked conversations | `read` |
| `anda_brain_get_conversation` | Read one tracked conversation or delta | `read` |

If authentication is enabled, pass a CWT or space token. Remote MCP reads it from the HTTP `Authorization` header; stdio reads it from `MCP_AUTH_TOKEN` or `--mcp-auth-token`. For local-only development with auth disabled, the token can be omitted. Remote MCP auto-create requires `ED25519_PUBKEYS` plus a `write` CWT for the target space before the missing space is created. Use `MCP_HTTP_ALLOWED_HOSTS` when exposing remote MCP behind a company domain or reverse proxy.

### Content Negotiation

Triple serialization via `Content-Type` / `Accept` headers:

- `application/json` — JSON (default)
- `application/cbor` — CBOR (binary, more compact)
- `text/markdown` — Markdown (human-readable text)

All responses use an RPC envelope:

```json
{"result": { ... }, "error": null}
```

### Authentication

All endpoints (except `/`, `/info` and `/SKILL.md`) require a Bearer token:

```
Authorization: Bearer <base64_encoded_cose_sign1_token>
```

If `ED25519_PUBKEYS` is not provided (empty), authentication is effectively disabled: API requests are accepted without signature verification.

Token format: COSE Sign1 message signed with Ed25519 keys, containing CWT claims:

| Claim   | Purpose                                                 |
| ------- | ------------------------------------------------------- |
| `sub`   | Principal ID (who is making the request)                |
| `aud`   | Audience — the space ID being accessed (or `*` for any) |
| `scope` | Permission level: `read`, `write` (or `*` for any)      |

### POST /admin/create_space

Create a new isolated memory space. Requires manager principal.

**Request:**
```json
{
  "user": "<owner_principal_id>",
  "space_id": "my_space_001",
  "tier": 0
}
```

**Response:**
```json
{
  "result": {
    "space_id": "my_space_001",
    "owner": "owner_principal_id",
    ...
  }
}
```

### POST /v1/{space_id}/formation

Submit conversation messages for memory encoding. Processing is asynchronous — returns immediately while encoding continues in the background.

**Request:**
```json
{
  "messages": [
    {
      "role": "user",
      "content": "I prefer dark mode. My timezone is UTC+8.",
      "name": "Alice"
    },
    {
      "role": "assistant",
      "content": "Got it! I've noted your preferences."
    }
  ],
  "context": {
    "counterparty": "alice_principal_id",
    "agent": "customer_bot_001",
    "source": "source_123",
    "topic": "settings"
  },
  "timestamp": "2026-03-09T10:30:00Z"
}
```

| Field                  | Type        | Required | Description                                                     |
| ---------------------- | ----------- | -------- | --------------------------------------------------------------- |
| `messages`             | `Message[]` | Yes      | Conversation messages (`role`: `user` / `assistant` / `system`) |
| `context.counterparty` | `string`    | No       | User identifier                                                 |
| `context.agent`        | `string`    | No       | Calling agent identifier                                        |
| `context.source`       | `string`    | No       | Identifier of the source of the current interaction content     |
| `context.topic`        | `string`    | No       | Conversation topic                                              |
| `timestamp`            | `string`    | No (recommended) | ISO 8601 timestamp                                      |

**Response:**
```json
{
  "result": {
    "conversation": 1,
    ...
  }
}
```

### POST /v1/{space_id}/recall

Query memory with natural language. Returns a synthesized answer from the knowledge graph and conversation history.

**Request:**
```json
{
  "query": "What are Alice's preferences?",
  "context": {
    "counterparty": "alice_principal_id",
    "topic": "settings"
  }
}
```

| Field                  | Type     | Required | Description                   |
| ---------------------- | -------- | -------- | ----------------------------- |
| `query`                | `string` | Yes      | Natural language question     |
| `context.counterparty` | `string` | No       | User identifier               |
| `context.agent`        | `string` | No       | Calling agent identifier      |
| `context.topic`        | `string` | No       | Topic hint for disambiguation |

**Response:**
```json
{
  "result": {
    "content": "Alice prefers dark mode and operates in UTC+8 timezone.",
    ...
  }
}
```

### POST /v1/{space_id}/maintenance

Trigger a memory maintenance cycle. Runs asynchronously with single-execution guard.

**Request:**
```json
{
  "trigger": "on_demand",
  "scope": "daydream",
  "timestamp": "2026-03-10T03:00:00Z",
  "parameters": {
    "stale_event_threshold_days": 7,
    "confidence_decay_factor": 0.95,
    "unsorted_max_backlog": 20,
    "orphan_max_count": 10
  }
}
```

| Field                                   | Type     | Required | Description                                               |
| --------------------------------------- | -------- | -------- | --------------------------------------------------------- |
| `trigger`                               | `string` | No       | `scheduled` / `threshold` / `on_demand` (default: `on_demand`) |
| `scope`                                 | `string` | No       | `full` (all phases) / `quick` (assessment + urgent tasks) / `daydream` (idle-time salience scoring & micro-consolidation, default) |
| `timestamp`                             | `string` | No       | ISO 8601 timestamp                                        |
| `parameters.stale_event_threshold_days` | `u32`    | No       | Days before events are considered stale (default: 7)      |
| `parameters.confidence_decay_factor`    | `f64`    | No       | Decay multiplier per cycle (default: 0.95)                |
| `parameters.unsorted_max_backlog`       | `u32`    | No       | Max unsorted items to process (default: 20)               |
| `parameters.orphan_max_count`           | `u32`    | No       | Max orphans to process (default: 10)                      |

**Response:**
```json
{
  "result": {
    "conversation": 8,
    ...
  }
}
```

### GET /v1/{space_id}/info

Get space statistics and health information.

**Response:**
```json
{
  "result": {
    "space_id": "my_space_001",
    "owner": "principal_id",
    "db_stats": { "total_items": 150, "total_bytes": 524288 },
    "concepts": 85,
    "propositions": 120,
    "conversations": 12,
    ...
  }
}
```

## Recall Function Definition

Business agents can register the Recall endpoint as an LLM tool/function call. See [RecallFunctionDefinition.json](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/RecallFunctionDefinition.json) for the OpenAI function-calling format.

## Memory Space Lifecycle

### Creation
1. Creates a new `AndaDB` instance.
2. Initializes `CognitiveNexus` (knowledge graph).
3. Loads bootstrap KIP definitions (`$self`, `$system`, core meta-types).
4. Stores creator/owner principal IDs.

### Runtime
- Spaces are **lazy-loaded** on first access via `OnceCell`.
- In-memory cache with access tracking.
- **5-minute interval**: Flush active spaces to storage.
- **9-minute idle timeout**: Evict unused spaces from cache (skipped while a space is pinned, processing, or still referenced by requests).
- Graceful shutdown: Close all space databases before exit.

### Memory Types in the Cognitive Nexus

| Type            | Nodes                                   | Description                                 |
| --------------- | --------------------------------------- | ------------------------------------------- |
| **Concept**     | `{type: "UpperCamelCase", name: "..."}` | Entities with typed attributes and metadata |
| **Proposition** | `(Subject, Predicate, Object)`          | Directed relationships between concepts     |
| **Domain**      | Grouping node                           | Organizational containers for concepts      |

The schema is self-describing — all type definitions are stored as nodes within the graph itself. Types can be defined on-the-fly by the Formation agent as needed.

## Configuration

### CLI Arguments / Environment Variables

| Env Variable           | CLI Flag                 | Default                             | Description                                                                          |
| ---------------------- | ------------------------ | ----------------------------------- | ------------------------------------------------------------------------------------ |
| `LISTEN_ADDR`          | `--addr`                 | `127.0.0.1:8042`                    | Listen address                                                                       |
| `ED25519_PUBKEYS`      | `--ed25519-pubkeys`      | —                                   | Comma-separated Base64 Ed25519 public keys; if empty, API authentication is disabled |
| `MODEL_FAMILY`         | `--model-family`         | `anthropic`                         | Model family to use for encoding and recall (e.g., `gemini`, `anthropic`, `openai`)  |
| `MODEL_API_KEY`        | `--model-api-key`        | —                                   | API key for the configured model provider                                            |
| `MODEL_API_BASE`       | `--model-api-base`       | `https://api.deepseek.com/anthropic` | Model API base URL                                                                  |
| `MODEL_NAME`           | `--model-name`           | `deepseek-v4-pro`                   | LLM model for agents                                                                 |
| `MODEL_CONTEXT_WINDOW` | `--model-context-window` | `400000`                            | Model context window size (tokens)                                                   |
| `MODEL_MAX_OUTPUT`     | `--model-max-output`     | `384000`                            | Model max output size (tokens)                                                       |
| `HTTPS_PROXY`          | `--https-proxy`          | —                                   | HTTPS proxy URL                                                                      |
| `SHARDING_IDX`         | `--sharding-idx`         | `0`                                 | Shard index for this instance                                                        |
| `MANAGERS`             | `--managers`             | —                                   | Comma-separated manager principal IDs                                                |
| `CORS_ORIGINS`         | `--cors-origins`         | —                                   | CORS allowed origins: empty = disabled, `*` = allow all, or comma-separated origins  |
| `MCP_HTTP_ENABLED`     | `--mcp-http-enabled`     | `true`                              | Mount Streamable HTTP MCP with the HTTP service                                      |
| `MCP_HTTP_PATH_PREFIX` | `--mcp-http-path-prefix` | `/mcp`                              | Remote MCP prefix; clients connect to `{prefix}/{space_id}`                          |
| `MCP_HTTP_ALLOWED_HOSTS` | `--mcp-http-allowed-hosts` | —                                | Comma-separated Host allowlist for remote MCP; use `*` only behind trusted controls  |
| `MCP_HTTP_ALLOWED_ORIGINS` | `--mcp-http-allowed-origins` | —                            | Comma-separated browser Origin allowlist for remote MCP                              |
| `MCP_HTTP_AUTO_CREATE_SPACE` | `--mcp-http-auto-create-space` | `false`                    | Create remote MCP spaces on first use after a valid `write` CWT                      |
| `MCP_HTTP_AUTO_CREATE_TIER` | `--mcp-http-auto-create-tier` | `1`                         | Tier used for remote MCP auto-created spaces                                         |
| `MCP_SPACE_ID`         | `mcp --space-id`         | —                                   | Space exposed by the MCP stdio server                                                |
| `MCP_AUTH_TOKEN`       | `mcp --mcp-auth-token`   | —                                   | CWT or space token used by MCP tools                                                 |
| `MCP_AUTO_CREATE_SPACE` | `mcp --mcp-auto-create-space` | `false`                       | Create the MCP space if it does not exist                                            |
| `MCP_AUTO_CREATE_TIER` | `mcp --mcp-auto-create-tier` | `1`                            | Tier used for MCP auto-created spaces                                                |

`CORS_ORIGINS` examples:
- `""` (empty): CORS disabled
- `"*"`: allow all origins
- `"https://app.example.com,https://admin.example.com"`: allow specific origins

### Storage Backends

| Subcommand | Description                     | Key Env Variables                                                        |
| ---------- | ------------------------------- | ------------------------------------------------------------------------ |
| *(none)*   | In-memory storage (dev/testing) | —                                                                        |
| `local`    | Local filesystem storage        | `LOCAL_DB_PATH` (default `./db`)                                         |
| `aws`      | AWS S3 storage                  | `AWS_BUCKET`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` |

## Running

```bash
# Development (in-memory storage)
cargo run -p anda_brain

# Local filesystem storage
cargo run -p anda_brain -- local --db ./data

# HTTP service also serves remote MCP at /mcp/{space_id}
MCP_HTTP_ALLOWED_HOSTS="brain.example.com" \
  cargo run -p anda_brain -- local --db ./data

# AWS S3 storage
cargo run -p anda_brain -- aws --bucket my-bucket --region us-east-1

# MCP stdio server for local MCP clients
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  cargo run -p anda_brain -- mcp --space-id my_space_001 local --db ./data
```

### Run with Docker image

```bash
# Pull image
docker pull ghcr.io/ldclabs/anda_brain_amd64:latest

# Run with ENV (in-memory by default)
docker run --rm -p 8042:8042 \
  -e LISTEN_ADDR=0.0.0.0:8042 \
  -e MODEL_API_KEY=your_key \
  ghcr.io/ldclabs/anda_brain_amd64:latest

# Override startup args (example: local storage)
docker run --rm -p 8042:8042 \
  -v $(pwd)/data:/data \
  ghcr.io/ldclabs/anda_brain_amd64:latest local --db /data

# Override startup args (example: AWS S3 storage)
docker run --rm -p 8042:8042 \
  -e AWS_ACCESS_KEY_ID=your_ak \
  -e AWS_SECRET_ACCESS_KEY=your_sk \
  ghcr.io/ldclabs/anda_brain_amd64:latest aws --bucket my-bucket --region us-east-1
```

## Dependencies

Key crates from the Anda ecosystem:

| Crate                  | Purpose                                                        |
| ---------------------- | -------------------------------------------------------------- |
| `anda_core`            | Core traits (`Agent`, `Tool`, `AgentContext`) and types        |
| `anda_engine`          | Agent engine, model integration, memory management             |
| `anda_db`              | Persistent database layer (`AndaDB`) with configurable storage |
| `anda_kip`             | KIP syntax parser and built-in knowledge templates             |
| `anda_cognitive_nexus` | Cognitive Nexus knowledge graph implementation                 |
| `object_store`         | Object store abstraction                                       |

## License

Copyright © LDC Labs

Licensed under the Apache License, Version 2.0.
