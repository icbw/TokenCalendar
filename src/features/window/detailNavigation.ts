import type { DayRange, SubscriptionPlatform } from '../../services'

export interface ExecutionContext {
  project?: string
  agent?: string
  range?: DayRange
  section?: 'tasks' | 'quota'
  platform?: SubscriptionPlatform
}

// 主窗口内导航；不持久化、不跨窗口复制业务筛选。
const executionListeners = new Set<(context: ExecutionContext) => void>()
const projectListeners = new Set<() => void>()

export function openExecutionDetails(context: ExecutionContext = {}): void {
  for (const fn of executionListeners) fn(context)
}

export function subscribeExecutionDetails(fn: (context: ExecutionContext) => void): () => void {
  executionListeners.add(fn)
  return () => executionListeners.delete(fn)
}

export function openProjectSettings(): void {
  for (const fn of projectListeners) fn()
}

export function subscribeProjectSettings(fn: () => void): () => void {
  projectListeners.add(fn)
  return () => projectListeners.delete(fn)
}
