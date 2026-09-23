// 时间范围模型:Tasks 与 Insights 共用。
// 选择 = 三档预设 / All（全部数据首日 → 今天）/ Custom（起止日）/ 项目生命周期（选定单个项目时自动或按钮套用）。
// 解析结果统一为本地日闭区间 DayRange,直接喂 get_task_list / get_effort_series / get_gap_histogram / get_range_series。
import type { DayRange, DaySpan } from '../../services'
import { fmt, getT } from '../../lib/i18n'

export type PresetDays = 7 | 30 | 90

export type RangeSel =
  | { kind: 'preset'; days: PresetDays }
  | { kind: 'all' }
  | { kind: 'custom'; startDay: string; endDay: string }
  | { kind: 'project'; project: string; startDay: string; endDay: string }

export const DEFAULT_RANGE: RangeSel = { kind: 'preset', days: 30 }

/** 与后端 commands.rs MAX_RANGE_DAYS 同值（防御性限长）。 */
export const MAX_RANGE_DAYS = 3660

/** 小时粒度只在 ≤ 31 天的范围给（更长的范围小时点数无意义）。 */
export const HOUR_BUCKET_MAX_DAYS = 31

const pad2 = (n: number) => String(n).padStart(2, '0')

export const ymd = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`

export function todayYmd(): string {
  return ymd(new Date())
}

function parseYmd(s: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(s)
  if (!m) return null
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]))
  return d.getMonth() === Number(m[2]) - 1 ? d : null
}

/** 闭区间天数（start = end 为 1）。 */
export function spanDays(r: DayRange): number {
  const a = parseYmd(r.startDay)
  const b = parseYmd(r.endDay)
  if (!a || !b) return 0
  return Math.round((b.getTime() - a.getTime()) / 86_400_000) + 1
}

/** 选择 → 本地日闭区间。All 在数据跨度未到时退化为今天;项目 / 自定义的终点夹到今天。 */
export function resolveRange(sel: RangeSel, dataSpan: DaySpan | null): DayRange {
  const today = todayYmd()
  switch (sel.kind) {
    case 'preset': {
      const now = new Date()
      const start = new Date(now.getFullYear(), now.getMonth(), now.getDate())
      start.setDate(start.getDate() - (sel.days - 1))
      return { startDay: ymd(start), endDay: today }
    }
    case 'all':
      return { startDay: dataSpan && dataSpan.firstDay < today ? dataSpan.firstDay : today, endDay: today }
    case 'custom':
    case 'project': {
      const endDay = sel.endDay > today ? today : sel.endDay
      return { startDay: sel.startDay > endDay ? endDay : sel.startDay, endDay }
    }
  }
}

/** 自定义起止校验:合法返回 null,否则返回当前语言的提示（调用时刻取文案）。 */
export function validateCustom(startDay: string, endDay: string): string | null {
  const t = getT('insights')
  if (!parseYmd(startDay) || !parseYmd(endDay)) return t('rangeErrPickBoth')
  if (endDay > todayYmd()) return t('rangeErrEndAfterToday')
  if (startDay > endDay) return t('rangeErrStartAfterEnd')
  if (spanDays({ startDay, endDay }) > MAX_RANGE_DAYS) return t('rangeErrTooLong', { n: MAX_RANGE_DAYS })
  return null
}

/** en: "Sep 15, 2026" / "Jun 23 – Sep 15, 2026" / 跨年 "Dec 30, 2025 – Jan 2, 2026";
 * zh: "2026年9月15日" / "2026年6月23日 – 9月15日" / 跨年两端各带年。 */
export function formatSpan(r: DayRange): string {
  const a = parseYmd(r.startDay)
  const b = parseYmd(r.endDay)
  if (!a || !b) return `${r.startDay} – ${r.endDay}`
  const full = (d: Date) => fmt.date(d, { year: 'numeric', month: 'short', day: 'numeric' })
  if (r.startDay === r.endDay) return full(b)
  if (a.getFullYear() === b.getFullYear()) {
    const md = (d: Date) => fmt.date(d, { month: 'short', day: 'numeric' })
    return getT('insights')('spanSameYear', { a: md(a), b: md(b), y: fmt.date(b, { year: 'numeric' }) })
  }
  return `${full(a)} – ${full(b)}`
}

/** 短标签（占比环中心等）:7d / 30d / 90d / All / Custom / Project span（调用时刻的语言）。 */
export function rangeShortLabel(sel: RangeSel): string {
  const t = getT('insights')
  switch (sel.kind) {
    case 'preset':
      return t('rangePreset', { d: sel.days })
    case 'all':
      return t('rangeAll')
    case 'custom':
      return t('rangeCustom')
    case 'project':
      return t('rangeProjectSpan')
  }
}
