// 装配层内部形状（驼峰命名）。
// 线上 snake_case 契约（contract.ts）→ 此处驼峰形状的映射在各 service 内完成。

import type { TaskSortField, TokenMetric } from './contract'

export interface UsageRow {
  key: string
  label: string
  values: (number | null)[] | null
  /** 与 values 平行的请求/对话数。 */
  messageCounts?: number[]
  monthTotal: number
}

export interface UsageResult {
  month: string
  daysInMonth: number
  generatedAt: number
  rows: UsageRow[] | null
}

export interface SourceSummary {
  id: string
  adapterId: string
  adapterName: string
  location: string
  kind: string
  probeStatus: string
  schemaFingerprint?: string
  lastSuccessAt?: string
  lastAttemptAt?: string
  lastErrorCode?: string
  lastErrorMessage?: string
  eventsCollected: number
  stale: boolean
}

export interface ExportResult {
  path: string
  rows: number
  format: string
}

// 窗口身份由入口装配决定（main.tsx → FullWindow / widget.tsx → WidgetWindow 等），
// 可见性状态在 windowService.WindowVisibility + 事件广播。

export interface QueryOptions {
  month: string
  groupBy: 'agent' | 'model'
  bucket: 'day'
  metric: TokenMetric
  normalization: 'global' | 'perRow'
}

// ---- 项目维与任务分析（契约 snake_case 见 contract.ts）----

export interface DayRange {
  startDay: string
  endDay: string
}

export interface TaskFilters {
  agent?: string
  project?: string
}

export interface TaskSort {
  field: TaskSortField
  direction: 'asc' | 'desc'
}

export interface TaskPageReq {
  offset: number
  limit: number
}

export interface TaskRow {
  agent: string
  sessionId: string
  /** 有效项目键（project_meta 解析层）。 */
  project: string
  projectLabel: string
  /** 原始目录键（hover 真实路径）。 */
  projectRaw: string
  startedAt: number
  endedAt: number | null
  /** 内容列:仅本地展示。 */
  title: string | null
  turns: number
  steps: number
  toolCalls: number
  wallMs: number | null
  modelMs: number | null
  toolMs: number | null
  /** API / 工具错误（不含用户中止）。 */
  errorCount: number
  abortedCount: number
  subagentCount: number
  subagentCalls: number
  totalTokens: number
}

export interface TaskPage {
  total: number
  rows: TaskRow[]
}

export interface TaskTurn {
  turnSeq: number
  day: string
  project: string
  model: string
  startedAt: number
  endedAt: number | null
  wallMs: number | null
  modelMs: number | null
  toolMs: number | null
  ttftMs: number | null
  gapMs: number | null
  steps: number
  toolCalls: number
  subagentCount: number
  subagentCalls: number
  errorCount: number
  retryCount: number
  aborted: boolean
  inputTokens: number
  outputTokens: number
  totalTokens: number
}

/** 本地日闭区间（数据跨度 / 项目生命周期）。 */
export interface DaySpan {
  firstDay: string
  lastDay: string
}

export interface GapBucket {
  loMs: number
  hiMs: number | null
  count: number
}

export interface GapHistogram {
  thresholdMs: number
  buckets: GapBucket[]
  total: number
  withinCount: number
  withinMs: number
  beyondCount: number
  beyondMs: number
}

export interface IdleThresholdInfo {
  minutes: number
  defaultMinutes: number
  minMinutes: number
  maxMinutes: number
}

export interface IdleThresholdApplied {
  minutes: number
  recomputedDays: number
  elapsedMs: number
}
