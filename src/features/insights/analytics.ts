// 共享口径工具:项目展示名、时长格式化、z-score 离群判定。
// 矩阵（UsageMatrixView / MatrixPanel）、洞察（InsightsView）与 Tasks 视图共用,口径单一源。
// 项目键是解析层的有效键（合并目标 / __scratch / 原键）,展示名优先取后端标签缓存（alias）。
import { cachedProjectLabel } from '../../services/projectLabels'
import type { TokenMetric } from '../../services'
import { fmt, getT, type MessageKey } from '../../lib/i18n'

/** 与 Rust turns:UNKNOWN_PROJECT 对齐（无目录源 / 解析失败）。 */
export const UNKNOWN_PROJECT_KEY = 'unknown'
/** 与 Rust project_meta:SCRATCH_KEY / HIDDEN_SLICE_KEY 对齐。 */
export const SCRATCH_PROJECT_KEY = '__scratch'
export const HIDDEN_PROJECTS_KEY = '__hidden'

/** project_key → 展示名:后端标签缓存（alias / 同名消歧）→ 固定名 → 路径最后一段。 */
export function projectDisplayName(key: string): string {
  const cached = cachedProjectLabel(key)
  if (cached) return cached
  if (key === UNKNOWN_PROJECT_KEY) return getT('insights')('projUnknown')
  if (key === SCRATCH_PROJECT_KEY) return getT('insights')('projScratch')
  if (key === HIDDEN_PROJECTS_KEY) return getT('insights')('projHidden')
  const parts = key.split('/').filter((s) => s.length > 0)
  return parts[parts.length - 1] ?? key
}

/** 项目行 / 图例的 hover 提示:完整路径（unknown / Scratch / Hidden 说明来源;有别名时首行为名称）。 */
export function projectTooltip(key: string): string {
  if (key === UNKNOWN_PROJECT_KEY) return getT('insights')('projUnknownTip')
  if (key === SCRATCH_PROJECT_KEY) return getT('insights')('projScratchTip')
  if (key === HIDDEN_PROJECTS_KEY) return getT('insights')('projHiddenTip')
  const name = cachedProjectLabel(key)
  return name && name !== key && !key.endsWith(`/${name}`) ? `${name}\n${key}` : key
}

/** token 分项（互斥,相加 = Tokens 总量）。工具栏顺序:Tokens → Input → Cache write → Cache read → Output。
 * input = 未命中缓存的输入;cache_write = 写入提示缓存的输入（价格面板「缓存写」);
 * cache_read = 命中缓存、从缓存读出的输入（价格面板「缓存读」）。 */
export type TokenPart = 'input' | 'cache_write' | 'cache_read' | 'output'
export const TOKEN_PARTS: TokenPart[] = ['input', 'cache_write', 'cache_read', 'output']
/** 分项的价格顺序（单价从高到低）:图表色阶由深到浅、堆叠自下而上、图例与 tooltip 行都按此序。 */
export const PART_PRICE_ORDER: TokenPart[] = ['output', 'input', 'cache_write', 'cache_read']

type MetricText = { label: string; short: string; hint: string; unit: string }
type K = MessageKey<'insights'>

/** 字段是 getter:每次读取按当前语言取文案（模块顶层不存成品字符串,切换语言即时生效）。 */
function metricText(label: K, short: K, hint: K, unit: K): MetricText {
  const t = () => getT('insights')
  return {
    get label() { return t()(label) },
    get short() { return t()(short) },
    get hint() { return t()(hint) },
    get unit() { return t()(unit) },
  }
}

/** label = 图表 / 图例全名;short = 工具栏按钮（Insights 工具栏单行不换行,默认 1120 宽窗口下放得下）。 */
export const TOKEN_METRIC_LABELS: Record<TokenMetric, MetricText> = {
  total: metricText('mTotal', 'mTotal', 'mTotalHint', 'mTotalUnit'),
  input: metricText('mInput', 'mInput', 'mInputHint', 'mInputUnit'),
  cache_write: metricText('mCacheWrite', 'mCacheWriteShort', 'mCacheWriteHint', 'mCacheWriteUnit'),
  cache_read: metricText('mCacheRead', 'mCacheReadShort', 'mCacheReadHint', 'mCacheReadUnit'),
  output: metricText('mOutput', 'mOutput', 'mOutputHint', 'mOutputUnit'),
}
export const TOKEN_METRICS: TokenMetric[] = ['total', ...TOKEN_PARTS]

/** 毫秒 → "3h 12m" / "45m" / "<1m"（0 → "0m";null → "—"）。 */
export function formatDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return '—'
  if (ms <= 0) return '0m'
  if (ms < 60_000) return '<1m'
  const totalMin = Math.floor(ms / 60_000)
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  if (h === 0) return `${m}m`
  if (h >= 100 || m === 0) return `${fmt.number(h)}h`
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
