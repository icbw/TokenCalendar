// 共享口径工具:项目展示名、时间成本格式化、z-score 离群判定。
// 矩阵（UsageMatrixView / MatrixPanel）、洞察（InsightsView）与 Tasks 视图共用,口径单一源。
// 项目键是解析层的有效键（合并目标 / __scratch / 原键）,展示名优先取后端标签缓存（alias）。
import { cachedProjectLabel } from '../../services/projectLabels'

/** 与 Rust turns:UNKNOWN_PROJECT 对齐（无目录源 / 解析失败）。 */
export const UNKNOWN_PROJECT_KEY = 'unknown'
/** 与 Rust project_meta:SCRATCH_KEY / HIDDEN_SLICE_KEY 对齐。 */
export const SCRATCH_PROJECT_KEY = '__scratch'
export const HIDDEN_PROJECTS_KEY = '__hidden'

/** project_key → 展示名:后端标签缓存（alias / 同名消歧）→ 固定名 → 路径最后一段。 */
export function projectDisplayName(key: string): string {
  const cached = cachedProjectLabel(key)
  if (cached) return cached
  if (key === UNKNOWN_PROJECT_KEY) return 'Unknown project'
  if (key === SCRATCH_PROJECT_KEY) return 'Scratch'
  if (key === HIDDEN_PROJECTS_KEY) return 'Hidden projects'
  const parts = key.split('/').filter((s) => s.length > 0)
  return parts[parts.length - 1] ?? key
}

/** 项目行 / 图例的 hover 提示:完整路径（unknown / Scratch / Hidden 说明来源;有别名时首行为名称）。 */
export function projectTooltip(key: string): string {
  if (key === UNKNOWN_PROJECT_KEY) return 'No working directory recorded by the source'
  if (key === SCRATCH_PROJECT_KEY) return 'Scratch: short sessions collapsed by the project rule (Settings › Projects)'
  if (key === HIDDEN_PROJECTS_KEY) return 'Projects hidden in Settings › Projects'
  const name = cachedProjectLabel(key)
  return name && name !== key && !key.endsWith(`/${name}`) ? `${name}\n${key}` : key
}

/** 时间成本指标（毫秒值;与 token 并列不相加）。 */
export type TimeMetric = 'wait' | 'human'

export function isTimeMetric(m: string): m is TimeMetric {
  return m === 'wait' || m === 'human'
}

export const TIME_METRIC_LABELS: Record<TimeMetric, { label: string; hint: string; unit: string }> = {
  wait: { label: 'Wait', hint: 'Wait time: sum of turn wall-clock durations', unit: 'wait' },
  human: { label: 'Human', hint: 'Human time: sum of gaps between turns within the idle threshold', unit: 'human time' },
}

/** 毫秒 → "3h 12m" / "45m" / "<1m"（0 → "0m";null → "—"）。 */
export function formatDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return '—'
  if (ms <= 0) return '0m'
  if (ms < 60_000) return '<1m'
  const totalMin = Math.floor(ms / 60_000)
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  if (h === 0) return `${m}m`
  if (h >= 100 || m === 0) return `${h.toLocaleString('en-US')}h`
  return `${h}h ${m}m`
}

// ---- z-score（与 InsightsView 异常日同款阈值）----

export const OUTLIER_Z = 2.0
export const OUTLIER_MIN_SAMPLES = 7

/** 总体标准差 z-score;样本 < OUTLIER_MIN_SAMPLES 或 sd = 0 → null（不判）。 */
export function zScores(vals: number[]): number[] | null {
  if (vals.length < OUTLIER_MIN_SAMPLES) return null
  const mean = vals.reduce((a, b) => a + b, 0) / vals.length
  const sd = Math.sqrt(vals.reduce((a, b) => a + (b - mean) ** 2, 0) / vals.length)
  if (sd === 0) return null
  return vals.map((v) => (v - mean) / sd)
}
