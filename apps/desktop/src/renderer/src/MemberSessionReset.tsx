import { useEffect, useState } from 'react'
import * as Dialog from '@radix-ui/react-dialog'
import { AppDialogContent, AppDialogFooter, AppDialogHeader } from './AppDialog'
import { useThreadClient } from './camp-client'
import { uiAttribute as t, useInterfaceLanguage } from './interface-language'
import { readErrorMessage } from './error-message'

type Session = { conversationId: string; version: number; active: boolean }

export function MemberSessionReset({ campId, member, onClose, onNotify }: {
  campId: string; member: { agentId: string; displayName: string }
  onClose(): void; onNotify(message: string): void
}): React.JSX.Element {
  useInterfaceLanguage()
  const client = useThreadClient()
  const [session, setSession] = useState<Session | null>(null)
  const [loading, setLoading] = useState(true)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState('')
  useEffect(() => {
    let current = true
    void client.request<Session | null>('conversations.memberSession.get', { campId, agentId: member.agentId })
      .then(value => { if (current) { setSession(value); setLoading(false) } })
      .catch(error => { if (current) { setError(readErrorMessage(error)); setLoading(false) } })
    return () => { current = false }
  }, [client, campId, member.agentId])
  const reset = async (): Promise<void> => {
    if (!session || pending) return
    setPending(true); setError('')
    try {
      const result = await client.request<{ status: string; code: string }>('conversations.restartNativeSession', {
        commandId: crypto.randomUUID(), command: { conversationId: session.conversationId, expectedVersion: session.version }
      })
      if (result.status !== 'applied') throw new Error(result.code === 'conversation.active_run'
        ? t('队员正在执行任务，请结束后再重置会话。') : t('会话已发生变化，请关闭后重新打开。'))
      onNotify(t('已重置{0}的会话。', member.displayName)); onClose()
    } catch (error) { setError(readErrorMessage(error)) } finally { setPending(false) }
  }
  return <Dialog.Root open onOpenChange={open => { if (!open && !pending) onClose() }}>
    <Dialog.Portal><Dialog.Overlay className="dialog-overlay app-dialog-overlay"/>
      <AppDialogContent aria-describedby="member-session-reset-description">
        <AppDialogHeader title={t('重置{0}的会话', member.displayName)} descriptionId="member-session-reset-description"
          description={t('下次启动将建立新的队员会话，不再续接原生会话历史。Camp 中的消息保留。')}/>
        {loading ? <p>{t('正在读取会话…')}</p> : session?.active ? <p>{t('队员正在执行任务，请结束后再重置会话。')}</p>
          : !session && !error ? <p>{t('该队员尚未建立会话。')}</p> : null}
        {error && <p className="environment-error" role="alert">{error}</p>}
        <AppDialogFooter>
          <button type="button" className="quiet-button" disabled={pending} onClick={onClose}>{t('取消')}</button>
          <button type="button" className="danger-button" disabled={loading || pending || !session || session.active} onClick={() => void reset()}>{pending ? t('正在重置…') : t('重置会话')}</button>
        </AppDialogFooter>
      </AppDialogContent>
    </Dialog.Portal>
  </Dialog.Root>
}
