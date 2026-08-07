# AGENTS.md

Guidance for coding agents working in this repository.

## Scope

These instructions apply to the whole repository unless a more specific
`AGENTS.md` exists in a subdirectory.

## Project Overview

Anda Brain is a Rust service that provides long-term memory for LLM agents. The
main crate is `anda_brain`, which exposes:

- Formation: encode conversations into structured memory.
- Recall: answer natural-language queries from memory.
- Maintenance: consolidate, prune, and optimize memory.

The service stores memory in an AndaDB-backed Cognitive Nexus and uses KIP
(Knowledge Interaction Protocol) internally. Business agents should not need to
write KIP directly.

## Repository Layout

- `anda_brain/`: Rust library and binary for the service.
- `anda_brain/src/agents/`: Formation, Recall, and Maintenance agents.
- `anda_brain/src/space.rs`: space lifecycle, AndaDB setup, auth checks, and
  background flushing/eviction.
- `anda_brain/src/handler.rs`: HTTP route handlers and API entry points.
- `anda_brain/src/payload.rs`: JSON/CBOR/Markdown payload negotiation.
- `anda_brain/src/types.rs`: API input/output and persisted config types.
- `anda_brain/assets/`: agent prompts, KIP syntax notes, and tool definitions.
- `anda_brain/API*.md`, `anda_brain/README.md`, `anda_brain/SKILL.md`: public
  API and integration documentation.
- `skills/anda-brain/`: packaged skill content for external agents.
- `deploy/`, `anda-brain-demo/`, `anda-brain-openclaw/`, `anda-cli/`: deployment
  and integration material. Do not change these unless the task is explicitly
  about them.

## Development Commands

Run these from the repository root:

```bash
cargo fmt --check
cargo clippy -p anda_brain --all-targets --all-features -- -D warnings
cargo test -p anda_brain --all-features
```

`cargo test -p anda_brain --all-features` includes a bin test that binds an
ephemeral localhost port. In restricted sandboxes it may fail with
`PermissionDenied`; rerun it with the required permission rather than treating
that as a code failure.

The library is feature-gated (see "Cargo Features" below), so also check that
the lean build still compiles when you touch `space.rs`, `handler.rs`,
`authz.rs`, or `types.rs`:

```bash
cargo test -p anda_brain --lib
cargo test -p anda_brain --lib --features wiki
cargo test -p anda_brain --lib --features mcp
```

For local manual testing:

```bash
cargo run -p anda_brain --features mcp,wiki
cargo run -p anda_brain --features mcp,wiki -- local --db ./db
```

Authentication is disabled when `ED25519_PUBKEYS` is empty. Do not assume this
is safe for production.

## Cargo Features

The `anda_brain` library defaults to memory only — formation, recall,
maintenance, and their HTTP routes. Two optional features add the rest:

- `wiki`: the structured wiki (documents, versions, ACL-scoped reads, OKF
  import/export), its agent tools, the WikiDigest graph extraction, the
  `/v1/{space_id}/wiki/*` routes, and the `wiki_*` fields of `SpaceInfo` and
  `UpdateSpaceInput`.
- `mcp`: the MCP channel (stdio and Streamable HTTP). With `wiki` also on, the
  wiki tools join the MCP tool router.

The `anda_brain` binary declares `required-features = ["mcp", "wiki"]`: it is
the full product, so every build of it must pass `--features mcp,wiki`. Cargo
silently skips the binary when they are absent.

When adding code that touches the wiki or MCP, gate it with
`#[cfg(feature = "…")]` rather than widening the default surface, and keep the
lean build compiling.

## Coding Conventions

- Follow the existing Rust 2024 style and keep `cargo fmt` clean.
- Prefer existing crate patterns over new abstractions.
- Keep behavior changes scoped to `anda_brain` unless the task explicitly
  targets demos, deploy files, or packaged skills.
- Avoid external network calls in tests. Unit tests should use in-memory or local
  storage and must not require a live model provider.
- Add tests close to the module being changed when behavior changes.
- Preserve JSON, CBOR, and Markdown payload compatibility in `payload.rs` and
  route handlers.
- Preserve compact persisted field aliases in `types.rs`; they are storage/API
  compatibility details.
- Be careful with dirty worktrees. Do not revert or overwrite unrelated user
  changes.

## Brain-Specific Invariants

- Formation and Maintenance are guarded against concurrent processing. Do not
  weaken `processing_conversation` or `processing` semantics.
- Formation should process queued formation conversations sequentially and resume
  after maintenance completes.
- Maintenance should be single-flight per space and should trigger formation
  resumption when it finishes.
- Recall and read-only KIP execution must remain read-only and bounded by the
  configured timeouts.
- Space-level token scopes are `read`, `write`, and `*`; keep auth changes
  explicit and test them.
- Space metadata and database extension updates must be persisted with
  `save_extension*`, `flush_metadata`, `flush`, or `close` as appropriate.
- On shutdown or eviction, close databases when possible so AndaDB flushes
  collections and metadata.

## API and Docs

When changing public request/response shapes, auth behavior, content negotiation,
or endpoints:

- Update `anda_brain/API.md` and `anda_brain/API_cn.md`.
- Update `anda_brain/README.md` and `README*.md` when user-facing behavior
  changes.
- Update `anda_brain/SKILL.md` and `skills/anda-brain/SKILL.md` when integration
  instructions or endpoint usage changes.
- Keep English and Chinese docs in sync for user-facing API changes.

## Prompt and Asset Changes

Agent prompts in `anda_brain/assets/` are part of runtime behavior. Edit them
only when the task calls for prompt behavior changes, and describe the intended
agent behavior clearly in the diff. Avoid prompt edits as a workaround for a
code bug.
