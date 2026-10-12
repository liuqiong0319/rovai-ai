import { describe, expect, it } from 'vitest'
import type { AgentRunView, ThreadMessageView } from '@contracts'
import { conversationRunPresenceTimeline, runPresencePreview } from './conversation-run-presence'
import type { ThreadConversationTimelineItem } from './ThreadWorkspace'
import { buildLiveExecutionProgress, type LiveRuntimeEvent } from './ui-model'

const run = (id: string, agentId = id, patch: Partial<AgentRunView> = {}): AgentRunView => ({
  id, agentId, status: 'running', invocationKind: 'batch', createdAt: '2026-10-10T12:00:00Z',
  startedAt: '2026-10-10T12:00:01Z', cancelRequestedAt: null, failure: null, inputMessageIds: ['message-1'],
  anchorMessageId: 'message-1', ...patch
} as AgentRunView)
const message = (sequence: number, sourceAgentRunId: string | null, patch: Partial<ThreadMessageView> = {}): ThreadConversationTimelineItem => ({
  kind: 'camp_message', id: patch.id ?? 'message-' + sequence, createdAt: '2026-10-10T12:00:00Z', runtimeImageGroups: [],
  message: { id: 'message-' + sequence, sequence, sourceAgentRunId, authorType: sourceAgentRunId ? 'agent' : 'user',
    withdrawn: false, createdAt: '2026-10-10T12:00:00Z', ...patch } as ThreadMessageView
})
const project = (runs: AgentRunView[], timeline: ThreadConversationTimelineItem[], ready = true) =>
  conversationRunPresenceTimeline(timeline, runs, ready)
const order = (runs: AgentRunView[], timeline: ThreadConversationTimelineItem[]) => project(runs, timeline).map(item => item.id)
const slots = (runs: AgentRunView[], timeline: ThreadConversationTimelineItem[] = [], ready = true) =>
  project(runs, timeline, ready).flatMap(item => item.kind === 'run_presence' ? [[item.slot.run.id, item.slot.continuation]] : [])

describe('conversation Run presence', () => {
  it('pins parallel Runs before their own send and advances only the successful sender', () => {
    const a = run('a'), b = run('b'), start = [message(1, null)]
    expect(order([b,a], start)).toEqual(['message-1','run-presence:a','run-presence:b'])
    const peerSend = [...start, message(2, 'b')]
    expect(order([a,b], peerSend)).toEqual(['message-1','run-presence:a','message-2','run-presence:b'])
    expect(slots([a,b], peerSend)).toEqual([['a',false],['b',true]])
    const ownSend = [...peerSend, message(3, 'a')]
    expect(order([a,b], ownSend)).toEqual(['message-1','message-2','run-presence:b','message-3','run-presence:a'])
    expect(order([a,b], [...ownSend, message(4, 'review')])).toEqual([
      'message-1','message-2','run-presence:b','message-3','run-presence:a','message-4'
    ])
    expect(order([a,b], [...ownSend, message(4, 'review'), message(5, 'a')])).toEqual([
      'message-1','message-2','run-presence:b','message-3','message-4','message-5','run-presence:a'
    ])
  })

  it('uses publication sequence and exact Run identity, excluding uncommitted or withdrawn output', () => {
    const a = run('a','alice')
    const messages = [message(1,null), message(2,'a'), message(3,'other-alice-run'),
      message(4,'a',{createdAt:'2000-01-01T00:00:00Z'}), message(5,'a',{withdrawn:true}),
      message(6,'a',{id:'optimistic:send'}), message(7,null)]
    expect(order([a],messages)).toEqual(['message-1','message-2','message-3','message-4','run-presence:a','message-5','optimistic:send','message-7'])
    expect(slots([a],messages)).toEqual([['a',true]])
    expect(order([a],[message(1,null),message(2,'other-alice-run'),message(3,null)])).toEqual([
      'message-1','run-presence:a','message-2','message-3'
    ])
  })

  it('restores an unsent Run from its input anchor, including late paged history', () => {
    const a = run('a')
    const later = message(2,'peer',{createdAt:'2026-10-10T12:05:00Z'})
    expect(order([a],[message(1,null),later,message(3,null)])).toEqual(['message-1','run-presence:a','message-2','message-3'])
    expect(order([a],[later])).toEqual(['run-presence:a','message-2'])
    expect(order([a],[message(2,'peer',{createdAt:'2000-01-01T00:00:00Z'})])).toEqual(['run-presence:a','message-2'])
    expect(order([a],[message(1,null),later])).toEqual(['message-1','run-presence:a','message-2'])
    expect(order([run('legacy','alice',{anchorMessageId:null})],[message(1,null),later])).toEqual(['message-1','run-presence:legacy','message-2'])
  })

  it('keeps message artifacts together and continues the adjacent Run identity', () => {
    const a = run('a','alice',{status:'failed'})
    const artifact = {kind:'run_artifacts',id:'artifact-a',createdAt:a.createdAt,run:a,imageGroups:[],fileChanges:[],memberCreations:[]} as ThreadConversationTimelineItem
    expect(order([a],[message(1,null),artifact,message(2,'peer')])).toEqual(['message-1','artifact-a','run-presence:a','message-2'])
    expect(slots([a],[artifact])).toEqual([['a',true]])
    const files = {kind:'run_file_changes',id:'files-a',createdAt:a.createdAt,changes:{agentRunId:'a'}} as ThreadConversationTimelineItem
    expect(order([a],[message(1,null),message(2,'a'),files,message(3,'peer')])).toEqual(['message-1','message-2','files-a','run-presence:a','message-3'])
    expect(slots([a],[message(2,'a'),files])).toEqual([['a',true]])
    // Publishing an intermediate artifact alone does not move a running row.
    expect(order([{...a,status:'running'}],[message(1,null),message(2,'peer'),files])).toEqual([
      'message-1','run-presence:a','message-2','files-a'
    ])
    expect(order([{...a,status:'running'}],[message(1,null),files,message(2,'peer')])).toEqual([
      'message-1','run-presence:a','files-a','message-2'
    ])
  })

  it('omits queues, private and terminal Runs; preserves started approval waits', () => {
    const runs = [run('queued','',{status:'queued'}),run('unstarted','',{status:'waiting',startedAt:null}),
      run('private','',{invocationKind:'single_chat'}),run('done','',{status:'succeeded'}),
      run('stopped','',{status:'cancelled'}),run('approval','alice',{status:'waiting',waitReason:'approval'})]
    expect(slots(runs)).toEqual([['approval',false]])
    expect(runPresencePreview(runs[5]).text).toBe('等待审批')
  })

  it('keeps only the latest unacknowledged batch failure after authoritative re-entry', () => {
    const failed = run('failed','alice',{status:'failed'})
    const next = run('next','alice',{createdAt:'2026-10-10T12:01:00Z'})
    expect(slots([failed])).toEqual([['failed',false]])
    expect(slots([failed],[],false)).toEqual([])
    expect(slots([failed,next])).toEqual([['next',false]])
    expect(slots([failed,{...next,status:'succeeded'}])).toEqual([])
    expect(slots([failed,{...next,status:'queued'}])).toEqual([['failed',false]])
    expect(slots([{...failed,hasAcceptedContinuation:true},{...next,status:'queued'}])).toEqual([])
    expect(slots([{...failed,hasAcceptedContinuation:true},{...next,status:'failed'}])).toEqual([['next',false]])
    expect(slots([{...failed,invocationKind:'direct'}])).toEqual([])
  })

  it('advances only at source newlines, preserving partial lines and command precedence', () => {
    const event = (id: number, delta: string): LiveRuntimeEvent => ({
      id:String(id),agentRunId:'a',eventType:'agent.text.delta',payload:{itemId:'text',delta},createdAt:'2026-10-10T12:00:00Z'
    } as LiveRuntimeEvent)
    const events: LiveRuntimeEvent[] = []
    for (const [delta,expected] of [
      ['第一行。继续','第一行。继续'],['追加','第一行。继续追加'],['\r\n','第一行。继续追加'],
      ['\r\n第二','第二'],['行','第二行'],['\n  \n','第二行'],['第三行','第三行']
    ]) {
      events.push(event(events.length+1,delta))
      expect(runPresencePreview(run('a'),buildLiveExecutionProgress(events,'a',{includePublicResults:false}))).toEqual({kind:'narration',text:expected})
    }
    events.push({id:'command',agentRunId:'a',eventType:'activity.started',createdAt:'2026-10-10T12:01:00Z',
      payload:{item:{id:'command',type:'commandExecution',command:'pnpm typecheck',status:'inProgress'}}} as LiveRuntimeEvent)
    const progress = buildLiveExecutionProgress(events,'a',{includePublicResults:false})
    expect(runPresencePreview(run('a'),progress)).toMatchObject({kind:'command',text:'pnpm typecheck',iconKind:'terminal'})
    expect(runPresencePreview(run('a','alice',{status:'failed'}),progress).kind).toBe('error')
    expect(runPresencePreview(run('a','alice',{cancelRequestedAt:'now'}),progress)).toEqual({kind:'status',text:'正在停止…'})
  })
})
