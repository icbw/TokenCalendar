// 月矩阵色阶：P99 截断 + log1p 归一化，支持 global / perRow
// 与 react-grid-heatmap 的线性 min-max 不同；拉满让高值与中值不挤（视觉）

export type ScaleMode = 'global' | 'perRow'
export type CellStatus = 'future' | 'zero' | 'normal' | 'estimated' | 'error' | 'today' | 'selected'

/** 非零值的 P99 作为色阶上限，避免单日极端峰值压扁其余数据 */
export function p99Cap(values: number[]): number {
  const nz = values.filter((v) => v > 0).sort((a, b) => a - b)
  if (nz.length === 0) return 0
  const idx = Math.min(nz.length - 1, Math.floor(nz.length * 0.99))
  return nz[idx]
}

/** P95 截断（保留为兼容性；新代码用 p99Cap） */
export function p95Cap(values: number[]): number {
  const nz = values.filter((v) => v > 0).sort((a, b) => a - b)
  if (nz.length === 0) return 0
  const idx = Math.min(nz.length - 1, Math.floor(nz.length * 0.95))
  return nz[idx]
}

/** log1p 归一化：intensity = clamp（log1p（v)/log1p（cap), 0, 1) */
export function intensity(value: number, cap: number): number {
  if (value <= 0 || cap <= 0) return 0
  return Math.min(1, Math.log1p(value) / Math.log1p(cap))
}

/** 分档色带（视觉：4 桶 + 峰值档），让 log1p 压缩后的中低值仍有清晰色差。
 *  反馈：整体色系太深、饱和度拉满不好看 → 整条下移：
 *  桶 0~3 用 blue-100~400（浅到中浅），仅峰值用 blue-500，饱和度适中。 */
const PALETTE_NORMAL = [
  '#dbeafe', // blue-100  浅    t<0.35
  '#bfdbfe', // blue-200  中浅  0.35~0.7
  '#93c5fd', // blue-300  中    0.7~0.9
  '#60a5fa', // blue-400  中深  0.9~1.0
] as const
const PEAK_NORMAL = '#3b82f6' // blue-500 峰值（唯一饱和档）

const PALETTE_ESTIMATED = [
  '#fef3c7', // amber-100
  '#fde68a', // amber-200
  '#fcd34d', // amber-300
  '#fbbf24', // amber-400
] as const
const PEAK_ESTIMATED = '#f59e0b' // amber-500

/** 把 t 映射到分档索引 0~3，t≥1 时返回 4（峰值档） */
function bucketIndex(t: number): number {
  if (t >= 1) return 4
  if (t >= 0.9) return 3
  if (t >= 0.7) return 2
  if (t >= 0.35) return 1
  return 0
}

/** 非零单元格背景色（蓝阶分档）：P95 截断 + log1p，
 *  4 桶 + 峰值档让中低值有清晰色差。 */
export function normalCellBackground(value: number, cap: number): string {
  const t = intensity(value, cap)
  const idx = bucketIndex(t)
  return idx === 4 ? PEAK_NORMAL : PALETTE_NORMAL[idx]
}

/** 估算单元格背景（橙阶分档，配合 .is-estimated 虚线边框） */
export function estimatedCellBackground(value: number, cap: number): string {
  const t = intensity(value, cap)
  const idx = bucketIndex(t)
  return idx === 4 ? PEAK_ESTIMATED : PALETTE_ESTIMATED[idx]
}

export interface CellVisual {
  background: string
  status: CellStatus
}

/** 根据值 + 状态计算单元格视觉。
 *  - null = 未来：transparent，无数据也无底色干扰
 *  - 0 = 真实零：有底色（--zero-bg）—— 区别于"未来"
 *  - >0 = 正常/估算色阶
 *  - error = 红斜纹
 *  zeroBg 可覆盖零值底色（widget 卡片内用 --widget-zero-bg 保持可见） */
export function cellVisual(
  value: number | null,
  cap: number,
  opts: { estimated?: boolean; error?: boolean; zeroBg?: string } = {},
): CellVisual {
  if (value === null) return { background: 'var(--future-bg)', status: 'future' }
  if (value === 0) return { background: opts.zeroBg ?? 'var(--zero-bg)', status: 'zero' }
  if (opts.error) return { background: 'var(--error-bg)', status: 'error' }
  if (opts.estimated) {
    return { background: estimatedCellBackground(value, cap), status: 'estimated' }
  }
  return { background: normalCellBackground(value, cap), status: 'normal' }
}

/** 千分位格式化（tooltip 用完整格式，v0.3） */
export function formatFull(n: number): string {
  return n.toLocaleString('en-US')
}

/** 紧凑格式：1.2K / 3.4M */
export function formatCompact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return String(n)
}
