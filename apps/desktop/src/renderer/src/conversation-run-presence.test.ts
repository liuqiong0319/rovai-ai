import { describe, expect, it } from 'vitest'
import type { AgentRunView, ThreadMessageView } from '@contracts'
import { conversationRunPresence, runPresencePreview } from './conversation-run-presence'
import { buildLiveExecutionProgress, type LiveRuntimeEvent } from './ui-model'

const run = (id: string, agentId = id, patch: Partial<AgentRunView> = {}): AgentRunView => ({
  id, agentId, status: 'running', invocationKind: 'batch', createdAt: '2026-10-10T12:00:00Z',
  startedAt: '2026-10-10T12:00:01Z', cancelRequestedAt: null, failure: null, inputMessageIds: ['input'], ...patch
} as AgentRunView)
const message = (sequence: number, sourceAgentRunId: string | null, authorType = 'agent'): ThreadMessageView => ({
  id: `message-${sequence}`, sequence, sourceAgentRunId, authorType, withdrawn: false
} as ThreadMessageView)
const project = (runs: AgentRunView[], messages: ThreadMessageView[], continued = new Set<string>()) =>
  conversationRunPresence(runs, messages, continued).map(slot => [slot.run.id, slot.continuation])

describe('conversation Run presence', () => {
  it('follows the highest public sequence, across interleaved and same-member different Runs', () => {
    const a = run('a', 'alice'), b = run('b', 'audrey')
    expect(project([a, b], [message(1, 'a')])).toEqual([['a', true], ['b', false]])
    expect(project([a, b], [message(2, 'b'), message(1, 'a')])).toEqual([['b', true], ['a', false]])
    expect(project([a], [message(2, 'other-alice-run'), message(1, 'a')])).toEqual([['a', false]])
    expect(project([a], [message(3, 'a'), message(2, 'other-alice-run')])).toEqual([['a', true]])
    expect(project([a], [message(4, null, 'user'), message(3, 'a')])).toEqual([['a', false]])
  })

  it('omits queues, private Runs and successful/stopped Runs; keeps a started approval wait', () => {
    const runs = [run('queued', '', { status: 'queued' }), run('unstarted', '', { status: 'waiting', startedAt: null }),
      run('private', '', { invocationKind: 'single_chat' }), run('done', '', { status: 'succeeded' }),
      run('stopped', '', { status: 'cancelled' }), run('approval', 'alice', { status: 'waiting', waitReason: 'approval' })]
    expect(project(runs, [])).toEqual([['approval', false]])
    expect(runPresencePreview(runs[5]).text).toBe('等待审批')
  })

  it('continues an adjacent artifact author but restores identity after a different output or Task', () => {
    const failed = run('a', 'alice', { status: 'failed' })
    expect(conversationRunPresence([failed], [], new Set(), 'a')[0].continuation).toBe(true)
    expect(conversationRunPresence([failed], [message(1, 'a')], new Set(), 'b')[0].continuation).toBe(false)
    expect(conversationRunPresence([failed], [message(1, 'a')], new Set(), null)[0].continuation).toBe(false)
  })

  it('does not append every historical failure or resurrect a retried failure', () => {
    const failed = run('failed', 'alice', { status: 'failed' })
    const next = run('next', 'alice', { createdAt: '2026-10-10T12:01:00Z' })
    expect(project([failed], [])).toEqual([['failed', false]])
    expect(project([failed], [], new Set(['failed']))).toEqual([])
    expect(project([failed, next], [])).toEqual([['next', false]])
    expect(project([failed, { ...next, status: 'succeeded' }], [])).toEqual([])
    expect(project([failed, { ...next, status: 'queued' }], [])).toEqual([['failed', false]])
    // A fresh component has no local acknowledgement. The persisted Delivery
    // fact suppresses the source even before a successor Run exists.
    const accepted = { ...failed, hasAcceptedContinuation: true }
    expect(project([accepted], [])).toEqual([])
    expect(project([accepted, { ...next, status: 'queued' }], [])).toEqual([])
    expect(project([accepted, { ...next, status: 'failed' }], [])).toEqual([['next', false]])
    expect(project([{ ...failed, invocationKind: 'direct' }], [])).toEqual([])
    expect(failed.status).toBe('failed')
  })

  it('uses real incremental narration and the canonical command and icon from the Run card', () => {
    const event = (id: string, payload: unknown, eventType = 'agent.text.delta'): LiveRuntimeEvent => ({
      id, agentRunId: 'a', eventType, payload, createdAt: `2026-10-10T12:00:0${id}Z`
    } as LiveRuntimeEvent)
    const text = event('1', { itemId: 'text', delta: '我先检查\n消息结构' })
    const first = buildLiveExecutionProgress([text], 'a', { includePublicResults: false })
    expect(runPresencePreview(run('a'), first)).toMatchObject({ kind: 'narration', text: '我先检查 消息结构' })
    const streamed = buildLiveExecutionProgress([text, event('2', { itemId: 'text', delta: '，再核对按钮。' })], 'a')
    expect(runPresencePreview(run('a'), streamed).text).toBe('我先检查 消息结构，再核对按钮。')
    const command = event('3', { item: { id: 'command', type: 'commandExecution', command: 'pnpm typecheck', status: 'inProgress' } }, 'activity.started')
    const progress = buildLiveExecutionProgress([text, command], 'a', { includePublicResults: false })
    expect(runPresencePreview(run('a'), progress)).toMatchObject({ kind: 'command', text: 'pnpm typecheck', iconKind: 'terminal' })
    expect(runPresencePreview(run('a', 'alice', { status: 'failed' }), progress).kind).toBe('error')
  })
})
