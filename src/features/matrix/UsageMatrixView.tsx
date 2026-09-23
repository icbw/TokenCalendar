// 视图状态与数据装配：usageService 加载真实数据；mock 仅降级。
// 窗口 31 列恒定、最右列 = 今天（与挂件「最右列 = 当前周」同语义的天粒度版）——
// 窗口跨月时按需拉多个月份矩阵在前端拼接；hover 显示年月日 + tokens·对话数（message_counts）。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import UsageMatrix, { MatrixRow } from './UsageMatrix'
import RowBreakdown from '../breakdown/RowBreakdown'
import { generateMockData } from '../../mock/mockData'
import { events, usageService } from '../../services'
import type { BreakdownDay, TokenMetric } from '../../services'
import type { UsageRow } from '../../services/types'
import { getDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import { TOKEN_METRICS, projectDisplayName, projectTooltip } from '../insights/analytics'
import { fmt, useT, type MessageKey } from '../../lib/i18n'
import { monthDayLabel, monthShort } from './matrixScale'
import './matrixView.css'

/** project 维走 get_project_month_rows / get_project_breakdown;agent / model 维走月矩阵命令。 */
export type GroupBy = 'agent' | 'model' | 'project'
type Bucket = 'day' | 'week' | 'cumulative'
/** 只出 token（时间统计已迁到 Tasks 视图的 Time spent,见 TASK_TIME_SPENT_DESIGN）。 */
type Metric = TokenMetric
/** 排序策略按视图分账：各视图各自记忆互不串扰。
 * byTotal（激活 = 按总量,未激活 = 按名称）;family（家族聚合,仅 model 视图生效）叠加在 byTotal 之上。 */
type ViewSort = Record<GroupBy, { byTotal: boolean; family: boolean }>

/** 指标按钮的字典键（短名 / 悬停说明 / hover 读数单位）。只存键,文案在渲染时取。 */
const METRIC_KEYS: Record<Metric, { short: MessageKey<'matrix'>; hint: MessageKey<'matrix'>; unit: MessageKey<'matrix'> }> = {
  total: { short: 'metricTokens', hint: 'metricTokensHint', unit: 'unitTokens' },
  uncached: { short: 'metricUncached', hint: 'metricUncachedHint', unit: 'unitUncached' },
  input: { short: 'metricInput', hint: 'metricInputHint', unit: 'unitInput' },
  cache_write: { short: 'metricCacheW', hint: 'metricCacheWHint', unit: 'unitCacheW' },
  cache_read: { short: 'metricCacheR', hint: 'metricCacheRHint', unit: 'unitCacheR' },
  output: { short: 'metricOutput', hint: 'metricOutputHint', unit: 'unitOutput' },
}

const NOW = new Date()
/** 滚动窗口：31 列恒定，最右列 = 今天。 */
const WINDOW_DAYS = 31
const WINDOW_DATES: Date[] = Array.from({ length: WINDOW_DAYS }, (_, i) => {
  const d = new Date(NOW.getFullYear(), NOW.getMonth(), NOW.getDate())
  d.setDate(d.getDate() - (WINDOW_DAYS - 1 - i))
  return d
})
const pad2 = (n: number) => String(n).padStart(2, '0')
const ymd = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`
/** 窗口覆盖的月份（通常 2 个,极端 3 个：月尾 + 28 天 2 月 + 月初）。 */
const WINDOW_MONTHS = [...new Set(WINDOW_DATES.map(ymd).map((s) => s.slice(0, 7)))]
/** 当月 1 号在窗口中的下标（其前为上月列,表头淡化）。 */
const CURRENT_MONTH_START = WINDOW_DATES.findIndex(
  (d) => d.getMonth() === NOW.getMonth() && d.getFullYear() === NOW.getFullYear(),
)

function applyBucket(values: (number | null)[], bucket: Bucket): (number | null)[] {
  if (bucket === 'day') return values
  if (bucket === 'week') {
    // 复合周视图：保持 31 格布局不变，
    // 每周 7 格的值都替换为该周聚合值（同一周成块，优于压成 5 列）
    const out: (number | null)[] = []
    for (let i = 0; i < values.length; i++) {
      const weekStart = Math.floor(i / 7) * 7
      const chunk = values.slice(weekStart, weekStart + 7)
      if (chunk.every((v) => v === null)) {
        out.push(null)
        continue
      }
      let sum = 0
      let seen = false
      for (const v of chunk) {
        if (v !== null) {
          sum += v
          seen = true
        }
      }
      out.push(seen ? sum : null)
    }
    return out
  }
  const out: (number | null)[] = []
  let acc = 0
  for (const v of values) {
    if (v === null) out.push(null)
    else {
      acc += v
      out.push(acc)
    }
  }
  return out
}

function bucketTotal(values: (number | null)[], bucket: Bucket): number {
  if (bucket === 'week') {
    // 复合周视图：同一周 7 格同值，按周取一个求和（避免重复累加 7 倍）
    let total = 0
    for (let w = 0; w < values.length; w += 7) {
      const nz = values.slice(w, w + 7).find((x): x is number => x !== null)
      if (nz !== undefined) total += nz
    }
    return total
  }
  const v = values.filter((x): x is number => x !== null)
  if (v.length === 0) return 0
  return bucket === 'cumulative' ? v[v.length - 1] : v.reduce((a, b) => a + b, 0)
}

// model 视图显示名简化：canonical key → "provider family tier"
function simplifyLabel(key: string, groupBy: GroupBy): string {
  if (groupBy === 'project') return projectDisplayName(key)
  if (groupBy === 'agent') return key
  const parts = key.split('/')
  if (parts.length >= 5) return `${parts[0]}/${parts[1]} ${parts[3]}`
  return key
}

/** 模型家族键（综合排序用）：key 首段即家族——
 * deepseek-v4-flash/pro → deepseek、gpt-5.6-sol/luna/terra → gpt、
 * claude-opus-5/sonnet-5 → claude;agent 视图与无分段 key 原样成组。 */
function familyOf(key: string, groupBy: GroupBy): string {
  if (groupBy !== 'model') return key
  return key.split(/[-_/]/)[0] ?? key
}

export default function UsageMatrixView({ groupBy, onGroupByChange, selectedRow, onRowSelect }: {
  /** groupBy 由 FullWindow 持有（model 视图下方全系列曲线跟随切换）。
   * 不传时内部自持（保持独立可用）。 */
  groupBy?: GroupBy
  onGroupByChange?: (g: GroupBy) => void
  /** 行联动:面板选中行由 FullWindow 持有（受控高亮）;不传时内部自持
   * （独立使用 = 内联展开）。 */
  selectedRow?: string | null
  /** 点行名上抛（null = 取消/恢复预设）。不传时内部内联展开。 */
  onRowSelect?: (rowKey: string | null) => void
}) {
  const t = useT('matrix')
    // 区间标题与表头日标签依赖语言,渲染时生成（模块顶层不拼文案）。
  const rangeLabel = useMemo(() => {
    const last = WINDOW_DATES[WINDOW_DAYS - 1]
    return t('rangeLabel', {
      from: monthDayLabel(t, WINDOW_DATES[0]),
      to: monthDayLabel(t, last),
      year: last.getFullYear(),
    })
  }, [t])
  const dayLabels = useMemo(
    () => WINDOW_DATES.map((d) => (d.getDate() === 1 ? monthShort(t, d.getMonth()) : String(d.getDate()))),
    [t],
  )
  const [internalGroupBy, setInternalGroupBy] = useState<GroupBy>('agent')
  const effectiveGroupBy = groupBy ?? internalGroupBy
  const [bucket, setBucket] = useState<Bucket>('day')
  const [metric, setMetric] = useState<Metric>('total')
  const setGroupBy = (g: GroupBy) => {
    if (onGroupByChange) onGroupByChange(g)
    else setInternalGroupBy(g)
  }
  const [sortBy, setSortBy] = useState<ViewSort>({
    agent: { byTotal: true, family: true },
    model: { byTotal: true, family: true },
    project: { byTotal: true, family: true },
  })
  const [scaleMode, setScaleMode] = useState<'global' | 'perRow'>('global')
  const [selected, setSelected] = useState<{ rowKey: string; day: number } | null>(null)
  const [internalExpanded, setInternalExpanded] = useState<string | null>(null)
  const effectiveExpanded = selectedRow !== undefined ? selectedRow : internalExpanded
  const [expandedData, setExpandedData] = useState<BreakdownDay[] | null>(null)
  const [serverRows, setServerRows] = useState<MatrixRow[]>([])
  const [loading, setLoading] = useState(false)
  const [useMock, setUseMock] = useState(false)
  const [refreshTick, setRefreshTick] = useState(0)
  // 矩阵最大行数（设置页 General → Matrix max rows,默认 15;0 = 不限）——排序后
  // 只保留 top N 行,超出行静默隐藏,保证热力图主体在小窗也完整可见（不出现内部滚动）。
  const [maxRows, setMaxRows] = useState(() => getDesignPrefs().matrixMaxRows)
  useEffect(() => subscribeDesignPrefs((p) => setMaxRows(p.matrixMaxRows)), [])

  // 格子等比缩放（双轴）见 visibleRows 之后的 effect（依赖行数）。
  const matrixRef = useRef<HTMLDivElement>(null)
  const [cellPx, setCellPx] = useState(17)

  // 采集完成事件（usage:changed）→ 300ms 去抖后重新拉取
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

  // 加载窗口覆盖月份的矩阵,前端拼接 31 天窗口（真实数据;失败降级 mock）
  useEffect(() => {
    let cancelled = false
    setLoading(true)
    void Promise.all(
      WINDOW_MONTHS.map((m) =>
        (effectiveGroupBy === 'project'
          ? usageService.getProjectMonthRows(m, 'project', metric)
          : usageService.getMonthlyMatrix({ month: m, groupBy: effectiveGroupBy, metric, bucket: 'day', normalization: scaleMode })
        ).then((res) => ({ m, res })),
      ),
    ).then((list) => {
      if (cancelled) return
      if (list.every((x) => x.res === null)) {
        fallbackToMock()
        setLoading(false)
        return
      }
      // 月份 → key → row：窗口逐日按日期归属月取值
      const byMonth = new Map<string, Map<string, UsageRow>>()
      for (const { m, res } of list) {
        if (!res) continue
        byMonth.set(m, new Map((res.rows ?? []).map((r) => [r.key, r])))
      }
      const merged = new Map<
        string,
        { key: string; label: string; values: (number | null)[]; counts: (number | null)[] }
      >()
      // 行的键集 = 窗口内任一月份有记录的 key;键集内的格子全部走灰底语义:
      // 后端行只覆盖「该月有记录」的月份,跨月拼接时缺月份的格子若留 null
      // 会渲染成透明空白——窗口最右列 = 今天,窗口内不存在未来日,
      // 缺月份 = 该月无记录 = 真实零,补 0 而非 null。
      const allKeys = new Map<string, string>()
      for (const monthRows of byMonth.values()) {
        for (const r of monthRows.values()) allKeys.set(r.key, r.label)
      }
      for (const [key, label] of allKeys) {
        const values: (number | null)[] = Array(WINDOW_DAYS).fill(0)
        const counts: (number | null)[] = Array(WINDOW_DAYS).fill(0)
        WINDOW_DATES.forEach((d, i) => {
          const r = byMonth.get(ymd(d).slice(0, 7))?.get(key)
          if (!r) return
          const idx = d.getDate() - 1
          values[i] = r.values?.[idx] ?? 0
          counts[i] = r.messageCounts?.[idx] ?? 0
        })
        merged.set(key, { key, label, values, counts })
      }
      setUseMock(false)
      setServerRows(
        [...merged.values()].map((e) => ({
          key: e.key,
          label: simplifyLabel(e.key, effectiveGroupBy),
          subtitle: effectiveGroupBy === 'project' ? projectTooltip(e.key) : undefined,
          values: e.values,
          counts: e.counts,
          total: e.values.reduce<number>((s, v) => s + (v ?? 0), 0),
        })),
      )
      setLoading(false)
    })
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [effectiveGroupBy, metric, refreshTick])

  const fallbackToMock = useCallback(() => {
    setUseMock(true)
    const raw = generateMockData()
    setServerRows(
      raw.map((a) => {
        // mock 按当月 1 号起生成 → 只回填窗口中属于当月的日期,上月部分留空
        const values: (number | null)[] = Array(WINDOW_DAYS).fill(null)
        const counts: (number | null)[] = Array(WINDOW_DAYS).fill(null)
        WINDOW_DATES.forEach((d, i) => {
          if (d.getMonth() === NOW.getMonth() && d.getFullYear() === NOW.getFullYear()) {
            const day = a.days[d.getDate() - 1]
            values[i] = day ? day.total : null
            counts[i] = day ? 1 : 0
          }
        })
        return {
          key: a.key,
          label: a.label,
          values,
          counts,
          total: values.reduce<number>((s, v) => s + (v ?? 0), 0),
          health: a.health,
        }
      }),
    )
  }, [])

  // 行名点击:受控模式（onRowSelect）→ 上抛 FullWindow（面板联动,不内联展开）;
  // 独立模式 → 内联展开钻取（GetBreakdown 跨月拼 31 天）。
  const toggleExpand = useCallback(
    async (rowKey: string) => {
      if (onRowSelect) {
        onRowSelect(rowKey)
        return
      }
      if (internalExpanded === rowKey) {
        setInternalExpanded(null)
        setExpandedData(null)
        return
      }
      setInternalExpanded(rowKey)
      setExpandedData(null)
      if (useMock) return
      try {
        const list = await Promise.all(
          WINDOW_MONTHS.map((m) =>
            effectiveGroupBy === 'project'
              ? usageService.getProjectBreakdown('project', rowKey, m)
              : usageService.getBreakdown(effectiveGroupBy, rowKey, m),
          ),
        )
        if (list.every((x) => x === null)) {
          setExpandedData(null)
          return
        }
        const byDay = new Map<string, BreakdownDay>()
        for (const days of list) {
          for (const d of days ?? []) byDay.set(d.day, d)
        }
        setExpandedData(WINDOW_DATES.map((d) => byDay.get(ymd(d)) ?? { day: ymd(d), slices: null }))
      } catch (e) {
        console.error('[usageService] breakdown failed:', e)
        setExpandedData(null)
      }
    },
    [internalExpanded, effectiveGroupBy, useMock, onRowSelect],
  )

  // bucket 前端聚合 + 排序。family = 综合版：模型按家族
  // （key 首段）聚合总量,家族间按总量降序;同家族行连续排列,家族内按行总量降序。
  // 总量开关关闭时家族仍成组,家族间与家族内改按名称。
  // 排序策略随视图分账:sortBy[groupBy],切换视图各用各的策略互不覆盖。
  const viewSort = sortBy[effectiveGroupBy]
  const byTotal = viewSort.byTotal
  const familySort = effectiveGroupBy === 'model' && viewSort.family
  const toggleSort = (field: 'byTotal' | 'family') =>
    setSortBy((prev) => ({
      ...prev,
      [effectiveGroupBy]: { ...prev[effectiveGroupBy], [field]: !prev[effectiveGroupBy][field] },
    }))
  const matrixRows: MatrixRow[] = useMemo(() => {
    const rows = serverRows.map((r) => ({
      ...r,
      values: applyBucket(r.values, bucket),
      total: bucketTotal(applyBucket(r.values, bucket), bucket),
    }))
    if (familySort) {
      const familyTotal = new Map<string, number>()
      for (const r of rows) {
        const fam = familyOf(r.key, effectiveGroupBy)
        familyTotal.set(fam, (familyTotal.get(fam) ?? 0) + r.total)
      }
      rows.sort((a, b) => {
        const fa = familyOf(a.key, effectiveGroupBy)
        const fb = familyOf(b.key, effectiveGroupBy)
        if (!byTotal) return fmt.compare(fa, fb) || fmt.compare(a.label, b.label)
        return (
          (familyTotal.get(fb) ?? 0) - (familyTotal.get(fa) ?? 0) ||
          b.total - a.total ||
          fmt.compare(a.label, b.label)
        )
      })
    } else {
      rows.sort((a, b) => (byTotal ? b.total - a.total : fmt.compare(a.label, b.label)))
    }
    return rows
    // t：显示排序（fmt.compare）随语言变化,切换后重排
  }, [serverRows, bucket, effectiveGroupBy, byTotal, familySort, t])

  // 行数上限截断（排序在前 = 保留 top N;超出行静默隐藏,上限在设置页调）。
  const visibleRows = useMemo(
    () => (maxRows > 0 ? matrixRows.slice(0, maxRows) : matrixRows),
    [matrixRows, maxRows],
  )

  // 格子等比缩放（双轴）:CSS minmax 只按宽度收缩、高度不跟,缩窗后行距 > 列距失衡,
  // 所以由 JS 计算。ResizeObserver 量测矩阵区可用宽高:cell = min（17,
  // 宽预算/31, 高预算/行数)（下限 8）——宽高同一尺寸;gap 与 cell 同步等比
  // （cell:gap = 5:1）,行距恒 = 列距。结果写进 CSS 变量,
  // 格子网格与图表面板列模板同源消费（图表两缘 = 格子两缘,随缩放同步）。
  useEffect(() => {
    const el = matrixRef.current
    if (!el) return
    const CELL_MAX = 17
    const rows = Math.max(1, visibleRows.length)
    const update = () => {
      const padX = 24 // .matrix-view padding 12×2
      const labelW = 150
      const totalW = 92
      const colGaps = 12 // 两个 6px 列间距（label|cells|total）
      const headH = 30 // 表头行 + gap
      const w = el.clientWidth - labelW - totalW - colGaps - padX
      const h = el.clientHeight - headH - 30 // 30 ≈ 工具栏下矩阵安全余量
      const gapMin = 2
      // 解 cell + gap = 5:1（gap = cell/5）的等式:31·cell + 30·gap = w
      const byW = (w - 30 * gapMin) / (WINDOW_DAYS + (WINDOW_DAYS - 1) / 5)
      const byH = (h - (rows - 1) * gapMin) / (rows + ((rows - 1) / 5))
      const cell = Math.max(8, Math.min(CELL_MAX, byW, byH))
      setCellPx(cell)
    }
    update()
    const ro = new ResizeObserver(update)
    ro.observe(el)
    return () => ro.disconnect()
  }, [visibleRows.length])

  // 共享列模板变量:工具栏 / 矩阵行 / 图表面板三方同源。变量挂在
  // .matrix-stage（本视图与图表面板的共同父级）上——面板是 stage 的直接子元素、
  // 不在 .matrix-view 内,挂 view 上会断链（面板拿到 fallback 值,图表与格子区错位）。
  // cellPx 变化时同步写 stage.style。
  const cellGap = Math.max(2, cellPx / 5)
  const cellsW = WINDOW_DAYS * cellPx + (WINDOW_DAYS - 1) * cellGap
  useEffect(() => {
    const stage = matrixRef.current?.closest('.matrix-stage') as HTMLElement | null
    if (!stage) return
    stage.style.setProperty('--cell-px', `${cellPx.toFixed(2)}px`)
    stage.style.setProperty('--cell-gap-px', `${cellGap.toFixed(2)}px`)
    stage.style.setProperty('--cells-w', `${cellsW.toFixed(2)}px`)
  }, [cellPx, cellGap, cellsW])

  return (
    <div className="matrix-view" ref={matrixRef}>
      <div className="matrix-toolbar">
        <span className="matrix-month">{rangeLabel}{useMock ? t('demoSuffix') : ''}</span>
        <div className="toolbar-groups">
          <div className="toolbar-group">
            {TOKEN_METRICS.map((m) => (
              <button
                key={m}
                className={`seg${metric === m ? ' is-active' : ''}`}
                title={t(METRIC_KEYS[m].hint)}
                onClick={() => setMetric(m)}
              >
                {t(METRIC_KEYS[m].short)}
              </button>
            ))}
          </div>
          <div className="toolbar-group">
            {(['agent', 'model', 'project'] as GroupBy[]).map((g) => (
              <button
                key={g}
                className={`seg${effectiveGroupBy === g ? ' is-active' : ''}`}
                title={g === 'agent' ? t('groupAgentHint') : g === 'model' ? t('groupModelHint') : t('groupProjectHint')}
                onClick={() => setGroupBy(g)}
              >
                {g === 'agent' ? t('groupAgent') : g === 'model' ? t('groupModel') : t('groupProject')}
              </button>
            ))}
          </div>
          <div className="toolbar-group">
            {(['day', 'week', 'cumulative'] as Bucket[]).map((b) => (
              <button
                key={b}
                className={`seg${bucket === b ? ' is-active' : ''}`}
                title={b === 'day' ? t('bucketDayHint') : b === 'week' ? t('bucketWeekHint') : t('bucketCumHint')}
                onClick={() => setBucket(b)}
              >
                {b === 'day' ? t('granDaily') : b === 'week' ? t('granWeekly') : t('bucketCumShort')}
              </button>
            ))}
          </div>
          {loading && <span className="matrix-loading">{t('loading')}</span>}
        </div>
      </div>

      <div className="matrix-body">
        {/* 色阶与排序放在矩阵区右缘竖排图标开关（不占工具栏宽度）,与图表面板图标列
            同风格同右缘。单钮双态,默认态不高亮:色阶默认 = Global / 高亮 = Per row;
            排序默认 = 按 tokens / 高亮 = 按名称;Family 仅 model 视图提供
            （家族聚合对 agent/project 无意义）,叠加在排序之上,默认开启即高亮
            （表示子排序激活）。*/}
        <div className="matrix-side-icons">
          <button
            className={`matrix-panel-icon${scaleMode === 'perRow' ? ' is-active' : ''}`}
            onClick={() => setScaleMode((m) => (m === 'global' ? 'perRow' : 'global'))}
            title={scaleMode === 'global' ? t('scaleGlobal') : t('scalePerRow')}
            aria-pressed={scaleMode === 'perRow'}
          >
            <ScaleIcon />
          </button>
          <button
            className={`matrix-panel-icon${!byTotal ? ' is-active' : ''}`}
            onClick={() => toggleSort('byTotal')}
            title={byTotal ? t('sortByTokens') : t('sortByName')}
            aria-pressed={!byTotal}
          >
            <SortTotalIcon />
          </button>
          {effectiveGroupBy === 'model' && (
            <button
              className={`matrix-panel-icon${viewSort.family ? ' is-active' : ''}`}
              onClick={() => toggleSort('family')}
              title={viewSort.family ? t('familyOn') : t('familyOff')}
              aria-pressed={viewSort.family}
            >
              <FamilyIcon />
            </button>
          )}
        </div>

        <UsageMatrix
          rows={visibleRows}
          dayLabels={dayLabels}
          dates={WINDOW_DATES}
          headerMutedFrom={CURRENT_MONTH_START}
          scaleMode={scaleMode}
          selected={selected}
          selectedRow={effectiveExpanded}
          onSelectCell={(rowKey, day) =>
            // Toggle off: re-clicking the same cell clears selection.
            setSelected((prev) =>
              prev?.rowKey === rowKey && prev.day === day ? null : { rowKey, day }
            )
          }
          onSelectRow={toggleExpand}
          valueUnit={t(METRIC_KEYS[metric].unit)}
        />
      </div>

      {/* 受控模式（面板联动）不渲染内联展开——构成曲线由 FullWindow 的
          MatrixPanel 绘制;仅独立模式保留内联 RowBreakdown。*/}
      {!onRowSelect && effectiveExpanded && (
        <RowBreakdown
          kind={effectiveGroupBy}
          rowKey={effectiveExpanded}
          label={matrixRows.find((r) => r.key === effectiveExpanded)?.label ?? effectiveExpanded}
          month={rangeLabel}
          days={expandedData}
          onClose={() => {
            setInternalExpanded(null)
            setExpandedData(null)
          }}
        />
      )}
    </div>
  )
}

/* 右缘开关图标:12px 线性/实心风格,currentColor,与 MatrixPanel 图标列同尺度。 */
function ScaleIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <rect x="1" y="1" width="4.5" height="4.5" rx="1" fill="currentColor" opacity="0.35" />
      <rect x="6.5" y="1" width="4.5" height="4.5" rx="1" fill="currentColor" opacity="0.6" />
      <rect x="1" y="6.5" width="4.5" height="4.5" rx="1" fill="currentColor" opacity="0.8" />
      <rect x="6.5" y="6.5" width="4.5" height="4.5" rx="1" fill="currentColor" />
    </svg>
  )
}

function SortTotalIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <path d="M1.5 3 H10.5 M1.5 6 H7.5 M1.5 9 H4.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  )
}

function FamilyIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <path d="M1.5 1.5 V5 M1.5 7 V10.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      <path d="M4 2.2 H10.5 M4 4.3 H8 M4 7.7 H10.5 M4 9.8 H7" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
    </svg>
  )
}
