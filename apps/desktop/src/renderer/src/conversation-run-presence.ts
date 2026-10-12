import type { AgentRunView, ThreadMessageView } from '@contracts'
import { agentRunPresentation, type ActivityIconKind, type LiveExecutionProgress } from './ui-model'
import { uiAttribute } from './interface-language'

export interface ConversationRunPresenceSlot {
  run: AgentRunView
  continuation: boolean
}

/** Live activity follows public message sequence, never its input or wall clock. */
export function conversationRunPresence(
  runs: readonly AgentRunView[],
  messages: readonly ThreadMessageView[],
  continuedRunIds: ReadonlySet<string>,
  precedingOutputRunId?: string | null
): ConversationRunPresenceSlot[] {
  const publicRuns = runs.filter(run => run.invocationKind !== 'single_chat')
  const latestByAgent = new Map<string, AgentRunView>()
  for (const run of publicRuns) {
    if (run.status === 'queued') continue
    const previous = latestByAgent.get(run.agentId)
    if (!previous || run.createdAt > previous.createdAt
      || run.createdAt === previous.createdAt && run.id > previous.id) latestByAgent.set(run.agentId, run)
  }
  const lastMessage = messages.reduce<ThreadMessageView | null>((last, message) =>
    !last || message.sequence > last.sequence ? message : last, null)
  const lastRunId = precedingOutputRunId !== undefined ? precedingOutputRunId
    : !lastMessage?.withdrawn && lastMessage?.authorType === 'agent' ? lastMessage.sourceAgentRunId : null
  return publicRuns.filter(run => run.status === 'running'
    || run.status === 'waiting' && run.startedAt !== null
    || run.status === 'failed' && run.invocationKind === 'batch'
      && latestByAgent.get(run.agentId)?.id === run.id
      && !run.hasAcceptedContinuation && !continuedRunIds.has(run.id))
    .sort((a, b) => Number(b.id === lastRunId) - Number(a.id === lastRunId)
      || (a.startedAt ?? a.createdAt).localeCompare(b.startedAt ?? b.createdAt)
      || a.id.localeCompare(b.id))
    .map((run, index) => ({ run, continuation: index === 0 && run.id === lastRunId }))
}

export interface RunPresencePreview {
  kind: 'narration' | 'command' | 'status' | 'error'
  text: string
  iconKind?: ActivityIconKind
}

const singleLine = (text: string): string => text.replace(/\s+/g, ' ').trim()

export function runPresencePreview(run: AgentRunView, progress?: LiveExecutionProgress): RunPresencePreview {
  if (run.status === 'failed') {
    const disconnected = run.failure?.code === 'runtime_network_interrupted'
      || run.failure?.code === 'runtime_disconnected'
    return { kind: 'error', text: disconnected ? uiAttribute('连接中断 · 查看原因') : uiAttribute('执行中断 · 查看原因') }
  }
  if (run.cancelRequestedAt) return { kind: 'status', text: uiAttribute('正在停止…') }
  if (run.status === 'waiting') return { kind: 'status', text: uiAttribute(agentRunPresentation(run).label) }
  const latest = progress?.items.findLast(item => item.kind === 'tool' || item.kind === 'narration' && item.body.trim())
  if (latest?.kind === 'tool') return {
    kind: 'command',
    text: singleLine(latest.step.publicCommand || latest.step.currentInstruction || latest.step.title),
    iconKind: latest.step.publicCommand && latest.step.iconKind === 'unknown' ? 'terminal' : latest.step.iconKind
  }
  if (latest?.kind === 'narration') return { kind: 'narration', text: singleLine(latest.body) }
  return { kind: 'status', text: progress?.runtimeThinkingTitle || uiAttribute('正在处理…') }
}
