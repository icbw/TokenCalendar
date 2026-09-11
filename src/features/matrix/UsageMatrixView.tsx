// 视图状态与数据装配（自前代项目移植）：usageService 加载真实数据；mock 仅降级。
// 窗口改造：31 列恒定、最右列 = 今天（与挂件「最右列 = 当前周」同语义的
// 天粒度版）——窗口跨月时按需拉多个月份矩阵在前端拼接；hover 显示年月日 +
// tokens·对话数（message_counts 契约扩展）。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import UsageMatrix, { MatrixRow } from './UsageMatrix'
import RowBreakdown from '../breakdown/RowBreakdown'
import { generateMockData } from '../../mock/mockData'
import { events, usageService } from '../../services'
import type { BreakdownDay } from '../../services'
import type { UsageRow } from '../../services/types'
import { getDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import './matrixView.css'

type GroupBy = 'agent' | 'model'
type Bucket = 'day' | 'week' | 'cumulative'
type Metric = 'total' | 'input' | 'output'
/** 排序策略按视图分账：agent/model 各自记忆互不串扰;
 *  'group'（By family,家族聚合排序）仅 model 视图合法。 */
type SortBy = 'monthTotal' | 'name' | 'group'
type ViewSort = Record<GroupBy, SortBy>

const MONTH_ABBR = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

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
const RANGE_LABEL = `${MONTH_ABBR[WINDOW_DATES[0].getMonth()]} ${WINDOW_DATES[0].getDate()} – ${MONTH_ABBR[WINDOW_DATES[WINDOW_DAYS - 1].getMonth()]} ${WINDOW_DATES[WINDOW_DAYS - 1].getDate()}, ${WINDOW_DATES[WINDOW_DAYS - 1].getFullYear()}`

const DAY_LABELS = WINDOW_DATES.map((d) => (d.getDate() === 1 ? MONTH_ABBR[d.getMonth()] : String(d.getDate())))

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
  if (groupBy === 'agent') return key
  const parts = key.split('/')
  if (parts.length >= 5) return `${parts[0]}/${parts[1]} ${parts[3]}`
  return key
}

/** 模型家族键：key 首段即家族——
 *  deepseek-v4-flash/pro → deepseek、gpt-5.6-sol/luna/terra → gpt、
 *  claude-opus-5/sonnet-5 → claude;agent 视图与无分段 key 原样成组。 */
function familyOf(key: string, groupBy: GroupBy): string {
  if (groupBy === 'agent') return key
  return key.split(/[-_/]/)[0] ?? key
}

export default function UsageMatrixView({ groupBy, onGroupByChange, selectedRow, onRowSelect }: {
  /** 9.1 联动：groupBy 由 FullWindow 持有（model 视图下方全系列曲线跟随切换）。
   *  不传时内部自持（保持独立可用）。 */
  groupBy?: GroupBy
  onGroupByChange?: (g: GroupBy) => void
  /** v3.1 行联动:面板选中行由 FullWindow 持有（受控高亮）;不传时内部自持
   *  （独立使用 = 旧行为内联展开）。 */
  selectedRow?: string | null
  /** 点行名上抛（null = 取消/恢复预设）。不传时内部内联展开。 */
  onRowSelect?: (rowKey: string | null) => void
}) {
  const [internalGroupBy, setInternalGroupBy] = useState<GroupBy>('agent')
  const effectiveGroupBy = groupBy ?? internalGroupBy
  const setGroupBy = (g: GroupBy) => {
    if (onGroupByChange) onGroupByChange(g)
    else setInternalGroupBy(g)
  }
  const [bucket, setBucket] = useState<Bucket>('day')
  const [metric, setMetric] = useState<Metric>('total')
  const [sortBy, setSortBy] = useState<ViewSort>({ agent: 'monthTotal', model: 'group' })
  const [scaleMode, setScaleMode] = useState<'global' | 'perRow'>('global')
  const [selected, setSelected] = useState<{ rowKey: string; day: number } | null>(null)
  const [internalExpanded, setInternalExpanded] = useState<string | null>(null)
  const effectiveExpanded = selectedRow !== undefined ? selectedRow : internalExpanded
  const [expandedData, setExpandedData] = useState<BreakdownDay[] | null>(null)
  const [serverRows, setServerRows] = useState<MatrixRow[]>([])
  const [loading, setLoading] = useState(false)
  const [useMock, setUseMock] = useState(false)
  const [refreshTick, setRefreshTick] = useState(0)
  // 矩阵最大行数（设置页 General → Matrix max rows,默认 15）——限制
  // 显示的 agent/model 行数上限,超出部分并入「+N more」摘要行,保证热力图
  // 主体在小窗也完整可见（不出现内部滚动）。
  const [maxRows, setMaxRows] = useState(() => getDesignPrefs().matrixMaxRows)
  useEffect(() => subscribeDesignPrefs((p) => setMaxRows(p.matrixMaxRows)), [])

  // 格子等比缩放（双轴）——见 visibleRows 之后的 effect（依赖行数）。
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
        usageService
          .getMonthlyMatrix({ month: m, groupBy: effectiveGroupBy, metric, bucket: 'day', normalization: scaleMode })
          .then((res) => ({ m, res })),
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
      // 会渲染成透明空白——窗口最右列 = 今天,窗口内
      // 不存在未来日,缺月份 = 该月无记录 = 真实零,补 0 而非 null。
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

  // 行名点击:v3.1 受控模式（onRowSelect）→ 上抛 FullWindow（面板联动,不再内联
  // 展开）;独立模式 → 旧行为内联展开钻取（GetBreakdown 跨月拼 31 天）。
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
        const kind = effectiveGroupBy === 'agent' ? 'agent' : 'model'
        const list = await Promise.all(WINDOW_MONTHS.map((m) => usageService.getBreakdown(kind, rowKey, m)))
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

  // bucket 前端聚合 + 排序。group = 综合版：模型按家族
  // （key 首段）聚合总量,家族间按总量降序;同家族行连续排列,家族内按行总量降序。
  // 排序策略随视图分账:sortBy[groupBy],切换视图各用各的策略互不覆盖。
  const viewSort = sortBy[effectiveGroupBy]
  const matrixRows: MatrixRow[] = useMemo(() => {
    const rows = serverRows.map((r) => ({
      ...r,
      values: applyBucket(r.values, bucket),
      total: bucketTotal(applyBucket(r.values, bucket), bucket),
    }))
    if (effectiveGroupBy === 'model' && viewSort === 'group') {
      const familyTotal = new Map<string, number>()
      for (const r of rows) {
        const fam = familyOf(r.key, effectiveGroupBy)
        familyTotal.set(fam, (familyTotal.get(fam) ?? 0) + r.total)
      }
      rows.sort((a, b) => {
        const fa = familyOf(a.key, effectiveGroupBy)
        const fb = familyOf(b.key, effectiveGroupBy)
        return (
          (familyTotal.get(fb) ?? 0) - (familyTotal.get(fa) ?? 0) ||
          b.total - a.total ||
          a.label.localeCompare(b.label)
        )
      })
    } else {
      rows.sort((a, b) => (viewSort === 'name' ? a.label.localeCompare(b.label) : b.total - a.total))
    }
    return rows
  }, [serverRows, bucket, effectiveGroupBy, viewSort])

  // 行数上限截断（排序在前 = 保留 top N;超出行静默隐藏,上限在设置页调）。
  const visibleRows = useMemo(
    () => (maxRows > 0 ? matrixRows.slice(0, maxRows) : matrixRows),
    [matrixRows, maxRows],
  )

  // 格子等比缩放（双轴）。此前 CSS minmax 只按宽度收缩、高度不跟,缩窗后
  // 行距 > 列距失衡。ResizeObserver 量测矩阵区可用宽高:cell = min（17,
  // 宽预算/31, 高预算/行数)——宽高同一尺寸;gap 与 cell 同步等比（cell:gap =
  // 5:1,与原版 15:3 一致）,行距恒 = 列距。结果写进本区 style 的 CSS 变量,
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

  // v3.7 共享列模板变量:工具栏 / 矩阵行 / 图表面板三方同源。变量挂在
  // .matrix-stage（本视图与图表面板的共同父级）上——面板是 stage 的直接子元素、
  // 不在 .matrix-view 内,挂 view 上会断链（面板拿到 fallback 值,
  // 图表 517px ≠ 格子区 629px 错位）。cellPx 变化时同步写 stage.style。
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
        <span className="matrix-month">{RANGE_LABEL}{useMock ? ' (demo)' : ''}</span>
        <div className="toolbar-groups">
          <div className="toolbar-group">
            {(['total', 'input', 'output'] as Metric[]).map((m) => (
              <button key={m} className={`seg${metric === m ? ' is-active' : ''}`} onClick={() => setMetric(m)}>
                {m === 'total' ? 'Tokens' : m === 'input' ? 'Input' : 'Output'}
              </button>
            ))}
          </div>
          <div className="toolbar-group">
            {(['agent', 'model'] as GroupBy[]).map((g) => (
              <button key={g} className={`seg${effectiveGroupBy === g ? ' is-active' : ''}`} onClick={() => setGroupBy(g)}>
                {g === 'agent' ? 'Agent' : 'Model'}
              </button>
            ))}
          </div>
          <div className="toolbar-group">
            {(['day', 'week', 'cumulative'] as Bucket[]).map((b) => (
              <button key={b} className={`seg${bucket === b ? ' is-active' : ''}`} onClick={() => setBucket(b)}>
                {b === 'day' ? 'Daily' : b === 'week' ? 'Weekly' : 'Cum.'}
              </button>
            ))}
          </div>
          <div className="toolbar-group">
            <button className={`seg${scaleMode === 'global' ? ' is-active' : ''}`} onClick={() => setScaleMode('global')}>Global</button>
            <button className={`seg${scaleMode === 'perRow' ? ' is-active' : ''}`} onClick={() => setScaleMode('perRow')}>Per Row</button>
          </div>
          {/* By family 仅 model 视图提供（家族聚合对 agent 无意义）;排序策略随
              视图分账:切换 agent/model 各用各的记忆,互不覆盖。*/}
          <select
            className="matrix-sort"
            value={viewSort}
            onChange={(e) =>
              setSortBy((prev) => ({ ...prev, [effectiveGroupBy]: e.target.value as SortBy }))
            }
            aria-label="Sort"
          >
            <option value="monthTotal">By total</option>
            {effectiveGroupBy === 'model' && <option value="group">By family</option>}
            <option value="name">By name</option>
          </select>
          {loading && <span className="matrix-loading">Loading…</span>}
        </div>
      </div>

      <UsageMatrix
        rows={visibleRows}
        dayLabels={DAY_LABELS}
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
      />

      {/* 受控模式（面板联动）不渲染内联展开——构成曲线由 FullWindow 的
          MatrixPanel 绘制;仅独立模式保留内联 RowBreakdown。*/}
      {!onRowSelect && effectiveExpanded && (
        <RowBreakdown
          kind={effectiveGroupBy === 'agent' ? 'agent' : 'model'}
          rowKey={effectiveExpanded}
          label={matrixRows.find((r) => r.key === effectiveExpanded)?.label ?? effectiveExpanded}
          month={RANGE_LABEL}
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
