---
name: anda-brain
description: |
  Long-term memory service for LLM agents.
  Provides persistent, structured memory (Cognitive Nexus) through three operations:
  Formation (encode conversations into memory), Recall (query memory with natural language), and Maintenance (consolidate and prune memory).

  Use this service when:
  - You need to persist facts, preferences, relationships, or events across sessions
  - You want to recall previous conversations, decisions, or user context
  - You need structured long-term memory without understanding KIP syntax
  - You want to trigger memory consolidation or cleanup

  Common trigger phrases:
  - "remember this", "save this for later", "don't forget"
  - "what did I say last time?", "recall my preferences"
  - "what do we know about X?", "who is X?"
  - "run memory maintenance", "consolidate memory"
metadata:
  version: 0.2.0
  url: https://github.com/ldclabs/anda-brain/blob/main/skills/anda-brain/SKILL.md
  keywords:
    - long-term memory
    - agent memory
    - knowledge graph
    - cognitive nexus
    - memory formation
    - memory recall
    - memory maintenance
    - KIP
    - persistent memory
---

# 🧠 Anda Brain

Persistent long-term memory service for LLM agents, powered by a Knowledge Graph (Cognitive Nexus) and KIP (Knowledge Interaction Protocol). Anda Brain is [open-source software](https://github.com/ldclabs/anda-brain) designed to be **self-hosted** — deploy your own instance with the [Quick Start guide](https://github.com/ldclabs/anda-brain/blob/main/deploy/quick_start.md).

> **Note:** The hosted cloud service (`brain.anda.ai`) and its console (`anda.ai/brain`) have been discontinued. All examples below assume your own deployment.

For a complete, ready-to-run agent built on Anda Brain, see [Anda Bot](https://github.com/ldclabs/anda-bot).

Business agents interact entirely through **natural language** and a simple REST API — no KIP knowledge required.

```
Business Agent  ──natural language──▶  Brain  ──KIP──▶  Cognitive Nexus
 (your agent)                         (this service)          (knowledge graph)
```

---

## What You Get

Three operational modes cover the full memory lifecycle:

| Mode | Endpoint | Purpose | Auth |
|------|----------|---------|------|
| **Formation** | `POST /v1/{space_id}/formation` | Encode conversations into structured memory | `write` (CWT or space token) |
| **Recall** | `POST /v1/{space_id}/recall` | Query memory with natural language | `read` (CWT or space token) |
| **Maintenance** | `POST /v1/{space_id}/maintenance` | Trigger memory consolidation & pruning cycle | `write` (CWT or space token) |

Supporting endpoints:

| Method | Endpoint | Purpose | Auth |
|--------|----------|---------|------|
| `GET` | `/` | Anda Brain website | — |
| `GET` | `/info` | Service info (name, version, sharding) | — |
| `GET` | `/SKILL.md` | This skill description | — |
| `GET` | `/v1/{space_id}/info` | Space status and statistics | `read` (CWT or space token) |
| `GET` | `/v1/{space_id}/formation_status` | Formation progress (lightweight monitoring) | `read` (CWT or space token) |
| `POST` | `/v1/{space_id}/execute_kip_readonly` | Execute a read-only KIP request | `read` (CWT or space token) |
| `GET` | `/v1/{space_id}/conversations/{conversation_id}` | Get one conversation detail | `read` (CWT or space token) |
| `GET` | `/v1/{space_id}/conversations/{conversation_id}/delta` | Get incremental conversation updates | `read` (CWT or space token) |
| `GET` | `/v1/{space_id}/conversations` | List conversations (cursor pagination) | `read` (CWT or space token) |
| `GET` | `/v1/{space_id}/management/space_tokens` | List space tokens | `write` (CWT) |
| `POST` | `/v1/{space_id}/management/add_space_token` | Add a space token | `write` (CWT) |
| `POST` | `/v1/{space_id}/management/revoke_space_token` | Revoke a space token | `write` (CWT) |
| `PATCH` | `/v1/{space_id}/management/update_space` | Update space information (name, description, public/private) | `write` (CWT) |
| `PATCH` | `/v1/{space_id}/management/restart_formation` | Restart formation for a conversation (re-encode with updated model/config) | `write` (CWT) |
| `GET` | `/v1/{space_id}/management/space_byok` | Get BYOK configuration for the space | `write` (CWT) |
| `PATCH` | `/v1/{space_id}/management/space_byok` | Update BYOK configuration for the space | `write` (CWT) |
| `POST` | `/admin/{space_id}/update_space_tier` | Update a space tier | manager (CWT) |
| `POST` | `/admin/create_space` | Create a new memory space | manager (CWT) |
> Auth scopes in tables apply when authentication is enabled (`ED25519_PUBKEYS` is set).

---

## When to Use This Service

Use Anda Brain when your agent needs to:

- **Persist knowledge across sessions** — user preferences, facts, decisions, relationships, events
- **Recall previous context** — what happened before, what the user said, what decisions were made
- **Share memory across agents** — multiple agents can read/write to the same space
- **Maintain memory health** — consolidate old events, deduplicate facts, decay stale knowledge

The service handles all the complexity of knowledge graph management. Your agent just sends messages and asks questions in natural language.

## When NOT to Use

- Temporary conversation context that only matters in the current session
- Large file storage (use object storage instead)
- Real-time data streaming
- Secrets, passwords, or API keys (the service is not a vault)

---

## Concepts

### Memory Space

Each space is an isolated environment with its own knowledge graph, conversation history, and database. Spaces are identified by a `space_id` string.

### Memory Types

The Formation agent extracts three types of memory from conversations:

1. **Episodic Memory** (Events) — What happened, when, who participated, outcome
2. **Semantic Memory** (Stable Knowledge) — Facts, preferences, relationships, domain knowledge
3. **Cognitive Memory** (Patterns) — Behavioral patterns, decision criteria, communication style

### Cognitive Nexus

The underlying knowledge graph consists of:

- **Concept Nodes** — Entities with a type and name (e.g., `{type: "Person", name: "Alice"}`, `{type: "Preference", name: "dark_mode"}`)
- **Proposition Links** — Directed relationships between concepts (e.g., `(Alice, "prefers", dark_mode)`)

---

## Authentication

If `ED25519_PUBKEYS` is configured, protected endpoints require a Bearer token in the `Authorization` header.

If `ED25519_PUBKEYS` is empty/not provided, authentication is disabled and requests are accepted without signature verification.

```
Authorization: Bearer <base64_encoded_cose_sign1_token>
```

Management endpoints (`/v1/{space_id}/management/*`) and admin endpoints (`/admin/*`) still follow their role/scope checks when auth is enabled.

---

## API Reference

For complete endpoint and TypeScript schema details, see:
- `https://github.com/ldclabs/anda-brain/blob/main/anda_brain/API.md` (English)
- `https://github.com/ldclabs/anda-brain/blob/main/anda_brain/API_cn.md` (中文)

### Content Negotiation

The API supports triple serialization. Set `Content-Type` and `Accept` headers accordingly:

- `application/json` — JSON (default)
- `application/cbor` — CBOR (binary, more compact)
- `text/markdown` — Markdown (raw text or formatted Markdown)

All responses are wrapped in an RPC envelope when using JSON or CBOR:

```json
{
  "result": { ... },
  "error": null
}
```

When `Accept: text/markdown` is used, the response is returned as raw text or a Markdown formatted string.

On error:

```json
{
  "result": null,
  "error": {
    "message": "error description",
    "data": { ... }
  }
}
```

#### Markdown Serialization Sample

If `Accept: text/markdown` is specified, the `result` field's content will be directly serialized as the response body.

**Request:**
```http
POST /v1/my_space_001/recall
Accept: text/markdown

What are Alice's preferences?
```

**Response (HTTP 200):**
```markdown
Alice has the following known preferences:
- **Dark mode** in all applications (confidence: 0.9, since 2025-01-15)
- **Email communication** preferred over phone calls (confidence: 0.8, since 2025-01-10)

Alice is currently working on **Project Aurora** and was last seen on 2025-01-15 discussing settings preferences.

Gaps:
- No information found about Alice's language preferences.
```

---

### Create Space

Create a new isolated memory space.

```
POST /admin/create_space
Authorization: Bearer <token>
Content-Type: application/json
```

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
  "result": { ... }
}
```

---

### Formation — Encode Conversations into Memory

Send conversation messages to be analyzed and encoded into the knowledge graph. The service extracts facts, preferences, relationships, events, and patterns, then stores them as structured knowledge.

Processing is asynchronous — the endpoint returns immediately with a conversation ID while encoding continues in the background. New submissions are queued and processed sequentially.

```
POST /v1/{space_id}/formation
Authorization: Bearer <token>
Content-Type: application/json
```

**Request:**

```json
{
  "messages": [
    {
      "role": "user",
      "content": "I prefer dark mode for all my apps. My timezone is UTC+8.",
      "name": "Alice"
    },
    {
      "role": "assistant",
      "content": "Got it! I've noted your preference for dark mode and UTC+8 timezone."
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

**Response:**

```json
{
  "result": { "conversation": 1, ... }
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `messages` | `Message[]` | Yes | Conversation messages (role: `user` / `assistant` / `system`) |
| `context` | `InputContext` | No | Contextual metadata to help with encoding |
| `context.counterparty` | `string` | No | User identifier |
| `context.agent` | `string` | No | Calling agent identifier |
| `context.source` | `string` | No | Identifier of the source of the current interaction content |
| `context.topic` | `string` | No | Conversation topic |
| `timestamp` | `string` | No (recommended) | ISO 8601 timestamp of the conversation |

**Tips for best results:**

- Include the `context` field whenever possible — it helps the encoder associate knowledge correctly
- Send complete conversation segments, not individual messages
- Include timestamps to enable proper temporal reasoning
- The `name` field in messages helps distinguish between multiple users in the same conversation

---

### Recall — Query Memory

Ask a natural language question and receive a synthesized answer drawn from the knowledge graph and conversation history.

```
POST /v1/{space_id}/recall
Authorization: Bearer <token>
Content-Type: application/json
```

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

**Response:**

```json
{
  "result": {
    "content": "Alice prefers dark mode for all applications and operates in the UTC+8 timezone.",
    ...
  }
}
```

Note: `result.content` is the primary contract. Additional fields may vary by model/runtime.

**Query examples:**

| Intent | Example query |
|--------|--------------|
| Entity lookup | "Who is Alice?" |
| Relationship | "Who does Alice work with?" |
| Attribute | "What are Alice's preferences?" |
| Event recall | "What happened in our last meeting?" |
| Domain exploration | "What do we know about Project Aurora?" |
| Pattern detection | "Does Alice prefer email or chat?" |
| Existence check | "Have we discussed the pricing strategy?" |

---

### Space Status

Get statistics and health information for a memory space.

```
GET /v1/{space_id}/info
Authorization: Bearer <token>
```

**Response:**

```json
{
  "result": {
    "space_id": "my_space_001",
    "owner": "principal_id",
    "db_stats": {
      "total_items": 150,
      "total_bytes": 524288
    },
    "concepts": 85,
    "propositions": 120,
    "conversations": 12,
    ...
  }
}
```

---

### Wiki — Versioned Reference Documents with Citations

The wiki is the space's reference memory: policies, manuals, SOPs, API docs and FAQs stored as immutable Markdown commits (git-like), retrieved by BM25 keyword search, and quoted through verifiable `wiki://` citations. Search is deterministic and LLM-free; compose answers yourself and cite the URIs.

**Commit (create or update):**

```
POST /v1/{space_id}/wiki/docs
Authorization: Bearer <token with write scope>
```

```json
{
  "title": "Deployment Guide",
  "content": "# Deployment Guide\n\n## Rollback\n\nUse the previous snapshot...",
  "namespace": "engineering",
  "tags": ["sop"],
  "message": "initial import"
}
```

- Create: omit `doc_id`. Update: pass `doc_id` **and** `parent_version` (the `current_version` you read). A stale `parent_version` returns `409` with the current version in `error.data` — re-read, merge, retry.
- Committing identical content is a no-op (`"idempotent": true`); safe to retry and re-import.
- Content is whole-document Markdown (not a diff), at most 1 MiB after normalization (`413` beyond).

**Search with citations:**

```
POST /v1/{space_id}/wiki/search
```

```json
{ "query": "rollback checksum", "namespaces": ["engineering"], "top_k": 8, "mode": "chunks" }
```

Each hit carries the matching text and a citation: `{ "uri": "wiki://{space}/{doc_id}@{version_id}#{start}-{end}", "checksum": "sha3-256:...", "anchor": "...", "quote": "..." }`. `mode: "docs"` returns one best hit per document. BM25 favors exact terms (product names, error codes); reformulate keywords rather than sending full sentences.

**Read progressively:**

```
GET /v1/{space_id}/wiki/docs/{doc_id}                      → metadata + table of contents
GET /v1/{space_id}/wiki/docs/{doc_id}/content?anchor=...   → one section
GET /v1/{space_id}/wiki/docs/{doc_id}/content?start=&end=  → byte range
GET /v1/{space_id}/wiki/docs/{doc_id}/content              → full text (bounded)
GET /v1/{space_id}/wiki/docs/{doc_id}/content?version=...  → historical version
```

Prefer TOC → section over full reads for long documents.

**Manage and audit:**

```
GET  /v1/{space_id}/wiki/docs?namespace=&tag=&status=&cursor=&limit=
GET  /v1/{space_id}/wiki/docs/{doc_id}/versions
POST /v1/{space_id}/wiki/docs/{doc_id}/archive     (hidden from search, still readable)
POST /v1/{space_id}/wiki/docs/{doc_id}/restore
POST /v1/{space_id}/wiki/verify                    {"uri": "wiki://...", "checksum": "sha3-256:..."}
GET  /v1/{space_id}/wiki/events?kind=&doc_id=
```

`verify` answers `valid`, `superseded` (a newer version exists — it names it), or `invalid`. Versions are immutable, so citations never rot.

---

## Integration Pattern

A typical integration workflow for a business agent (replace `your-brain-host` with your deployment address, e.g. `localhost:8042`):

### 1. Remember: Send conversations for memory encoding

After each meaningful conversation with a user, send the messages to Formation:

```bash
curl -sX POST https://your-brain-host/v1/my_space_001/formation \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "messages": [
      {"role": "user", "content": "I work at Acme Corp as a senior engineer."},
      {"role": "assistant", "content": "Nice to meet you! Noted that you are a senior engineer at Acme Corp."}
    ],
    "context": {"counterparty": "user_123", "agent": "onboarding_bot"},
    "timestamp": "2026-03-09T10:30:00Z"
  }'
```

### 2. Recall: Query memory before responding

Before generating a response, check if relevant memory exists:

```bash
curl -sX POST https://your-brain-host/v1/my_space_001/recall \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "query": "Where does this user work and what is their role?",
    "context": {"counterparty": "user_123"}
  }'
```

---

## MCP Integration

If your agent has an MCP client, connect to the HTTP service's Streamable HTTP MCP endpoint:

```text
https://your-brain-host/mcp/my_space_001
```

Use the same `spaceId` and `spaceToken` you would use for REST. Pass the token as `Authorization: Bearer <token>`. This is the preferred setup for company or team deployments where each employee's agent is assigned a dedicated Brain space.

For local desktop or development clients, run Anda Brain as a stdio MCP server and register it with that client:

```bash
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  anda_brain mcp --space-id my_space_001 local --db ./data
```

Core MCP tools:

| Tool | Purpose |
|------|---------|
| `anda_brain_remember_conversation` | Encode conversation messages into long-term memory |
| `anda_brain_recall_memory` | Query memory with natural language |
| `anda_brain_run_maintenance` | Trigger consolidation and pruning |
| `anda_brain_get_space_info` | Inspect space statistics and metadata |
| `anda_brain_get_formation_status` | Check formation/maintenance progress |
| `anda_brain_execute_kip_readonly` | Run read-only KIP for advanced graph inspection |

If `ED25519_PUBKEYS` is empty, local MCP development can omit tokens. Add `--mcp-auto-create-space` for stdio development or `MCP_HTTP_AUTO_CREATE_SPACE=true` for remote development when the target space does not exist yet; remote auto-create requires `ED25519_PUBKEYS` plus a CWT with `write` scope for the target space before creating the missing space. Set `MCP_HTTP_ALLOWED_HOSTS` when remote MCP is exposed behind a company domain or reverse proxy.

---

## OpenClaw Integration

The [`anda-brain`](https://github.com/ldclabs/anda-brain/tree/main/anda-brain-openclaw) plugin integrates Anda Brain into [OpenClaw](https://openclaw.ai/) agents, providing automatic memory encoding and a `recall_memory` tool — no manual API calls needed.

### Prerequisites: Deploy Anda Brain and Create a Space

Before installing the plugin, you need a running Anda Brain deployment plus a `spaceId` and `spaceToken`:

1. **Deploy Anda Brain** — see the [Quick Start guide](https://github.com/ldclabs/anda-brain/blob/main/deploy/quick_start.md) (binary or Docker, a few minutes).
2. **Create a brain space** via `POST /admin/create_space` — the `space_id` you choose is your `spaceId`.
3. **Create an API key** via `POST /v1/{space_id}/management/add_space_token` — the returned token is your `spaceToken`.

### Install

1. Install the plugin package:
```bash
openclaw plugins install anda-brain
```

2. Update anda-brain configuration in `openclaw.json` with the `spaceId` and `spaceToken` obtained from the console:
```json
{
  "plugins": {
    "entries": {
      "anda-brain": {
        "enabled": true,
        "config": {
          "spaceId": "my_space_001",
          "spaceToken": "STxxxxx",
          "baseUrl": "http://localhost:8042" // your Anda Brain deployment URL
        }
      }
    }
  }
}
```

3. Restart OpenClaw Gateway.
```sh
openclaw gateway restart
```

Required fields:

- `spaceId`: your Brain space ID (created via `POST /admin/create_space`)
- `spaceToken`: your space API Key (created via `POST /v1/{space_id}/management/add_space_token`)
- `baseUrl`: your Anda Brain deployment URL (e.g. `http://localhost:8042`) — always set this; the legacy default `https://brain.anda.ai` has been discontinued

### What It Does

| Feature | Mechanism | Description |
|---------|-----------|-------------|
| **Memory encoding** | `agent_end` hook | After each agent turn, conversation messages are automatically sent to `POST /v1/{space_id}/formation` (fire-and-forget). |
| **Memory recall** | `recall_memory` tool | Registered as an agent tool; the LLM can call it with a natural language query to retrieve knowledge via `POST /v1/{space_id}/recall`. |

### Configuration Options

| Option | Type | Required | Default | Description |
|--------|------|----------|---------|-------------|
| `spaceId` | `string` | Yes | — | Memory space ID |
| `spaceToken` | `string` | Yes | — | Space token for API authentication |
| `baseUrl` | `string` | Yes (in practice) | `https://brain.anda.ai` (discontinued) | Your Anda Brain deployment URL — always set this |
| `defaultContext` | `InputContext` | No | — | Default context included with every request (`counterparty`, `agent`, `source`, `topic`) |
| `formationTimeoutMs` | `number` | No | `30000` | Formation request timeout (ms) |
| `recallTimeoutMs` | `number` | No | `120000` | Recall request timeout (ms) — recall may take 10–100s |

### `recall_memory` Tool Parameters

The plugin registers a `recall_memory` tool that the LLM can invoke:

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | `string` | Yes | Natural language question (e.g. "What are Alice's preferences?") |
| `context.counterparty` | `string` | No | Current user identifier |
| `context.agent` | `string` | No | Calling agent identifier |
| `context.topic` | `string` | No | Topic hint for disambiguation |

---

## Anda Bot

[**Anda Bot**](https://github.com/ldclabs/anda-bot) is a complete, open-source AI agent built on Anda Brain, using Brain as its long-term memory and cognitive backbone. Use it directly, or as a reference implementation for integrating Anda Brain into your own agent.

---

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `401 Unauthorized` | If auth is enabled (`ED25519_PUBKEYS` set), check Bearer token signature, `aud` (space ID), and required `scope` (`read`/`write`) |
| `404 Not Found` on space endpoints | Verify the `space_id` exists and the token `aud` matches the target space |
| Formation returns but nothing in memory | Formation is async — check space status after a few seconds; look at the conversation status |
| Recall seems empty or insufficient | Memory may not be encoded yet, or the query is too narrow; try broader phrasing and include `context` |
| Maintenance rejected | Only one maintenance cycle can run at a time per space; wait for the current one to finish |
| Empty recall for new space | Expected — a new space has no memory yet; send conversations via Formation first |

---

## Configuration Reference

The service is configured via CLI arguments and environment variables:

| Env Variable | Default | Description |
|--------------|---------|-------------|
| `LISTEN_ADDR` | `127.0.0.1:8042` | Listen address |
| `ED25519_PUBKEYS` | — | Comma-separated Base64-encoded Ed25519 public keys; if empty, API authentication is disabled |
| `MODEL_FAMILY` | `anthropic` | Model family to use for encoding and recall (e.g., `gemini`, `anthropic`, `openai`) |
| `MODEL_API_KEY` | — | API key for the configured model provider |
| `MODEL_API_BASE` | `https://api.deepseek.com/anthropic` | Model API base URL |
| `MODEL_NAME` | `deepseek-v4-pro` | LLM model for agents |
| `HTTPS_PROXY` | — | HTTPS proxy URL |
| `SHARDING_IDX` | `0` | Shard index for this instance |
| `MANAGERS` | — | Comma-separated manager principal IDs |
| `CORS_ORIGINS` | — | CORS allowed origins: empty = disabled, `*` = allow all, or comma-separated origins |
| `MCP_HTTP_ENABLED` | `true` | Mount Streamable HTTP MCP with the HTTP service |
| `MCP_HTTP_PATH_PREFIX` | `/mcp` | Remote MCP prefix; clients connect to `{prefix}/{space_id}` |
| `MCP_HTTP_ALLOWED_HOSTS` | — | Comma-separated Host allowlist for remote MCP |
| `MCP_HTTP_ALLOWED_ORIGINS` | — | Comma-separated browser Origin allowlist for remote MCP |
| `MCP_HTTP_AUTO_CREATE_SPACE` | `false` | Create remote MCP spaces on first use after a valid `write` CWT |
| `MCP_HTTP_AUTO_CREATE_TIER` | `1` | Tier used for remote MCP auto-created spaces |
| `MCP_SPACE_ID` | — | Space exposed by the MCP stdio server |
| `MCP_AUTH_TOKEN` | — | CWT or space token used by MCP tools |
| `MCP_AUTO_CREATE_SPACE` | `false` | Create the MCP space if it does not exist |
| `MCP_AUTO_CREATE_TIER` | `1` | Tier used for MCP auto-created spaces |

**Storage backends:**

| Backend | Command | Key Config |
|---------|---------|------------|
| In-memory (dev) | `cargo run -p anda_brain` | — |
| Local filesystem | `cargo run -p anda_brain -- local` | `LOCAL_DB_PATH` (default `./db`) |
| AWS S3 | `cargo run -p anda_brain -- aws` | `AWS_BUCKET`, `AWS_REGION` |
| MCP HTTP | `cargo run -p anda_brain -- local` then connect `/mcp/{space_id}` | `MCP_HTTP_ALLOWED_HOSTS`, bearer token |
| MCP stdio | `cargo run -p anda_brain -- mcp --space-id my_space_001 local` | `MCP_AUTH_TOKEN`, `LOCAL_DB_PATH` |
