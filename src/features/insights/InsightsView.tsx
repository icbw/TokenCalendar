// 数据洞察视图：v2 仪表盘 + v3 打磨。
// 布局: 1) 主图卡（曲线⇄堆叠 + 口径控件 + 图例筛选）; 2) 同口径占比环（环居中
// 图例分列左右）; 3) 异常日 31 格单行热力条（替代列表）; 4) tokens × 积分双组图
// （双段堆叠柱 + credit 曲线,区分模型开关）。
// 手写 SVG 图表见 charts.tsx。
import { useCallback, useEffect, useMemo, useState } from 'react'
import { events, usageService } from '../../services'
import type { CreditSummary, RangeSeriesPoint, RangeSeriesResult } from '../../services'
import { getDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import { DonutChart, LineChart, StackedBarChart, ComboChart, colorFor, COMBO_IN, COMBO_OUT, COMBO_CREDIT, type ComboSeries, type SeriesSpec } from './charts'
import { formatFull } from '../matrix/matrixScale'
import './insights.css'

const MONTH_ABBR = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

const OUTLIER_Z = 2.0
const OUTLIER_MIN_SAMPLES = 7

const pad2 = (n: number) => String(n).padStart(2, '0')
const ymd = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`

const fullMonthLabel = (m: string) => {
  const [y, mo] = m.split('-')
  return `${MONTH_ABBR[Number(mo) - 1]} ${y}`
}

// ---- 口径控件（分段控件,样式复用 .seg） ----

type Bucket = 'day' | 'hour'
type Dimension = 'agent' | 'model' | 'total'
type Metric = 'total' | 'input' | 'output'
type ChartKind = 'line' | 'stack'
type RangeDays = 7 | 30 | 90

const RANGE_LABELS: Record<RangeDays, string> = { 7: '7d', 30: '30d', 90: '90d' }

function Seg<T extends string | number>({ value, options, onChange }: {
  value: T
  /** hint = hover 提示（缺省回落到 label）。 */
  options: { v: T; label: string; hint?: string }[]
  onChange: (v: T) => void
}) {
  return (
    <div className="toolbar-group">
      {options.map((o) => (
        <button
          key={String(o.v)}
          className={`seg${value === o.v ? ' is-active' : ''}`}
          title={o.hint ?? o.label}
          onClick={() => onChange(o.v)}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}

// ---- 主图卡 ----

// 自定义 hook 形态——工具栏与卡片拆成两个返回件,由
// InsightsView 装配:工具栏固定在滚动区外（设置页同款）,卡片随内容滚动。
function useTrendBlock() {
  const now = new Date()
  const [rangeDays, setRangeDays] = useState<RangeDays>(30)
  const [bucket, setBucket] = useState<Bucket>('day')
  const [dimension, setDimension] = useState<Dimension>('model')
  const [metric, setMetric] = useState<Metric>('total')
  const [kind, setKind] = useState<ChartKind>('line')
  const [filterKey, setFilterKey] = useState<string>('') // '' = 全部
  const [data, setData] = useState<RangeSeriesResult | null>(null)
  const [loading, setLoading] = useState(false)
  const [refreshTick, setRefreshTick] = useState(0)

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

  const endDay = ymd(now)
  const startDay = useMemo(() => {
    const d = new Date(now.getFullYear(), now.getMonth(), now.getDate())
    d.setDate(d.getDate() - (rangeDays - 1))
    return ymd(d)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rangeDays, refreshTick])

  // 范围 × 粒度合法性:小时粒度只在 7/30 天给（90 天 × 24 = 2160 点无意义）
  const effectiveBucket: Bucket = bucket === 'hour' && rangeDays === 90 ? 'day' : bucket

  const filterOptions = useMemo(() => {
    // 筛选维度与展示维度同族（agent 维筛 agent,model 维筛 model）;total 维无筛选
    return (data?.seriesKeys ?? []).map((k, i) => ({ key: k, label: data?.seriesLabels[i] ?? k }))
  }, [data])

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    void usageService
      .getRangeSeries({
        startDay,
        endDay,
        bucket: effectiveBucket,
        dimension,
        metric,
        // 筛选:展示维 = agent → 筛 agent;展示维 = model → 筛 model;total 无筛选
        filterDimension: dimension === 'total' || !filterKey ? undefined : dimension,
        filterKey: dimension === 'total' || !filterKey ? undefined : filterKey,
      })
      .then((res) => {
        if (cancelled) return
        setData(res)
        setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [startDay, endDay, effectiveBucket, dimension, metric, filterKey, refreshTick])

  const series: SeriesSpec[] = useMemo(() => {
    if (!data) return []
    return data.seriesKeys.map((k, i) => ({ key: k, label: data.seriesLabels[i] ?? k, values: data.points.map((p) => p.values[i] ?? 0) }))
  }, [data])
  const buckets = data?.points.map((p) => p.bucket) ?? []

  // 占比环数据:主图范围内各系列合计
  const donutItems = useMemo(
    () =>
      series
        .filter(() => dimension !== 'total')
        .map((s) => ({ key: s.key, label: s.label, value: s.values.reduce((a, b) => a + b, 0) }))
        .filter((s) => s.value > 0)
        .sort((a, b) => b.value - a.value),
    [series, dimension],
  )
  const donutUnit = metric === 'total' ? 'tokens' : metric === 'input' ? 'input tokens' : 'output tokens'

  const pickFilter = useCallback(
    (k: string) => {
      setFilterKey((prev) => (prev === k ? '' : k)) // 再点同项取消筛选
    },
    [],
  )

  // 拆成两个返回件——toolbar（固定行）与 card（随滚动内容）。
  return {
    toolbar: (
      <header className="insight-toolbar">
        <span className="insight-card-title">Usage trend</span>
        <Seg
          value={kind}
          options={[
            { v: 'line' as ChartKind, label: 'Lines', hint: 'Line chart' },
            { v: 'stack' as ChartKind, label: 'Stack', hint: 'Stacked chart' },
          ]}
          onChange={setKind}
        />
        <Seg
          value={rangeDays}
          options={([7, 30, 90] as RangeDays[]).map((d) => ({ v: d, label: RANGE_LABELS[d], hint: `Last ${d} days` }))}
          onChange={(v) => { setRangeDays(v); setFilterKey('') }}
        />
        <Seg
          value={effectiveBucket}
          options={[
            { v: 'day' as Bucket, label: 'Day', hint: 'Group by day' },
            { v: 'hour' as Bucket, label: 'Hour', hint: 'Group by hour' },
          ]}
          onChange={setBucket}
        />
        <Seg
          value={dimension}
          options={[
            { v: 'model' as Dimension, label: 'Model', hint: 'One series per model' },
            { v: 'agent' as Dimension, label: 'Agent', hint: 'One series per agent' },
            { v: 'total' as Dimension, label: 'Total', hint: 'Single all-source series' },
          ]}
          onChange={(v) => { setDimension(v); setFilterKey('') }}
        />
        <Seg
          value={metric}
          options={[
            { v: 'total' as Metric, label: 'Tokens', hint: 'Total tokens' },
            { v: 'input' as Metric, label: 'Input', hint: 'Input tokens' },
            { v: 'output' as Metric, label: 'Output', hint: 'Output tokens' },
          ]}
          onChange={setMetric}
        />
        {loading && <span className="matrix-loading">Loading…</span>}
      </header>
    ),
    card: (
      <section className="insight-card">
        {/* 系列图例（可点击 = 筛选到该系列;再点取消;total 维无图例）*/}
        {dimension !== 'total' && series.length > 0 && (
          <div className="series-legend">
            {filterOptions.map((o) => (
              <button
                key={o.key}
                className={`legend-item${filterKey === o.key ? ' is-active' : ''}`}
                onClick={() => pickFilter(o.key)}
                title={filterKey === o.key ? 'Click to clear filter' : `Only ${o.label}`}
              >
                <span className="legend-swatch" style={{ background: colorFor(o.key) }} />
                {o.label}
              </button>
            ))}
          </div>
        )}

        {data === null ? (
          <div className="insight-empty">Data unavailable (service not running)</div>
        ) : series.length === 0 || series.every((s) => s.values.every((v) => v === 0)) ? (
          <div className="insight-empty">No usage records in this range</div>
        ) : kind === 'line' ? (
          <LineChart series={series} buckets={buckets} />
        ) : (
          <StackedBarChart series={series} buckets={buckets} />
        )}

        {/* 占比环:与主图同口径（维度/指标/范围/筛选）*/}
        {dimension !== 'total' && donutItems.length > 0 && (
          <div className="donut-wrap">
            <DonutChart items={donutItems} centerLabel={RANGE_LABELS[rangeDays] + ' · ' + donutUnit} unitLabel={donutUnit} />
          </div>
        )}
      </section>
    ),
  }
}

// ---- 异常日（保留:31 天 z-score） ----

interface OutlierDay {
  ymd: string
  total: number
  z: number
}

function AnomalyBlock() {
  const [days, setDays] = useState<{ ymd: string; total: number }[] | null>(null)
  const [unavailable, setUnavailable] = useState(false)
  const [refreshTick, setRefreshTick] = useState(0)

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

  useEffect(() => {
    let cancelled = false
    // get_range_series 后端聚合版:31 天窗口全源合计日序列
    const end = ymd(new Date())
    const d = new Date()
    d.setDate(d.getDate() - 30)
    const start = ymd(d)
    void usageService.getRangeSeries({ startDay: start, endDay: end, bucket: 'day', dimension: 'total', metric: 'total' }).then((res) => {
      if (cancelled) return
      setUnavailable(res === null)
      setDays((res?.points ?? []).map((p) => ({ ymd: p.bucket, total: p.values[0] ?? 0 })))
    })
    return () => {
      cancelled = true
    }
  }, [refreshTick])

  const outliers = useMemo<OutlierDay[]>(() => {
    if (!days) return []
    const vals = days.map((d) => d.total)
    if (vals.length < OUTLIER_MIN_SAMPLES) return []
    const mean = vals.reduce((a, b) => a + b, 0) / vals.length
    const sd = Math.sqrt(vals.reduce((a, b) => a + (b - mean) ** 2, 0) / vals.length)
    if (sd === 0) return []
    return days
      .map((d) => ({ ...d, z: (d.total - mean) / sd }))
      .filter((d) => Math.abs(d.z) >= OUTLIER_Z)
      .sort((a, b) => Math.abs(b.z) - Math.abs(a.z))
  }, [days])

  // 逐日 z（热力条上色用）:与 outliers 同一 mean/sd;sd=0 全零窗口不判。
  const zByDay = useMemo(() => {
    if (!days) return new Map<string, number>()
    const vals = days.map((d) => d.total)
    if (vals.length < OUTLIER_MIN_SAMPLES) return new Map<string, number>()
    const mean = vals.reduce((a, b) => a + b, 0) / vals.length
    const sd = Math.sqrt(vals.reduce((a, b) => a + (b - mean) ** 2, 0) / vals.length)
    if (sd === 0) return new Map<string, number>()
    return new Map(days.map((d) => [d.ymd, (d.total - mean) / sd]))
  }, [days])

  return (
    <section className="insight-card">
      <header className="insight-card-header">
        <span className="insight-card-title">Anomaly days · last 31 days</span>
        <span className="insight-card-sub" title="Daily totals beyond mean ± 2 std-dev; rightmost = today">|z| ≥ 2 outliers</span>
      </header>
      {days === null || unavailable ? (
        <div className="insight-empty">Data unavailable (service not running)</div>
      ) : (
        <div className="anomaly-heatmap">
          {/* 单行 31 格热力条:最右 = 今日（与主矩阵窗口语义一致）;
              异常日按 |z| 强度上色（z≥2 深红 / ≤-2 深蓝,更强更深）,非异常日有数据
              中性灰、无数据更浅。行首保留一段文字说明。*/}
          <span className="anomaly-heatmap-label">Outliers</span>
          <div className="anomaly-strip">
            {days.map((d) => {
              const z = zByDay.get(d.ymd)
              const isOut = z !== undefined && Math.abs(z) >= OUTLIER_Z
              // |z| 强度落档:2~3 浅 / 3~4 中 / ≥4 深（按强度上色）
              const zLevel = z === undefined ? '' : ` z${Math.min(4, Math.max(2, Math.floor(Math.abs(z))))}`
              const cls = isOut ? (z! > 0 ? ' is-high' : ' is-low') + zLevel : d.total > 0 ? ' is-flat' : ' is-zero'
              const zTxt = z === undefined ? 'not enough samples' : `${z > 0 ? '+' : ''}${z.toFixed(1)}σ`
              return (
                <div
                  key={d.ymd}
                  className={`anomaly-cell${cls}`}
                  title={`${d.ymd} · ${formatFull(d.total)} tokens · ${zTxt}${isOut ? ' · outlier' : ''}`}
                />
              )
            })}
          </div>
          {outliers.length === 0 ? (
            <span className="anomaly-heatmap-none">None</span>
          ) : (
            <span className="anomaly-heatmap-count">{outliers.length} days</span>
          )}
        </div>
      )}
    </section>
  )
}

// ---- tokens × 积分（双组图,v3.1 可选组件,默认关） ----

function CreditBlock() {
  const [month, setMonth] = useState(() => {
    const now = new Date()
    return `${now.getFullYear()}-${pad2(now.getMonth() + 1)}`
  })
  const [summary, setSummary] = useState<CreditSummary | null>(null)
  const [loading, setLoading] = useState(false)
  const [byModel, setByModel] = useState(false) // 区分模型开关（用户可选,设计 §9.5）
  const [bucket, setBucket] = useState<'day' | 'hour'>('day') // v3.1 横轴粒度切换
  const [refreshTick, setRefreshTick] = useState(0)

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

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    void usageService.getCreditSummary(month).then((res) => {
      if (cancelled) return
      setSummary(res)
      setLoading(false)
    })
    return () => {
      cancelled = true
    }
  }, [month, refreshTick])

  return (
    <>
      {/* 工具行独立成行（参照矩阵视图）,月份选择跟在控件组后,不再 auto 靠右*/}
      <header className="insight-toolbar">
        <span className="insight-card-title">tokens × credit · CodeBuddy</span>
        <Seg
          value={bucket}
          options={[
            { v: 'day' as const, label: 'Day', hint: 'Group by day' },
            { v: 'hour' as const, label: 'Hour', hint: 'Group by hour' },
          ]}
          onChange={setBucket}
        />
        <input
          className="credit-month"
          type="month"
          value={month}
          max={`${new Date().getFullYear()}-${pad2(new Date().getMonth() + 1)}`}
          onChange={(e) => e.target.value && setMonth(e.target.value)}
          aria-label="Pick month"
        />
        {loading && <span className="matrix-loading">Loading…</span>}
      </header>

      <section className="insight-card">
        {summary === null ? (
        <div className="insight-empty">{loading ? 'Loading…' : 'Data unavailable (service not running)'}</div>
      ) : !summary.hasData ? (
        <div className="credit-guide">
          <p className="credit-guide-title">{fullMonthLabel(month)} has no credit data yet</p>
          <p className="credit-guide-body">
            Credits come from the official website export (no login credentials collected):
            sign in to the CodeBuddy site → usage page → export monthly xlsx → drop it into
            the <code>imports\</code> folder of the data root; the collector ingests it on
            its next round.
          </p>
        </div>
      ) : (
        <div className="credit-body">
          <div className="credit-totals">
            <div className="credit-total-item">
              <span className="credit-total-label">Total credit</span>
              <span className="credit-total-value">{formatFull(Math.round(summary.totalCredit * 100) / 100)}</span>
            </div>
            <div className="credit-total-item">
              <span className="credit-total-label">Requests</span>
              <span className="credit-total-value">{formatFull(summary.totalRequests)}</span>
            </div>
            <div className="credit-total-item credit-total-note">
              <span className="credit-total-label">The credit curve only draws up to the last covered day</span>
            </div>
            <label className="credit-mode-toggle" title="Split token bars into per-model groups with one credit curve each">
              <input type="checkbox" checked={byModel} onChange={(e) => setByModel(e.target.checked)} />
              By model
            </label>
          </div>
          <ComboBlock month={month} summary={summary} byModel={byModel} bucket={bucket} />
        </div>
      )}
      </section>
    </>
  )
}

/** 双组图装配:tokens 序列（get_range_series 月内 input/output;天/小时粒度可切）
 * + credit 日序列（credit_summary.by_day / by_model_day）。共享横轴:天粒度 =
 * 每日一组柱;小时粒度 = 每日内 24 根小时细柱（横轴仍按天分组,组内并排）。
 * credit 账本只有日粒度（xlsx 按日）,小时档下曲线保持按日对齐。
 * 积分账本按月手动导出,曲线画到覆盖最后一天为止,缺口断线表达
 * （红线:勿伪装成 0）。 */
function ComboBlock({ month, summary, byModel, bucket }: {
  month: string
  summary: CreditSummary
  byModel: boolean
  bucket: 'day' | 'hour'
}) {
  const [tokIn, setTokIn] = useState<RangeSeriesResult | null>(null)
  const [tokOut, setTokOut] = useState<RangeSeriesResult | null>(null)
  const [refreshTick, setRefreshTick] = useState(0)

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

  // tokens 侧:该月首日 → 月末（含未来日,tokens 轴恒整月;未来日无数据自然为 0）。
  // input / output 各拉一条（合计维度也能拿到精确分项,零迁移部分）。
  useEffect(() => {
    let cancelled = false
    const [y, m] = month.split('-').map(Number)
    const last = new Date(y, m, 0).getDate()
    const base = {
      startDay: `${month}-01`,
      endDay: `${month}-${pad2(last)}`,
      bucket,
      dimension: 'total' as const,
    }
    void Promise.all([
      usageService.getRangeSeries({ ...base, metric: 'input' }),
      usageService.getRangeSeries({ ...base, metric: 'output' }),
    ]).then(([i, o]) => {
      if (cancelled) return
      setTokIn(i)
      setTokOut(o)
    })
    return () => {
      cancelled = true
    }
  }, [month, bucket, refreshTick])

  // 横轴 = 积分轴（后端已补零至 min（月末, 今日)）;tokens 值按日并入
  const buckets = summary.byDay.map((d) => d.day)
  // tokens 值表:day 粒度直接 day → 值;hour 粒度按天分桶（组内小时并排由
  // ComboChart 的 hourPoints 承载,这里把 24 点聚到所属日）。
  const tokInByDay = useMemo(() => {
    const map = new Map<string, number[]>()
    if (tokIn === null) return map
    if (bucket === 'day') {
      for (const p of tokIn.points) map.set(p.bucket, p.values)
    } else {
      for (const p of tokIn.points) {
        const day = p.bucket.slice(0, 10)
        map.set(day, [...(map.get(day) ?? []), ...p.values])
      }
    }
    return map
  }, [tokIn, bucket])
  const tokOutByDay = useMemo(() => {
    const map = new Map<string, number[]>()
    if (tokOut === null) return map
    if (bucket === 'day') {
      for (const p of tokOut.points) map.set(p.bucket, p.values)
    } else {
      for (const p of tokOut.points) {
        const day = p.bucket.slice(0, 10)
        map.set(day, [...(map.get(day) ?? []), ...p.values])
      }
    }
    return map
  }, [tokOut, bucket])

  // 合计模式:单组合图（in/out 两段柱 + 总 credit 曲线）
  const comboSeries = useMemo<ComboSeries[]>(() => {
    if (byModel) return []
    return buckets.map((day) => ({
      input: (tokInByDay.get(day) ?? [0]).reduce((a, b) => a + b, 0),
      output: (tokOutByDay.get(day) ?? [0]).reduce((a, b) => a + b, 0),
      credit: summary.byDay.find((d) => d.day === day)?.credit ?? null,
      inputHours: bucket === 'hour' ? tokInByDay.get(day) : undefined,
      outputHours: bucket === 'hour' ? tokOutByDay.get(day) : undefined,
    }))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [byModel, buckets.join(','), tokInByDay, tokOutByDay, summary, bucket])

  // 按模型模式:每模型一组并排柱（in/out 两段）+ 每模型一条 credit 曲线。
  // tokens 按模型 = get_range_series（model 维) 的 in/out 两条序列按日并入;
  // credit 按模型 = by_model_day。只列积分账本里出现过的模型（有 credit 的）,
  // tokens-only 模型不单独成组（其用量已含在别处柱高,避免卡片爆炸）。
  const perModel = useMemo(() => {
    if (!byModel || tokIn === null || tokOut === null) return []
    const inByKey = new Map(tokIn.seriesKeys.map((k, i) => [k, i]))
    const outByKey = new Map(tokOut.seriesKeys.map((k, i) => [k, i]))
    return summary.byModelDay.map((m) => {
      const creditByDay = new Map(m.byDay.map((d) => [d.day, d.credit]))
      const inIdx = inByKey.get(m.key)
      const outIdx = outByKey.get(m.key)
      const series: ComboSeries[] = buckets.map((day, bi) => ({
        input: inIdx !== undefined ? dayValue(tokIn, inIdx, day, bi, bucket) : 0,
        output: outIdx !== undefined ? dayValue(tokOut, outIdx, day, bi, bucket) : 0,
        credit: creditByDay.get(day) ?? null,
        inputHours: inIdx !== undefined && bucket === 'hour' ? splitDayHours(tokIn.points, bi, inIdx) : undefined,
        outputHours: outIdx !== undefined && bucket === 'hour' ? splitDayHours(tokOut.points, bi, outIdx) : undefined,
      }))
      return { key: m.key, label: m.label, series }
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [byModel, buckets.join(','), tokIn, tokOut, summary, bucket])

  if (tokIn === null || tokOut === null) {
    return <div className="insight-empty">Loading…</div>
  }

  return (
    <div className="credit-combo">
      {!byModel && comboSeries.length > 0 && (
        <ComboChart series={comboSeries} buckets={buckets} />
      )}
      {byModel && perModel.map((g) => (
        <div key={g.key} className="credit-combo-model">
          <div className="credit-combo-model-label">
            <span className="legend-swatch" style={{ background: colorFor(g.key) }} />
            {g.label}
          </div>
          <ComboChart series={g.series} buckets={buckets} height={150} />
        </div>
      ))}
      <div className="combo-legend">
        <span className="legend-item"><span className="legend-swatch" style={{ background: COMBO_IN }} />Input tokens</span>
        <span className="legend-item"><span className="legend-swatch" style={{ background: COMBO_OUT }} />Output tokens</span>
        <span className="legend-item"><span className="legend-swatch" style={{ background: COMBO_CREDIT }} />credit (right axis)</span>
      </div>
    </div>
  )
}

/** hour 粒度:取某模型某日在 hour 轴上的 24 值。 */
function splitDayHours(points: RangeSeriesPoint[], dayIdx: number, seriesIdx: number): number[] {
  const base = dayIdx * 24
  const out: number[] = []
  for (let h = 0; h < 24; h++) out.push(points[base + h]?.values[seriesIdx] ?? 0)
  return out
}

/** hour 粒度的日合计（in/out 总值）。 */
function dayValue(res: RangeSeriesResult, seriesIdx: number, _day: string, dayIdx: number, bucket: 'day' | 'hour'): number {
  if (bucket === 'day') return res.points[dayIdx]?.values[seriesIdx] ?? 0
  return splitDayHours(res.points, dayIdx, seriesIdx).reduce((a, b) => a + b, 0)
}

export default function InsightsView() {
  // CodeBuddy 积分卡 = 可选组件（设置·General 开,默认关）——
  // 没用过 CodeBuddy 的用户不应看到常驻空引导卡。
  const [showCredit, setShowCredit] = useState(() => getDesignPrefs().insightsCredit)
  useEffect(() => subscribeDesignPrefs((p) => setShowCredit(p.insightsCredit)), [])

  const trend = useTrendBlock()

  // 工具栏固定不随滚动——参照设置页结构:工具栏行在
  // 滚动区外（flex-shrink:0）,只有 .insights-scroll 包着卡片做内容滚动;
  // 此前整个 .insights-view 是滚动容器,滚动条贯穿工具栏。
  return (
    <div className="insights-view">
      {trend.toolbar}
      <div className="insights-scroll">
        {trend.card}
        <AnomalyBlock />
        {showCredit && <CreditBlock />}
      </div>
    </div>
  )
}
