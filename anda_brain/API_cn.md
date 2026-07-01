# Anda Brain API 文档（含 TypeScript 类型）

## 1) 通用约定

- Base URL: `http://{host}:{port}`
- 认证头：`Authorization: Bearer <token>`
- 若 `ED25519_PUBKEYS` 为空或未提供，则鉴权将被关闭。
- 支持的序列化格式：
  - 请求：`Content-Type: application/json | application/cbor | text/markdown`
  - 响应：`Accept: application/json | application/cbor | text/markdown`
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
  scope?: 'full' | 'quick' | 'daydream'; // 默认 'daydream'
  timestamp?: string; // ISO 8601
  parameters?: MaintenanceParameters;
}

export interface AddSpaceTokenInput {
  scope: TokenScope;
  name: string;
  expires_at?: number; // Unix timestamp in milliseconds
}

export interface RevokeSpaceTokenInput {
  token: string;
}

export interface UpdateSpaceInput {
  name?: string;
  description?: string;
  public?: boolean;
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
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）
- Query:
  - `collection?: string` // 使用 "recall" 区分召回 vs 记忆会话
- 响应：`RpcResponse<Conversation>`

### GET `/v1/{space_id}/conversations/{conversation_id}/delta?collection=<collection>&messages_offset=<n>&artifacts_offset=<n>`

- 作用：按客户端已消费的 offset 获取会话增量更新
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）
- Query:
  - `collection?: string` // 使用 "recall" 或 "maintenance" 区分非默认会话集合
  - `messages_offset?: number` // 仅返回该偏移量之后的新消息，默认 `0`
  - `artifacts_offset?: number` // 仅返回该偏移量之后的新 artifacts，默认 `0`
- 响应：`RpcResponse<ConversationDelta>`

### GET `/v1/{space_id}/conversations?collection=<collection>&cursor=<cursor>&limit=<n>`

- 作用：分页列出会话
- 鉴权：SpaceToken/CWT `read`（公开空间免鉴权，私有空间需有效 token）
- Query:
  - `collection?: string` // 使用 "recall" 区分召回 vs 记忆会话
  - `cursor?: string`
  - `limit?: number`
- 响应：`RpcResponse<Conversation[]>`（并通过 `next_cursor` 给出下一页游标）

---

## 4.3 空间管理接口（`/v1/{space_id}/management`）

### GET `/v1/{space_id}/management/space_tokens`

- 作用：列出 Space Token
- 鉴权：必须通过 CWT `write`（用户管理级鉴权；响应包含原始 token secret）
- 响应：`RpcResponse<SpaceToken[]>`

### POST `/v1/{space_id}/management/add_space_token`

- 作用：新增 Space Token
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）
- 请求体：`AddSpaceTokenInput`
- 响应：`RpcResponse<SpaceToken>`（新 token，前缀总是 `ST`）

### POST `/v1/{space_id}/management/revoke_space_token`

- 作用：吊销 Space Token
- 鉴权：必须通过 CWT `write`（用户管理级鉴权）
- 请求体：`RevokeSpaceTokenInput`
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

## 4.4 管理员接口（`/admin`）

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
