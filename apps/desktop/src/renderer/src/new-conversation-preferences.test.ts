import { describe, expect, it } from 'vitest'
import type {
  AgentProfile,
  GeneralPreferencesSnapshot,
  NavigationThreadItem,
  NavigationSnapshot
} from '@contracts'
import {
  currentProjectAccessDecision,
  currentProjectForThread,
  currentProjectWorkspace,
  defaultsNeedInvalidation,
  forgetOpenedProject,
  navigationIncludingCurrentWorkspace,
  navigationWithOpenedProjects,
  navigationWithProjectAuthority,
  navigationWithProjectOrder,
  navigationWithoutRemovedProjects,
  parseCurrentProject,
  parseKnownProjects,
  persistKnownProjects,
  projectTargetKey,
  readKnownProjects,
  rememberOpenedProject,
  resolveNewConversationDefaults,
  resolveAvailableNewConversationDefaults,
  shouldInvalidateNewConversationDefaults
} from './new-conversation-preferences'

describe('new conversation preferences', () => {
  it('restores only exact current Project selections', () => {
    expect(parseCurrentProject(null)).toEqual({ kind: 'quick_chat' })
    expect(parseCurrentProject('{broken')).toEqual({ kind: 'quick_chat' })
    expect(parseCurrentProject(JSON.stringify({ kind: 'directory', projectPath: '/repo' })))
      .toEqual({ kind: 'directory', projectPath: '/repo' })
    expect(parseCurrentProject(JSON.stringify({ kind: 'directory', projectPath: '' })))
      .toEqual({ kind: 'quick_chat' })
  })

  it('derives the current Project from a Thread without coupling it to disclosure state', () => {
    expect(currentProjectForThread(thread('directory', '/repo')))
      .toEqual({ kind: 'directory', projectPath: '/repo' })
    expect(currentProjectForThread(thread('quick_chat', '/quick-chat')))
      .toEqual({ kind: 'quick_chat' })
  })

  it('keeps a validated current workspace visible when it has no navigable Threads', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 0, recentThreads: [] },
      projects: []
    }
    const currentProject = { kind: 'directory', projectPath: '/repo/empty-project' } as const
    const displayed = navigationIncludingCurrentWorkspace(navigation, currentProject, {
      name: 'empty-project',
      projectPath: '/repo/empty-project'
    })

    expect(displayed).not.toBe(navigation)
    expect(displayed?.projects).toEqual([{
      projectKey: 'directory:/repo/empty-project',
      name: 'empty-project',
      projectPath: '/repo/empty-project',
      lastActivityAt: '',
      lastActivityGlobalSequence: 0,
      totalCount: 0,
      recentThreads: []
    }])
    expect(currentProjectWorkspace(displayed, currentProject)).toEqual({
      name: 'empty-project',
      projectPath: '/repo/empty-project'
    })
    expect(navigationIncludingCurrentWorkspace(navigation, currentProject, {
      name: 'other',
      projectPath: '/repo/other'
    })).toBe(navigation)
  })

  it('removes only explicitly hidden directory Projects from the Renderer navigation', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 1, recentThreads: [] },
      projects: [
        project('/repo/a'),
        project('/repo/b')
      ]
    }

    const displayed = navigationWithoutRemovedProjects(
      navigation,
      new Set([projectTargetKey('/repo/a')])
    )

    expect(displayed?.quickChat).toBe(navigation.quickChat)
    expect(displayed?.projects.map((candidate) => candidate.projectPath)).toEqual(['/repo/b'])
    expect(navigation.projects).toHaveLength(2)
  })

  it('projects the persisted Project order without mutating the activity-sorted snapshot', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 0, recentThreads: [] },
      projects: [project('/repo/c'), project('/repo/a'), project('/repo/b')]
    }

    const displayed = navigationWithProjectOrder(navigation, [
      projectTargetKey('/repo/b'),
      projectTargetKey('/repo/a')
    ])

    expect(displayed?.projects.map((candidate) => candidate.projectPath))
      .toEqual(['/repo/b', '/repo/a', '/repo/c'])
    expect(navigation.projects.map((candidate) => candidate.projectPath))
      .toEqual(['/repo/c', '/repo/a', '/repo/b'])
    expect(navigationWithProjectOrder(navigation, null)).toBe(navigation)
  })

  it('appends a newly selected empty Project after the existing Project order', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 0, recentThreads: [] },
      projects: [project('/repo/existing')]
    }
    const displayed = navigationIncludingCurrentWorkspace(
      navigation,
      { kind: 'directory', projectPath: '/repo/new' },
      { name: 'new', projectPath: '/repo/new' }
    )

    expect(displayed?.projects.map((candidate) => candidate.projectPath))
      .toEqual(['/repo/existing', '/repo/new'])
  })

  it('keeps Project navigation hidden until removed authority is ready', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 1, recentThreads: [] },
      projects: [project('/repo/a'), project('/repo/b')]
    }

    expect(navigationWithProjectAuthority(navigation, new Set(), false)?.projects).toEqual([])
    expect(navigationWithProjectAuthority(
      navigation,
      new Set([projectTargetKey('/repo/a')]),
      true
    )?.projects.map((candidate) => candidate.projectPath)).toEqual(['/repo/b'])
  })

  it('does not inspect a cached Project until removed-Project authority is ready', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 0, recentThreads: [] },
      projects: []
    }
    const currentProject = { kind: 'directory', projectPath: '/Users/person/Downloads' } as const

    expect(currentProjectAccessDecision({
      currentProject,
      currentWorkspaceHint: null,
      navigation,
      removedProjectKeys: new Set(),
      removedProjectAuthorityReady: false
    })).toBe('wait')
    expect(currentProjectAccessDecision({
      currentProject,
      currentWorkspaceHint: null,
      navigation,
      removedProjectKeys: new Set([projectTargetKey(currentProject.projectPath)]),
      removedProjectAuthorityReady: true
    })).toBe('fallback')
    expect(currentProjectAccessDecision({
      currentProject,
      currentWorkspaceHint: null,
      navigation,
      removedProjectKeys: new Set(),
      removedProjectAuthorityReady: true
    })).toBe('inspect')
    expect(currentProjectAccessDecision({
      currentProject,
      currentWorkspaceHint: { name: 'Downloads', projectPath: currentProject.projectPath },
      navigation,
      removedProjectKeys: new Set(),
      removedProjectAuthorityReady: true
    })).toBe('keep_hint')
  })

  it('ignores runtime readiness but latches missing, away, removed, and invalid Lead configurations', () => {
    const preferences = configuredPreferences()
    const present = profile('agent-a', 'present')
    const unready = profile('agent-b', 'present')
    unready.runtimeReadiness.status = 'needs_attention'
    expect(resolveNewConversationDefaults(preferences, [present, unready])?.lead.agentId).toBe('agent-a')
    expect(defaultsNeedInvalidation(preferences, [present, unready])).toBe(false)

    expect(defaultsNeedInvalidation(preferences, [present, profile('agent-b', 'away')])).toBe(true)
    expect(defaultsNeedInvalidation(preferences, [present])).toBe(true)
    expect(defaultsNeedInvalidation({
      ...preferences,
      newConversationDefaultsRequireConfirmation: true
    }, [present, unready])).toBe(false)
  })

  it('blocks one-click for unsaved or unavailable runtimes and recovers without invalidating the team', () => {
    const preferences = configuredPreferences()
    const members = ['agent-a', 'agent-b'].map(id => profile(id, 'present'))
    for (const member of members) {
      member.runtimeConfiguration = {
        adapterKind: 'codex-cli', model: { mode: 'runtime_default' },
        permissions: { adapterKind: 'codex-cli', schemaVersion: 1,
          values: { sandbox_mode: 'workspace-write', approval_policy: 'on-request' } }
      }
      member.runtimeReadiness.status = 'light_ready'
    }
    members[1].runtimeReadiness.status = 'ready'
    expect(resolveAvailableNewConversationDefaults(preferences, members)?.lead.agentId).toBe('agent-a')
    members[1].runtimeReadiness.status = 'needs_attention'
    expect(resolveAvailableNewConversationDefaults(preferences, members)).toBeNull()
    expect(defaultsNeedInvalidation(preferences, members)).toBe(false)
    members[1].runtimeReadiness.status = 'light_ready'
    expect(resolveAvailableNewConversationDefaults(preferences, members)?.members).toHaveLength(2)
    members[1].runtimeConfiguration = null
    expect(resolveAvailableNewConversationDefaults(preferences, members)).toBeNull()
    expect(defaultsNeedInvalidation(preferences, members)).toBe(false)
  })

  it('waits for the authoritative Member overview before invalidating saved defaults', () => {
    const preferences = configuredPreferences()
    expect(shouldInvalidateNewConversationDefaults(preferences, [], false)).toBe(false)
    expect(shouldInvalidateNewConversationDefaults(preferences, [], true)).toBe(true)
  })

  it('parses the persisted opened-project list leniently and de-duplicates', () => {
    expect(parseKnownProjects(null)).toEqual([])
    expect(parseKnownProjects('{broken')).toEqual([])
    expect(parseKnownProjects(JSON.stringify([
      { projectPath: '/repo/a', name: 'A', openedAt: '2026-01-01' },
      { projectPath: '/repo/a', name: 'A-dup', openedAt: '2026-01-02' },
      { projectPath: '', name: 'x', openedAt: '' },
      { name: 'missing-path' }
    ]))).toEqual([
      { projectPath: '/repo/a', name: 'A', openedAt: '2026-01-01' }
    ])
  })

  it('promotes every remembered empty Project, not just the current one', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 0, recentThreads: [] },
      projects: [project('/repo/existing')]
    }
    const displayed = navigationWithOpenedProjects(navigation, [
      { projectPath: '/repo/a', name: 'a', openedAt: '2026-01-01T00:00:00.000Z' },
      { projectPath: '/repo/b', name: 'b', openedAt: '2026-01-02T00:00:00.000Z' }
    ], new Set())

    expect(displayed?.projects.map((candidate) => candidate.projectPath))
      .toEqual(['/repo/existing', '/repo/a', '/repo/b'])
    expect(displayed?.projects[1]).toMatchObject({
      projectKey: 'directory:/repo/a',
      name: 'a',
      projectPath: '/repo/a',
      totalCount: 0,
      recentThreads: []
    })
  })

  it('does not resurrect removed or already-listed empty Projects', () => {
    const navigation: NavigationSnapshot = {
      schemaVersion: 3,
      throughGlobalSequence: 7,
      quickChat: { totalCount: 0, recentThreads: [] },
      projects: [project('/repo/a')]
    }
    expect(navigationWithOpenedProjects(navigation, [
      { projectPath: '/repo/a', name: 'a', openedAt: 'x' },
      { projectPath: '/repo/removed', name: 'removed', openedAt: 'x' }
    ], new Set([projectTargetKey('/repo/removed')]))).toBe(navigation)
  })

  it('remembers opened projects durably and forgets them on removal', () => {
    const values = new Map<string, string>()
    globalThis.window = {
      localStorage: {
        getItem: (key: string) => values.get(key) ?? null,
        setItem: (key: string, value: string) => { values.set(key, value) }
      }
    } as unknown as Window & typeof globalThis

    rememberOpenedProject({ name: 'a', projectPath: '/repo/a' })
    rememberOpenedProject({ name: 'b', projectPath: '/repo/b' })
    rememberOpenedProject({ name: 'a', projectPath: '/repo/a' })
    expect(readKnownProjects().map((entry) => entry.projectPath)).toEqual(['/repo/a', '/repo/b'])

    forgetOpenedProject('/repo/a')
    expect(readKnownProjects().map((entry) => entry.projectPath)).toEqual(['/repo/b'])

    persistKnownProjects([])
    delete (globalThis as { window?: unknown }).window
  })
})

function configuredPreferences(): GeneralPreferencesSnapshot {
  return {
    schemaVersion: 5,
    interfaceLanguage: 'zh-CN',
    startupLocationMode: 'last_location',
    lastSettingsSection: 'general',
    executionConsolePlacement: 'bottom',
    newConversationDefaults: {
      memberAgentIds: ['agent-a', 'agent-b'],
      defaultLeadAgentId: 'agent-a'
    },
    newConversationDefaultsRequireConfirmation: false,
    oneClickNewConversationEnabled: true,
    worldMapEnabled: true
  }
}

function profile(agentId: string, presence: AgentProfile['presence']): AgentProfile {
  return {
    agentId,
    displayName: agentId,
    avatarRef: null,
    accent: null,
    teamRole: '',
    professionalResponsibilities: '',
    personalityTraits: [],
    workingPrinciples: '',
    growthTopic: '',
    defaultCapabilities: [],
    presence,
    runtimeConfiguration: null,
    runtimeReadiness: { status: 'runtime_not_configured', blockers: [] },
    memberOrder: 0,
    version: 1,
    createdAt: '2026-08-09T00:00:00Z',
    updatedAt: '2026-08-09T00:00:00Z',
    removedAt: presence === 'removed' ? '2026-08-09T00:00:00Z' : null
  }
}

function thread(
  projectBindingKind: NavigationThreadItem['projectBindingKind'],
  projectPath: string
): Pick<NavigationThreadItem, 'projectBindingKind' | 'projectPath'> {
  return { projectBindingKind, projectPath }
}

function project(projectPath: string): NavigationSnapshot['projects'][number] {
  return {
    projectKey: projectTargetKey(projectPath),
    name: projectPath.split('/').at(-1) ?? projectPath,
    projectPath,
    lastActivityAt: '2026-08-09T00:00:00Z',
    lastActivityGlobalSequence: 1,
    totalCount: 0,
    recentThreads: []
  }
}
