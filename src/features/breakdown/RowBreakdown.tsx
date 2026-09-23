// 行展开面板：SVG 双模式图表——曲线（默认,多系列平滑）⇄ 堆叠柱;
// 数据是服务端 BreakdownDay（每日构成）,前端转成 SeriesSpec。配色沿用稳定哈希色板（charts.colorFor 同源）。
import { useMemo, useState } from 'react'
import type { BreakdownDay } from '../../services'
import { LineChart, StackedBarChart, colorFor, type SeriesSpec } from '../insights/charts'
import { formatCompact } from '../matrix/matrixScale'
import { useT } from '../../lib/i18n'
import './breakdown.css'

interface RowBreakdownProps {
  kind: 'agent' | 'model' | 'project'
  rowKey: string
  label: string
  month: string
  days: BreakdownDay[] | null // null = 服务不可用（如 demo 模式）
  onClose(): void
}

type ViewKind = 'line' | 'stack'

export default function RowBreakdown({ kind, label, month, days, onClose }: RowBreakdownProps) {
  const t = useT('matrix')
  const [viewKind, setViewKind] = useState<ViewKind>('line')

  const view = useMemo(() => {
    if (!days) return null
    // BreakdownDay[] → 系列轴:每天一个 bucket;slice key = 系列
    const seriesMap = new Map<string, { label: string; values: number[] }>()
    const buckets: string[] = []
    for (const d of days) {
      buckets.push(d.day)
    }
    for (const d of days) {
      for (const s of d.slices ?? []) {
        let e = seriesMap.get(s.key)
        if (!e) {
          e = { label: s.label, values: Array(days.length).fill(0) }
          seriesMap.set(s.key, e)
        }
      }
    }
    days.forEach((d, i) => {
      for (const s of d.slices ?? []) {
        seriesMap.get(s.key)!.values[i] = s.tokens
      }
    })
    const series: SeriesSpec[] = [...seriesMap.entries()]
      .map(([key, e]) => ({ key, label: e.label, values: e.values }))
      .sort((a, b) => b.values.reduce((x, y) => x + y, 0) - a.values.reduce((x, y) => x + y, 0))
    const monthTotal = series.reduce((s, sr) => s + sr.values.reduce((x, y) => x + y, 0), 0)
    return { series, buckets, monthTotal }
  }, [days])

  const title = kind === 'agent' ? t('dailyModelBreakdown') : t('dailyAgentBreakdown')

  return (
    <div className="breakdown">
      <div className="breakdown-header">
        <span className="breakdown-title">{label} · {month} · {title}</span>
        {view && <span className="breakdown-month-total">{t('totalValue', { n: formatCompact(view.monthTotal) })}</span>}
        <div className="toolbar-group breakdown-kind">
          <button className={`seg${viewKind === 'line' ? ' is-active' : ''}`} title={t('lineChart')} onClick={() => setViewKind('line')}>{t('lines')}</button>
          <button className={`seg${viewKind === 'stack' ? ' is-active' : ''}`} title={t('stackedChart')} onClick={() => setViewKind('stack')}>{t('stack')}</button>
        </div>
        <button className="breakdown-close" onClick={onClose} title={t('closeBreakdown')} aria-label={t('closeBreakdown')}>×</button>
      </div>

      {!view && (
        <div className="breakdown-empty">{t('breakdownUnavailable')}</div>
      )}

      {view && view.series.length === 0 && (
        <div className="breakdown-empty">{t('noBreakdownData')}</div>
      )}

      {view && view.series.length > 0 && (
        <div className="breakdown-chart">
          {viewKind === 'line' ? (
            <LineChart series={view.series} buckets={view.buckets} height={190} />
          ) : (
            <StackedBarChart series={view.series} buckets={view.buckets} height={190} />
          )}
          <div className="series-legend">
            {view.series.slice(0, 8).map((s) => (
              <span key={s.key} className="legend-item" style={{ cursor: 'default' }}>
                <span className="legend-swatch" style={{ background: colorFor(s.key) }} />
                {s.label}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  )
}
