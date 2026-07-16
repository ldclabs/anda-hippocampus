# Anda CLI

A command-line tool for interacting with the [Anda Brain](https://brain.anda.ai) memory service API.

## Install

```bash
go install github.com/ldclabs/anda-brain/anda-cli@latest
```

Or build from source:

```bash
cd anda-cli
go build -o anda-cli .
```

## Configuration

Configuration can be provided via flags or environment variables:

| Flag         | Env Variable    | Description                                       | Default                 |
| ------------ | --------------- | ------------------------------------------------- | ----------------------- |
| `--base-url` | `ANDA_BASE_URL` | API base URL                                      | `http://127.0.0.1:8042` |
| `--space-id` | `ANDA_SPACE_ID` | Space ID                                          |                         |
| `--token`    | `ANDA_TOKEN`    | Auth token                                        |                         |
| `--shard`    | `ANDA_SHARD`    | Shard index (`Shard-Id` header) for sharded setup | `0`                     |
| `--timeout`  | `ANDA_TIMEOUT`  | HTTP request timeout in seconds                   | `120`                   |

**CWT command flags:**

| Flag         | Env Variable        | Description                                                         | Default |
| ------------ | ------------------- | ------------------------------------------------------------------- | ------- |
| `--key`      | `ANDA_CWT_KEY`      | Ed25519 private key (base64/base64url CBOR, or file path / `@file`) |         |
| `--subject`  | `ANDA_CWT_SUBJECT`  | Subject claim - user/principal ID                                   |         |
| `--audience` | `ANDA_CWT_AUDIENCE` | Audience claim - space ID or `*`                                    |         |
| `--scope`    | `ANDA_CWT_SCOPE`    | Scope claim: read, write or `*`                                     | `read`  |
| `--issuer`   | `ANDA_CWT_ISSUER`   | Issuer claim                                                        |         |

## Commands

### Key Generation & CWT

```bash
# Generate a new Ed25519 key pair (base64url-encoded COSE Key)
anda-cli keygen
anda-cli keygen --json

# Create a CWT (CBOR Web Token) signed with Ed25519 private key
anda-cli cwt --key <base64url_private_key> --subject <user_id> --audience <space_id> --scope write

# Create a CWT using a key stored in a file (base64 or base64url content)
anda-cli cwt --key ./private_key.txt --subject <user_id> --audience <space_id> --scope write
anda-cli cwt --key @./private_key.txt --subject <user_id> --audience <space_id> --scope write

# Create a CWT with wildcard audience and 2-hour expiration
anda-cli cwt --key <base64url_private_key> --subject <user_id> --audience "*" --scope "*" --expiration 7200
```

### Service

```bash
# Get service information (name, version, sharding)
anda-cli status
```

### Memory Operations

```bash
# Submit memory formation
anda-cli --space-id my_space --token $TOKEN formation \
  --messages '[{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi!"}]'

# Submit memory formation with plain text (--messages)
anda-cli --space-id my_space --token $TOKEN formation \
  --messages 'Hello, this is a plain text memory.'

# Submit memory formation from file (JSON or plain text)
anda-cli --space-id my_space --token $TOKEN formation \
  --file ./message.txt

# Or pipe from stdin
echo '[{"role":"user","content":"Hello"}]' | \
  anda-cli --space-id my_space --token $TOKEN formation

# Or pipe plain text from stdin
echo 'Hello from stdin plain text' | \
  anda-cli --space-id my_space --token $TOKEN formation

# Batch submit files by exact filename (recursive).
# Hidden entries (dot-prefixed, e.g. .git) and the checklist file are skipped.
anda-cli --space-id my_space --token $TOKEN formation \
  --batch-dir ./docs \
  --batch-file-name Skill.md

# Batch submit files by extension (recursive)
anda-cli --space-id my_space --token $TOKEN formation \
  --batch-dir ./docs \
  --batch-ext .md

# Resume from checklist and retry only previously failed files
anda-cli --space-id my_space --token $TOKEN formation \
  --batch-dir ./docs \
  --batch-ext .md \
  --batch-retry-failed

# Use a custom checklist path
anda-cli --space-id my_space --token $TOKEN formation \
  --batch-dir ./docs \
  --batch-file-name Skill.md \
  --batch-report ./tmp/formation-batch-checklist.json

# Dry run: only scan and print matched files, no formation submission
anda-cli --space-id my_space --token $TOKEN formation \
  --batch-dir ./docs \
  --batch-ext .md \
  --batch-dry-run

# Recall memory
anda-cli --space-id my_space --token $TOKEN recall "What are the user's preferences?"

# Recall with context
anda-cli --space-id my_space --token $TOKEN recall \
  --context-user u1 "What happened in the last meeting?"

# Trigger maintenance
anda-cli --space-id my_space --token $TOKEN maintenance
anda-cli --space-id my_space --token $TOKEN maintenance --trigger on_demand --scope full

# Execute a single read-only KIP command
anda-cli --space-id my_space --token $TOKEN execute-kip-readonly \
  --request '{"command":"DESCRIBE PRIMER"}'

# Execute read-only KIP request from inline JSON (batch commands)
anda-cli --space-id my_space --token $TOKEN execute-kip-readonly \
  --request '{"commands":[{"command":"query_domain","parameters":{"query":"user preferences"}}]}'

# Execute read-only KIP request from file
anda-cli --space-id my_space --token $TOKEN execute-kip-readonly --file ./kip_request.json

# Execute read-only KIP request from stdin
cat ./kip_request.json | anda-cli --space-id my_space --token $TOKEN execute-kip-readonly
```

### Space Info & Conversations

```bash
# Get space information and statistics
anda-cli --space-id my_space --token $TOKEN info

# Get formation processing status
anda-cli --space-id my_space --token $TOKEN formation-status

# Get or initialize a user concept
anda-cli --space-id my_space --token $TOKEN get-or-init-user principal_123 --name Alice

# List conversations
anda-cli --space-id my_space --token $TOKEN conversations list --limit 10

# Get a specific conversation
anda-cli --space-id my_space --token $TOKEN conversations get 42

# Get incremental updates for a conversation
anda-cli --space-id my_space --token $TOKEN conversations delta 42 \
  --messages-offset 10 \
  --artifacts-offset 2
```

### Space Management (requires CWT auth)

```bash
# List space tokens
anda-cli --space-id my_space --token $CWT_TOKEN management list-tokens

# Add a space token (--name is required; full token value is only shown once)
anda-cli --space-id my_space --token $CWT_TOKEN management add-token --name writer --scope write

# Add a label-restricted wiki viewer token (labels require --scope read;
# the token sees unlabeled content plus the listed ACL labels)
anda-cli --space-id my_space --token $CWT_TOKEN management add-token \
  --name hr-viewer --scope read --labels hr,finance

# Revoke a space token by full value
anda-cli --space-id my_space --token $CWT_TOKEN management revoke-token ST_xxx

# Revoke by unique token name (list-tokens only echoes a token prefix, so use
# --name when the full value was not saved at mint time)
anda-cli --space-id my_space --token $CWT_TOKEN management revoke-token --name hr-viewer

# Update space info
anda-cli --space-id my_space --token $CWT_TOKEN management update-space --name "My Space" --public

# Update wiki settings: enable WikiDigest extraction, audit external wiki
# reads, and set namespace default ACL labels (namespace=label pairs; the
# map is replaced as a whole — pass --wiki-acl-defaults "" to clear it)
anda-cli --space-id my_space --token $CWT_TOKEN management update-space \
  --wiki-digest \
  --wiki-audit-reads \
  --wiki-acl-defaults internal=staff,hr=hr

# Restart formation for a conversation
anda-cli --space-id my_space --token $CWT_TOKEN management restart-formation --conversation 42

# Get BYOK configuration
anda-cli --space-id my_space --token $CWT_TOKEN management get-byok

# Update BYOK configuration. --api-key accepts a literal value, @file/path
# (recommended: keeps the key out of shell history and `ps`), or the
# ANDA_BYOK_API_KEY environment variable as its default.
anda-cli --space-id my_space --token $CWT_TOKEN management update-byok \
  --family anthropic \
  --model claude-opus-4-6 \
  --api-base https://api.anthropic.com/v1 \
  --api-key @./api_key.txt \
  --stream \
  --context-window 200000 \
  --max-output 8192

# Or via environment variable:
export ANDA_BYOK_API_KEY=sk-xxx
anda-cli --space-id my_space --token $CWT_TOKEN management update-byok \
  --family anthropic --model claude-opus-4-6 --api-base https://api.anthropic.com/v1
```

### Admin (requires platform admin auth)

```bash
# Create a space
anda-cli --token $ADMIN_TOKEN admin create-space --user owner_id --space-id new_space --tier 1

# Update space tier
anda-cli --token $ADMIN_TOKEN admin update-tier --user owner_id --space-id my_space --tier 2
```
