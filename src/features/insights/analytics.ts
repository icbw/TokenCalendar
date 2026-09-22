// 共享口径工具:项目展示名、时间成本格式化、z-score 离群判定。
// 矩阵（UsageMatrixView / MatrixPanel）、洞察（InsightsView）与 Tasks 视图共用,口径单一源。
// 项目键是解析层的有效键（合并目标 / __scratch / 原键）,展示名优先取后端标签缓存（alias）。
import { cachedProjectLabel } from '../../services/projectLabels'
import type { TokenMetric } from '../../services'

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

/** token 分项（互斥,相加 = Tokens 总量）。工具栏顺序:Tokens → Input → Cache write → Cache read → Output。
 * input = 未命中缓存的输入;cache_write = 写入提示缓存的输入（价格面板「缓存写」);
 * cache_read = 命中缓存、从缓存读出的输入（价格面板「缓存读」）。 */
export type TokenPart = 'input' | 'cache_write' | 'cache_read' | 'output'
export const TOKEN_PARTS: TokenPart[] = ['input', 'cache_write', 'cache_read', 'output']
/** 分项的价格顺序（单价从高到低）:图表色阶由深到浅、堆叠自下而上、图例与 tooltip 行都按此序。 */
export const PART_PRICE_ORDER: TokenPart[] = ['output', 'input', 'cache_write', 'cache_read']

/** label = 图表 / 图例全名;short = 工具栏按钮（Insights 工具栏单行不换行,默认 1120 宽窗口下放得下）。 */
export const TOKEN_METRIC_LABELS: Record<TokenMetric, { label: string; short: string; hint: string; unit: string }> = {
  total: { label: 'Tokens', short: 'Tokens', hint: 'Total tokens = input + cache write + cache read + output', unit: 'tokens' },
  input: { label: 'Input', short: 'Input', hint: 'Input tokens not served from cache (cache miss)', unit: 'input tokens' },
  cache_write: { label: 'Cache write', short: 'Cache W', hint: 'Cache write: input tokens written to the prompt cache', unit: 'cache-write tokens' },
  cache_read: { label: 'Cache read', short: 'Cache R', hint: 'Cache read: input tokens served from the prompt cache (cache hit)', unit: 'cache-read tokens' },
  output: { label: 'Output', short: 'Output', hint: 'Output tokens', unit: 'output tokens' },
}
export const TOKEN_METRICS: TokenMetric[] = ['total', ...TOKEN_PARTS]

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
