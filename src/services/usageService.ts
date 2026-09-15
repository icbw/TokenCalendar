// Usage 契约的取数封装：契约 snake_case → 内部驼峰形状。
// 对齐旧 UsageService.GetMonthlyMatrix / GetBreakdown 的语义（含失败返回 null）。

import type {
  BreakdownDay,
  CreditSummaryContract,
  EffortSeriesArgs,
  MatrixQuery,
  MatrixResult,
  ProjectBreakdownArgs,
  ProjectGroupBy,
  ProjectMetric,
  ProjectMonthRowsArgs,
  RangeSeriesQueryContract,
  RangeSeriesResultContract,
} from './contract'
import type { QueryOptions, UsageResult, UsageRow } from './types'
import { tryInvoke } from './tauri'
import { rememberProjectLabels } from './projectLabels'

export async function getMonthlyMatrix(opts: QueryOptions): Promise<UsageResult | null> {
  const query: MatrixQuery = {
    month: opts.month,
    group_by: opts.groupBy,
    bucket: opts.bucket,
    metric: opts.metric,
    normalization: opts.normalization,
  }
  const res = await tryInvoke<MatrixResult>('get_monthly_matrix', { query })
  if (!res) return null
  const rows: UsageRow[] = (res.rows ?? []).map((r) => ({
    key: r.key,
    label: r.label,
    values: r.values,
    messageCounts: r.message_counts,
    monthTotal: r.month_total,
  }))
  return {
    month: res.month,
    daysInMonth: res.days_in_month,
    generatedAt: res.generated_at,
    rows,
  }
}

export async function getBreakdown(kind: string, key: string, month: string): Promise<BreakdownDay[] | null> {
  return tryInvoke<BreakdownDay[]>('get_breakdown', { kind, key, month })
}

/** credit 月报（数据洞察;snake_case → 驼峰一次映射）。 */
export interface CreditModelSlice {
  key: string
  label: string
  credit: number
  requests: number
}

export interface CreditDayPoint {
  day: string
  credit: number
}

export interface CreditModelDaySlice {
  key: string
  label: string
  byDay: CreditDayPoint[]
}

export interface CreditSummary {
  month: string
  hasData: boolean
  totalCredit: number
  totalRequests: number
  byModel: CreditModelSlice[]
  byDay: CreditDayPoint[]
  byModelDay: CreditModelDaySlice[]
}

export async function getCreditSummary(month: string): Promise<CreditSummary | null> {
  const res = await tryInvoke<CreditSummaryContract>('get_credit_summary', { month })
  if (!res) return null
  return {
    month: res.month,
    hasData: res.has_data,
    totalCredit: res.total_credit,
    totalRequests: res.total_requests,
    byModel: (res.by_model ?? []).map((r) => ({
      key: r.key,
      label: r.label,
      credit: r.credit,
      requests: r.requests,
    })),
    byDay: (res.by_day ?? []).map((d) => ({ day: d.day, credit: d.credit })),
    byModelDay: (res.by_model_day ?? []).map((m) => ({
      key: m.key,
      label: m.label,
      byDay: (m.by_day ?? []).map((d) => ({ day: d.day, credit: d.credit })),
    })),
  }
}

// ---- 时间范围序列----

export interface RangeSeriesQuery {
  startDay: string
  endDay: string
  bucket: 'day' | 'hour'
  dimension: 'agent' | 'model' | 'total'
  metric: 'total' | 'input' | 'output'
  filterDimension?: 'agent' | 'model'
  filterKey?: string
}

export interface RangeSeriesPoint {
  bucket: string
  values: number[]
}

export interface RangeSeriesResult {
  seriesKeys: string[]
  seriesLabels: string[]
  points: RangeSeriesPoint[]
}

export async function getRangeSeries(q: RangeSeriesQuery): Promise<RangeSeriesResult | null> {
  const body: RangeSeriesQueryContract = {
    start_day: q.startDay,
    end_day: q.endDay,
    bucket: q.bucket,
    dimension: q.dimension,
    metric: q.metric,
  }
  if (q.filterDimension && q.filterKey) {
    body.filter_dimension = q.filterDimension
    body.filter_key = q.filterKey
  }
  const res = await tryInvoke<RangeSeriesResultContract>('get_range_series', { query: body })
  if (!res) return null
  return {
    seriesKeys: res.series_keys ?? [],
    seriesLabels: res.series_labels ?? [],
    points: (res.points ?? []).map((p) => ({ bucket: p.bucket, values: p.values ?? [] })),
  }
}

// ---- 项目维（读 daily_project,形状复用 UsageResult / BreakdownDay / RangeSeriesResult）----

export async function getProjectMonthRows(month: string, groupBy: ProjectGroupBy, metric: ProjectMetric): Promise<UsageResult | null> {
  const args: ProjectMonthRowsArgs = { month, group_by: groupBy, metric }
  const res = await tryInvoke<MatrixResult>('get_project_month_rows', { ...args })
  if (!res) return null
  if (groupBy === 'project') rememberProjectLabels((res.rows ?? []).map((r) => [r.key, r.label]))
  return {
    month: res.month,
    daysInMonth: res.days_in_month,
    generatedAt: res.generated_at,
    rows: (res.rows ?? []).map((r) => ({
      key: r.key,
      label: r.label,
      values: r.values,
      messageCounts: r.message_counts,
      monthTotal: r.month_total,
    })),
  }
}

/** kind=project → 每日 Agent 构成;kind=agent|model → 每日项目构成（tokens = total）。 */
export async function getProjectBreakdown(kind: ProjectGroupBy, key: string, month: string): Promise<BreakdownDay[] | null> {
  const args: ProjectBreakdownArgs = { kind, key, month }
  const res = await tryInvoke<BreakdownDay[]>('get_project_breakdown', { ...args })
  if (res && kind !== 'project') rememberProjectLabels(res.flatMap((d) => (d.slices ?? []).map((s) => [s.key, s.label] as [string, string])))
  return res
}

export interface EffortSeriesQuery {
  startDay: string
  endDay: string
  dimension: ProjectGroupBy | 'total'
  metric: ProjectMetric
  filter?: { dimension: ProjectGroupBy; key: string }
}

/** 时间成本 / 项目维曲线（bucket 仅 day）。 */
export async function getEffortSeries(q: EffortSeriesQuery): Promise<RangeSeriesResult | null> {
  const args: EffortSeriesArgs = {
    range: { start_day: q.startDay, end_day: q.endDay },
    bucket: 'day',
    dimension: q.dimension,
    metric: q.metric,
  }
  if (q.filter && q.filter.key) args.filter = q.filter
  const res = await tryInvoke<RangeSeriesResultContract>('get_effort_series', { ...args })
  if (!res) return null
  if (q.dimension === 'project') rememberProjectLabels((res.series_keys ?? []).map((k, i) => [k, res.series_labels?.[i] ?? k]))
  return {
    seriesKeys: res.series_keys ?? [],
    seriesLabels: res.series_labels ?? [],
    points: (res.points ?? []).map((p) => ({ bucket: p.bucket, values: p.values ?? [] })),
  }
}
