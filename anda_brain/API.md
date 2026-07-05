# Anda Brain API Documentation (with TypeScript Types)

## 1) Common Conventions

- Base URL: `http://{host}:{port}`
- Auth header: `Authorization: Bearer <token>`
- If `ED25519_PUBKEYS` is empty/not provided, authentication is disabled.
- Supported serialization formats:
  - Request: `Content-Type: application/json | application/cbor | text/markdown`
  - Response: `Accept: application/json | application/cbor | text/markdown`
- Most business endpoints return an RPC envelope: `RpcResponse<T>`
- MCP clients can use the built-in Streamable HTTP endpoint: `/mcp/<space_id>`, or the local stdio server: `anda_brain mcp --space-id <space_id> [local|aws]`

---

## 2) TypeScript Type Definitions

```ts
export type TokenScope = 'read' | 'write' | '*';

export interface RpcError {
  message: string;
  data?: unknown;
}

export interface RpcResponse<T> {
  result?: T;
  error?: RpcError;
  next_cursor?: string;
}

export interface InputContext {
  counterparty?: string;
  agent?: string;
  source?: string;
  topic?: string;
}

export type MessageRole = 'system' | 'user' | 'assistant' | 'tool';

export type MessageContentPart =
  | string
  | {
      type: string;
      text?: string;
      [k: string]: unknown;
    };

export interface Message {
  role: MessageRole;
  content: string | MessageContentPart[];
  name?: string;  // user or tool name
  user?: string;  // user ID
  timestamp?: number; // Unix timestamp in milliseconds
}

export interface FormationInput {
  messages: Message[];
  context?: InputContext;
  timestamp: string; // ISO 8601
}

export interface RecallInput {
  query: string;
  context?: InputContext;
}

export interface MaintenanceParameters {
  stale_event_threshold_days?: number;
  confidence_decay_factor?: number;
  unsorted_max_backlog?: number;
  orphan_max_count?: number;
}

export interface MaintenanceInput {
  trigger?: 'scheduled' | 'threshold' | 'on_demand';
  scope?: 'full' | 'quick' | 'daydream'; // defaults to 'daydream'
  timestamp?: string; // ISO 8601
  parameters?: MaintenanceParameters;
}

export interface AddSpaceTokenInput {
  scope: TokenScope;
  name: string;
  expires_at?: number; // Unix timestamp in milliseconds
  labels?: string[]; // wiki ACL labels; omitted = unrestricted
}

export interface RevokeSpaceTokenInput {
  token: string;
}

export interface UpdateSpaceInput {
  name?: string;
  description?: string;
  public?: boolean;
  wiki_digest?: boolean; // enable WikiDigest graph extraction (default false)
  wiki_audit_reads?: boolean; // event external wiki reads (default false)
  wiki_acl_defaults?: Record<string, string>; // namespace -> default ACL label
}

export interface FormationRestartInput {
  conversation: number;
}

export interface CreateOrUpdateSpaceInput {
  user: string;
  space_id: string;
  tier: number;
}

export interface GetOrInitUserInput {
  user: string;
  name?: string;
}

// ── Wiki: versioned reference documents with verifiable citations ──────────

export type WikiDocStatus = 'active' | 'archived';
export type WikiSearchMode = 'chunks' | 'docs';

export interface WikiCommitInput {
  doc_id?: number; // omit to create a new document
  parent_version?: number; // required on update (CAS); stale value -> 409
  namespace?: string; // default "default"
  slug?: string; // display slug; derived from title when omitted
  title: string;
  content: string; // full Markdown document (not a diff); <= 1 MiB normalized
  tags?: string[]; // omit to keep stored tags on update
  acl_label?: string; // omit to keep/inherit namespace default; "" clears
  source_uri?: string; // omit to keep
  message?: string; // commit message
  metadata?: Record<string, unknown>; // omit to keep
}

export interface WikiDocInfo {
  id: number;
  namespace: string;
  slug: string;
  title: string;
  status: WikiDocStatus;
  current_version: number;
  current_checksum: string; // "sha3-256:..."
  tags: string[];
  acl_label?: string;
  source_uri?: string;
  metadata?: Record<string, unknown>;
  created_by: string;
  updated_by: string;
  created_at: number;
  updated_at: number;
}

export interface WikiVersionInfo {
  id: number;
  doc_id: number;
  parent_version?: number;
  checksum: string;
  size: number;
  author: string;
  message?: string;
  created_at: number;
}

export interface WikiCommitOutput {
  doc: WikiDocInfo;
  version: WikiVersionInfo;
  chunks: number;
  created: boolean;
  idempotent: boolean; // true when nothing changed (no new version written)
}

export interface WikiSearchInput {
  query: string; // BM25 keywords: exact terms, product names, error codes
  namespaces?: string[];
  doc_ids?: number[];
  tags?: string[];
  top_k?: number; // 1-50, default 8
  mode?: WikiSearchMode; // 'docs' = one best hit per document
  expand?: number; // 0-2 neighbor expansion; citations widen accordingly
}

export interface WikiCitation {
  uri: string; // wiki://{space}/{doc_id}@{version_id}#{start}-{end}
  doc_id: number;
  version_id: number;
  chunk_id: number;
  heading_path: string[];
  anchor: string; // stable section anchor for wiki_read
  byte_range: [number, number];
  checksum: string; // verifiable via /wiki/verify
  quote: string;
}

export interface WikiHit {
  text: string;
  doc_title: string;
  heading_path: string[];
  score?: number;
  citation: WikiCitation;
}

export interface WikiSearchOutput {
  hits: WikiHit[];
  total_docs_matched: number;
}

export type WikiSelector =
  | { type: 'toc' }
  | { type: 'section'; anchor: string }
  | { type: 'range'; start: number; end: number }
  | { type: 'full' };

export interface WikiReadInput {
  doc_id: number;
  version?: number; // time-travel read of a historical version
  selector?: WikiSelector; // default { type: 'full' }
}

export interface WikiTocEntry {
  anchor: string;
  heading_path: string[];
  byte_start: number;
  byte_end: number;
}

export interface WikiReadOutput {
  doc_id: number;
  version_id: number;
  is_current: boolean;
  title: string;
  status: WikiDocStatus;
  checksum: string;
  size: number;
  toc?: WikiTocEntry[]; // for the 'toc' selector
  content?: string; // for section/range/full selectors
  byte_range?: [number, number];
  truncated: boolean; // full reads are bounded (256 KiB)
}

export interface WikiVerifyInput {
  uri?: string; // wiki:// citation URI, or pass the explicit fields below
  doc_id?: number;
  version_id?: number;
  byte_range?: [number, number];
  checksum?: string; // compared against the recomputed checksum when present
}

export type WikiVerifyStatus = 'valid' | 'superseded' | 'invalid' | 'not_found';

export interface WikiVerifyOutput {
  status: WikiVerifyStatus; // 'superseded' = intact but a newer version exists
  current_version?: number;
  checksum?: string; // recomputed from immutable content
  quote?: string;
}

export interface WikiBundleEntry {
  path: string; // bundle-relative path, e.g. "guides/setup.md"
  content: string;
}

export interface WikiImportInput {
  entries: WikiBundleEntry[]; // OKF v0.1 bundle files (Markdown + YAML frontmatter)
  namespace?: string; // default "default"; bundles round-trip per namespace
}

export type WikiImportStatus = 'created' | 'updated' | 'unchanged';

export interface WikiImportOutput {
  created: number;
  updated: number;
  unchanged: number; // checksum-idempotent: re-imports never grow versions
  docs: { path: string; doc_id: number; version_id: number; status: WikiImportStatus }[];
  skipped?: { path: string; reason: string }[];
}

export interface WikiExportOutput {
  namespace: string;
  entries: WikiBundleEntry[]; // concept .md files + index.md + manifest.json
  docs: number;
}

export interface WikiEventInfo {
  id: number;
  // DocCreated | VersionCommitted | DocArchived | DocRestored | OrphanSwept
  // | CitationVerifyFailed | ImportCompleted | ExportCompleted
  // | DigestExtracted | WikiQueried | WikiRead | StaleReport | EventsPruned
  kind: string;
  doc_id?: number;
  version_id?: number;
  actor: string;
  detail?: Record<string, unknown>;
  created_at: number;
}

export interface WikiDigestReport {
  digested: number; // versions distilled into the Cognitive Nexus
  facts: number; // propositions written (with wiki:// citation metadata)
  superseded: number; // stale propositions marked superseded
  skipped: number;
  citations_checked: number; // post-run citation sample
  citations_invalid: number;
  usage: Usage;
}

export interface McpServerConfig {
  space_id: string;
  auth_token?: string;
  auto_create_space?: boolean;
  auto_create_tier?: number;
}

export interface McpHttpServerConfig {
  path_prefix?: string; // default "/mcp"; clients connect to {path_prefix}/{space_id}
  allowed_hosts?: string[]; // default loopback-only in rmcp; set company domains explicitly
  allowed_origins?: string[]; // for browser-based MCP clients
  auto_create_space?: boolean;
  auto_create_tier?: number;
}

export interface Concept {
  id?: string;
  type?: string;
  name?: string;
  attributes?: Record<string, unknown>;
  metadata?: Record<string, unknown>;
}

export interface ModelConfig {
  family: string; // "gemini", "anthropic", "openai", "deepseek", "mimo" etc.
  model: string;
  api_base: string;
  api_key: string;
  disabled: boolean;
  label?: string;
  bearer_auth?: boolean;
  stream?: boolean;
  context_window?: number;
  max_output?: number;
}

export interface SpaceTier {
  tier: number;
  updated_at: number; // Unix timestamp in milliseconds
}

export interface SpaceToken {
  token: string;
  name: string;
  scope: TokenScope;
  usage: number;
  created_at: number; // Unix timestamp in milliseconds
  updated_at: number; // Unix timestamp in milliseconds
  expires_at?: number; // Unix timestamp in milliseconds
  labels?: string[]; // wiki ACL labels: token sees unlabeled content plus these
}

export interface StorageStats {
  [k: string]: number | string | boolean | null;
}

export interface SpaceInfo {
  id: string;
  name?: string;
  description?: string;
  owner: string;
  db_stats: StorageStats;
  concepts: number;
  propositions: number;
  conversations: number;
  public: boolean;
  tier: SpaceTier;
  formation_usage: Usage;
  recall_usage: Usage;
  maintenance_usage: Usage;
  formation_processed_id: number;
  maintenance_processed_id: number;
  maintenance_at: MaintenanceAt;
  wiki_docs: number;
  wiki_chunks: number;
  wiki_versions: number;
  wiki_queries: number;
  wiki_digested: number; // digest high-water mark (version id)
  wiki_stale_docs: number; // from the last housekeeping stale scan
}

export interface FormationStatus {
  id: string;
  concepts: number;
  propositions: number;
  conversations: number;
  formation_processing: boolean;
  maintenance_processing: boolean;
  formation_processed_id: number;
  maintenance_processed_id: number;
  maintenance_at: MaintenanceAt;
}

export interface MaintenanceAt {
  daydream: number;
  full: number;
  quick: number;
  /** Start time of the latest maintenance task in unix milliseconds, 0 if none started. */
  start_at: number;
}

export interface Usage {
  input_tokens?: number;
  output_tokens?: number;
  total_tokens?: number;
}

export interface AgentOutput {
  content: string;
  conversation?: number;
  failed_reason?: string;
  usage?: Usage;
  model?: string;
  [k: string]: unknown;
}

export type ConversationStatus =
  | 'submitted'
  | 'working'
  | 'idle'
  | 'completed'
  | 'failed'
  | 'cancelled';

export interface Conversation {
  _id: number;
  user: string;
  thread?: string;
  label?: string;
  messages: Message[];
  resources: unknown[];
  artifacts: unknown[];
  status: ConversationStatus;
  failed_reason?: string | null;
  period: number;
  created_at: number;
  updated_at: number;
  usage: Usage;
  steering_messages?: string[];
  follow_up_messages?: string[];
  ancestors?: number[];
}

export interface ConversationDelta {
  _id: number;
  messages: unknown[];
  artifacts: unknown[];
  status: ConversationStatus;
  usage: Usage;
  failed_reason?: string | null;
  updated_at: number;
  child?: number | null;
}

export interface ServiceInfo {
  name: string;
  version: string;
  sharding: number;
  description: string;
}

export type KipCommandItem = string | { command: string; parameters: Record<string, unknown> };

export interface KipRequest {
  commands: KipCommandItem[];
  parameters?: Record<string, unknown>;
  dry_run?: boolean; // if true, the request will be parsed and validated but not executed (no side effects)
}

export interface KipError {
  code: string;
  message: string;
  hint?: string;
  data?: unknown;
}

export interface KipResponse<T> {
  result?: T;
  error?: KipError;
  next_cursor?: string;
}
```

---

## 3) MCP Server

When the HTTP service starts, Anda Brain exposes a Streamable HTTP MCP endpoint for MCP-capable agents:

```text
https://your-brain-host/mcp/my_space_001
```

Clients select the target memory space from the URL path and pass the same CWT or space token used by REST as `Authorization: Bearer <token>`. This is the recommended mode for internal multi-user agent platforms where each employee receives a dedicated Brain space.

Anda Brain can also run as a local MCP stdio server:

```bash
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  anda_brain mcp --space-id my_space_001 local --db ./data
```

Both MCP modes use the same model, auth, and storage configuration as the HTTP service. For stdio, the nested storage subcommand is optional; omit it for in-memory development, use `local --db ./data` for local persistence, or use `aws --bucket ... --region ...` for S3.

| Tool | Input | Output | Scope |
| ---- | ----- | ------ | ----- |
| `anda_brain_remember_conversation` | `FormationInput` shape (`messages`, `context`, `timestamp`) | `AgentOutput` | `write` |
| `anda_brain_recall_memory` | `RecallInput` shape (`query`, `context`) | `AgentOutput` | `read` |
| `anda_brain_run_maintenance` | `MaintenanceInput` shape | `AgentOutput` | `write` |
| `anda_brain_get_space_info` | none | `SpaceInfo` | `read` |
| `anda_brain_get_formation_status` | none | `FormationStatus` | `read` |
| `anda_brain_execute_kip_readonly` | `{ command?, commands?, parameters?, dry_run? }` | `KipResponse` | `read` |
| `anda_brain_get_or_init_user` | `{ user, name? }` | `Concept` | `write` |
| `anda_brain_list_conversations` | `{ collection?, cursor?, limit? }` | `{ conversations, next_cursor }` | `read` |
| `anda_brain_get_conversation` | `{ conversation_id, collection?, delta?, messages_offset?, artifacts_offset? }` | `Conversation` or `ConversationDelta` | `read` |

When `ED25519_PUBKEYS` is set, configure the remote MCP client with an `Authorization` bearer token, or configure stdio with `MCP_AUTH_TOKEN` / `--mcp-auth-token`. `read` tools can also access public spaces without a token. For remote MCP behind a company domain or reverse proxy, set `MCP_HTTP_ALLOWED_HOSTS` to the accepted Host values. Use `--mcp-auto-create-space` for local stdio development or `MCP_HTTP_AUTO_CREATE_SPACE=true` for remote development if the target space does not exist yet; remote auto-create requires `ED25519_PUBKEYS` plus a CWT with `write` scope for the target space before the missing space is created.

---

## 4) Endpoint List

## 4.1 Public Endpoints

### GET `/`

- Description: Returns the product website (HTML or Markdown).
- Auth: None
- Response: `text/html` or `text/markdown`

### GET `/info`

- Description: Service information
- Auth: None
- Response (JSON): `ServiceInfo`

### GET `/SKILL.md`

- Description: Returns the skill description in Markdown
- Auth: None
- Response: `text/markdown`

---

## 4.2 Space Business Endpoints (`/v1/{space_id}`)

### POST `/v1/{space_id}/formation`

- Purpose: Submit a memory formation task
- Auth: SpaceToken/CWT `write`
- Request body: `FormationInput` (raw string is also accepted in Markdown mode)
- Response (JSON/CBOR): `RpcResponse<AgentOutput>`
- Response (Markdown): `string` (returns only `AgentOutput.content`)

### POST `/v1/{space_id}/recall`

- Purpose: Recall memory via natural-language query
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Request body: `RecallInput` (raw string is also accepted in Markdown mode)
- Response: `RpcResponse<AgentOutput>`

### POST `/v1/{space_id}/maintenance`

- Purpose: Trigger maintenance (sleep/consolidation)
- Auth: SpaceToken/CWT `write`
- Request body: `MaintenanceInput`
- Response: `RpcResponse<AgentOutput>`

### POST `/v1/{space_id}/execute_kip_readonly`

- Purpose: Execute a KIP request (read-only mode, suitable for queries)
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Request body: `KipRequest`
- Response: `KipResponse<T>` (returns different result types based on the commands)

### POST `/v1/{space_id}/get_or_init_user`

- Purpose: Get or initialize a user concept node for the given principal
- Auth: SpaceToken/CWT `write`
- Request body: `GetOrInitUserInput`
- Response: `RpcResponse<Concept>`

### GET `/v1/{space_id}/info`

- Purpose: Get space status and statistics
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Response: `RpcResponse<SpaceInfo>`

### GET `/v1/{space_id}/formation_status`

- Purpose: Get formation status
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Response: `RpcResponse<FormationStatus>`

### GET `/v1/{space_id}/conversations/{conversation_id}?collection=<collection>`

- Purpose: Get a single conversation detail
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Query:
  - `collection?: string` // use "recall" to distinguish recall vs memory conversations
- Response: `RpcResponse<Conversation>`

### GET `/v1/{space_id}/conversations/{conversation_id}/delta?collection=<collection>&messages_offset=<n>&artifacts_offset=<n>`

- Purpose: Get incremental conversation updates after client-side offsets
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Query:
  - `collection?: string` // use "recall" or "maintenance" to distinguish non-default conversation collections
  - `messages_offset?: number` // returns only messages after this offset, defaults to `0`
  - `artifacts_offset?: number` // returns only artifacts after this offset, defaults to `0`
- Response: `RpcResponse<ConversationDelta>`

### GET `/v1/{space_id}/conversations?collection=<collection>&cursor=<cursor>&limit=<n>`

- Purpose: List conversations with pagination
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Query:
  - `collection?: string` // use "recall" to distinguish recall vs memory conversations
  - `cursor?: string`
  - `limit?: number`
- Response: `RpcResponse<Conversation[]>` (next page cursor is returned via `next_cursor`)

---

## 4.3 Wiki Endpoints (`/v1/{space_id}/wiki`)

The wiki is the space's versioned reference memory (policies, manuals, SOPs, API docs). Writes are git-like immutable commits with CAS concurrency control; searches return verifiable `wiki://` citations. ACL: documents may carry an `acl_label`; space tokens with `labels` see unlabeled content plus their granted labels — enforced inside the retrieval query itself. Anonymous readers of public spaces see unlabeled content only; denials surface as 404.

Wiki-specific error semantics: `409` commit conflict (`error.data.current_version` carries the version to rebase on), `413` content over 1 MiB, `404` not found / ACL-denied.

### POST `/v1/{space_id}/wiki/docs`

- Purpose: Commit a document (create, or CAS update with `doc_id` + `parent_version`); identical content is a no-op
- Auth: SpaceToken/CWT `write`
- Request body: `WikiCommitInput` (raw Markdown string is also accepted; the title derives from the first heading)
- Response: `RpcResponse<WikiCommitOutput>`

### GET `/v1/{space_id}/wiki/docs?namespace=<ns>&status=<status>&tag=<tag>&cursor=<cursor>&limit=<n>`

- Purpose: List documents with pagination
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<WikiDocInfo[]>` (next page cursor via `next_cursor`)

### GET `/v1/{space_id}/wiki/docs/{doc_id}`

- Purpose: Document metadata plus table of contents
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<{ doc: WikiDocInfo; toc: WikiTocEntry[] }>`

### GET `/v1/{space_id}/wiki/docs/{doc_id}/content?version=<id>&anchor=<anchor>&start=<n>&end=<n>`

- Purpose: Progressive reading — `anchor` reads one section, `start`+`end` a byte range, neither reads the bounded full text; `version` time-travels
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<WikiReadOutput>`

### GET `/v1/{space_id}/wiki/docs/{doc_id}/versions?cursor=<cursor>&limit=<n>`

- Purpose: Version history (immutable commit chain)
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<WikiVersionInfo[]>` (next page cursor via `next_cursor`)

### POST `/v1/{space_id}/wiki/docs/{doc_id}/archive`

- Purpose: Archive a document (hidden from search, still readable by id, restorable)
- Auth: SpaceToken/CWT `write`
- Response: `RpcResponse<WikiDocInfo>`

### POST `/v1/{space_id}/wiki/docs/{doc_id}/restore`

- Purpose: Restore an archived document into search
- Auth: SpaceToken/CWT `write`
- Response: `RpcResponse<WikiDocInfo>`

### POST `/v1/{space_id}/wiki/search`

- Purpose: BM25 keyword retrieval over document passages, returning snippets with verifiable citations
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Request body: `WikiSearchInput` (raw query string is also accepted)
- Response: `RpcResponse<WikiSearchOutput>`

### POST `/v1/{space_id}/wiki/verify`

- Purpose: Verify a citation against immutable stored content
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Request body: `WikiVerifyInput` (raw `wiki://` URI string is also accepted)
- Response: `RpcResponse<WikiVerifyOutput>`

### GET `/v1/{space_id}/wiki/events?kind=<kind>&doc_id=<id>&cursor=<cursor>&limit=<n>`

- Purpose: Query the append-only audit log (writes, imports, digests; reads too when `wiki_audit_reads` is enabled)
- Auth: SpaceToken/CWT `read`; tokens restricted by ACL labels are rejected with `403`
- Response: `RpcResponse<WikiEventInfo[]>` (next page cursor via `next_cursor`)

### POST `/v1/{space_id}/wiki/import`

- Purpose: Import an OKF v0.1 bundle; checksum-idempotent (re-imports never grow version chains); unknown frontmatter keys survive round-trips verbatim
- Auth: SpaceToken/CWT `*` (full scope)
- Request body: `WikiImportInput`
- Response: `RpcResponse<WikiImportOutput>`

### GET `/v1/{space_id}/wiki/export?namespace=<ns>`

- Purpose: Export one namespace as an OKF bundle (concept `.md` files + `index.md` + `manifest.json` with checksums); replayable into an empty space
- Auth: SpaceToken/CWT `*` (full scope)
- Response: `RpcResponse<WikiExportOutput>`

### POST `/v1/{space_id}/wiki/digest`

- Purpose: Distill pending wiki versions into the Cognitive Nexus as propositions with `wiki://` citation metadata; supersedes facts the newest version no longer asserts (requires `update_space {"wiki_digest": true}`)
- Auth: SpaceToken/CWT `write`
- Response: `RpcResponse<WikiDigestReport>`

---

## 4.4 Space Management Endpoints (`/v1/{space_id}/management`)

### GET `/v1/{space_id}/management/space_tokens`

- Purpose: List Space Tokens
- Auth: Must pass CWT `write` (user management-level auth; raw token values are secret material)
- Response: `RpcResponse<SpaceToken[]>`

### POST `/v1/{space_id}/management/add_space_token`

- Purpose: Add a Space Token
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `AddSpaceTokenInput`
- Response: `RpcResponse<SpaceToken>` (new token, always prefixed with `ST`)

### POST `/v1/{space_id}/management/revoke_space_token`

- Purpose: Revoke a Space Token
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `RevokeSpaceTokenInput`
- Response: `RpcResponse<boolean>` (whether revocation succeeded)

### PATCH `/v1/{space_id}/management/update_space`

- Purpose: Update space information (name, description, public/private)
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `UpdateSpaceInput`
- Response: `RpcResponse<true>`

### PATCH `/v1/{space_id}/management/restart_formation`
- Purpose: Restart a formation task by conversation ID (for failed/stale formations)
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `FormationRestartInput`
- Response: `RpcResponse<true>`

### GET `/v1/{space_id}/management/space_byok`
- Purpose: Get BYOK (Bring Your Own Key) configuration, i.e., use custom model configuration
- Auth: Must pass CWT `write` (user management-level auth; response includes provider credentials)
- Response: `RpcResponse<ModelConfig>`

### PATCH `/v1/{space_id}/management/space_byok`
- Purpose: Update BYOK (Bring Your Own Key) configuration, i.e., use custom model configuration
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `ModelConfig`
- Response: `RpcResponse<true>`

---

## 4.5 Admin Endpoints (`/admin`)

### POST `/admin/create_space`

- Purpose: Create a space
- Auth: Platform admin + CWT `write`
- Request body: `CreateOrUpdateSpaceInput`
- Response: `RpcResponse<SpaceInfo>`

### POST `/admin/{space_id}/update_space_tier`

- Purpose: Update space tier
- Auth: Platform admin + CWT `write`
- Request body: `CreateOrUpdateSpaceInput`
- Response: `RpcResponse<SpaceTier>`

---

## 5) Frontend Call Example (TS)

```ts
async function rpcPost<TReq, TRes>(
  url: string,
  body: TReq,
  token?: string
): Promise<RpcResponse<TRes>> {
  const res = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify(body),
  });

  return (await res.json()) as RpcResponse<TRes>;
}

// Recall
const recall = await rpcPost<RecallInput, AgentOutput>(
  '/v1/my_space_001/recall',
  { query: 'What are this user\'s preferences?', context: { counterparty: 'user_1' } },
  'YOUR_TOKEN'
);

if (recall.error) {
  console.error(recall.error.message);
} else {
  console.log(recall.result?.content);
}
```

---

## 6) Error Semantics

- Authentication failure: HTTP `401`, response body is `RpcError`
- Invalid request/parameters: HTTP `400`, response body is `RpcError`
- Success: HTTP `200`, response body is usually `RpcResponse<T>`
