// 手写 SVG 图表库：零依赖,项目不引图表库。
// 提供:平滑曲线（Catmull-Rom → bezier）、堆叠柱、环形占比、双组组合图、阶梯图,
// 统一 crosshair tooltip / 轴刻度 / 稳定配色（家族编码）。
import { useEffect, useMemo, useRef, useState } from 'react'
import { formatCompact, formatFull } from '../matrix/matrixScale'
import { useT } from '../../lib/i18n'

// ---- 配色 ----
// 家族绑定 + 精选色板:手工挑选的 12 色环（紫→蓝→青→品红域,避开与整体不协调的
// 黄绿/草绿）;家族按注册序依次绑定,族内成员同色相明度分层。
// 模块级注册表:同 key 永远同色（跨图表一致）。

/** HSL → CSS color string（h 度,s/l 0-1） */
export function hsl(h: number, s: number, l: number): string {
  return `hsl(${((h % 360) + 360) % 360} ${Math.round(s * 100)}% ${Math.round(l * 100)}%)`
}

/** 模型家族键（与 UsageMatrixView.familyOf 同一支:首段即家族）。 */
function familyOf(key: string): string {
  return key.split(/[-_/]/)[0] ?? key
}

/** 家族 → 色相绑定池（协调紫蓝系,饱和度/明度统一在图表友好区）。 */
export const FAMILY_HUES = [
  262, // 紫（主品牌族）
  217, // 蓝
  190, // 青
  292, // 品紫
  232, // 靛蓝
  322, // 品红
  205, // 天蓝
  340, // 玫红
  245, // 蓝紫
  172, // 蓝青
  275, // 紫罗兰
  200, // 深天蓝
] as const

const familyRegistry = new Map<string, { hue: number; members: Map<string, number> }>()
let familySlot = 0

/** 系列 → 稳定色:族间精选紫蓝系色相 + 族内明度分层（最多 5 层,超出回绕并降饱和）。 */
export function colorFor(key: string): string {
  const { h, s, l } = colorParts(key)
  return hsl(h, s, l)
}

/** 同一系列的 n 级深浅（[0] = colorFor（key) 本色,逐级变浅到接近底色）。
 * 用于 token 分项:按价格从高到低（Output → Input → Cache write → Cache read）依次取色,
 * 最深一级与该系列总量同色。 */
export function colorShades(key: string, n: number): string[] {
  const { h, s, l } = colorParts(key)
  return shadeLadder(h, s, l, n)
}

/** 从 （h,s,l) 起逐级均匀变浅到 L=0.88 的 n 级色阶（最浅一级在白底上仍可辨）。 */
export function shadeLadder(h: number, s: number, l: number, n: number): string[] {
  const top = 0.88
  return Array.from({ length: n }, (_, i) => hsl(h, s, n <= 1 ? l : l + ((top - l) * i) / (n - 1)))
}

/** 分项之外的「只报总量、无拆分」余量（Codex）:不属于价格色阶,用中性灰。 */
export const UNITEMIZED_COLOR = hsl(220, 0.08, 0.74)

function colorParts(key: string): { h: number; s: number; l: number } {
  const fam = familyOf(key)
  let entry = familyRegistry.get(fam)
  if (!entry) {
    entry = { hue: FAMILY_HUES[familySlot++ % FAMILY_HUES.length], members: new Map() }
    familyRegistry.set(fam, entry)
  }
  let idx = entry.members.get(key)
  if (idx === undefined) {
    idx = entry.members.size
    entry.members.set(key, idx)
  }
  const layer = idx % 5
  const light = 0.68 - layer * 0.09 // 0.68/0.59/0.50/0.41/0.32
  const sat = idx >= 5 ? 0.5 : 0.66
  return { h: entry.hue, s: sat, l: light }
}

/** 系列实际用色:显式 color 优先（分项深浅),否则按 key 稳定取色。 */
function seriesColor(s: { key: string; color?: string }): string {
  return s.color ?? colorFor(s.key)
}

// 双组图合计模式:tokens 四分项取蓝色阶（Output 最深 → Cache read 最浅）,credit = 玫红——
// 玫红与紫蓝系协调,琥珀等暖黄不协调;credit 曲线与主图同用 smoothPath。
export const COMBO_PART_SHADES = shadeLadder(217, 0.72, 0.45, 4)
export const COMBO_CREDIT = hsl(348, 0.78, 0.58)

// ---- 公共几何 ----

export interface SeriesSpec {
  key: string
  label: string
  values: number[]
  /** 覆盖按 key 取的稳定色（单模型分项视图用同色深浅）。 */
  color?: string
}

/** 数值格式化钩子（缺省 = token 口径:hover 千分位、轴 / 环紧凑）。
 * 时间成本指标（毫秒）传入 formatDuration,hover / 轴 / 环同一格式。 */
export type ValueFormat = (v: number) => string

interface Margin {
  top: number
  right: number
  bottom: number
  left: number
}

// top 20:最高档 Y 轴刻度文本（10px,基线在刻度线 +3）需要顶部余量,否则被裁。
// left 52 配合 formatAxis（去尾 .0、≥1000M 进位 B）,最宽刻度文本不被左缘裁切。
const MARGIN: Margin = { top: 20, right: 16, bottom: 22, left: 52 }

/** 坐标轴刻度专用紧凑格式:1B / 750M / 187.5M / 12.5K——去掉无意义的 .0。 */
function formatAxis(n: number): string {
  const trim = (v: number) => v.toFixed(1).replace(/\.0$/, '')
  if (n >= 1_000_000_000) return `${trim(n / 1_000_000_000)}B`
  if (n >= 1_000_000) return `${trim(n / 1_000_000)}M`
  if (n >= 1_000) return `${trim(n / 1_000)}K`
  return String(n)
}

function niceMax(v: number): number {
  if (v <= 0) return 1
  const exp = Math.floor(Math.log10(v))
  const base = 10 ** exp
  const f = v / base
  const nice = f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10
  return nice * base
}

function yTicks(max: number): number[] {
  return [0, max / 4, max / 2, (max * 3) / 4, max]
}

/** Catmull-Rom → cubic bezier 平滑路径（张力 1/6,无过冲尖刺）。 */
export function smoothPath(pts: [number, number][]): string {
  if (pts.length === 0) return ''
  if (pts.length < 3) return pts.map((p, i) => `${i === 0 ? 'M' : 'L'}${p[0]},${p[1]}`).join(' ')
  let d = `M${pts[0][0]},${pts[0][1]}`
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[Math.max(0, i - 1)]
    const p1 = pts[i]
    const p2 = pts[i + 1]
    const p3 = pts[Math.min(pts.length - 1, i + 2)]
    const c1x = p1[0] + (p2[0] - p0[0]) / 6
    const c1y = p1[1] + (p2[1] - p0[1]) / 6
    const c2x = p2[0] - (p3[0] - p1[0]) / 6
    const c2y = p2[1] - (p3[1] - p1[1]) / 6
    d += ` C${c1x.toFixed(1)},${c1y.toFixed(1)} ${c2x.toFixed(1)},${c2y.toFixed(1)} ${p2[0].toFixed(1)},${p2[1].toFixed(1)}`
  }
  return d
}

function xTickLabel(bucket: string, total: number, i: number): string {
  // 自适应密度:最多 ~8 个刻度
  const step = Math.max(1, Math.ceil(total / 8))
  if (i % step !== 0 && i !== total - 1) return ''
  if (bucket.includes(' ')) return bucket.slice(5, 10) // hour bucket "YYYY-MM-DD HH" → "MM-DD"
  return bucket.slice(5).replace(/^0/, '')
}

/** 热力图格子列几何（CSS 像素）：图表叠在格子区下方时用它把点 / 柱对到格子中心。 */
export interface CellColumns {
  cell: number
  gap: number
}

/** 紧凑模式（无 Y 轴）margin,SVG 宽 = 格子区宽 W = n·cell + （n−1)·gap。
 * 第 i 格中心 = cell/2 + i·（cell+gap),对 i 线性,两种图各自解出 margin:
 * - `edge`（折线,点从绘图区左缘等距排到右缘）：margin = cell/2;
 * - `slot`（柱,点在 slot 中心、slot = cell+gap）：margin = −gap/2,slot 两端各伸出半个间距。
 * 换成 viewBox 单位乘 760/W。没有列几何时退回 slot = 宽/（n+1) 的近似。 */
function compactMargin(n: number, showXAxis: boolean, cols: CellColumns | undefined, anchor: 'edge' | 'slot'): Margin {
  const colsW = cols && n > 0 ? n * cols.cell + (n - 1) * cols.gap : 0
  const m = colsW > 0
    ? (760 * (anchor === 'edge' ? cols!.cell / 2 : -cols!.gap / 2)) / colsW
    : n > 0 ? 760 / (2 * (n + 1)) : 12
  return { top: MARGIN.top, right: m, bottom: showXAxis ? MARGIN.bottom : 6, left: m }
}

/** SVG 实际渲染宽 / viewBox 宽 —— tooltip 反缩放系数。图表随格子
 * 缩放时 viewBox 内的 foreignObject 会跟着缩小/放大,tooltip 必须以真实
 * CSS 像素显示（否则小窗下小到不可读）。 */
function useSvgScale(ref: React.RefObject<SVGSVGElement | null>): number {
  const [scale, setScale] = useState(1)
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const update = () => setScale(el.getBoundingClientRect().width / 760 || 1)
    update()
    const ro = new ResizeObserver(update)
    ro.observe(el)
    return () => ro.disconnect()
  }, [ref])
  return scale
}

interface HoverState {
  index: number
  x: number
  y: number
}

// ---- 通用 hover 层（crosshair + 卡片 tooltip） ----

function HoverCard({ hover, plot, bucketLabels, series, height, margin, yMax, stackedTotal, scale = 1, fmt = formatFull, ordered = false }: {
  hover: HoverState
  plot: { w: number; h: number }
  bucketLabels: string[]
  series: SeriesSpec[]
  height: number
  margin: Margin
  yMax: number
  stackedTotal?: boolean
  /** SVG 渲染缩放系数——foreignObject 内容按 1/scale 放大,图表随
   * 格子缩小时 tooltip 保持真实 CSS 像素尺寸可读。 */
  scale?: number
  fmt?: ValueFormat
  /** true = 行按系列原顺序（分项的价格顺序）,不按值降序。 */
  ordered?: boolean
}) {
  const t = useT('insights')
  const i = hover.index
  const total = series.reduce((s, sr) => s + (sr.values[i] ?? 0), 0)
  // 行数动态上限:图表高度放得下几行就列几行,余量并作「+N more」摘要行,
  // 卡体永不超出图表高度。不用卡内滚动:卡片 pointerEvents none,滚动条无法交互
  // （鼠标移近即触发 crosshair 重渲染）。
  const ROW_H = 18
  const MORE_H = 16
  const HEAD_H = 32 // 卡片 padding+border+标题行固定开销
  const allRows = series
    .map((s) => ({ label: s.label, value: s.values[i] ?? 0, color: seriesColor(s) }))
    .filter((r) => r.value > 0)
    .sort((a, b) => (ordered ? 0 : b.value - a.value))
  const availH = Math.max(HEAD_H + ROW_H, height - 10)
  const fitsAll = HEAD_H + allRows.length * ROW_H <= availH
  const shown = fitsAll ? allRows.length : Math.max(1, Math.floor((availH - HEAD_H - MORE_H) / ROW_H))
  const rows = allRows.slice(0, shown)
  const restCount = allRows.length - rows.length
  const restValue = allRows.slice(shown).reduce((s, r) => s + r.value, 0)
  const inv = 1 / scale
  const cardW = 190 * inv
  const rowsH = HEAD_H + rows.length * ROW_H + (restCount > 0 ? MORE_H : 0)
  const cardH = rowsH * inv
  const flip = hover.x > plot.w / 2
  // 水平钳制:窄面板下按中点 flip 后 left 仍可能越左缘。
  const totalW = margin.left + plot.w + margin.right
  const left = Math.max(2, Math.min(flip ? hover.x - cardW - 12 : hover.x + 12, totalW - cardW - 2))
  const top = Math.max(2, Math.min(margin.top, height - cardH - margin.bottom))
  void yMax
  void stackedTotal
  return (
    <g>
      <line
        x1={hover.x} x2={hover.x} y1={margin.top} y2={margin.top + plot.h}
        stroke="var(--border-strong)" strokeDasharray="3 3" strokeWidth={1}
      />
      <foreignObject x={left} y={top} width={cardW} height={cardH}>
        <div
          style={{
            background: 'var(--panel)', border: '1px solid var(--border)', borderRadius: 8,
            boxShadow: 'var(--shadow-2)', padding: '6px 10px', fontSize: 11,
            color: 'var(--text)',
            transform: `scale(${inv})`, transformOrigin: 'top left',
            width: 190, boxSizing: 'border-box', pointerEvents: 'none', whiteSpace: 'nowrap',
          }}
        >
          <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12, fontWeight: 700, marginBottom: 3 }}>
            <span>{bucketLabels[i]}</span>
            <span style={{ fontVariantNumeric: 'tabular-nums' }}>{fmt(total)}</span>
          </div>
          {rows.map((r, ri) => (
            <div key={`${ri}-${r.label}`} style={{ display: 'flex', alignItems: 'center', gap: 5, lineHeight: '18px' }}>
              <span style={{ width: 7, height: 7, borderRadius: 2, background: r.color, flexShrink: 0 }} />
              <span style={{ color: 'var(--text-muted)', overflow: 'hidden', textOverflow: 'ellipsis', flex: 1 }}>{r.label}</span>
              <span style={{ fontVariantNumeric: 'tabular-nums' }}>{fmt(r.value)}</span>
            </div>
          ))}
          {restCount > 0 && (
            <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12, lineHeight: '16px', color: 'var(--text-faint)' }}>
              <span>{t('chartMore', { n: restCount })}</span>
              <span style={{ fontVariantNumeric: 'tabular-nums' }}>{fmt(restValue)}</span>
            </div>
          )}
          {allRows.length === 0 && <div style={{ color: 'var(--text-faint)' }}>{t('chartNoData')}</div>}
        </div>
      </foreignObject>
    </g>
  )
}

// ---- 曲线图（多系列平滑 + 渐变填充） ----
// showYAxis=false 去左轴刻度文本,showGrid=false 去横向网格线（Matrix 面板浮层用:
// 叠在热力图上,轴/网格纯添乱）。
// showXAxis=false 再去底部日期刻度（面板与矩阵格子共用列模板,日期由表头行表达）;
// 无 Y 轴时 margin 走 compactMargin,绘图区与格子列对齐,宽度随 --cells-w 等比缩放。

export function LineChart({ series, buckets, height = 220, showYAxis = true, showGrid = true, showXAxis = true, formatValue, columns, ordered }: {
  /** tooltip 行保持系列顺序（分项视图）。 */
  ordered?: boolean
  series: SeriesSpec[]
  buckets: string[]
  height?: number
  showYAxis?: boolean
  showGrid?: boolean
  showXAxis?: boolean
  formatValue?: ValueFormat
  /** 紧凑模式下对齐的热力图格子列几何（见 compactMargin）。 */
  columns?: CellColumns
}) {
  const width = 760
  const n = buckets.length
  const [hover, setHover] = useState<HoverState | null>(null)
  const svgRef = useRef<SVGSVGElement>(null)
  const svgScale = useSvgScale(svgRef)
  const margin: Margin = showYAxis
    ? MARGIN
    : compactMargin(n, showXAxis, columns, 'edge')
  const plot = { w: width - margin.left - margin.right, h: height - margin.top - margin.bottom }
  const yMax = useMemo(() => niceMax(Math.max(1, ...series.flatMap((s) => s.values))), [series])
  const xOf = (i: number) => margin.left + (n <= 1 ? plot.w / 2 : (i / (n - 1)) * plot.w)
  const yOf = (v: number) => margin.top + plot.h - (v / yMax) * plot.h

  const onMove = (e: React.MouseEvent) => {
    const rect = svgRef.current?.getBoundingClientRect()
    if (!rect || n === 0) return
    const x = ((e.clientX - rect.left) / rect.width) * width
    const i = Math.round(((x - margin.left) / plot.w) * (n - 1))
    setHover({ index: Math.max(0, Math.min(n - 1, i)), x: xOf(Math.max(0, Math.min(n - 1, i))) , y: e.clientY - rect.top })
  }

  return (
    <svg
      ref={svgRef} viewBox={`0 0 ${width} ${height}`} style={{ width: '100%', display: 'block' }}
      onMouseMove={onMove} onMouseLeave={() => setHover(null)}
    >
      <defs>
        {/* id 带系列序号:项目路径含非 ASCII 字符时清洗后会撞名（渐变串色）*/}
        {series.map((s, si) => (
          <linearGradient key={s.key} id={`grad-${si}-${s.key.replace(/[^a-zA-Z0-9]/g, '_')}`} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={seriesColor(s)} stopOpacity={0.22} />
            <stop offset="100%" stopColor={seriesColor(s)} stopOpacity={0.02} />
          </linearGradient>
        ))}
      </defs>
      {showGrid && yTicks(yMax).map((t, i) => (
        <g key={i}>
          <line x1={margin.left} x2={margin.left + plot.w} y1={yOf(t)} y2={yOf(t)} stroke="var(--border)" strokeWidth={1} />
          <text x={margin.left - 6} y={yOf(t) + 3} textAnchor="end" fontSize={10} fill="var(--text-faint)">
            {(formatValue ?? formatAxis)(t)}
          </text>
        </g>
      ))}
      {showXAxis && buckets.map((b, i) => {
        const lbl = xTickLabel(b, n, i)
        return lbl ? (
          <text key={b} x={xOf(i)} y={height - 6} textAnchor="middle" fontSize={10} fill="var(--text-faint)">{lbl}</text>
        ) : null
      })}
      {series.map((s, si) => {
        const pts = s.values.map((v, i) => [xOf(i), yOf(v)] as [number, number])
        const line = smoothPath(pts)
        const area = `${line} L${xOf(n - 1)},${margin.top + plot.h} L${xOf(0)},${margin.top + plot.h} Z`
        return (
          <g key={s.key}>
            <path d={area} fill={`url(#grad-${si}-${s.key.replace(/[^a-zA-Z0-9]/g, '_')})`} />
            <path d={line} fill="none" stroke={seriesColor(s)} strokeWidth={1.8} strokeLinejoin="round" strokeLinecap="round" />
          </g>
        )
      })}
      {hover && hover.index < n && (
        <>
          {series.map((s) => (
            <circle
              key={s.key} cx={xOf(hover.index)} cy={yOf(s.values[hover.index] ?? 0)} r={3}
              fill={seriesColor(s)} stroke="var(--panel)" strokeWidth={1.5}
            />
          ))}
          <HoverCard
            hover={hover} plot={plot} bucketLabels={buckets} series={series} height={height} margin={margin} yMax={yMax} scale={svgScale} fmt={formatValue} ordered={ordered}
          />
        </>
      )}
    </svg>
  )
}

// ---- 堆叠柱图（圆角顶 + crosshair） ----

export function StackedBarChart({ series, buckets, height = 220, showYAxis = true, showGrid = true, showXAxis = true, formatValue, columns, ordered }: {
  /** tooltip 行保持系列顺序（分项视图）。 */
  ordered?: boolean
  series: SeriesSpec[]
  buckets: string[]
  height?: number
  showYAxis?: boolean
  showGrid?: boolean
  showXAxis?: boolean
  formatValue?: ValueFormat
  /** 紧凑模式下对齐的热力图格子列几何（见 compactMargin）。 */
  columns?: CellColumns
}) {
  const width = 760
  const [hover, setHover] = useState<HoverState | null>(null)
  const svgRef = useRef<SVGSVGElement>(null)
  const svgScale = useSvgScale(svgRef)
  const margin: Margin = showYAxis
    ? MARGIN
    : compactMargin(buckets.length, showXAxis, columns, 'slot')
  const plot = { w: width - margin.left - margin.right, h: height - margin.top - margin.bottom }
  const totals = buckets.map((_, i) => series.reduce((s, sr) => s + (sr.values[i] ?? 0), 0))
  const yMax = useMemo(() => niceMax(Math.max(1, ...totals)), [totals])
  const n = buckets.length
  const slot = n > 0 ? plot.w / n : plot.w
  const barW = Math.max(2, Math.min(slot * 0.72, 26))

  const onMove = (e: React.MouseEvent) => {
    const rect = svgRef.current?.getBoundingClientRect()
    if (!rect || n === 0) return
    const x = ((e.clientX - rect.left) / rect.width) * width
    const i = Math.floor((x - margin.left) / slot)
    if (i < 0 || i >= n) { setHover(null); return }
    setHover({ index: i, x: margin.left + (i + 0.5) * slot, y: e.clientY - rect.top })
  }

  return (
    <svg
      ref={svgRef} viewBox={`0 0 ${width} ${height}`} style={{ width: '100%', display: 'block' }}
      onMouseMove={onMove} onMouseLeave={() => setHover(null)}
    >
      {showGrid && yTicks(yMax).map((t, i) => {
        const y = margin.top + plot.h - (t / yMax) * plot.h
        return (
          <g key={i}>
            <line x1={margin.left} x2={margin.left + plot.w} y1={y} y2={y} stroke="var(--border)" strokeWidth={1} />
            <text x={margin.left - 6} y={y + 3} textAnchor="end" fontSize={10} fill="var(--text-faint)">
              {(formatValue ?? formatAxis)(t)}
            </text>
          </g>
        )
      })}
      {showXAxis && buckets.map((b, i) => {
        const lbl = xTickLabel(b, n, i)
        return lbl ? (
          <text key={b} x={margin.left + (i + 0.5) * slot} y={height - 6} textAnchor="middle" fontSize={10} fill="var(--text-faint)">{lbl}</text>
        ) : null
      })}
      {buckets.map((b, i) => {
        let acc = 0
        const x = margin.left + (i + 0.5) * slot - barW / 2
        return (
          <g key={b}>
            {series.map((s) => {
              const v = s.values[i] ?? 0
              if (v <= 0) return null
              const h = (v / yMax) * plot.h
              const y = margin.top + plot.h - acc - h
              acc += h
              const isTop = acc >= totals[i] - 0.001
              const r = Math.min(3, barW / 2)
              return (
                <path
                  key={s.key}
                  // 顶部圆角只在最上段画（d 用 rect 近似:顶层圆角,其余直角）
                  d={isTop && r > 0
                    ? `M${x},${y + r} Q${x},${y} ${x + r},${y} L${x + barW - r},${y} Q${x + barW},${y} ${x + barW},${y + r} L${x + barW},${margin.top + plot.h} L${x},${margin.top + plot.h} Z`
                    : `M${x},${y} L${x + barW},${y} L${x + barW},${y + h} L${x},${y + h} Z`}
                  fill={seriesColor(s)}
                />
              )
            })}
          </g>
        )
      })}
      {hover && hover.index < n && (
        <HoverCard
          hover={hover} plot={plot} bucketLabels={buckets} series={series} height={height} margin={margin} yMax={yMax} stackedTotal scale={svgScale} fmt={formatValue} ordered={ordered}
        />
      )}
    </svg>
  )
}

// ---- 双组组合图:tokens 分项堆叠柱 + credit 曲线叠加 ----
// 两套账本两个纵轴（左 = tokens,右 = credit）,共享横轴 = 天;积分曲线只画到
// 导出覆盖的最后一天（数据缺口断线表达,不伪装成 0）。
// 多个模型画在同一张图里:天粒度每日一组并排柱（每模型一根,柱内分项堆叠,取该模型色阶）,
// 每模型一条 credit 曲线（模型本色）;小时粒度每日内 24 根细柱（只画第一个柱组）。

export interface ComboPart {
  key: string
  label: string
}

/** 一个柱组（合计或一个模型）。values / hours 的分项维与 parts 同序,自下而上堆叠。 */
export interface ComboGroup {
  key: string
  label: string
  /** 与 parts 同序的分项色。 */
  colors: string[]
  /** [桶][分项] */
  values: number[][]
  /** 小时粒度:[桶][小时 0-23][分项]（缺省 = 天粒度单柱）。 */
  hours?: number[][][]
}

/** 一条 credit 曲线;null = 积分账本未覆盖该日（断线,≠0）。 */
export interface ComboCredit {
  key: string
  label: string
  color: string
  values: (number | null)[]
}

export function ComboChart({ parts, groups, credits, buckets, height = 230 }: {
  parts: ComboPart[]
  groups: ComboGroup[]
  credits: ComboCredit[]
  buckets: string[]
  height?: number
}) {
  const t = useT('insights')
  const width = 760
  const [hover, setHover] = useState<HoverState | null>(null)
  const svgRef = useRef<SVGSVGElement>(null)
  // 双纵轴需要更宽的右缘,否则右轴刻度文本（如 200.0M）被右边缘裁切。
  const margin = { ...MARGIN, right: 52 }
  const plot = { w: width - margin.left - margin.right, h: height - margin.top - margin.bottom }
  const n = buckets.length
  const slot = n > 0 ? plot.w / n : plot.w
  const hasHours = groups.some((g) => (g.hours?.length ?? 0) > 0)
  const G = Math.max(1, groups.length)
  const gap = G > 1 ? 1 : 0
  const barW = hasHours
    ? Math.max(1.5, Math.min((slot * 0.9) / 24 - 0.6, 6))
    : Math.max(1.5, Math.min((slot * 0.8 - (G - 1) * gap) / G, 26))

  const sum = (v: number[] | undefined) => (v ?? []).reduce((a, b) => a + b, 0)
  const groupTotal = (g: ComboGroup, i: number) => sum(g.values[i])
  const barPeak = hasHours
    ? Math.max(1, ...groups.flatMap((g) => (g.hours ?? []).flatMap((day) => day.map((h) => sum(h)))))
    : Math.max(1, ...groups.flatMap((g) => buckets.map((_, i) => groupTotal(g, i))))
  const tokMax = niceMax(barPeak)
  const creditVals = credits.flatMap((c) => c.values).filter((c): c is number => c !== null && c > 0)
  // credit 恒 ≥ 0;轴上限给 10% 余量防贴顶
  const creditMax = niceMax(creditVals.length > 0 ? Math.max(...creditVals) * 1.1 : 1)

  const base = margin.top + plot.h
  const xOf = (i: number) => margin.left + (i + 0.5) * slot
  const yTok = (v: number) => base - (v / tokMax) * plot.h
  const yCredit = (v: number) => base - (v / creditMax) * plot.h

  // 一根分项堆叠柱:底 = 第 0 项（价格最高的 Output）;只有最上一段画顶部圆角
  const stackBar = (key: string, x: number, w: number, vals: number[], colors: string[]) => {
    if (sum(vals) <= 0) return null
    let acc = 0
    const rr = Math.min(3, w / 2)
    return (
      <g key={key}>
        {vals.map((v, pi) => {
          if (v <= 0) return null
          const h = (v / tokMax) * plot.h
          const y = base - acc - h
          acc += h
          const isTop = vals.slice(pi + 1).every((u) => u <= 0)
          return (
            <path
              key={pi}
              d={isTop && rr > 0 && h > rr
                ? `M${x},${y + rr} Q${x},${y} ${x + rr},${y} L${x + w - rr},${y} Q${x + w},${y} ${x + w},${y + rr} L${x + w},${y + h} L${x},${y + h} Z`
                : `M${x},${y} L${x + w},${y} L${x + w},${y + h} L${x},${y + h} Z`}
              fill={colors[pi]}
            />
          )
        })}
      </g>
    )
  }

  // credit 曲线:跳过 null（账本未覆盖日断线）;连续 <2 点不画曲线只画点。
  const creditPaths = credits.map((c) => {
    const pts: [number, number][] = []
    buckets.forEach((_, i) => {
      const v = c.values[i]
      if (v !== null && v !== undefined) pts.push([xOf(i), yCredit(v)])
    })
    return { ...c, pts, line: smoothPath(pts) }
  })

  const onMove = (e: React.MouseEvent) => {
    const rect = svgRef.current?.getBoundingClientRect()
    if (!rect || n === 0) return
    const x = ((e.clientX - rect.left) / rect.width) * width
    const i = Math.floor((x - margin.left) / slot)
    if (i < 0 || i >= n) { setHover(null); return }
    setHover({ index: i, x: xOf(i), y: e.clientY - rect.top })
  }

  // tooltip:单柱组 = 各分项（价格顺序）;多柱组 = 每模型总量;再列各 credit
  type Row = { key: string; label: string; value: number; color: string; credit?: boolean }
  const tooltipRows: Row[] = []
  let dayTotal = 0
  let uncovered = false
  if (hover) {
    const i = hover.index
    dayTotal = groups.reduce((a, g) => a + groupTotal(g, i), 0)
    if (groups.length === 1) {
      parts.forEach((p, pi) => {
        const v = groups[0].values[i]?.[pi] ?? 0
        if (v > 0) tooltipRows.push({ key: p.key, label: p.label, value: v, color: groups[0].colors[pi] })
      })
    } else {
      for (const g of groups) {
        const v = groupTotal(g, i)
        if (v > 0) tooltipRows.push({ key: `t:${g.key}`, label: g.label, value: v, color: g.colors[0] })
      }
    }
    for (const c of credits) {
      const v = c.values[i]
      if (v !== null && v !== undefined) {
        tooltipRows.push({ key: `c:${c.key}`, label: credits.length > 1 ? t('chartModelCredit', { label: c.label }) : t('chartCredit'), value: v, color: c.color, credit: true })
      } else {
        uncovered = true
      }
    }
  }
  const tipW = 220

  return (
    <svg
      ref={svgRef} viewBox={`0 0 ${width} ${height}`} style={{ width: '100%', display: 'block' }}
      onMouseMove={onMove} onMouseLeave={() => setHover(null)}
    >
      <defs>
        {/* 单条 credit 曲线带渐变面积;多条（按模型）只画线,免得面积互相遮盖*/}
        <linearGradient id="combo-credit-grad" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={COMBO_CREDIT} stopOpacity={0.18} />
          <stop offset="100%" stopColor={COMBO_CREDIT} stopOpacity={0.02} />
        </linearGradient>
      </defs>
      {/* 左轴网格（tokens）+ 右轴刻度（credit,共享网格线,按各自量程标值）*/}
      {yTicks(tokMax).map((t, i) => (
        <g key={i}>
          <line x1={margin.left} x2={margin.left + plot.w} y1={yTok(t)} y2={yTok(t)} stroke="var(--border)" strokeWidth={1} />
          <text x={margin.left - 6} y={yTok(t) + 3} textAnchor="end" fontSize={10} fill="var(--text-faint)">
            {formatAxis(t)}
          </text>
          <text x={margin.left + plot.w + 6} y={yTok(t) + 3} textAnchor="start" fontSize={10} fill={COMBO_CREDIT} opacity={0.75}>
            {formatAxis((t / tokMax) * creditMax)}
          </text>
        </g>
      ))}
      {buckets.map((b, i) => {
        const lbl = xTickLabel(b, n, i)
        return lbl ? (
          <text key={b} x={xOf(i)} y={height - 6} textAnchor="middle" fontSize={10} fill="var(--text-faint)">{lbl}</text>
        ) : null
      })}
      {/* tokens 柱:小时档 = 组内 24 根细柱;天档 = 每模型一根并排,柱内分项堆叠,柱高 = 该模型总 tokens*/}
      {buckets.map((b, i) => {
        if (hasHours) {
          const g = groups[0]
          const hours = g?.hours?.[i] ?? []
          const x0 = margin.left + i * slot + (slot - 24 * barW) / 2
          return <g key={b}>{hours.map((vals, h) => stackBar(`h${h}`, x0 + h * barW, Math.max(1, barW - 0.4), vals, g.colors))}</g>
        }
        const x0 = xOf(i) - (G * barW + (G - 1) * gap) / 2
        return <g key={b}>{groups.map((g, gi) => stackBar(g.key, x0 + gi * (barW + gap), barW, g.values[i] ?? [], g.colors))}</g>
      })}
      {/* credit 曲线（右轴,平滑）+ 覆盖区内数据点*/}
      {creditPaths.map((c) => (
        <g key={c.key}>
          {c.pts.length >= 2 && credits.length === 1 && (
            <path
              d={`${c.line} L${c.pts[c.pts.length - 1][0]},${base} L${c.pts[0][0]},${base} Z`}
              fill="url(#combo-credit-grad)"
            />
          )}
          {c.pts.length >= 2 && (
            <path d={c.line} fill="none" stroke={c.color} strokeWidth={1.8} strokeLinejoin="round" strokeLinecap="round" />
          )}
          {c.pts.map(([px, py], i) => (
            <circle key={i} cx={px} cy={py} r={2} fill={c.color} />
          ))}
        </g>
      ))}
      {hover && hover.index < n && (
        <g>
          <line
            x1={hover.x} x2={hover.x} y1={margin.top} y2={base}
            stroke="var(--border-strong)" strokeDasharray="3 3" strokeWidth={1}
          />
          <foreignObject
            x={hover.x > plot.w / 2 ? hover.x - tipW - 12 : hover.x + 12}
            y={margin.top}
            width={tipW}
            height={34 + tooltipRows.length * 18 + (uncovered ? 16 : 0)}
          >
            <div
              style={{
                background: 'var(--panel)', border: '1px solid var(--border)', borderRadius: 8,
                boxShadow: 'var(--shadow-2)', padding: '6px 10px', fontSize: 11, color: 'var(--text)',
                pointerEvents: 'none', whiteSpace: 'nowrap',
              }}
            >
              <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12, fontWeight: 700, marginBottom: 3 }}>
                <span>{buckets[hover.index]}</span>
                <span style={{ fontVariantNumeric: 'tabular-nums' }}>{formatFull(dayTotal)}</span>
              </div>
              {tooltipRows.map((r) => (
                <div key={r.key} style={{ display: 'flex', alignItems: 'center', gap: 5, lineHeight: '18px' }}>
                  <span style={{ width: 7, height: 7, borderRadius: r.credit ? 4 : 2, background: r.color, flexShrink: 0 }} />
                  <span style={{ color: 'var(--text-muted)', flex: 1, overflow: 'hidden', textOverflow: 'ellipsis' }}>{r.label}</span>
                  <span style={{ fontVariantNumeric: 'tabular-nums' }}>{r.credit ? r.value.toFixed(2) : formatFull(r.value)}</span>
                </div>
              ))}
              {tooltipRows.length === 0 && <div style={{ color: 'var(--text-faint)' }}>{t('chartNoData')}</div>}
              {tooltipRows.length > 0 && uncovered && (
                <div style={{ color: 'var(--text-faint)', lineHeight: '16px' }}>{t('chartCreditUncovered')}</div>
              )}
            </div>
          </foreignObject>
        </g>
      )}
    </svg>
  )
}

// ---- 环形占比图（圆环居中,图例分列环体左右,压缩高度） ----
// 每条目 = 色点 + 名称 | tokens 值 + 百分比（值/百分比放条目行两端）;
// 超过每侧 6 项截断,余量并入「+N more」行（title 提示完整清单,不做 tooltip 卡）。

export function DonutChart({ items, centerLabel, unitLabel, height = 190, formatValue = formatCompact, titleFor }: {
  items: { key: string; label: string; value: number; color?: string }[]
  centerLabel: string
  unitLabel: string
  height?: number
  formatValue?: ValueFormat
  /** 图例名 hover 提示（缺省 = label;项目维传完整路径）。 */
  titleFor?: (key: string, label: string) => string
}) {
  const t = useT('insights')
  const total = items.reduce((s, it) => s + it.value, 0)
  if (total <= 0 || items.length === 0) {
    return <div className="insight-empty">{t('chartNoData')}</div>
  }
  const cx = 95
  const cy = height / 2
  const rOuter = Math.min(cy - 8, 88)
  const rInner = rOuter * 0.62
  let angle = -Math.PI / 2
  const arcs = items.map((it) => {
    const frac = it.value / total
    const a0 = angle
    const a1 = angle + frac * Math.PI * 2
    angle = a1
    const large = frac > 0.5 ? 1 : 0
    const p = (r: number, a: number) => `${(cx + r * Math.cos(a)).toFixed(2)},${(cy + r * Math.sin(a)).toFixed(2)}`
    const d = frac >= 0.9999
      ? // 全圆:双弧拼合（arc 无法画 360°）
        `M${cx},${cy - rOuter} A${rOuter},${rOuter} 0 1 1 ${cx - 0.01},${cy - rOuter} M${cx},${cy - rInner} A${rInner},${rInner} 0 1 0 ${cx - 0.01},${cy - rInner} Z`
      : `M${p(rOuter, a0)} A${rOuter},${rOuter} 0 ${large} 1 ${p(rOuter, a1)} L${p(rInner, a1)} A${rInner},${rInner} 0 ${large} 0 ${p(rInner, a0)} Z`
    return { ...it, d, frac }
  })

  // 左右分列:按值降序轮流分配到左右两列（左列 = 序 1/3/5…,右列 = 序 2/4/6…）,
  // 每侧最多 6 项,超出截断合并为摘要行。
  const MAX_PER_SIDE = 6
  const leftItems: typeof arcs = []
  const rightItems: typeof arcs = []
  arcs.forEach((a, i) => {
    if (i % 2 === 0) {
      if (leftItems.length < MAX_PER_SIDE) leftItems.push(a)
    } else {
      if (rightItems.length < MAX_PER_SIDE) rightItems.push(a)
    }
  })
  const overflowCount = arcs.length - leftItems.length - rightItems.length
  const overflowValue = arcs.slice(leftItems.length + rightItems.length).reduce((s, a) => s + a.value, 0)

  const legendRow = (a: (typeof arcs)[number]) => (
    <div key={a.key} style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 11, lineHeight: '20px' }}>
      <span style={{ width: 8, height: 8, borderRadius: 2, background: seriesColor(a), flexShrink: 0 }} />
      <span style={{ color: 'var(--text)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1, minWidth: 0 }} title={titleFor ? titleFor(a.key, a.label) : a.label}>{a.label}</span>
      <span style={{ color: 'var(--text-muted)', fontVariantNumeric: 'tabular-nums', flexShrink: 0 }}>{formatValue(a.value)}</span>
      <span style={{ color: 'var(--text-faint)', width: 38, textAlign: 'right', fontVariantNumeric: 'tabular-nums', flexShrink: 0 }}>
        {(a.frac * 100).toFixed(1)}%
      </span>
    </div>
  )

  return (
    <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
      <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 2 }}>
        {leftItems.map(legendRow)}
        {overflowCount > 0 && (
          <div style={{ fontSize: 10, color: 'var(--text-faint)', lineHeight: '20px' }} title={arcs.map((a) => `${a.label} ${formatValue(a.value)}`).join('\n')}>
            {t('chartMoreValue', { n: overflowCount, v: formatValue(overflowValue) })}
          </div>
        )}
      </div>
      <svg viewBox={`0 0 190 ${height}`} width={190} height={height} style={{ flexShrink: 0 }}>
        {arcs.map((a) => (
          <path key={a.key} d={a.d} fill={seriesColor(a)} stroke="var(--panel)" strokeWidth={1.5} />
        ))}
        <text x={cx} y={cy - 3} textAnchor="middle" fontSize={15} fontWeight={700} fill="var(--text)">
          {formatValue(total)}
        </text>
        <text x={cx} y={cy + 13} textAnchor="middle" fontSize={10} fill="var(--text-faint)">{unitLabel}</text>
      </svg>
      <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 2 }}>
        <div style={{ fontSize: 11, fontWeight: 700, color: 'var(--text)', lineHeight: '20px', marginBottom: 2, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={centerLabel}>{centerLabel}</div>
        {rightItems.map(legendRow)}
      </div>
    </div>
  )
}
