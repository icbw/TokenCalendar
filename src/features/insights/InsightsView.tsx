// 数据洞察视图。
// 布局: 1) 主图卡（曲线⇄堆叠 + 口径控件 + 图例筛选）; 2) 同口径占比环（环居中
// 图例分列左右）; 3) 异常日 31 格单行热力条; 4) 价格面板（PricingBlock）;
// 5) tokens × 积分双组图（双段堆叠柱 + credit 曲线,区分模型开关;可选组件）。
// 手写 SVG 图表见 charts.tsx。
import { useCallback, useEffect, useMemo, useState } from 'react'
import { events, usageService } from '../../services'
import type { CreditSummary, RangeSeriesResult, TokenMetric } from '../../services'
import { getDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import { DonutChart, LineChart, StackedBarChart, ComboChart, colorFor, colorShades, shadeLadder, COMBO_CREDIT, COMBO_PART_SHADES, UNITEMIZED_COLOR, type ComboCredit, type ComboGroup, type ComboPart, type SeriesSpec } from './charts'
import { formatFull } from '../matrix/matrixScale'
import { OUTLIER_Z, TIME_METRIC_LABELS, PART_PRICE_ORDER, TOKEN_METRICS, TOKEN_METRIC_LABELS, TOKEN_PARTS, formatDuration, isTimeMetric, projectDisplayName, projectTooltip, zScores } from './analytics'
import { Seg } from './Seg'
import RangeControl from './RangeControl'
import PricingBlock from './PricingBlock'
import { HOUR_BUCKET_MAX_DAYS, rangeShortLabel, spanDays } from './range'
import { useRangeSelection } from './useRangeSelection'
import { openProjectManager } from '../projects/projectManagerStore'
import '../projects/projects.css'
import './insights.css'

const MONTH_ABBR = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

const pad2 = (n: number) => String(n).padStart(2, '0')
const ymd = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`

const fullMonthLabel = (m: string) => {
  const [y, mo] = m.split('-')
  return `${MONTH_ABBR[Number(mo) - 1]} ${y}`
}

// ---- 口径控件（分段控件,样式复用 .seg） ----

type Bucket = 'day' | 'hour'
/** project 维 / 时间指标走 get_effort_series（仅 day 粒度）;其余组合走 get_range_series。 */
type Dimension = 'agent' | 'model' | 'project' | 'total'
type Metric = TokenMetric | 'wait' | 'human'
type ChartKind = 'line' | 'stack'

// ---- 主图卡 ----

// 自定义 hook 形态:工具栏与卡片拆成两个返回件,由 InsightsView 装配:
// 工具栏固定在滚动区外,卡片随内容滚动。
function useTrendBlock() {
  const [bucket, setBucket] = useState<Bucket>('day')
  const [dimension, setDimension] = useState<Dimension>('model')
  const [metricPick, setMetric] = useState<Metric>('total')
  // 时间指标只在 project / total 维有数据:其它维回落 Tokens（切维时也显式复位）
  const metric: Metric = isTimeMetric(metricPick) && (dimension === 'agent' || dimension === 'model') ? 'total' : metricPick
  const [kind, setKind] = useState<ChartKind>('line')
  const [filterKey, setFilterKey] = useState<string>('') // '' = 全部
  const [data, setData] = useState<RangeSeriesResult | null>(null)
  /** 单模型分项:TOKEN_PARTS 顺序的四条序列（仅 partsMode 时拉取）。 */
  const [parts, setParts] = useState<(RangeSeriesResult | null)[] | null>(null)
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

  // 范围:7d/30d/90d/All/Custom;project 维筛到单个项目时默认切到该项目生命周期
  const selection = useRangeSelection(dimension === 'project' ? filterKey : '', refreshTick)
  const { startDay, endDay } = selection.range

  // project 维或时间指标 → daily_project 曲线（get_effort_series,无小时表）
  const useEffort = dimension === 'project' || isTimeMetric(metric)
  // 范围 × 粒度合法性:小时粒度只在 ≤ 31 天的范围给（更长范围小时点数无意义）;effort 曲线仅 day
  const hourTooLong = spanDays(selection.range) > HOUR_BUCKET_MAX_DAYS
  const effectiveBucket: Bucket = hourTooLong || useEffort ? 'day' : bucket

  // 系列展示名:合计系列统一英文名;project 维取路径末段（完整路径走 hover）
  const seriesLabel = useCallback(
    (key: string, label: string | undefined) =>
      key === '__total__' ? 'All sources' : dimension === 'project' ? projectDisplayName(key) : label ?? key,
    [dimension],
  )

  const filterOptions = useMemo(() => {
    // 筛选维度与展示维度同族（agent 维筛 agent,model 维筛 model,project 维筛 project）;total 维无筛选
    return (data?.seriesKeys ?? []).map((k, i) => ({ key: k, label: seriesLabel(k, data?.seriesLabels[i]) }))
  }, [data, seriesLabel])

  // 模型维筛到单个模型 + Tokens 指标 → 该模型按 token 分项出图（柱高 / 分项和 = 总量）
  const partsMode = dimension === 'model' && !!filterKey && metric === 'total'

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    const partsReq = partsMode
      ? Promise.all(
          TOKEN_PARTS.map((m) =>
            usageService.getRangeSeries({ startDay, endDay, bucket: effectiveBucket, dimension, metric: m, filterDimension: dimension, filterKey }),
          ),
        )
      : Promise.resolve(null)
    const req =
      dimension === 'project' || isTimeMetric(metric)
        ? usageService.getEffortSeries({
            startDay,
            endDay,
            dimension,
            metric,
            filter: dimension === 'total' || !filterKey ? undefined : { dimension, key: filterKey },
          })
        : usageService.getRangeSeries({
            startDay,
            endDay,
            bucket: effectiveBucket,
            dimension,
            metric,
            // 筛选:展示维 = agent → 筛 agent;展示维 = model → 筛 model;total 无筛选
            filterDimension: dimension === 'total' || !filterKey ? undefined : dimension,
            filterKey: dimension === 'total' || !filterKey ? undefined : filterKey,
          })
    void Promise.all([req, partsReq]).then(([res, partRes]) => {
      if (cancelled) return
      setData(res)
      setParts(partRes)
      setLoading(false)
    })
    return () => {
      cancelled = true
    }
  }, [startDay, endDay, effectiveBucket, dimension, metric, filterKey, refreshTick, partsMode])

  const partsView = partsMode && parts !== null
  const series: SeriesSpec[] = useMemo(() => {
    if (!data) return []
    if (partsMode && parts && data.seriesKeys.length > 0) return partSeries(data, parts, filterKey)
    return data.seriesKeys.map((k, i) => ({ key: k, label: seriesLabel(k, data.seriesLabels[i]), values: data.points.map((p) => p.values[i] ?? 0) }))
  }, [data, parts, partsMode, filterKey, seriesLabel])
  const timeFmt = isTimeMetric(metric) ? formatDuration : undefined
  const buckets = data?.points.map((p) => p.bucket) ?? []

  // 占比环数据:主图范围内各系列合计
  const donutItems = useMemo(
    () =>
      series
        .filter(() => dimension !== 'total')
        .map((s) => ({ key: s.key, label: s.label, color: s.color, value: s.values.reduce((a, b) => a + b, 0) }))
        .filter((s) => s.value > 0)
        .sort((a, b) => (partsView ? 0 : b.value - a.value)),
    [series, dimension, partsView],
  )
  const donutUnit = isTimeMetric(metric)
    ? TIME_METRIC_LABELS[metric].unit
    : TOKEN_METRIC_LABELS[metric].unit

  const pickFilter = useCallback(
    (k: string) => {
      setFilterKey((prev) => (prev === k ? '' : k)) // 再点同项取消筛选
    },
    [],
  )

  // 手改范围:非项目维时清图例筛选（系列集随范围变）;项目维保留——筛选即项目选择,
  // 清掉会连带退出项目生命周期
  const rangeSelection = useMemo(
    () => ({
      ...selection,
      choose: (next: Parameters<typeof selection.choose>[0]) => {
        selection.choose(next)
        if (dimension !== 'project') setFilterKey('')
      },
    }),
    [selection, dimension],
  )

  // 两个返回件:toolbar（固定行）与 card（随滚动内容）。
  // 范围控件（含自定义日期与区间文字）单列第二行,工具栏主行仍保持单行不换行。
  return {
    toolbar: (
      <>
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
          value={effectiveBucket}
          options={[
            { v: 'day' as Bucket, label: 'Day', hint: 'Group by day' },
            {
              v: 'hour' as Bucket,
              label: 'Hour',
              hint: useEffort
                ? 'Hourly buckets are not available for projects or time metrics'
                : hourTooLong
                  ? `Hourly buckets need a range of ${HOUR_BUCKET_MAX_DAYS} days or less`
                  : 'Group by hour',
              disabled: useEffort || hourTooLong,
            },
          ]}
          onChange={setBucket}
        />
        <Seg
          value={dimension}
          options={[
            { v: 'model' as Dimension, label: 'Model', hint: 'One series per model' },
            { v: 'agent' as Dimension, label: 'Agent', hint: 'One series per agent' },
            { v: 'project' as Dimension, label: 'Project', hint: 'One series per project (working directory)' },
            { v: 'total' as Dimension, label: 'Total', hint: 'Single all-source series' },
          ]}
          onChange={(v) => {
            setDimension(v)
            setFilterKey('')
            if ((v === 'agent' || v === 'model') && isTimeMetric(metricPick)) setMetric('total')
          }}
        />
        <Seg
          value={metric}
          options={[
            ...TOKEN_METRICS.map((m) => ({ v: m as Metric, label: TOKEN_METRIC_LABELS[m].short, hint: TOKEN_METRIC_LABELS[m].hint })),
            ...(['wait', 'human'] as const).map((m) => {
              const off = dimension === 'agent' || dimension === 'model'
              return {
                v: m as Metric,
                label: TIME_METRIC_LABELS[m].label,
                hint: off ? `${TIME_METRIC_LABELS[m].label}: time data is only available in Project or Total` : TIME_METRIC_LABELS[m].hint,
                disabled: off,
              }
            }),
          ]}
          onChange={setMetric}
        />
        {loading && <span className="matrix-loading">Loading…</span>}
      </header>
      <div className="insight-rangebar">
        <RangeControl selection={rangeSelection} noun="Usage" />
      </div>
      </>
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
                title={filterKey === o.key ? 'Click to clear filter' : `Only ${dimension === 'project' ? projectTooltip(o.key) : o.label}`}
              >
                <span className="legend-swatch" style={{ background: colorFor(o.key) }} />
                {o.label}
              </button>
            ))}
            {/* 项目维图例即项目选择器,末尾放管理入口（打开主窗口内弹出层,不改筛选）*/}
            {dimension === 'project' && (
              <button className="legend-item pm-manage-link" onClick={openProjectManager} title="Rename, hide or merge projects">
                Manage projects…
              </button>
            )}
          </div>
        )}

        {data === null ? (
          <div className="insight-empty">Data unavailable (service not running)</div>
        ) : series.length === 0 || series.every((s) => s.values.every((v) => v === 0)) ? (
          <div className="insight-empty">No usage records in this range</div>
        ) : kind === 'line' ? (
          <LineChart series={series} buckets={buckets} formatValue={timeFmt} ordered={partsView} />
        ) : (
          <StackedBarChart series={series} buckets={buckets} formatValue={timeFmt} ordered={partsView} />
        )}

        {/* 占比环:与主图同口径（维度/指标/范围/筛选）*/}
        {dimension !== 'total' && donutItems.length > 0 && (
          <div className="donut-wrap">
            <DonutChart
              items={donutItems}
              centerLabel={rangeShortLabel(selection.sel) + ' · ' + donutUnit}
              unitLabel={donutUnit}
              formatValue={timeFmt}
              titleFor={dimension === 'project' ? (k) => projectTooltip(k) : undefined}
            />
          </div>
        )}
      </section>
    ),
  }
}

/** 单模型的 token 分项序列:按价格从高到低（Output → Input → Cache write → Cache read）固定排列,
 * 依次取该模型本色由深到浅——最深的 Output 与该模型总量同色,堆叠柱自下而上同序。
 * 源里只报总量、无分项的余量（Codex）另列 Unitemized（中性灰,不在价格色阶内）。
 * 全零分项不出现（颜色仍按固定档位,不因缺项而顺移）;每个桶的分项和恒等于总量。 */
function partSeries(total: RangeSeriesResult, parts: (RangeSeriesResult | null)[], modelKey: string): SeriesSpec[] {
  const totals = total.points.map((p) => p.values[0] ?? 0)
  const valuesOf = (m: TokenMetric) => {
    const r = parts[TOKEN_PARTS.indexOf(m as (typeof TOKEN_PARTS)[number])]
    return totals.map((_, i) => r?.points[i]?.values[0] ?? 0)
  }
  const shades = colorShades(modelKey, PART_PRICE_ORDER.length)
  const specs: SeriesSpec[] = PART_PRICE_ORDER.map((m, k) => ({
    key: `${modelKey}::${m}`,
    label: TOKEN_METRIC_LABELS[m].label,
    values: valuesOf(m),
    color: shades[k],
  }))
  const rest = totals.map((t, i) => Math.max(0, t - specs.reduce((a, s) => a + s.values[i], 0)))
  specs.push({ key: `${modelKey}::unitemized`, label: 'Unitemized', values: rest, color: UNITEMIZED_COLOR })
  return specs.filter((s) => s.values.some((v) => v > 0))
}

// ---- 异常日（31 天 z-score） ----

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

  // 逐日 z（热力条上色用）:样本 < 7 或 sd=0 全零窗口不判（与 Tasks 离群共用 analytics.zScores）。
  const zByDay = useMemo(() => {
    const zs = days ? zScores(days.map((d) => d.total)) : null
    if (!days || !zs) return new Map<string, number>()
    return new Map(days.map((d, i) => [d.ymd, zs[i]]))
  }, [days])

  const outliers = useMemo<OutlierDay[]>(() => {
    if (!days) return []
    return days
      .flatMap((d) => {
        const z = zByDay.get(d.ymd)
        return z === undefined ? [] : [{ ...d, z }]
      })
      .filter((d) => Math.abs(d.z) >= OUTLIER_Z)
      .sort((a, b) => Math.abs(b.z) - Math.abs(a.z))
  }, [days, zByDay])

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
              // |z| 强度落档:2~3 浅 / 3~4 中 / ≥4 深
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

// ---- tokens × 积分（双组图;可选组件,默认关） ----

function CreditBlock() {
  const [month, setMonth] = useState(() => {
    const now = new Date()
    return `${now.getFullYear()}-${pad2(now.getMonth() + 1)}`
  })
  const [summary, setSummary] = useState<CreditSummary | null>(null)
  const [loading, setLoading] = useState(false)
  const [byModel, setByModel] = useState(false) // 区分模型开关
  const [bucket, setBucket] = useState<'day' | 'hour'>('day')
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
      {/* 工具行独立成行（参照矩阵视图）,月份选择跟在控件组后,不 auto 靠右*/}
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
            Credits are read from local CodeBuddy / WorkBuddy session data (shared credit pool);
            this month has no request with credits yet.
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
              <span className="credit-total-label">Local session credits · may run slightly below the official bill</span>
            </div>
            <label className="credit-mode-toggle" title="One bar per model side by side in the same chart, with one credit curve per model">
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

/** 双组图装配:tokens 分项序列（get_range_series 月内 total + 四分项;天/小时粒度可切）
 * + credit 日序列（credit_summary.by_day / by_model_day）。共享横轴:天粒度 =
 * 每日一组柱;小时粒度 = 每日内 24 根小时细柱（横轴仍按天分组,组内并排）。
 * 按模型:所有模型画在同一张图里——天粒度每日每模型一根并排柱 + 每模型一条 credit 曲线;
 * 小时粒度柱仍是全部合计（每模型 24 根细柱放不下）,credit 曲线按模型。
 * credit 只有日粒度（daily_usage.credit）,小时档下曲线保持按日对齐。
 * 无积分的日断线表达,不伪装成 0。 */
function ComboBlock({ month, summary, byModel, bucket }: {
  month: string
  summary: CreditSummary
  byModel: boolean
  bucket: 'day' | 'hour'
}) {
  // [total, ...PART_PRICE_ORDER] 五条序列（total 用来算只报总量的余量）
  const [tok, setTok] = useState<(RangeSeriesResult | null)[] | null>(null)
  const [refreshTick, setRefreshTick] = useState(0)
  const modelBars = byModel && bucket === 'day'

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
  useEffect(() => {
    let cancelled = false
    const [y, m] = month.split('-').map(Number)
    const last = new Date(y, m, 0).getDate()
    const base = {
      startDay: `${month}-01`,
      endDay: `${month}-${pad2(last)}`,
      bucket,
      dimension: modelBars ? ('model' as const) : ('total' as const),
    }
    void Promise.all((['total', ...PART_PRICE_ORDER] as TokenMetric[]).map((metric) => usageService.getRangeSeries({ ...base, metric }))).then(
      (res) => {
        if (!cancelled) setTok(res)
      },
    )
    return () => {
      cancelled = true
    }
  }, [month, bucket, modelBars, refreshTick])

  // 横轴 = 积分轴（后端已补零至 min（月末, 今日)）;tokens 与横轴同从月初起,按下标对齐
  const buckets = summary.byDay.map((d) => d.day)
  const parts: ComboPart[] = [
    ...PART_PRICE_ORDER.map((m) => ({ key: m, label: TOKEN_METRIC_LABELS[m].label })),
    { key: 'unitemized', label: 'Unitemized' },
  ]

  const view = useMemo(() => {
    if (tok === null || tok.some((r) => r === null)) return null
    const res = tok as RangeSeriesResult[]
    const at = (r: RangeSeriesResult, key: string, point: number) => {
      const idx = r.seriesKeys.indexOf(key)
      return idx < 0 ? 0 : r.points[point]?.values[idx] ?? 0
    }
    // 一个桶（或小时点）的分项:价格顺序四项 + 余量（total − 四项和,仅 Codex 只报总量的调用会有）
    const partVals = (key: string, point: number) => {
      const vals = PART_PRICE_ORDER.map((_, j) => at(res[j + 1], key, point))
      const rest = Math.max(0, at(res[0], key, point) - vals.reduce((a, b) => a + b, 0))
      return [...vals, rest]
    }
    const groups: ComboGroup[] = modelBars
      ? summary.byModelDay.map((m) => ({
          key: m.key,
          label: m.label,
          colors: [...colorShades(m.key, PART_PRICE_ORDER.length), UNITEMIZED_COLOR],
          values: buckets.map((_, bi) => partVals(m.key, bi)),
        }))
      : [
          {
            key: '__total__',
            label: 'All sources',
            colors: [...COMBO_PART_SHADES, UNITEMIZED_COLOR],
            values: buckets.map((_, bi) =>
              bucket === 'day'
                ? partVals('__total__', bi)
                : Array.from({ length: 24 }, (_, h) => partVals('__total__', bi * 24 + h)).reduce(
                    (acc, v) => acc.map((a, k) => a + v[k]),
                    new Array(parts.length).fill(0),
                  ),
            ),
            hours: bucket === 'hour' ? buckets.map((_, bi) => Array.from({ length: 24 }, (_, h) => partVals('__total__', bi * 24 + h))) : undefined,
          },
        ]
    const credits: ComboCredit[] = byModel
      ? summary.byModelDay.map((m) => {
          const byDay = new Map(m.byDay.map((d) => [d.day, d.credit]))
          return { key: m.key, label: m.label, color: colorFor(m.key), values: buckets.map((day) => byDay.get(day) ?? null) }
        })
      : [{ key: 'credit', label: 'credit', color: COMBO_CREDIT, values: buckets.map((day) => summary.byDay.find((d) => d.day === day)?.credit ?? null) }]
    const hasRest = groups.some((g) => g.values.some((v) => (v[PART_PRICE_ORDER.length] ?? 0) > 0))
    return { groups, credits, hasRest }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tok, buckets.join(','), summary, byModel, modelBars, bucket])

  if (view === null) {
    return <div className="insight-empty">Loading…</div>
  }

  // 按模型时图例:模型色（柱 = 该模型色阶,线 = 该模型 credit）+ 中性色阶说明分项深浅
  const legendShades = modelBars ? shadeLadder(220, 0.1, 0.36, PART_PRICE_ORDER.length) : COMBO_PART_SHADES
  return (
    <div className="credit-combo">
      <ComboChart parts={parts} groups={view.groups} credits={view.credits} buckets={buckets} />
      <div className="combo-legend">
        {byModel &&
          summary.byModelDay.map((m) => (
            <span key={m.key} className="legend-item">
              <span className="legend-swatch" style={{ background: colorFor(m.key) }} />
              {m.label}
            </span>
          ))}
        {PART_PRICE_ORDER.map((m, i) => (
          <span key={m} className="legend-item" title={TOKEN_METRIC_LABELS[m].hint}>
            <span className="legend-swatch" style={{ background: legendShades[i] }} />
            {TOKEN_METRIC_LABELS[m].label}
          </span>
        ))}
        {view.hasRest && (
          <span className="legend-item" title="Tokens reported only as a total, without a breakdown">
            <span className="legend-swatch" style={{ background: UNITEMIZED_COLOR }} />
            Unitemized
          </span>
        )}
        {!byModel && (
          <span className="legend-item"><span className="legend-swatch" style={{ background: COMBO_CREDIT }} />credit (right axis)</span>
        )}
        {byModel && <span className="legend-item combo-legend-note">bars: tokens by model (dark → light = Output → Cache read) · lines: credit (right axis)</span>}
      </div>
    </div>
  )
}

export default function InsightsView() {
  // CodeBuddy 积分卡是可选组件（设置·General 开,默认关）:
  // 没用过 CodeBuddy 的用户不应看到常驻空引导卡。
  const [showCredit, setShowCredit] = useState(() => getDesignPrefs().insightsCredit)
  useEffect(() => subscribeDesignPrefs((p) => setShowCredit(p.insightsCredit)), [])

  const trend = useTrendBlock()

  // 工具栏固定不随滚动（与设置页同构）:工具栏行在滚动区外（flex-shrink:0）,
  // 只有 .insights-scroll 包着卡片做内容滚动。
  return (
    <div className="insights-view">
      {trend.toolbar}
      <div className="insights-scroll">
        {trend.card}
        <AnomalyBlock />
        <PricingBlock />
        {showCredit && <CreditBlock />}
      </div>
    </div>
  )
}
