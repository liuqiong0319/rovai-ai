import { isNewConversationMemberAvailable } from './new-conversation-availability'
import type {
  AgentProfile,
  GeneralPreferencesSnapshot,
  NavigationThreadItem,
  NavigationSnapshot,
  NewConversationDefaults,
  ProjectNavigationGroup,
  WorkspaceSelection
} from '@contracts'

export const CURRENT_PROJECT_STORAGE_KEY = 'rovai.current-project.v1'
export const KNOWN_PROJECTS_STORAGE_KEY = 'rovai.known-projects.v1'

export interface KnownProject {
  projectPath: string
  name: string
  openedAt: string
}

export function parseKnownProjects(value: string | null): KnownProject[] {
  if (!value) return []
  try {
    const parsed = JSON.parse(value) as unknown
    if (!Array.isArray(parsed)) return []
    const projects: KnownProject[] = []
    for (const candidate of parsed) {
      if (candidate === null || typeof candidate !== 'object' || Array.isArray(candidate)) continue
      const record = candidate as Record<string, unknown>
      if (typeof record.projectPath !== 'string' || record.projectPath.length === 0) continue
      if (typeof record.name !== 'string') continue
      const openedAt = typeof record.openedAt === 'string' ? record.openedAt : ''
      if (projects.some((entry) => entry.projectPath === record.projectPath)) continue
      projects.push({ projectPath: record.projectPath, name: record.name, openedAt })
    }
    return projects
  } catch {
    return []
  }
}

export function readKnownProjects(): KnownProject[] {
  try {
    return parseKnownProjects(window.localStorage.getItem(KNOWN_PROJECTS_STORAGE_KEY))
  } catch {
    return []
  }
}

export function persistKnownProjects(projects: KnownProject[]): void {
  try {
    window.localStorage.setItem(KNOWN_PROJECTS_STORAGE_KEY, JSON.stringify(projects))
  } catch {
    // The current window still keeps its in-memory list when storage is blocked.
  }
}

// Every directory the user opens as a project stays in the project list even
// before it has any conversation/draft content. Bug reports: an empty project
// vanished from the list when the user navigated away, and opening a second
// empty project replaced the first (the Core snapshot only knows projects with
// visible Camps). Remembering opened projects durably is the display-layer fix.
export function rememberOpenedProject(workspace: { name: string; projectPath: string }): void {
  const existing = readKnownProjects().filter(
    (entry) => entry.projectPath !== workspace.projectPath
  )
  persistKnownProjects([{
    projectPath: workspace.projectPath,
    name: workspace.name,
    openedAt: new Date().toISOString()
  }, ...existing])
}

export function forgetOpenedProject(projectPath: string): void {
  const remaining = readKnownProjects().filter((entry) => entry.projectPath !== projectPath)
  persistKnownProjects(remaining)
}

export type CurrentProject =
  | { kind: 'quick_chat' }
  | { kind: 'directory'; projectPath: string }

export type CurrentProjectAccessDecision =
  | 'wait'
  | 'fallback'
  | 'clear_hint'
  | 'keep_hint'
  | 'inspect'

export interface CurrentProjectAccessInput {
  currentProject: CurrentProject
  currentWorkspaceHint: WorkspaceSelection | null
  navigation: NavigationSnapshot | null
  removedProjectKeys: ReadonlySet<string>
  removedProjectAuthorityReady: boolean
}

export interface ResolvedNewConversationDefaults {
  defaults: NewConversationDefaults
  members: AgentProfile[]
  lead: AgentProfile
}

export function parseCurrentProject(value: string | null): CurrentProject {
  if (!value) return { kind: 'quick_chat' }
  try {
    const parsed = JSON.parse(value) as unknown
    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return { kind: 'quick_chat' }
    }
    const record = parsed as Record<string, unknown>
    if (record.kind === 'quick_chat' && Object.keys(record).length === 1) {
      return { kind: 'quick_chat' }
    }
    if (
      record.kind === 'directory'
      && typeof record.projectPath === 'string'
      && record.projectPath.length > 0
      && Object.keys(record).length === 2
    ) {
      return { kind: 'directory', projectPath: record.projectPath }
    }
  } catch {
    // A damaged renderer preference safely falls back to Quick Chat.
  }
  return { kind: 'quick_chat' }
}

export function readCurrentProject(): CurrentProject {
  try {
    return parseCurrentProject(window.localStorage.getItem(CURRENT_PROJECT_STORAGE_KEY))
  } catch {
    return { kind: 'quick_chat' }
  }
}

export function persistCurrentProject(currentProject: CurrentProject): void {
  try {
    window.localStorage.setItem(CURRENT_PROJECT_STORAGE_KEY, JSON.stringify(currentProject))
  } catch {
    // The current window still keeps its in-memory selection when storage is blocked.
  }
}

export function currentProjectForThread(thread: Pick<NavigationThreadItem, 'projectBindingKind' | 'projectPath'>): CurrentProject {
  return thread.projectBindingKind === 'directory'
    ? { kind: 'directory', projectPath: thread.projectPath }
    : { kind: 'quick_chat' }
}

export function projectTargetKey(projectPath: string): string {
  return `directory:${projectPath}`
}

export function navigationWithoutRemovedProjects(
  navigation: NavigationSnapshot | null,
  removedProjectKeys: ReadonlySet<string>
): NavigationSnapshot | null {
  if (!navigation || removedProjectKeys.size === 0) return navigation
  const projects = navigation.projects.filter(
    (project) => !removedProjectKeys.has(project.projectKey)
  )
  return projects.length === navigation.projects.length
    ? navigation
    : { ...navigation, projects }
}

export function navigationWithProjectOrder(
  navigation: NavigationSnapshot | null,
  projectOrder: readonly string[] | null
): NavigationSnapshot | null {
  if (!navigation || projectOrder === null || navigation.projects.length < 2) return navigation
  const orderByProjectKey = new Map(
    projectOrder.map((projectKey, index) => [projectKey, index])
  )
  const originalIndexByProjectKey = new Map(
    navigation.projects.map((project, index) => [project.projectKey, index])
  )
  const projects = [...navigation.projects].sort((left, right) => {
    const leftOrder = orderByProjectKey.get(left.projectKey)
    const rightOrder = orderByProjectKey.get(right.projectKey)
    if (leftOrder !== undefined && rightOrder !== undefined) return leftOrder - rightOrder
    if (leftOrder !== undefined) return -1
    if (rightOrder !== undefined) return 1
    return (originalIndexByProjectKey.get(left.projectKey) ?? 0)
      - (originalIndexByProjectKey.get(right.projectKey) ?? 0)
  })
  return projects.every((project, index) => project === navigation.projects[index])
    ? navigation
    : { ...navigation, projects }
}

// Display overrides never mutate Core snapshots, workspace bindings, or Thread identity.
export function navigationWithProjectNames(
  navigation: NavigationSnapshot | null,
  projectNames: Readonly<Record<string, string>>
): NavigationSnapshot | null {
  if (!navigation) return null
  const projects = navigation.projects.map((project) => {
    const name = projectNames[project.projectKey] ?? project.name
    return name === project.name ? project : { ...project, name }
  })
  return projects.every((project, index) => project === navigation.projects[index])
    ? navigation
    : { ...navigation, projects }
}

export function navigationWithProjectAuthority(
  navigation: NavigationSnapshot | null,
  removedProjectKeys: ReadonlySet<string>,
  removedProjectAuthorityReady: boolean
): NavigationSnapshot | null {
  if (!navigation) return null
  if (!removedProjectAuthorityReady) {
    return navigation.projects.length === 0
      ? navigation
      : { ...navigation, projects: [] }
  }
  return navigationWithoutRemovedProjects(navigation, removedProjectKeys)
}

export function currentProjectGroup(
  navigation: NavigationSnapshot | null,
  currentProject: CurrentProject
): ProjectNavigationGroup | null {
  if (!navigation || currentProject.kind === 'quick_chat') return null
  return navigation.projects.find((project) => project.projectPath === currentProject.projectPath) ?? null
}

export function currentProjectExists(
  navigation: NavigationSnapshot | null,
  currentProject: CurrentProject
): boolean {
  return currentProject.kind === 'quick_chat'
    || currentProjectGroup(navigation, currentProject) !== null
}

export function currentProjectWorkspace(
  navigation: NavigationSnapshot | null,
  currentProject: CurrentProject
): WorkspaceSelection | null {
  const project = currentProjectGroup(navigation, currentProject)
  return project ? { name: project.name, projectPath: project.projectPath } : null
}

export function currentProjectAccessDecision({
  currentProject,
  currentWorkspaceHint,
  navigation,
  removedProjectKeys,
  removedProjectAuthorityReady
}: CurrentProjectAccessInput): CurrentProjectAccessDecision {
  if (!navigation || !removedProjectAuthorityReady) return 'wait'
  if (currentProject.kind === 'quick_chat') return 'clear_hint'
  if (removedProjectKeys.has(projectTargetKey(currentProject.projectPath))) return 'fallback'
  if (currentProjectGroup(navigation, currentProject)) return 'clear_hint'
  if (currentWorkspaceHint?.projectPath === currentProject.projectPath) return 'keep_hint'
  return 'inspect'
}

export function navigationIncludingCurrentWorkspace(
  navigation: NavigationSnapshot | null,
  currentProject: CurrentProject,
  currentWorkspace: WorkspaceSelection | null
): NavigationSnapshot | null {
  if (
    !navigation
    || currentProject.kind !== 'directory'
    || currentProjectGroup(navigation, currentProject)
    || currentWorkspace?.projectPath !== currentProject.projectPath
  ) return navigation

  const emptyProject: ProjectNavigationGroup = {
    projectKey: projectTargetKey(currentWorkspace.projectPath),
    name: currentWorkspace.name,
    projectPath: currentWorkspace.projectPath,
    lastActivityAt: '',
    lastActivityGlobalSequence: 0,
    totalCount: 0,
    recentThreads: []
  }
  return {
    ...navigation,
    projects: [...navigation.projects, emptyProject]
  }
}

// Empty (conversation-less) projects the user opened are promoted into the
// project list so they neither vanish on navigation (Bug1) nor get replaced by
// a newer open (Bug2). Projects already present in the snapshot (any Camp has
// made them navigable) and removed projects are left untouched.
export function navigationWithOpenedProjects(
  navigation: NavigationSnapshot | null,
  openedProjects: readonly KnownProject[],
  removedProjectKeys: ReadonlySet<string>
): NavigationSnapshot | null {
  if (!navigation || openedProjects.length === 0) return navigation
  const existingPaths = new Set(navigation.projects.map((project) => project.projectPath))
  const missing = openedProjects.filter((entry) =>
    !existingPaths.has(entry.projectPath)
    && !removedProjectKeys.has(projectTargetKey(entry.projectPath)))
  if (missing.length === 0) return navigation
  const emptyProjects: ProjectNavigationGroup[] = missing.map((entry) => ({
    projectKey: projectTargetKey(entry.projectPath),
    name: entry.name,
    projectPath: entry.projectPath,
    lastActivityAt: entry.openedAt,
    lastActivityGlobalSequence: 0,
    totalCount: 0,
    recentThreads: []
  }))
  return {
    ...navigation,
    projects: [...navigation.projects, ...emptyProjects]
  }
}

export function resolveNewConversationDefaults(
  preferences: GeneralPreferencesSnapshot | null,
  agents: AgentProfile[]
): ResolvedNewConversationDefaults | null {
  const defaults = preferences?.newConversationDefaults
  if (!defaults || preferences.newConversationDefaultsRequireConfirmation) return null
  const profileById = new Map(agents.map((agent) => [agent.agentId, agent]))
  const members = defaults.memberAgentIds.flatMap((agentId) => {
    const agent = profileById.get(agentId)
    return agent ? [agent] : []
  })
  if (
    members.length !== defaults.memberAgentIds.length
    || members.some((agent) => agent.presence !== 'present' || agent.removedAt !== null)
  ) return null
  const lead = profileById.get(defaults.defaultLeadAgentId)
  if (
    !lead
    || lead.presence !== 'present'
    || lead.removedAt !== null
    || !defaults.memberAgentIds.includes(lead.agentId)
  ) return null
  return { defaults, members, lead }
}

// A temporary runtime outage blocks one-click creation without invalidating the saved team.
export function resolveAvailableNewConversationDefaults(
  preferences: GeneralPreferencesSnapshot | null,
  agents: AgentProfile[]
): ResolvedNewConversationDefaults | null {
  const resolved = resolveNewConversationDefaults(preferences, agents)
  return resolved?.members.every((agent) => isNewConversationMemberAvailable({
    runtimeConfigured: agent.runtimeConfiguration !== null,
    runtimeReadiness: agent.runtimeReadiness.status
  })) ? resolved : null
}

export function defaultsNeedInvalidation(
  preferences: GeneralPreferencesSnapshot | null,
  agents: AgentProfile[]
): boolean {
  return Boolean(
    preferences?.newConversationDefaults
    && !preferences.newConversationDefaultsRequireConfirmation
    && !resolveNewConversationDefaults(preferences, agents)
  )
}

export function shouldInvalidateNewConversationDefaults(
  preferences: GeneralPreferencesSnapshot | null,
  agents: AgentProfile[],
  overviewReady: boolean
): boolean {
  return overviewReady && defaultsNeedInvalidation(preferences, agents)
}
