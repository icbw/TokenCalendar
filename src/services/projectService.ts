// 项目管理契约封装:列表 / 单条 upsert / 合并 / 取消合并 / 自动折叠规则 / 打开目录。
// 读命令失败返回 null（tryInvoke 口径）;写命令把后端错误串带回 UI（拒绝环、超长 alias 等需要给用户看）。
// 写命令成功后后端发 usage:changed,各视图（含管理面板）经既有监听重取。

import type {
  ProjectMetaListContract,
  ProjectMetaRowContract,
  ProjectStatus,
  ScratchRuleContract,
  ScratchRuleInfoContract,
} from './contract'
import { inTauri, tryInvoke } from './tauri'

export type { ProjectStatus }

export interface ProjectMetaRow {
  key: string
  label: string
  alias: string | null
  hidden: boolean
  mergedInto: string | null
  mergedLabel: string | null
  note: string | null
  status: ProjectStatus
  managed: boolean
  effectiveKey: string | null
  agents: string[]
  sessions: number
  turns: number
  tokens: number
  firstDay: string | null
  lastDay: string | null
  updatedAt: number | null
  folderExists: boolean
}

export interface ProjectMetaList {
  rows: ProjectMetaRow[]
  scratchHidden: boolean
  scratchAlias: string | null
}

export interface ScratchRule {
  enabled: boolean
  minSessions: number
  minTurns: number
  unknownAsScratch: boolean
}

export interface ScratchRuleInfo {
  rule: ScratchRule
  defaults: ScratchRule
  minSessionsBounds: [number, number]
  minTurnsBounds: [number, number]
}

/** alias / note 长度上限（与 Rust project_meta:META_TEXT_MAX 同值）。 */
export const META_TEXT_MAX = 120
/** 内置伪项目键（与 Rust SCRATCH_KEY 同值）。 */
export const SCRATCH_KEY = '__scratch'

export type WriteResult<T> = { ok: true; value: T } | { ok: false; error: string }

async function invokeWrite<T>(cmd: string, args: Record<string, unknown>): Promise<WriteResult<T>> {
  if (!inTauri) return { ok: false, error: 'Service not running' }
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return { ok: true, value: await invoke<T>(cmd, args) }
  } catch (e) {
    return { ok: false, error: typeof e === 'string' ? e : String(e) }
  }
}

const rowOf = (r: ProjectMetaRowContract): ProjectMetaRow => ({
  key: r.key,
  label: r.label,
  alias: r.alias,
  hidden: r.hidden,
  mergedInto: r.merged_into,
  mergedLabel: r.merged_label,
  note: r.note,
  status: r.status,
  managed: r.managed,
  effectiveKey: r.effective_key,
  agents: r.agents ?? [],
  sessions: r.sessions,
  turns: r.turns,
  tokens: r.tokens,
  firstDay: r.first_day,
  lastDay: r.last_day,
  updatedAt: r.updated_at,
  folderExists: r.folder_exists,
})

const ruleOf = (r: ScratchRuleContract): ScratchRule => ({
  enabled: r.enabled,
  minSessions: r.min_sessions,
  minTurns: r.min_turns,
  unknownAsScratch: r.unknown_as_scratch,
})

const infoOf = (r: ScratchRuleInfoContract): ScratchRuleInfo => ({
  rule: ruleOf(r.rule),
  defaults: ruleOf(r.defaults),
  minSessionsBounds: r.min_sessions_bounds,
  minTurnsBounds: r.min_turns_bounds,
})

export async function listProjectMeta(): Promise<ProjectMetaList | null> {
  const res = await tryInvoke<ProjectMetaListContract>('list_project_meta')
  if (!res) return null
  return { rows: (res.rows ?? []).map(rowOf), scratchHidden: res.scratch_hidden, scratchAlias: res.scratch_alias }
}

/** 单条 upsert:alias / note 传 null 或空串 = 清除;hidden 必填（取当前值即不变）。 */
export function setProjectMeta(key: string, patch: { alias: string | null; hidden: boolean; note: string | null }): Promise<WriteResult<null>> {
  return invokeWrite<null>('set_project_meta', { input: { project_key: key, alias: patch.alias, hidden: patch.hidden, note: patch.note, reset: false } })
}

/** 删 meta 行回到自动态（仍是合并目标时只清 alias / note / hidden）。 */
export function resetProjectMeta(key: string): Promise<WriteResult<null>> {
  return invokeWrite<null>('set_project_meta', { input: { project_key: key, alias: null, hidden: false, note: null, reset: true } })
}

export function mergeProjects(keys: string[], into: string): Promise<WriteResult<number>> {
  return invokeWrite<number>('merge_projects', { keys, into })
}

export function unmergeProjects(keys: string[]): Promise<WriteResult<number>> {
  return invokeWrite<number>('unmerge_projects', { keys })
}

export async function getScratchRule(): Promise<ScratchRuleInfo | null> {
  const res = await tryInvoke<ScratchRuleInfoContract>('get_scratch_rule')
  return res ? infoOf(res) : null
}

/** 改规则的唯一入口（Rust 合并写 prefs.json scratch* 四键 + 下发 + emit usage:changed）。
 * 调用方成功后须再 setDesignPrefs 同步四键（同 idleThresholdMin,见 designPrefs 注释）。 */
export async function setScratchRule(rule: ScratchRule): Promise<WriteResult<ScratchRuleInfo>> {
  const res = await invokeWrite<ScratchRuleInfoContract>('set_scratch_rule', {
    rule: { enabled: rule.enabled, min_sessions: rule.minSessions, min_turns: rule.minTurns, unknown_as_scratch: rule.unknownAsScratch },
  })
  return res.ok ? { ok: true, value: infoOf(res.value) } : res
}

export function openProjectFolder(key: string): Promise<WriteResult<null>> {
  return invokeWrite<null>('open_project_folder', { key })
}
