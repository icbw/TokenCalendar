// 任务分析契约封装:任务列表 / 逐轮明细 / 空档直方图 / 离开阈值。
// snake_case 契约（contract.ts）→ 驼峰内部形状（types.ts）一次映射;失败返回 null（tryInvoke 口径）。
// TaskRow.title 是内容列:只供 Tasks 列表本地渲染,不得进入导出 / 日志 / 其它视图。

import type {
  DaySpanContract,
  GapHistogramContract,
  IdleThresholdAppliedContract,
  IdleThresholdInfoContract,
  TaskListArgs,
  TaskPageContract,
  TaskTurnContract,
  TaskTurnsArgs,
} from './contract'
import type { DayRange, DaySpan, GapHistogram, IdleThresholdApplied, IdleThresholdInfo, TaskFilters, TaskPage, TaskPageReq, TaskSort, TaskTurn } from './types'
import { tryInvoke } from './tauri'
import { rememberProjectLabels } from './projectLabels'

export async function getTaskList(range: DayRange, filters: TaskFilters, sort: TaskSort, page: TaskPageReq): Promise<TaskPage | null> {
  const args: TaskListArgs = {
    range: { start_day: range.startDay, end_day: range.endDay },
    filters: {
      agent: filters.agent || undefined,
      project: filters.project || undefined,
    },
    sort,
    page,
  }
  const res = await tryInvoke<TaskPageContract>('get_task_list', { ...args })
  if (!res) return null
  rememberProjectLabels((res.rows ?? []).map((r) => [r.project, r.project_label]))
  return {
    total: res.total,
    rows: (res.rows ?? []).map((r) => ({
      agent: r.agent,
      sessionId: r.session_id,
      project: r.project,
      projectLabel: r.project_label,
      projectRaw: r.project_raw,
      startedAt: r.started_at,
      endedAt: r.ended_at,
      title: r.title,
      turns: r.turns,
      steps: r.steps,
      toolCalls: r.tool_calls,
      wallMs: r.wall_ms,
      modelMs: r.model_ms,
      toolMs: r.tool_ms,
      errorCount: r.error_count,
      abortedCount: r.aborted_count,
      subagentCount: r.subagent_count,
      subagentCalls: r.subagent_calls,
      totalTokens: r.total_tokens,
    })),
  }
}

export async function getTaskTurns(agent: string, sessionId: string): Promise<TaskTurn[] | null> {
  const args: TaskTurnsArgs = { agent, session_id: sessionId }
  const res = await tryInvoke<TaskTurnContract[]>('get_task_turns', { ...args })
  if (!res) return null
  return res.map((t) => ({
    turnSeq: t.turn_seq,
    day: t.day,
    project: t.project,
    model: t.model,
    startedAt: t.started_at,
    endedAt: t.ended_at,
    wallMs: t.wall_ms,
    modelMs: t.model_ms,
    toolMs: t.tool_ms,
    ttftMs: t.ttft_ms,
    gapMs: t.gap_ms,
    steps: t.steps,
    toolCalls: t.tool_calls,
    subagentCount: t.subagent_count,
    subagentCalls: t.subagent_calls,
    errorCount: t.error_count,
    aborted: t.aborted,
    retryCount: t.retry_count,
    inputTokens: t.input_tokens,
    outputTokens: t.output_tokens,
    totalTokens: t.total_tokens,
  }))
}

/** 项目生命周期（给定 project）或全部数据跨度（省略）;无数据 / 失败 → null。 */
export async function getProjectSpan(project?: string): Promise<DaySpan | null> {
  const res = await tryInvoke<DaySpanContract | null>('get_project_span', { project: project || null })
  if (!res) return null
  return { firstDay: res.first_day, lastDay: res.last_day }
}

export async function getGapHistogram(range: DayRange): Promise<GapHistogram | null> {
  const res = await tryInvoke<GapHistogramContract>('get_gap_histogram', { range: { start_day: range.startDay, end_day: range.endDay } })
  if (!res) return null
  return {
    thresholdMs: res.threshold_ms,
    buckets: (res.buckets ?? []).map((b) => ({ loMs: b.lo_ms, hiMs: b.hi_ms, count: b.count })),
    total: res.total,
    withinCount: res.within_count,
    withinMs: res.within_ms,
    beyondCount: res.beyond_count,
    beyondMs: res.beyond_ms,
  }
}

export async function getIdleThreshold(): Promise<IdleThresholdInfo | null> {
  const res = await tryInvoke<IdleThresholdInfoContract>('get_idle_threshold')
  if (!res) return null
  return { minutes: res.minutes, defaultMinutes: res.default_minutes, minMinutes: res.min_minutes, maxMinutes: res.max_minutes }
}

/** 改阈值的唯一入口（Rust 合并写 prefs.json + 同步重算 daily_project + emit usage:changed）。
 * 调用方成功后须再 setDesignPrefs（{ idleThresholdMin }) 同步内存快照（见 TasksView）。 */
export async function setIdleThreshold(minutes: number): Promise<IdleThresholdApplied | null> {
  const res = await tryInvoke<IdleThresholdAppliedContract>('set_idle_threshold', { minutes })
  if (!res) return null
  return { minutes: res.minutes, recomputedDays: res.recomputed_days, elapsedMs: res.elapsed_ms }
}
