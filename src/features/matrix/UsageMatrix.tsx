// UsageMatrix：自写月矩阵核心组件（不用 react-grid-heatmap）
// CSS Grid 布局：固定行头 + 31 列 + 行总量；P95+log1p 色阶（global/perRow）；
// 状态视觉（future/zero/estimated/error/today/selected）；自写 tooltip；键盘导航
import { useMemo, useRef, useState, useCallback } from 'react'
import { p95Cap, cellVisual, formatCompact } from './matrixScale'
import MatrixTooltip, { type TooltipContent } from './MatrixTooltip'
import './monthMatrix.css'

export interface MatrixRow {
  key: string
  label: string
  subtitle?: string
  values: (number | null)[] // null = 未来/不可用（非 0）
  /** 与 values 平行的请求/对话数（hover messages 用）。 */
  counts?: (number | null)[]
  cellOpts?: (day: number) => { estimated?: boolean; error?: boolean }
  total: number
  health?: 'healthy' | 'attention' | 'error'
}

export interface UsageMatrixProps {
  rows: MatrixRow[]
  dayLabels: string[]
  /** 与列对齐的真实日期（31 天滚动窗口）：hover 年月日显示用;缺省退回 label。 */
  dates?: Date[]
  /** 表头前 N 列淡化（窗口跨月的上月部分;月缩写列自然醒目）。 */
  headerMutedFrom?: number
  scaleMode: 'global' | 'perRow'
  selected: { rowKey: string; day: number } | null
  /** 面板联动的选中行（行名高亮 + is-selected-row）;与 cell selected 独立。 */
  selectedRow?: string | null
  onSelectCell(rowKey: string, day: number): void
  onSelectRow(rowKey: string): void
  /** 格值格式化（缺省 token 紧凑格式;时间成本指标传 formatDuration）。 */
  formatValue?: (v: number) => string
  /** hover 读数单位（缺省 "tokens";wait / human 时间指标传对应单位）。 */
  valueUnit?: string
}

interface TooltipState {
  anchor: HTMLElement | null
  content: TooltipContent | null
}

const MONTH_ABBR = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

export default function UsageMatrix({
  rows,
  dayLabels,
  dates,
  headerMutedFrom,
  scaleMode,
  selected,
  selectedRow,
  onSelectCell,
  onSelectRow,
  formatValue = formatCompact,
  valueUnit = 'tokens',
}: UsageMatrixProps) {
  const gridRef = useRef<HTMLDivElement>(null)
  const [tooltip, setTooltip] = useState<TooltipState | null>(null)
  const [hover, setHover] = useState<{ rowKey: string; day: number } | null>(null)

  // 色阶 cap：global = 全部非零值 P95；perRow = 每行非零值 P95
// 用 P95 而非 P99：让中段值落在 t=0.3~0.8 区间，颜色梯度明显
// 高于 cap 的格子落峰值档（matrixScale bucketIndex = 4）单独凸显
  const globalCap = useMemo(
    () => p95Cap(rows.flatMap((r) => r.values.filter((v): v is number => v !== null))),
    [rows],
  )
  const rowCap = useMemo(() => {
    const m = new Map<string, number>()
    for (const r of rows) m.set(r.key, p95Cap(r.values.filter((v): v is number => v !== null)))
    return m
  }, [rows])

  const capFor = useCallback(
    (rowKey: string) => (scaleMode === 'perRow' ? (rowCap.get(rowKey) ?? 0) : globalCap),
    [scaleMode, globalCap, rowCap],
  )

  // hover 格式：标题 = 年月日（月缩写缩窄）,
  // 行 = tokens · 对话数（compact 格式,与挂件 tooltip 一致）。
  const showTooltip = useCallback((rowKey: string, day: number, el: HTMLElement) => {
    const row = rows.find((r) => r.key === rowKey)
    if (!row) return
    const v = row.values[day]
    if (v === null) return
    const lines: string[] = []
    const c = row.counts?.[day]
    if (v === 0 && (c ?? 0) === 0) {
      lines.push('No usage')
    } else {
      lines.push(`${formatValue(v)} ${valueUnit}${c != null ? ` · ${c.toLocaleString('en-US')} messages` : ''}`)
      const isEst = row.cellOpts?.(day)?.estimated
      if (isEst) lines.push('Quality: estimated')
    }
    const d = dates?.[day]
    const title = d
      ? `${MONTH_ABBR[d.getMonth()]} ${d.getDate()}, ${d.getFullYear()}`
      : `${row.label} · ${dayLabels[day]}`
    setTooltip({ anchor: el, content: { title, lines } })
  }, [rows, dayLabels, dates, formatValue, valueUnit])

  const hideTooltip = useCallback(() => setTooltip(null), [])

  // 鼠标/键盘离开：同时清 hover（放大态）与 tooltip，避免最后一个格子不缩回
  const handleLeave = useCallback(() => {
    setHover(null)
    setTooltip(null)
  }, [])

  // 键盘导航：方向键/Enter/Esc
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (!selected || rows.length === 0) return
    const rowIdx = rows.findIndex((r) => r.key === selected.rowKey)
    let nr = rowIdx
    let nd = selected.day
    if (e.key === 'ArrowLeft') nd = Math.max(0, nd - 1)
    else if (e.key === 'ArrowRight') nd = Math.min(dayLabels.length - 1, nd + 1)
    else if (e.key === 'ArrowUp') nr = Math.max(0, rowIdx - 1)
    else if (e.key === 'ArrowDown') nr = Math.min(rows.length - 1, rowIdx + 1)
    else if (e.key === 'Enter') {
      onSelectRow(selected.rowKey)
      return
    } else if (e.key === 'Escape') {
      hideTooltip()
      return
    } else return
    e.preventDefault()
    if (rows[nr]) {
      const next = { rowKey: rows[nr].key, day: nd }
      onSelectCell(next.rowKey, next.day)
      setHover(next)
    }
  }

  return (
    <div
      ref={gridRef}
      className="month-matrix"
      role="grid"
      aria-label="Monthly usage matrix"
      tabIndex={0}
      onKeyDown={onKeyDown}
    >
      <div className="matrix-header">
        <div className="matrix-header-label" />
        <div className="matrix-header-days">
          {dayLabels.map((l, i) => (
            <div
              key={i}
              className={`matrix-header-day${
                headerMutedFrom != null && i < headerMutedFrom ? ' is-muted' : ''
              }`}
            >
              {l}
            </div>
          ))}
        </div>
        <div className="matrix-header-total">Total</div>
      </div>

      {rows.map((row) => {
        const cap = capFor(row.key)
        return (
          <div key={row.key} className={`matrix-row${row.key === (selectedRow ?? selected?.rowKey) ? ' is-selected-row' : ''}`} role="row">
            <button
              className={`matrix-row-label${row.key === selectedRow ? ' is-panel-selected' : ''}`}
              role="rowheader"
              title={row.subtitle}
              onClick={() => onSelectRow(row.key)}
            >
              <span className="matrix-row-name">{row.label}</span>
              {row.health && row.health !== 'healthy' && (
                <span className={`matrix-row-health is-${row.health}`} title={`Collector status: ${row.health}`} />
              )}
            </button>
            <div className="matrix-row-cells">
              {row.values.map((v, day) => {
                const isSel = selected?.rowKey === row.key && selected.day === day
                const visual = cellVisual(v, cap, row.cellOpts?.(day))
                return (
                  <button
                    key={day}
                    className={[
                      'day-cell',
                      `is-${visual.status}`,
                      isSel ? 'is-selected' : '',
                      row.key === hover?.rowKey && day === hover.day ? 'is-hover' : '',
                    ].filter(Boolean).join(' ')}
                    style={{ background: visual.background }}
                    role="gridcell"
                    aria-label={`${row.label} ${dayLabels[day]}: ${v === null ? 'N/A' : v === 0 ? 'No usage' : v.toLocaleString('en-US')}`}
                    tabIndex={-1}
                    onClick={() => onSelectCell(row.key, day)}
                    onMouseEnter={(e) => {
                      setHover({ rowKey: row.key, day })
                      if (v !== null) showTooltip(row.key, day, e.currentTarget)
                    }}
                    onMouseLeave={handleLeave}
                    onFocus={(e) => {
                      setHover({ rowKey: row.key, day })
                      if (v !== null) showTooltip(row.key, day, e.currentTarget)
                    }}
                    onBlur={handleLeave}
                  >
                    {/* 格子纯色块：数值仅 tooltip 显示 */}
                  </button>
                )
              })}
            </div>
            <div className="matrix-row-total">{formatValue(row.total)}</div>
          </div>
        )
      })}

      {/* Tooltip（与挂件共用组件同格式） */}
      <MatrixTooltip content={tooltip?.content ?? null} anchor={tooltip?.anchor ?? null} />
    </div>
  )
}
