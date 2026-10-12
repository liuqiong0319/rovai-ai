import { describe, expect, it } from 'vitest'
import type { AgentRunExecutionEvidenceView } from '@contracts'
import { groupExecutionEventsByRunId } from './execution-events'
import { buildLiveExecutionProgress, liveRuntimeEventFromExecutionEvidence } from './ui-model'

const narration = (revision: number, text: string, status = 'streaming'): AgentRunExecutionEvidenceView => ({
  id: 'text', agentRunId: 'run', executionEpoch: 1, sequence: 1, operationId: null,
  revision, changeSequence: revision, eventType: 'agent.text.block', kind: 'narration', phase: 'updated',
  payload: { blockId: 'text', status, text, textLength: text.length },
  contentBlobId: null, contentByteCount: text.length, isTruncated: false, occurredAt: '2026-10-11T00:00:00Z'
})

describe('restored and live execution progress', () => {
  it('keeps newer live narration across a late initial page, including uncheckpointed streaming growth', () => {
    for (const revision of [1, 2]) {
      const events = groupExecutionEventsByRunId([{ id: 'run' }], [narration(1, 'old')], [
        liveRuntimeEventFromExecutionEvidence(narration(revision, 'old and new live text'))
      ]).get('run')!
      expect(buildLiveExecutionProgress(events, 'run').items).toEqual([
        { key: 'narration:text', kind: 'narration', body: 'old and new live text' }
      ])
    }
  })

  it('keeps newer restored evidence over an older live buffer, even an older terminal event', () => {
    const restored = narration(3, 'current')
    for (const status of ['streaming', 'completed']) {
      const events = groupExecutionEventsByRunId([{ id: 'run' }], [restored], [
        liveRuntimeEventFromExecutionEvidence(narration(2, 'stale but longer text', status))
      ]).get('run')!
      expect(events).toEqual([liveRuntimeEventFromExecutionEvidence(restored)])
    }
  })

  it('keeps new live activity after a restored command instead of selecting the entire restored projection', () => {
    const command = { ...narration(1, ''), id: 'command', kind: 'command' as const, eventType: 'activity.started',
      payload: { activityId: 'command', activityType: 'shell', title: 'old command' } }
    const next = { ...narration(2, 'new live output'), occurredAt: '2026-10-11T00:00:01Z' }
    const events = groupExecutionEventsByRunId([{ id: 'run' }], [command], [liveRuntimeEventFromExecutionEvidence(next)]).get('run')!
    expect(events.map(event => event.id)).toEqual(['command', 'text'])
    expect(buildLiveExecutionProgress(events, 'run').items.at(-1)).toEqual({
      key: 'narration:text', kind: 'narration', body: 'new live output'
    })
  })
})
