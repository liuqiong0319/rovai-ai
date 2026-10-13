import React, { useState } from 'react'
import { createRoot } from 'react-dom/client'
import { MemberAvatarEditor } from '../../../apps/desktop/src/renderer/src/MemberAvatarEditor'
import { MemberAvatar } from '../../../apps/desktop/src/renderer/src/MemberAvatar'
import { ThreadClientProvider, type ThreadClient } from '../../../apps/desktop/src/renderer/src/camp-client'
import { browserMemberAvatars } from '../../../apps/web/src/member-avatars'
import { initializeInterfaceLanguage } from '../../../apps/desktop/src/renderer/src/interface-language'
import '../../../apps/desktop/src/renderer/src/styles.css'
import '../../../apps/desktop/src/renderer/src/member-editor.css'

declare global {
  interface Window {
    avatarFixture: {
      request(method: string, params: unknown): Promise<any>
    }
  }
}

// The real browser upload adapter and editor talk to an isolated Core. Chromium
// supplies the deterministic file selection from the acceptance harness.
const memberAvatars = browserMemberAvatars({
  avatar: (operation: string, params: unknown) => window.avatarFixture.request(`memberAvatars.${operation}`, params)
} as Parameters<typeof browserMemberAvatars>[0])
const client = { memberAvatars } as ThreadClient

function Fixture(): React.JSX.Element {
  const [value, setValue] = useState<string | null>(() => localStorage.getItem('avatar'))
  const [open, setOpen] = useState(!value)
  return <ThreadClientProvider client={client}>
    <main style={{ width: 640, margin: '16px auto' }}>
      <button data-reopen onClick={() => setOpen(true)}>重新裁剪</button>
      <output data-avatar-ref>{value}</output>
      <MemberAvatar agentId="fixture" avatarRef={value} displayName="Fixture" size="picker" />
      <MemberAvatarEditor value={value} open={open} disabled={false} onPendingChange={() => {}}
        onChange={next => { setValue(next); if (next) localStorage.setItem('avatar', next) }} onClose={() => setOpen(false)} />
    </main>
  </ThreadClientProvider>
}

initializeInterfaceLanguage({ interfaceLanguage: 'zh-CN' })
createRoot(document.getElementById('root')!).render(<Fixture />)
