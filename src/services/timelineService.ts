// 项目推进时间轴契约封装:get_project_timeline（from, to) 一条只读命令。
// snake_case 契约（contract.ts）→ 驼峰内部形状一次映射;失败返回 null（tryInvoke 口径）。
// TimelineCell.title 是内容列:只供 timeline 窗口本地渲染,不得进入导出 / 日志 / 其它视图。

import type {
  AckAttentionArgs,
  AttentionItemContract,
  FocusAgentWindowArgs,
  FocusResultContract,
  OpenAgentSessionArgs,
  OpenResultContract,
  TimelineQueryArgs,
  TimelineResultContract,
} from './contract'
import { tryInvoke } from './tauri'
import { rememberProjectLabels } from './projectLabels'

export interface TimelineCell {
  day: string
  turns: number
  tokens: number
  wallMs: number
  idleMs: number
  /** 当日有轮的根会话数。 */
  sessions: number
  /** 当日出现的 agent 展示名。 */
  agents: string[]
  /** 当日各根会话,最新开始的在前。 */
  items: TimelineSession[]
}

export interface TimelineSession {
  /** agent 展示名。 */
  agent: string
  /** agent 键（双击跳转用）。 */
  agentKey: string
  sessionId: string
  /** 【内容列】空 → 用 startedAt 回退成时刻。 */
  title: string | null
  startedAt: number
  /** 当日最后活动时刻（末轮 ended_at）;「最新」按它排,不按创建时刻。 */
  lastActiveAt: number
  turns: number
  tokens: number
  wallMs: number
}

export interface TimelineProject {
  key: string
  label: string
  agents: string[]
  firstDay: string | null
  lastDay: string | null
  /** today − lastDay（天）;缺失 / 未来 → null。 */
  inactiveDays: number | null
  /** 只含有活动的日（未来日恒不出现）。 */
  cells: TimelineCell[]
}

export interface TimelineResult {
  today: string
  /** from..to 逐日全列（含未来日）。 */
  days: string[]
  /** 可见项目,按 lastDay 倒序;数量由前端按 pin + 容量裁剪。 */
  projects: TimelineProject[]
}

export async function getProjectTimeline(from: string, to: string): Promise<TimelineResult | null> {
  const args: TimelineQueryArgs = { from, to }
  const res = await tryInvoke<TimelineResultContract>('get_project_timeline', { ...args })
  if (!res) return null
  const projects = (res.projects ?? []).map((p) => ({
    key: p.key,
    label: p.label,
    agents: p.agents ?? [],
    firstDay: p.first_day,
    lastDay: p.last_day,
    inactiveDays: p.inactive_days,
    cells: (p.cells ?? []).map((c) => ({
      day: c.day,
      turns: c.turns,
      tokens: c.tokens,
      wallMs: c.wall_ms,
      idleMs: c.idle_ms,
      sessions: c.sessions,
      agents: c.agents ?? [],
      items: (c.items ?? []).map((i) => ({
        agent: i.agent,
        agentKey: i.agent_key ?? '',
        sessionId: i.session_id,
        title: i.title,
        startedAt: i.started_at,
        lastActiveAt: i.last_active_at,
        turns: i.turns,
        tokens: i.tokens,
        wallMs: i.wall_ms,
      })),
    })),
  }))
  rememberProjectLabels(projects.map((p) => [p.key, p.label]))
  return { today: res.today, days: res.days ?? [], projects }
}

// ---- 注意力:内存表快照 + 确认。事件 timeline:attention 到达后重查。 ----

export type AttentionState = 'running' | 'waiting' | 'tool_pending'

export interface AttentionItem {
  agent: string
  agentLabel: string
  sessionId: string
  /** 原始目录键。 */
  projectKey: string
  /** 【内容列】 */
  title: string | null
  /** 桌面宿主线索（只透传,聚焦目标由 Rust 登记表决定）。 */
  host: string | null
  state: AttentionState
  since: number
  lastEvent: number
  acked: boolean
  /** 暂压：你正在该宿主窗口里,不亮也不算确认。 */
  held: boolean
}

export async function getAttention(): Promise<AttentionItem[] | null> {
  const res = await tryInvoke<AttentionItemContract[]>('get_attention')
  if (!res) return null
  return res.map((i) => ({
    agent: i.agent,
    agentLabel: i.agent_label,
    sessionId: i.session_id,
    projectKey: i.project_key,
    title: i.title,
    host: i.host ?? null,
    state: i.state,
    since: i.since,
    lastEvent: i.last_event,
    acked: i.acked,
    held: i.held ?? false,
  }))
}

export async function ackAttention(agent: string, sessionId: string): Promise<boolean> {
  const args: AckAttentionArgs = { agent, session_id: sessionId }
  return (await tryInvoke<boolean>('ack_attention', { ...args })) ?? false
}

// ---- 桌面窗口聚焦:true = 已前置并确认;false = 宿主窗口不存在（条目已移除）;
// null = 命令本身失败（非 Tauri 环境 / IPC 错误）,调用方按「未处理」对待。 ----
export async function focusAgentWindow(agent: string, sessionId: string): Promise<boolean | null> {
  const args: FocusAgentWindowArgs = { agent, session_id: sessionId }
  const res = await tryInvoke<FocusResultContract>('focus_agent_window', { ...args })
  return res ? res.found : null
}

// ---- 双击会话跳转:窗口在 → 前置;不在 → 启动宿主（IDE 类带项目目录）;宿主找不到 → 打开项目目录。
// null = 命令本身失败（非 Tauri 环境 / 未知会话）。 ----
export type OpenOutcome = OpenResultContract['outcome']

export async function openAgentSession(agentKey: string, sessionId: string): Promise<OpenOutcome | null> {
  const args: OpenAgentSessionArgs = { agent: agentKey, session_id: sessionId }
  const res = await tryInvoke<OpenResultContract>('open_agent_session', { ...args })
  return res ? res.outcome : null
}
