import { useEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import * as Dialog from '@radix-ui/react-dialog'
import type { AgentProfile, AgentRunView, ThreadSnapshot } from '@contracts'
import { AppDialogContent, AppDialogFooter, AppDialogHeader } from './AppDialog'
import { ContinueAgentRunButton } from './ContinueAgentRunButton'
import { ExecutionStopIcon } from './ExecutionIcons'
import { ExecutionStatusGlyph } from './ExecutionStatusGlyph'
import { ToolCallIcon } from './ExecutionToolGroup'
import { MemberAvatar } from './MemberAvatar'
import { conversationRunPresence, runPresencePreview } from './conversation-run-presence'
import { identityColorToken } from './theme'
import { uiAttribute, useInterfaceLanguage } from './interface-language'
import { buildLiveExecutionProgress, type LiveRuntimeEvent } from './ui-model'
import { groupExecutionEventsByRunId } from './execution-events'
import { useExecutionWindow } from './useExecutionWindow'
import type { AgentRunStopViewState } from './ThreadWorkspace'
import './conversation-run-presence.css'

export function ConversationRunPresence({ snapshot, snapshotReady, messages, precedingOutputRunId, profiles, eventsByRunId, windowedEvidence, stopState, onStop, onOpen, onError }: {
  snapshot: ThreadSnapshot
  snapshotReady: boolean
  messages: ThreadSnapshot['messages']
  precedingOutputRunId?: string | null
  profiles: readonly AgentProfile[]
  eventsByRunId: ReadonlyMap<string, LiveRuntimeEvent[]>
  windowedEvidence: boolean
  stopState(run: AgentRunView): AgentRunStopViewState
  onStop(run: AgentRunView): Promise<void>
  onOpen(agentId: string, trigger: HTMLButtonElement, options: { runId: string }): void
  onError(message: string): void
}): React.JSX.Element | null {
  useInterfaceLanguage()
  // Immediate receipt feedback while the refreshed Core projection is in flight.
  // Re-entry is owned by run.hasAcceptedContinuation, not this component Set.
  const [continuedRunIds, setContinuedRunIds] = useState<ReadonlySet<string>>(() => new Set())
  const [stopId, setStopId] = useState<string | null>(null)
  const [submittingId, setSubmittingId] = useState<string | null>(null)
  const submitting = useRef(false)
  const stopTrigger = useRef<HTMLButtonElement | null>(null)
  const slots = useMemo(() => conversationRunPresence(snapshot.agentRuns, messages, continuedRunIds, precedingOutputRunId)
    // A cached entry preview can predate the accepted Delivery. Do not briefly
    // revive an actionable failure while the authoritative open is pending.
    .filter(slot => snapshotReady || slot.run.status !== 'failed'),
    [snapshot.agentRuns, snapshotReady, messages, continuedRunIds, precedingOutputRunId])
  const target = snapshot.agentRuns.find(run => run.id === stopId)
  const canConfirm = Boolean(target && stopState(target) === 'available')
  const memberName = (run: AgentRunView): string => snapshot.members.find(member => member.agentId === run.agentId)?.displayName
    ?? profiles.find(profile => profile.agentId === run.agentId)?.displayName ?? run.agentId

  useEffect(() => {
    if (stopId && !canConfirm && !submittingId) setStopId(null)
  }, [stopId, canConfirm, submittingId])

  const confirmStop = async (): Promise<void> => {
    if (!target || !canConfirm || submitting.current) return
    submitting.current = true
    setSubmittingId(target.id)
    try {
      // Resolve the same ID against the latest snapshot. The existing command owns CAS and uncertainty.
      await onStop(target)
      setStopId(null)
    } catch {
      onError(uiAttribute('停止请求未成功，请重试。'))
    } finally {
      submitting.current = false
      setSubmittingId(null)
    }
  }

  return <>
    {slots.length > 0 && <div className={`conversation-run-presence-tail${slots[0].continuation ? ' has-continuation' : ''}`}>
      {slots.map(({ run, continuation }) => {
        const name = memberName(run)
        const member = snapshot.members.find(member => member.agentId === run.agentId)
        const profile = profiles.find(profile => profile.agentId === run.agentId)
        const state = submittingId === run.id ? 'stopping' : stopState(run)
        const canStop = state === 'available' || state === 'stopping' || state === 'confirming'
        const canRetry = run.status === 'failed' && run.invocationKind === 'batch' && (run.inputMessageIds?.length ?? 0) > 0
        const description = run.status === 'failed' ? uiAttribute('执行中断，查看原因')
          : uiAttribute('查看执行过程')
        return <article key={run.id} className="conversation-bubble agent conversation-run-presence"
          data-presence-run={run.id} data-presence-state={run.status} data-continuation={continuation}
          aria-label={`${name}，${description}`} style={{ '--agent-accent': identityColorToken(run.agentId) } as CSSProperties}>
          {!continuation && <MemberAvatar agentId={run.agentId} avatarRef={member?.avatarRef ?? profile?.avatarRef ?? null}
            displayName={name} size="list" decorative />}
          <div className="message-body">
            {!continuation && <div className="bubble-meta"><strong>{name}</strong></div>}
            <div className="run-presence-line">
              <RunPresenceOpen threadId={snapshot.thread.id} run={run} name={name} description={description}
                windowedEvidence={windowedEvidence} events={eventsByRunId.get(run.id)} onOpen={onOpen} />
              {(canStop || canRetry) && <span className="run-presence-guide" aria-hidden="true" />}
              {canStop && <button className="run-presence-stop" type="button" disabled={state !== 'available'}
                title={state === 'confirming' ? uiAttribute('正在确认停止结果') : uiAttribute('停止{0}的本次执行', name)}
                aria-label={state === 'stopping' ? uiAttribute('正在提交停止请求…') : state === 'confirming'
                  ? uiAttribute('正在确认停止结果') : uiAttribute('停止{0}的本次执行', name)}
                aria-busy={state === 'stopping' || state === 'confirming' || undefined}
                onClick={event => { stopTrigger.current = event.currentTarget; setStopId(run.id) }}>
                <ExecutionStopIcon />
              </button>}
              {canRetry &&
                <ContinueAgentRunButton threadId={snapshot.thread.id} agentRunId={run.id} appearance="retry"
                  onError={onError} onApplied={() => setContinuedRunIds(current => new Set(current).add(run.id))} />}
            </div>
          </div>
        </article>
      })}
    </div>}
    <Dialog.Root open={stopId !== null} onOpenChange={open => { if (!open && !submitting.current) setStopId(null) }}>
      <AppDialogContent className="run-presence-stop-dialog" onCloseAutoFocus={event => {
        event.preventDefault()
        if (stopTrigger.current?.isConnected) stopTrigger.current.focus()
      }} onEscapeKeyDown={event => { if (submitting.current) event.preventDefault() }}
        onPointerDownOutside={event => { if (submitting.current) event.preventDefault() }}>
        <AppDialogHeader title={uiAttribute('停止这次执行？')}
          description={uiAttribute('只停止{0}的这次执行，已经发送的消息会保留。', target ? memberName(target) : '')}
          closeDisabled={submittingId !== null} />
        <AppDialogFooter>
          <button className="quiet-button" data-dialog-autofocus disabled={submittingId !== null} onClick={() => setStopId(null)}>{uiAttribute('取消')}</button>
          <button className="danger-button" disabled={!canConfirm || submittingId !== null} onClick={() => void confirmStop()}>
            {uiAttribute(submittingId ? '正在提交停止请求…' : '停止执行')}
          </button>
        </AppDialogFooter>
      </AppDialogContent>
    </Dialog.Root>
  </>
}

function RunPresenceOpen({ threadId, run, name, description, windowedEvidence, events, onOpen }: {
  threadId: string
  run: AgentRunView
  name: string
  description: string
  windowedEvidence: boolean
  events?: LiveRuntimeEvent[]
  onOpen(agentId: string, trigger: HTMLButtonElement, options: { runId: string }): void
}): React.JSX.Element {
  const tail = useExecutionWindow(windowedEvidence && run.status === 'running', threadId, run, events, null, true)
  const progress = useMemo(() => {
    // Reconcile evidence by identity/revision before projecting a line. A late
    // restored page must not replace newer live text or a newly started command.
    const merged = groupExecutionEventsByRunId([{ id: run.id }], tail.evidence, windowedEvidence ? events?.slice(-48) ?? [] : events ?? [])
    return buildLiveExecutionProgress(merged.get(run.id) ?? [], run.id, { includePublicResults: false })
  }, [tail.evidence, events, run.id, windowedEvidence])
  const preview = runPresencePreview(run, progress)
  return <button className={`run-presence-open is-${preview.kind}`} type="button" data-preview-kind={preview.kind}
    title={uiAttribute('查看执行过程')} aria-label={`${name}，${preview.text}，${description}`}
    onClick={event => onOpen(run.agentId, event.currentTarget, { runId: run.id })}>
    <span className={`run-presence-status is-${run.status}`} aria-hidden="true"><ExecutionStatusGlyph status={run.status} /></span>
    {preview.iconKind && <ToolCallIcon iconKind={preview.iconKind} />}
    <span className={`run-presence-preview is-${preview.kind}`} aria-hidden="true">{preview.text}</span>
  </button>
}
