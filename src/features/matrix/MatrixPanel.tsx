// Matrix view chart panel: the single chart element below the heatmap — preset =
// all-series multi-model curves （same main chart as the Insights view, reusing
// get_range_series + LineChart/StackedBarChart); click a matrix row name → replace
// with that row's breakdown curves, click again → back to preset. "Total" toggle:
// preset switches to a single all-source curve. Collapsible （collapsed keeps only
// the title row). Panel state （selected row / collapsed / total) is held by
// FullWindow, survives view switches.
// No legend row （matrix already shows row names next to it — duplicates cost
// vertical space); chart height 150 so a tall matrix is never squeezed.
import { useEffect, useMemo, useRef, useState } from 'react'
import { events, usageService } from '../../services'
import type { BreakdownDay } from '../../services'
import { LineChart, StackedBarChart, type CellColumns, type SeriesSpec } from '../insights/charts'
import { projectDisplayName } from '../insights/analytics'
import { projectColor } from '../insights/projectColors'
import type { GroupBy } from './UsageMatrixView'

const pad2 = (n: number) => String(n).padStart(2, '0')
const ymd = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`

export interface MatrixPanelProps {
  /** project 维 → 选中行走 get_project_breakdown（每日 Agent 构成）,
   * 预设走 get_effort_series（project, total);曲线口径恒为 tokens,不跟随矩阵指标。 */
  groupBy: GroupBy
  /** null = preset （all series); otherwise = matrix row key （row linkage). */
  selectedRow: string | null
  collapsed: boolean
  totalOnly: boolean
  onToggleCollapsed(): void
  onToggleTotalOnly(): void
  onClearRow(): void
}

export default function MatrixPanel({
  groupBy,
  selectedRow,
  collapsed,
  totalOnly,
  onToggleCollapsed,
  onToggleTotalOnly,
  onClearRow,
}: MatrixPanelProps) {
  const [kind, setKind] = useState<'line' | 'stack'>('line')
  const [series, setSeries] = useState<SeriesSpec[] | null>(null)
  const [buckets, setBuckets] = useState<string[]>([])
  const [unavailable, setUnavailable] = useState(false)
  const [loading, setLoading] = useState(false)
  const [refreshTick, setRefreshTick] = useState(0)
  const bodyRef = useRef<HTMLDivElement>(null)
  const columns = useCellColumns(bodyRef)

  useEffect(() => {
    let timer = 0
    let off: (() => void) | null = null
    void events.onUsageChanged(() => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => setRefreshTick((t) => t + 1), 300)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      window.clearTimeout(timer)
      off?.()
    }
  }, [])

  // Data assembly: selected row → breakdown （row composition, same chain as
  // RowBreakdown); preset + totalOnly → total-dim single series; preset →
  // groupBy-dim multi-series （all models / all agents). Window = last 31 days
  // （same semantics as the matrix window, rightmost = today).
  useEffect(() => {
    let cancelled = false
    const end = new Date()
    const start = new Date()
    start.setDate(end.getDate() - 30)
    const startDay = ymd(start)
    const endDay = ymd(end)

    if (selectedRow !== null) {
      // Row breakdown: pull month by month across the window and stitch
      setLoading(true)
      const months = [...new Set([startDay.slice(0, 7), endDay.slice(0, 7)])]
      void Promise.all(
        months.map((m) =>
          groupBy === 'project'
            ? usageService.getProjectBreakdown('project', selectedRow, m)
            : usageService.getBreakdown(groupBy, selectedRow, m),
        ),
      ).then((list) => {
        if (cancelled) return
        setLoading(false)
        if (list.every((x) => x === null)) {
          setUnavailable(true)
          setSeries(null)
          return
        }
        setUnavailable(false)
        const byDay = new Map<string, BreakdownDay>()
        for (const days of list) {
          for (const d of days ?? []) byDay.set(d.day, d)
        }
        // 31-day continuous axis
        const axis: string[] = []
        const cur = new Date(start)
        while (cur <= end) {
          axis.push(ymd(cur))
          cur.setDate(cur.getDate() + 1)
        }
        const seriesMap = new Map<string, { label: string; values: number[] }>()
        axis.forEach((day, i) => {
          const d = byDay.get(day)
          for (const s of d?.slices ?? []) {
            let e = seriesMap.get(s.key)
            if (!e) {
              e = { label: s.label, values: Array(axis.length).fill(0) }
              seriesMap.set(s.key, e)
            }
            e.values[i] = s.tokens
          }
        })
        setBuckets(axis)
        setSeries(
          [...seriesMap.entries()]
            .map(([key, e]) => ({ key, label: e.label, values: e.values }))
            .sort((a, b) => b.values.reduce((x, y) => x + y, 0) - a.values.reduce((x, y) => x + y, 0)),
        )
      })
      return () => {
        cancelled = true
      }
    }

    // Preset: range_series （multi-series or total single-series)
    setLoading(true)
    void (groupBy === 'project' && !totalOnly
      ? usageService.getEffortSeries({ startDay, endDay, dimension: 'project', metric: 'total' })
      : usageService.getRangeSeries({
          startDay,
          endDay,
          bucket: 'day',
          dimension: totalOnly ? 'total' : (groupBy as 'agent' | 'model'),
          metric: 'total',
        })
    )
      .then((res) => {
        if (cancelled) return
        setLoading(false)
        if (res === null) {
          setUnavailable(true)
          setSeries(null)
          return
        }
        setUnavailable(false)
        setBuckets(res.points.map((p) => p.bucket))
        setSeries(
          res.seriesKeys.map((k, i) => ({
            key: k,
            label: k === '__total__' ? 'All sources' : groupBy === 'project' ? projectDisplayName(k) : res.seriesLabels[i] ?? k,
            values: res.points.map((p) => p.values[i] ?? 0),
            color: groupBy === 'project' && k !== '__total__' ? projectColor(k) : undefined,
          })),
        )
      })
    return () => {
      cancelled = true
    }
  }, [groupBy, selectedRow, totalOnly, refreshTick])

  const title = useMemo(() => {
    if (selectedRow !== null) return groupBy === 'project' ? projectDisplayName(selectedRow) : selectedRow
    if (totalOnly) return 'All · total'
    return groupBy === 'model' ? 'All models' : groupBy === 'project' ? 'All projects' : 'All agents'
  }, [selectedRow, totalOnly, groupBy])

  if (collapsed) {
    return (
      <section className="matrix-panel is-collapsed">
        {/* 折叠态:仅剩展开钮（▲),与展开态折叠钮（▾）同一坐标——右缘图标列正下方*/}
        <button className="matrix-panel-collapse" onClick={onToggleCollapsed} title={`Expand chart — ${title}`}>
          <ChevronUpIcon />
        </button>
      </section>
    )
  }

  return (
    <section className="matrix-panel">
      <header className="matrix-panel-header">
        {/* 右缘竖排图标列:Stack / Lines / Total（悬浮 title 提示语义）。compact 图表两侧
            边距让图标列落在图表区内侧右缘,不叠出区外;折叠钮不在列内,固定在右下角。*/}
        <button
          className={`matrix-panel-icon${kind === 'stack' ? ' is-active' : ''}`}
          onClick={() => setKind('stack')}
          title="Stacked bars"
        >
          <StackIcon />
        </button>
        <button
          className={`matrix-panel-icon${kind === 'line' ? ' is-active' : ''}`}
          onClick={() => setKind('line')}
          title="Lines"
        >
          <LinesIcon />
        </button>
        {selectedRow === null ? (
          <button
            className={`matrix-panel-icon${totalOnly ? ' is-active' : ''}`}
            onClick={onToggleTotalOnly}
            title={totalOnly ? 'Show per-series curves' : 'Show all-source total only'}
          >
            <TotalIcon />
          </button>
        ) : (
          <button className="matrix-panel-icon" onClick={onClearRow} title="Back to all-series preset">
            <CloseIcon />
          </button>
        )}
      </header>

      {/* 折叠/展开箭头固定右下角:折叠后也锚在同一坐标,两个状态一个位置,不跳动。*/}
      <button className="matrix-panel-collapse" onClick={onToggleCollapsed} title="Collapse chart panel">
        <ChevronDownIcon />
      </button>

      <div className="matrix-panel-body" ref={bodyRef}>
        {loading && series === null && <div className="insight-empty">Loading…</div>}
        {!loading && unavailable && <div className="insight-empty">Data unavailable (service not running)</div>}
        {!loading && !unavailable && (series === null || series.length === 0 || series.every((s) => s.values.every((v) => v === 0))) && (
          <div className="insight-empty">No usage records in this window</div>
        )}
        {series !== null && series.length > 0 && !series.every((s) => s.values.every((v) => v === 0)) && (
          kind === 'line' ? (
            <LineChart series={series} buckets={buckets} height={150} showYAxis={false} showGrid={false} showXAxis={false} columns={columns} />
          ) : (
            <StackedBarChart series={series} buckets={buckets} height={150} showYAxis={false} showGrid={false} showXAxis={false} columns={columns} />
          )
        )}
      </div>
    </section>
  )
}

/** 读 .matrix-stage 上的共享列变量（--cell-px / --cell-gap-px,由 UsageMatrixView 写入）,
 * 让图表的点 / 柱与格子中心对齐。格宽一变格子区宽（= 面板中列宽）就变,借 ResizeObserver 重读。 */
function useCellColumns(ref: React.RefObject<HTMLDivElement | null>): CellColumns | undefined {
  const [cols, setCols] = useState<CellColumns | undefined>(undefined)
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const read = () => {
      const cs = getComputedStyle(el)
      const cell = parseFloat(cs.getPropertyValue('--cell-px'))
      const gap = parseFloat(cs.getPropertyValue('--cell-gap-px'))
      setCols((prev) => {
        if (!(cell > 0) || !(gap >= 0)) return undefined
        return prev && prev.cell === cell && prev.gap === gap ? prev : { cell, gap }
      })
    }
    read()
    const ro = new ResizeObserver(read)
    ro.observe(el)
    return () => ro.disconnect()
  }, [ref])
  return cols
}

/* 图标:12px stroke 线性风格,currentColor,与标题栏窗口钮同尺度。 */
function ChevronDownIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <path d="M2.5 4.5 L6 8 L9.5 4.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

function ChevronUpIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <path d="M2.5 7.5 L6 4 L9.5 7.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

function LinesIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <path d="M1 9 C3 4, 5 4, 6.5 6.5 S10 9, 11 3" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

function StackIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <rect x="1.5" y="6.5" width="2.6" height="4" fill="currentColor" />
      <rect x="4.7" y="4" width="2.6" height="6.5" fill="currentColor" />
      <rect x="7.9" y="1.5" width="2.6" height="9" fill="currentColor" />
    </svg>
  )
}

function TotalIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" strokeWidth="1.4" />
      <path d="M6 3.5 V6 L8 7.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  )
}

function CloseIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <line x1="2.5" y1="2.5" x2="9.5" y2="9.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      <line x1="9.5" y1="2.5" x2="2.5" y2="9.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  )
}
