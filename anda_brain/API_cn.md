# Anda Brain API 文档（含 TypeScript 类型）

## 1) 通用约定

- Base URL: `http://{host}:{port}`
- 认证头：`Authorization: Bearer <token>`
- 若 `ED25519_PUBKEYS` 为空或未提供，则鉴权将被关闭。
- 支持的序列化格式：
  - 请求：`Content-Type: application/json | application/cbor | text/markdown`
  - 响应：`Accept: application/json | application/cbor | text/markdown`
  - 内容协商仅作用于成功响应体；错误响应体始终为 JSON，与 `Accept` 无关
- 大多数业务接口都会返回 RPC 包装后的结构体：`RpcResponse<T>`
- MCP 客户端可使用内置的支持流式传输的 HTTP MCP 端点：`/mcp/<space_id>`，也可以使用本地 stdio server：`anda_brain mcp --space-id <space_id> [local|aws]`

---

## 2) TypeScript 类型定义

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
  name?: string;  // user 或 tool 的名称
  user?: string;  // user ID
  timestamp?: number; // Unix timestamp in milliseconds
}

export interface FormationInput {
  messages: Message[]; // 至少包含一条非空消息（否则 400）
  context?: InputContext;
  timestamp: string; // ISO 8601
}

export interface RecallInput {
  query: string; // 不能为空/纯空白（否则 400）
  context?: InputContext;
}

export interface MaintenanceParameters {
  stale_event_threshold_days?: number; // [1, 365]
  confidence_decay_factor?: number; // (0, 1]
  unsorted_max_backlog?: number; // [1, 10000]
  orphan_max_count?: number; // [1, 10000]
}

export interface MaintenanceInput {
  trigger?: 'scheduled' | 'threshold' | 'on_demand';
  scope?: 'full' | 'quick' | 'daydream'; // 默认 'daydream'
  timestamp?: string; // ISO 8601
  parameters?: MaintenanceParameters;
}

export interface AddSpaceTokenInput {
  scope: TokenScope; // 铸造 "*" 需要 "*" scope 的 CWT
  name: string; // 必填，空间内唯一
  expires_at?: number; // Unix timestamp in milliseconds
  labels?: string[]; // wiki ACL 标签；缺省 = 不受限
}

export interface RevokeSpaceTokenInput {
  token?: string; // 完整 token 值……
  name?: string; // ……或唯一 token 名称（两者必填其一）
}

export interface UpdateSpaceInput {
  name?: string;
  description?: string;
  public?: boolean;
  wiki_digest?: boolean; // 开启 WikiDigest 图谱蒸馏（默认关闭）
  wiki_audit_reads?: boolean; // 外部 wiki 读操作写审计事件（默认关闭）
  wiki_acl_defaults?: Record<string, string>; // namespace -> 默认 ACL 标签
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

// ── Wiki：版本化参考文档与可校验引用 ──────────────────────────────

export type WikiDocStatus = 'active' | 'archived';
export type WikiSearchMode = 'chunks' | 'docs';

export interface WikiCommitInput {
  doc_id?: number; // 缺省 = 创建新文档
  parent_version?: number; // 更新必填（CAS）；过期返回 409
  namespace?: string; // 默认 "default"
  slug?: string; // 展示用；缺省从标题派生
  title: string;
  content: string; // 全量 Markdown（非 diff）；规范化后 ≤ 1 MiB
  tags?: string[]; // 缺省 = 更新时保持原值
  acl_label?: string; // 缺省 = 保持/继承 namespace 默认；"" 清除
  source_uri?: string; // 缺省 = 保持原值
  message?: string; // 提交说明
  metadata?: Record<string, unknown>; // 缺省 = 保持原值
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
  idempotent: boolean; // true = 内容未变，零写入
}

export interface WikiSearchInput {
  query: string; // BM25 关键词：术语、产品名、错误码优于整句
  namespaces?: string[];
  doc_ids?: number[];
  tags?: string[];
  top_k?: number; // 1-50，默认 8
  mode?: WikiSearchMode; // 'docs' = 每篇文档只返回最佳命中
  expand?: number; // 0-2 邻域扩展；引用范围相应扩大
}

export interface WikiCitation {
  uri: string; // wiki://{space}/{doc_id}@{version_id}#{start}-{end}
  doc_id: number;
  version_id: number;
  chunk_id: number;
  heading_path: string[];
  anchor: string; // 稳定章节锚点，可用于按节读取
  byte_range: [number, number];
  checksum: string; // 可经 /wiki/verify 校验
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
  version?: number; // 读取历史版本（time-travel）
  selector?: WikiSelector; // 默认 { type: 'full' }
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
  toc?: WikiTocEntry[]; // selector 为 'toc' 时返回
  content?: string; // section/range/full 时返回
  byte_range?: [number, number];
  truncated: boolean; // 全文读取有上限（256 KiB）
}

export interface WikiVerifyInput {
  uri?: string; // wiki:// 引用 URI；或使用下方显式字段
  doc_id?: number;
  version_id?: number;
  byte_range?: [number, number];
  checksum?: string; // 提供时与重算校验和比对
}

export type WikiVerifyStatus = 'valid' | 'superseded' | 'invalid' | 'not_found';

export interface WikiVerifyOutput {
  status: WikiVerifyStatus; // 'superseded' = 内容完好但已有新版本
  current_version?: number;
  checksum?: string; // 从不可变内容重算
  quote?: string;
}

export interface WikiBundleEntry {
  path: string; // bundle 相对路径，如 "guides/setup.md"
  content: string;
}

export interface WikiImportInput {
  entries: WikiBundleEntry[]; // OKF v0.1 bundle 文件（Markdown + YAML frontmatter）
  namespace?: string; // 默认 "default"；bundle 以 namespace 为单位往返
}

export type WikiImportStatus = 'created' | 'updated' | 'unchanged';

export interface WikiImportOutput {
  created: number;
  updated: number;
  unchanged: number; // checksum 幂等：重复导入零版本膨胀
  docs: { path: string; doc_id: number; version_id: number; status: WikiImportStatus }[];
  skipped?: { path: string; reason: string }[];
}

export interface WikiExportOutput {
  namespace: string;
  entries: WikiBundleEntry[]; // concept .md 文件 + index.md + manifest.json
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
  digested: number; // 本轮蒸馏进图谱的版本数
  facts: number; // 写入的命题数（metadata 携带 wiki:// 引用）
  superseded: number; // 被标记 superseded 的旧命题数
  skipped: number;
  citations_checked: number; // 蒸馏后引用抽检
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
  path_prefix?: string; // 默认 "/mcp"; client 连接 {path_prefix}/{space_id}
  allowed_hosts?: string[]; // rmcp 默认只允许 loopback；公司域名需要显式配置
  allowed_origins?: string[]; // 浏览器型 MCP client 使用
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
  labels?: string[]; // wiki ACL 标签：仅可见无标签内容 + 所列标签
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
  wiki_digested: number; // 蒸馏高水位（version id）
  wiki_stale_docs: number; // 最近一次 housekeeping 陈旧扫描结果
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
  /** 最近一次 maintenance 任务的启动时间（unix 毫秒），0 表示尚未启动过。 */
  start_at: number;
}

export interface Usage {
  /** 发送给 LLM 的输入 token 数。 */
  input_tokens: number;
  /** 从 LLM 接收的输出 token 数。 */
  output_tokens: number;
  /** 执行过程中命中缓存的 token 数。 */
  cached_tokens: number;
  /** 对模型、agent 或工具发起的请求次数。 */
  requests: number;
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

HTTP 服务启动时，Anda Brain 会暴露支持流式传输的 HTTP MCP 端点，供支持 MCP 客户端的智能体直接调用：

```text
https://your-brain-host/mcp/my_space_001
```

Client 通过 URL path 选择目标记忆空间，并使用与 REST 相同的 CWT 或 space token：`Authorization: Bearer <token>`。这适合公司内部多用户智能体平台：为每位员工分配一个 Brain space，员工的智能体通过 MCP 连接自己的空间。

Anda Brain 也可作为本地 MCP stdio server 运行：

```bash
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  anda_brain mcp --space-id my_space_001 local --db ./data
```

两种 MCP 模式都复用 HTTP 服务的模型、认证和存储配置。stdio 模式的嵌套 storage 子命令可省略（内存开发模式），也可以使用 `local --db ./data` 持久化到本地，或使用 `aws --bucket ... --region ...` 连接 S3。

| Tool | Input | Output | Scope |
| ---- | ----- | ------ | ----- |
| `anda_brain_remember_conversation` | `FormationInput` 形状（`messages`, `context`, `timestamp`） | `AgentOutput` | `write` |
| `anda_brain_recall_memory` | `RecallInput` 形状（`query`, `context`） | `AgentOutput` | `read` |
| `anda_brain_run_maintenance` | `MaintenanceInput` 形状 | `AgentOutput` | `write` |
| `anda_brain_get_space_info` | 无 | `SpaceInfo` | `read` |
| `anda_brain_get_formation_status` | 无 | `FormationStatus` | `read` |
| `anda_brain_execute_kip_readonly` | `{ command?, commands?, parameters?, dry_run? }` | `KipResponse` | `read` |
| `anda_brain_get_or_init_user` | `{ user, name? }` | `Concept` | `write` |
| `anda_brain_list_conversations` | `{ collection?, cursor?, limit? }` | `{ conversations, next_cursor }` | `read` |
| `anda_brain_get_conversation` | `{ conversation_id, collection?, delta?, messages_offset?, artifacts_offset? }` | `Conversation` 或 `ConversationDelta` | `read` |

当设置了 `ED25519_PUBKEYS` 时，远程 MCP 客户端需要携带 `Authorization` bearer token；stdio 模式请通过 `MCP_AUTH_TOKEN` 或 `--mcp-auth-token` 配置 CWT 或 space token。`read` 工具也可无 token 访问 public space。远程 MCP 经过公司域名或反向代理暴露时，请设置 `MCP_HTTP_ALLOWED_HOSTS`。本地 stdio 开发可用 `--mcp-auto-create-space` 自动创建目标 space；远程开发可用 `MCP_HTTP_AUTO_CREATE_SPACE=true`，但在远程自动创建不存在的 space 前，必须配置好 `ED25519_PUBKEYS`，且客户端需提供该 space 拥有 `write` 范围的 CWT。

---

## 4) 接口列表

## 4.1 公共接口

### GET `/`

- 说明：返回产品网页（HTML 或 Markdown）。
- 鉴权：无
- 响应：`text/html` 或 `text/markdown`

### GET `/info`

- 说明：服务信息
- 鉴权：无
- 响应（JSON）：`ServiceInfo`

### GET `/SKILL.md`

- 说明：返回技能描述 Markdown
- 鉴权：无
- 响应：`text/markdown`

---

## 4.2 空间业务接口（`/v1/{space_id}`）

### POST `/v1/{space_id}/formation`

- 作用：提交记忆写入任务
- 鉴权：SpaceToken/CWT `write`
- 请求体：`FormationInput`（Markdown 模式下也允许原始字符串）
- 响应（JSON/CBOR）：`RpcResponse<AgentOutput>`
- 响应（Markdown）：`string`（仅返回 `AgentOutput.content`）

### POST `/v1/{space_id}/recall`

- 作用：按自然语言召回记忆
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）
- 请求体：`RecallInput`（Markdown 模式下也允许原始字符串）
- 响应：`RpcResponse<AgentOutput>`

### POST `/v1/{space_id}/maintenance`

- 作用：触发维护（睡眠/整理）
- 鉴权：SpaceToken/CWT `write`
- 请求体：`MaintenanceInput`
- 响应：`RpcResponse<AgentOutput>`

### POST `/v1/{space_id}/execute_kip_readonly`

- 作用：执行 KIP 请求（只读模式，适用于查询）
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）
- 请求体：`KipRequest`
- 响应：`KipResponse<T>`（根据请求中的命令不同，返回不同的结果类型）

### POST `/v1/{space_id}/get_or_init_user`

- 作用：按给定 principal 获取或初始化用户 Concept 节点
- 鉴权：SpaceToken/CWT `write`
- 请求体：`GetOrInitUserInput`
- 响应：`RpcResponse<Concept>`

### GET `/v1/{space_id}/info`

- 作用：获取空间状态和统计
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）
- 响应：`RpcResponse<SpaceInfo>`

### GET `/v1/{space_id}/formation_status`

- 作用：获取记忆写入状态（更轻量级的接口，专门用于监控记忆写入进度）
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）
- 响应：`RpcResponse<FormationStatus>`

### GET `/v1/{space_id}/conversations/{conversation_id}?collection=<collection>`

- 作用：获取单条会话详情
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）；带 ACL 标签限制的 token 返回 `403`——会话持久化了完整的 agent 运行历史，不受标签过滤；`collection=recall` 对公开空间的匿名访问也返回 `403`（私有期的 recall 运行可能内嵌 labeled wiki 内容）
- Query:
  - `collection?: string` // "formation"（默认）、"recall" 或 "maintenance"；未知值返回 `400`
- 响应：`RpcResponse<Conversation>`

### GET `/v1/{space_id}/conversations/{conversation_id}/delta?collection=<collection>&messages_offset=<n>&artifacts_offset=<n>`

- 作用：按客户端已消费的 offset 获取会话增量更新
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）；带 ACL 标签限制的 token 返回 `403`（`collection=recall`：公开空间匿名访问同样拒绝）
- Query:
  - `collection?: string` // "formation"（默认）、"recall" 或 "maintenance"；未知值返回 `400`
  - `messages_offset?: number` // 仅返回该偏移量之后的新消息，默认 `0`
  - `artifacts_offset?: number` // 仅返回该偏移量之后的新 artifacts，默认 `0`
- 响应：`RpcResponse<ConversationDelta>`

### GET `/v1/{space_id}/conversations?collection=<collection>&cursor=<cursor>&limit=<n>`

- 作用：分页列出会话
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）；带 ACL 标签限制的 token 返回 `403`（`collection=recall`：公开空间匿名访问同样拒绝）
- Query:
  - `collection?: string` // "formation"（默认）、"recall" 或 "maintenance"；未知值返回 `400`
  - `cursor?: string`
  - `limit?: number`
- 响应：`RpcResponse<Conversation[]>`（并通过 `next_cursor` 给出下一页游标）

---

## 4.3 Wiki 接口（`/v1/{space_id}/wiki`）

Wiki 是空间的版本化参考记忆（政策、手册、SOP、API 文档）。写入是 Git 式不可变提交（CAS 并发控制）；检索返回可校验的 `wiki://` 引用。ACL：文档可携带 `acl_label`；带 `labels` 的 space token 仅可见无标签内容 + 所授标签——过滤在检索查询内部执行。公开空间的匿名读者仅可见无标签内容；越权一律表现为 404。

Wiki 专属错误语义：`409` 提交冲突（`error.data.current_version` 为应 rebase 的版本）、`413` 内容超 1 MiB、`404` 不存在或 ACL 拒绝。

### POST `/v1/{space_id}/wiki/docs`

- 作用：提交文档（创建；或携带 `doc_id` + `parent_version` 做 CAS 更新）；同内容提交为零写入
- 鉴权：SpaceToken/CWT `write`
- 请求体：`WikiCommitInput`（也接受原始 Markdown 字符串，标题取首个标题行）
- 响应：`RpcResponse<WikiCommitOutput>`

### GET `/v1/{space_id}/wiki/docs?namespace=<ns>&status=<status>&tag=<tag>&cursor=<cursor>&limit=<n>`

- 作用：分页列出文档
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权；ACL 标签生效）
- 响应：`RpcResponse<WikiDocInfo[]>`（下一页游标经 `next_cursor` 返回）

### GET `/v1/{space_id}/wiki/docs/{doc_id}`

- 作用：文档元信息 + 目录（TOC）
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权；ACL 标签生效）
- 响应：`RpcResponse<{ doc: WikiDocInfo; toc: WikiTocEntry[] }>`

### GET `/v1/{space_id}/wiki/docs/{doc_id}/content?version=<id>&anchor=<anchor>&start=<n>&end=<n>`

- 作用：渐进读取——`anchor` 读单节、`start`+`end` 读字节区间、均不传读受限全文；`version` 读历史版本
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权；ACL 标签生效）
- 响应：`RpcResponse<WikiReadOutput>`

### GET `/v1/{space_id}/wiki/docs/{doc_id}/versions?cursor=<cursor>&limit=<n>`

- 作用：版本历史（不可变提交链）
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权；ACL 标签生效）
- 响应：`RpcResponse<WikiVersionInfo[]>`（下一页游标经 `next_cursor` 返回）

### POST `/v1/{space_id}/wiki/docs/{doc_id}/archive`

- 作用：归档文档（退出检索，仍可按 id 读取，可恢复）
- 鉴权：SpaceToken/CWT `write`
- 响应：`RpcResponse<WikiDocInfo>`

### POST `/v1/{space_id}/wiki/docs/{doc_id}/restore`

- 作用：恢复归档文档进入检索
- 鉴权：SpaceToken/CWT `write`
- 响应：`RpcResponse<WikiDocInfo>`

### POST `/v1/{space_id}/wiki/search`

- 作用：BM25 关键词检索，返回片段与可校验引用
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权；ACL 标签生效）
- 请求体：`WikiSearchInput`（也接受原始查询字符串）
- 响应：`RpcResponse<WikiSearchOutput>`

### POST `/v1/{space_id}/wiki/verify`

- 作用：对照不可变存储校验引用
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权；ACL 标签生效）
- 请求体：`WikiVerifyInput`（也接受原始 `wiki://` URI 字符串）
- 响应：`RpcResponse<WikiVerifyOutput>`

### GET `/v1/{space_id}/wiki/events?kind=<kind>&doc_id=<id>&cursor=<cursor>&limit=<n>`

- 作用：查询 append-only 审计日志（写入、导入、蒸馏；开启 `wiki_audit_reads` 后含读操作）
- 鉴权：SpaceToken/CWT `read`；受 ACL 标签限制的 token 返回 `403`
- 响应：`RpcResponse<WikiEventInfo[]>`（下一页游标经 `next_cursor` 返回）

### POST `/v1/{space_id}/wiki/import`

- 作用：导入 OKF v0.1 bundle；checksum 幂等（重复导入零版本膨胀）；未知 frontmatter 字段逐字往返
- 鉴权：SpaceToken/CWT `*`（全量 scope）
- 请求体：`WikiImportInput`
- 响应：`RpcResponse<WikiImportOutput>`

### GET `/v1/{space_id}/wiki/export?namespace=<ns>`

- 作用：按 namespace 导出 OKF bundle（concept `.md` + `index.md` + 含校验和的 `manifest.json`）；可在空库完整重放
- 鉴权：SpaceToken/CWT `*`（全量 scope）
- 响应：`RpcResponse<WikiExportOutput>`

### POST `/v1/{space_id}/wiki/digest`

- 作用：把待处理 wiki 版本蒸馏进 Cognitive Nexus（命题 metadata 携带 `wiki://` 引用）；新版本不再断言的旧命题被标记 superseded（需先 `update_space {"wiki_digest": true}` 开启）
- 鉴权：SpaceToken/CWT `write`
- 响应：`RpcResponse<WikiDigestReport>`

---

## 4.4 空间管理接口（`/v1/{space_id}/management`）

### GET `/v1/{space_id}/management/space_tokens`

- 作用：列出 Space Token
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）
- 响应：`RpcResponse<SpaceToken[]>` —— `token` 字段仅显示前缀（如 `STabc123…`）；完整 token 值只在 `add_space_token` 响应中出现一次，铸造时务必保存，或后续凭 `name` 吊销

### POST `/v1/{space_id}/management/add_space_token`

- 作用：新增 Space Token
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）。铸造 `*`（全 scope）token 需要 `*` scope 的 CWT——`write` CWT 不能铸造高于自身 scope 的 token
- 请求体：`AddSpaceTokenInput` —— `name` 必填且空间内唯一（是 token 的审计身份与吊销句柄）
- 响应：`RpcResponse<SpaceToken>`（新 token，前缀总是 `ST`；这是唯一携带完整 token 值的响应）

### POST `/v1/{space_id}/management/revoke_space_token`

- 作用：吊销 Space Token
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）
- 请求体：`RevokeSpaceTokenInput` —— 传 `token`（完整 token 值）或 `name`（唯一 token 名称，供未保存 token 值的管理者使用）
- 响应：`RpcResponse<boolean>`（是否成功吊销）

### PATCH `/v1/{space_id}/management/update_space`

- 作用：更新空间信息（名称、描述、公开/私有）
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）
- 请求体：`UpdateSpaceInput`
- 响应：`RpcResponse<true>`

### PATCH `/v1/{space_id}/management/restart_formation`

- 作用：通过会话 ID 重启记忆写入任务（用于失败/过期的写入任务）
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）
- 请求体：`FormationRestartInput`
- 响应：`RpcResponse<true>`

### GET `/v1/{space_id}/management/space_byok`

- 作用：获取 BYOK（Bring Your Own Key）配置，即使用自定义模型配置
- 鉴权：必须通过 CWT `write`（用户管理级鉴权；响应包含模型供应商凭据）
- 响应：`RpcResponse<ModelConfig>`

### PATCH `/v1/{space_id}/management/space_byok`

- 作用：更新 BYOK（Bring Your Own Key）配置，即使用自定义模型配置
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）
- 请求体：`ModelConfig`
- 响应：`RpcResponse<true>`

---

## 4.5 管理员接口（`/admin`）

### POST `/admin/create_space`

- 作用：创建空间
- 鉴权：平台管理员 + CWT `write`
- 请求体：`CreateOrUpdateSpaceInput`
- 响应：`RpcResponse<SpaceInfo>`

### POST `/admin/{space_id}/update_space_tier`

- 作用：更新空间 tier
- 鉴权：平台管理员 + CWT `write`
- 请求体：`CreateOrUpdateSpaceInput`
- 响应：`RpcResponse<SpaceTier>`

---

## 5) 前端调用示例（TS）

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
  { query: '这个用户的偏好是什么？', context: { counterparty: 'user_1' } },
  'YOUR_TOKEN'
);

if (recall.error) {
  console.error(recall.error.message);
} else {
  console.log(recall.result?.content);
}
```

---

## 6) 错误语义

- 认证失败：HTTP `401`，响应体为 `RpcError`
- 参数错误：HTTP `400`，响应体为 `RpcError`
- 成功时：HTTP `200`，响应体通常为 `RpcResponse<T>`
- 错误响应体始终为 JSON，即使请求指定了 `application/cbor` 或 `text/markdown`（包括携带 `error.data.current_version` 的 wiki `409` 冲突响应体）；只有成功响应体遵循 `Accept` 协商
