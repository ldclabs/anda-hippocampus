import type { OpenClawPluginApi, AnyAgentTool } from 'openclaw/plugin-sdk'
import packageJson from '../package.json'
import { BrainClient } from './client.ts'
import type { BrainPluginConfig, InputContext, Message } from './types.ts'

// ---------------------------------------------------------------------------
// Message conversion: AgentMessage → Brain Message
// ---------------------------------------------------------------------------

function extractTextContent(content: unknown): string {
  if (typeof content === 'string') return content
  if (!Array.isArray(content)) return ''
  return content
    .map((part: { type?: string; text?: string }) => {
      if (typeof part === 'string') return part
      if (part.type === 'text' && typeof part.text === 'string')
        return part.text
      return ''
    })
    .filter(Boolean)
    .join('\n')
}

function convertAgentMessages(agentMessages: unknown[]): Message[] {
  const result: Message[] = []
  for (const msg of agentMessages) {
    if (typeof msg !== 'object' || msg === null) continue
    const m = msg as Record<string, unknown>
    const role = m['role'] as string | undefined

    if (role === 'user') {
      const text = extractTextContent(m['content'])
      if (text) {
        result.push({
          role: 'user',
          content: text,
          timestamp:
            typeof m['timestamp'] === 'number' ? m['timestamp'] : undefined
        })
      }
    } else if (role === 'assistant') {
      const parts = m['content']
      const text = extractTextContent(parts)
      if (text) {
        result.push({
          role: 'assistant',
          content: text,
          timestamp:
            typeof m['timestamp'] === 'number' ? m['timestamp'] : undefined
        })
      }
    }
    // Skip toolResult and custom messages — not useful for formation
  }
  return result
}

function normalizeInputContext(value: unknown): Partial<InputContext> {
  if (value == null) return {}

  if (typeof value === 'string') {
    const text = value.trim()
    if (!text) return {}
    try {
      return normalizeInputContext(JSON.parse(text))
    } catch {
      return {}
    }
  }

  if (typeof value !== 'object' || Array.isArray(value)) return {}

  const record = value as Record<string, unknown>
  const context: Partial<InputContext> = {}
  const setString = (field: keyof InputContext, raw: unknown) => {
    if (typeof raw === 'string' && raw.trim()) context[field] = raw.trim()
  }

  setString('counterparty', record['counterparty'] ?? record['user'])
  setString('agent', record['agent'])
  setString('source', record['source'])
  setString('topic', record['topic'])

  return context
}

const andaBrainPlugin = {
  id: 'anda-brain',
  name: 'Anda Brain',
  description:
    'Autonomous graph memory for OpenClaw agents. Encodes conversations to a knowledge graph and provides recall_memory tool to retrieve memory in natural language. https://brain.anda.ai/',
  version: packageJson.version,

  register(api: OpenClawPluginApi) {
    const config = (api.pluginConfig ?? {}) as unknown as BrainPluginConfig
    if (config.spaceId == null || config.spaceToken == null) {
      api.logger.error(
        '[anda-brain] Invalid configuration: spaceId and spaceToken are required. You can obtain them at https://anda.ai/brain'
      )
      return
    }

    const client = new BrainClient(config)
    const defaultContext = normalizeInputContext(config.defaultContext)
    // ── recall_memory tool ──────────────────────────────────────────
    const recallTool: AnyAgentTool = {
      name: 'recall_memory',
      label: 'Recall Memory',
      description:
        "Recall information from the assistant's long-term memory (the Cognitive Nexus owned by $self). Use only for information that is not already present in the active conversation. Do not call for facts just mentioned, just submitted to formation, or otherwise available in current context; formation is asynchronous and fresh memories may take a minute or more to become searchable.",
      parameters: {
        'type': 'object',
        'properties': {
          'query': {
            'type': 'string',
            'description':
              "A natural language question about older or out-of-context memory. Be specific and include the subject, timeframe, and topic when known. Examples: 'What do we know about the current user's communication preferences?', 'What happened in our last discussion about Project Aurora?', 'Who are the members of the engineering team?'"
          },
          'context': {
            'type': 'object',
            'description':
              "Optional current conversational context used only to disambiguate the query within $self's memory. Pass an object, not a JSON string. It does not change the memory owner.",
            'properties': {
              'counterparty': {
                'type': 'string',
                'description':
                  "Preferred. Durable identifier of the current external person or organization interacting with the business agent. Useful for resolving implicit references such as 'the current user', 'they', or omitted subjects."
              },
              'agent': {
                'type': 'string',
                'description':
                  'The identifier of the calling business agent, if applicable. Useful for provenance or caller-specific queries, but it does not change whose memory is searched.'
              },
              'source': {
                'type': 'string',
                'description':
                  'Identifier of the current source, thread, channel, or app context. Useful when the query refers to a previous discussion in the same place.'
              },
              'topic': {
                'type': 'string',
                'description':
                  'The topic of the current conversation, to help disambiguate the query.'
              }
            }
          }
        },
        'required': ['query']
      },

      async execute(
        _toolCallId: string,
        params: { query?: string; context?: Partial<InputContext> | string }
      ) {
        const query = params.query?.trim()
        if (!query) {
          return {
            content: [
              {
                type: 'text' as const,
                text: 'Error: "query" parameter is required and must be a non-empty string.'
              }
            ],
            details: { error: true }
          }
        }

        const callContext: InputContext = {
          ...defaultContext,
          ...normalizeInputContext(params.context)
        }

        const res = await client.recall(query, callContext)
        if (res.error) {
          api.logger.error(`[anda-brain] Recall failed: ${res.error.message}`)
          return {
            content: [
              {
                type: 'text' as const,
                text: `Error recalling memory: ${res.error.message}`
              }
            ],
            details: { error: true }
          }
        } else {
          api.logger.debug?.(
            `[anda-brain] Recall successful: ${JSON.stringify(res.result)}.`
          )
        }

        return {
          content: [
            {
              type: 'text' as const,
              text: res.result?.content ?? 'No relevant memory found.'
            }
          ],
          details: {
            conversation: res.result?.conversation,
            model: res.result?.model
          }
        }
      }
    }

    api.registerTool(recallTool)

    // ── agent_end hook → formation (fire-and-forget) ────────────────
    api.on('agent_end', (event, ctx) => {
      const originalMessages = (event.messages as unknown[]) ?? []
      const messages = convertAgentMessages(originalMessages)
      api.logger.info(
        `[anda-brain] agent_end: extracted ${messages.length} messages for formation.`
      )

      if (messages.length === 0) return
      const context: InputContext = {
        ...defaultContext,
        agent: ctx.agentId || defaultContext?.agent,
        source: ctx.sessionKey || ctx.channelId || defaultContext?.source
      }
      client
        .formation(messages, context)
        .then((res) => {
          api.logger.info(
            `[anda-brain] Formation completed: ${JSON.stringify(res)}.`
          )
        })
        .catch((err) => {
          api.logger.error(
            `[anda-brain] Formation failed: ${JSON.stringify(err)}`
          )
        })
    })

    api.logger.info(
      '[anda-brain] Plugin registered successfully. Ready to encode memories and handle recall_memory tool calls.'
    )
  }
}

export default andaBrainPlugin
