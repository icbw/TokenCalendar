// YearMatrix: tokscale-style card widget.
// 53 week-columns x 7 day-rows in a rounded card:
// - chrome（操作按钮）与月标签均为 hover 浮层，不占常驻空间，热力图静止时占满卡片；
// - 尺寸预设三档（单一源 SIZE_PRESETS），切换即重设窗口尺寸（Rust 侧几何落盘兜底）；
// - 格子提示用主窗口的 MatrixTooltip 组件与格式。
// 取数层：Tauri invoke（services）。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { events, inTauri, usageService, windowService } from '../../services'
import { cellVisual, monthShort } from './matrixScale'
import { fmt, useT } from '../../lib/i18n'
import MatrixTooltip, { type TooltipContent } from './MatrixTooltip'
import {
  getDesignPrefs,
  setDesignPrefs,
  subscribeDesignPrefs,
  SIZE_PRESETS,
  weekStartOf,
  type DesignPrefs,
  type SizePreset,
  type WeekStart,
} from '../settings/designPrefs'
import { applyWidgetTheme } from '../settings/widgetTheme'
import { blurCompensationAlpha, useWindowFocus } from '../settings/materialTheme'
import './yearMatrix.css'

type Granularity = 'daily' | 'weekly' | 'cumulative'

/** 粒度单钮循环顺序与显示名字典键（文案在渲染时取）。 */
const GRANULARITY_CYCLE: Granularity[] = ['daily', 'weekly', 'cumulative']
const GRANULARITY_LABEL = {
  daily: 'granDaily',
  weekly: 'granWeekly',
  cumulative: 'granCumulative',
} as const satisfies Record<Granularity, string>
const GAP = 4
const CELL_MIN = 3
const CELL_MAX = 18
// Card chrome deduction for cell fitting （must mirror yearMatrix.css padding):
// clientWidth/Height 已不含 border，扣减只算 padding——横向 8×2=16、纵向 12×2=24
// （月标签是浮层，不占布局位，无标签扣减）。
const CARD_INSET_X = 16
const CARD_INSET_Y = 24

function pad2(n: number): string {
  return String(n).padStart(2, '0')
}

function toISO(d: Date): string {
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`
}

/** Month strings covering the whole grid: from the grid's start month through
 * the current month （52 weeks ≈ 364 days, so 13 months covers it fully). */
function gridMonths(start: Date): string[] {
  const now = new Date()
  const out: string[] = []
  const m = new Date(start.getFullYear(), start.getMonth(), 1)
  while (m <= new Date(now.getFullYear(), now.getMonth(), 1)) {
    out.push(`${m.getFullYear()}-${pad2(m.getMonth() + 1)}`)
    m.setMonth(m.getMonth() + 1)
  }
  return out
}

/** 列尾 weekday：列 = 起始日…起始日前一天（sunday → 周六=6；monday → 周日=0）。 */
function weekEndDow(weekStart: WeekStart): number {
  return weekStart === 'sunday' ? 6 : 0
}

/** 网格日期范围：最右列 = 今天所在周（进行中，未来日以零值灰格显示），列边界整周对齐
 * （列 = 起始日…起始日前一天）；最左列 = 53 周前的列首——53 列恒为完整周列，无截断列。
 * start 必须回退到「列首」weekday：end 是列尾，-6 天才是本周起始日，再减 52 整周
 * （只减 364 会得到列尾 weekday，首列只剩一天）。 */
function gridRange(weekStart: WeekStart): { start: Date; end: Date } {
  const today = new Date()
  const end = new Date(today.getFullYear(), today.getMonth(), today.getDate())
  end.setDate(end.getDate() + ((weekEndDow(weekStart) - end.getDay() + 7) % 7))
  const start = new Date(end)
  start.setDate(start.getDate() - (6 + 52 * 7))
  return { start, end }
}

function aggregateMonthly(rows: { key: string; values: (number | null)[] | null }[]): Map<number, number> {
  const out = new Map<number, number>()
  for (const r of rows) {
    if (!r.values) continue
    r.values.forEach((v, idx) => {
      if (v == null) return
      out.set(idx + 1, (out.get(idx + 1) ?? 0) + v)
    })
  }
  return out
}

interface Cell {
  date: Date
  iso: string
  value: number | null // null = future date
}

interface DemoProfile {
  weight: number
  /** Most active weekdays （0=Sun). */
  days: number[]
}

// Deterministic demo data for running the widget in a plain browser （no Tauri
// backend): mirrors the shape of get_monthly_matrix so layout is testable.
const DEMO_AGENTS: DemoProfile[] = [
  { weight: 1.0, days: [1, 2, 3, 4, 5] },
  { weight: 0.85, days: [1, 2, 3, 4] },
  { weight: 0.6, days: [2, 3, 4] },
  { weight: 0.4, days: [1, 3, 5] },
]

function mulberry32(seed: number): () => number {
  let s = seed | 0
  return () => {
    s = (s + 0x6d2b79f5) | 0
    let t = Math.imul(s ^ (s >>> 15), 1 | s)
    t = Math.imul(t ^ (t >>> 7), 61 | t) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

/** Deterministic demo cell value: weekday activity pattern + weekend dips +
 * ~9% inactive days + occasional spikes （layout-testing fallback only). */
function demoValue(d: Date, rng: () => number): number {
  const active = DEMO_AGENTS.filter((a) => a.days.includes(d.getDay()))
  if (active.length === 0 || rng() < 0.09) return 0
  const base = active.reduce((s, a) => s + a.weight, 0) * 420_000
  const spike = rng() < 0.06 ? 6 : 1
  return Math.floor(base * (0.4 + rng()) * spike)
}

export default function YearMatrix() {
  const t = useT('matrix')
  const [granularity, setGranularity] = useState<Granularity>('daily')
  const [cells, setCells] = useState<Cell[][]>([]) // 53 columns x 7 rows
  const [loading, setLoading] = useState(true)
  const [cellSize, setCellSize] = useState(12)
  const wrapRef = useRef<HTMLDivElement>(null)

  // 尺寸档位来自 designPrefs（设置抽屉是写入口，本组件是「应用窗口尺寸」的唯一执行者——
  // 抽屉只写 pref，这里监听变化调 set_widget_size）。
  const [sizePreset, setSizePreset] = useState<SizePreset>(() => getDesignPrefs().sizePreset)
  const [locked, setLocked] = useState(() => getDesignPrefs().locked)
  const [weekStart, setWeekStart] = useState<WeekStart>(() => weekStartOf(getDesignPrefs()))
  useEffect(
    () =>
      subscribeDesignPrefs((p) => {
        setLocked(p.locked)
        setSizePreset(p.sizePreset)
        setWeekStart(weekStartOf(p))
      }),
    [],
  )

  // 档位变化 → 立即重设窗口尺寸（几何落盘由 Rust Resized 节流持久化兜底）。
  // 挂载时不应用：窗口几何以 window-state.json 恢复为准（尊重手动调整过的尺寸）。
  // snapAnchor=true：停靠顶点有效时切档位以右上角顶点为锚，贴边位置不漂移。
  const appliedPreset = useRef<SizePreset>(getDesignPrefs().sizePreset)
  useEffect(() => {
    if (appliedPreset.current === sizePreset) return
    appliedPreset.current = sizePreset
    const { w, h } = SIZE_PRESETS[sizePreset]
    windowService.setWidgetSize(w, h, true).catch(console.error)
  }, [sizePreset])

  // Load monthly data covering the grid, build day-value map, then derive cells.
  // 周起始在 load 内即时读取 pref（单一来源）；切换经 weekStart state 触发本 effect 重载，
  // 行序与列边界随之整体平移。
  const load = useCallback(async (g: Granularity) => {
    const weekStart = weekStartOf(getDesignPrefs())
    const endDow = weekEndDow(weekStart)
    setLoading(true)
    const { start, end } = gridRange(weekStart)
    const dayMap = new Map<string, number>() // YYYY-MM-DD -> total tokens

    const targets = gridMonths(start)
    const results = await Promise.all(
      targets.map((m) =>
        usageService
          .getMonthlyMatrix({ month: m, groupBy: 'agent', metric: 'total', bucket: 'day', normalization: 'global' })
          .then((res) => ({ month: m, res }))
          .catch(() => ({ month: m, res: null })),
      ),
    )
    // Every call failed ⇒ no Tauri backend （plain-browser layout testing):
    // fall back to deterministic demo data instead of rendering an all-gray grid.
    if (results.every((r) => r.res === null)) {
      const rng = mulberry32(20260901)
      const cursor = new Date(start)
      while (cursor <= end) {
        const v = demoValue(cursor, rng)
        if (v !== null) dayMap.set(toISO(cursor), v)
        cursor.setDate(cursor.getDate() + 1)
      }
    } else {
      for (const { month, res } of results) {
        const daily = aggregateMonthly((res && res.rows) || [])
        const [y, m] = month.split('-').map(Number)
        for (const [day, v] of daily) {
          dayMap.set(`${y}-${pad2(m)}-${pad2(day)}`, v)
        }
      }
    }

    // Build the week grid first （53 columns, 列边界按 weekStart), then aggregate per
    // week so weekly/cumulative granularity shows one value per week column —
    // GitHub contribution style （whole inactive weeks stay gray, not filled).
    const todayISO = toISO(new Date())
    const cols: Cell[][] = []
    const cursor = new Date(start)
    let week: Cell[] = []

    while (cursor <= end) {
      const iso = toISO(cursor)
      const raw = dayMap.get(iso)
      const isFuture = iso > todayISO
      // Future days have no value （null, 显示为零值灰格); past days default to 0.
      const dayValue: number | null = isFuture ? null : raw ?? 0
      week.push({ date: new Date(cursor), iso, value: dayValue })
      if (cursor.getDay() === endDow) {
        cols.push(week)
        week = []
      }
      cursor.setDate(cursor.getDate() + 1)
    }
    if (week.length > 0) cols.push(week)

    if (g !== 'daily') {
      // Weekly: whole week sum （0 if empty). Cumulative: year-to-date running
      // sum across weeks. Days after today inside the current partial week stay
      // null; future weeks stay null too.
      let ytd = 0
      for (const col of cols) {
        const weekRaw = col.reduce<number>((s, c) => s + (c.value ?? 0), 0)
        const pastCount = col.filter((c) => c.iso <= todayISO).length
        let weekValue: number | null
        if (pastCount === 0) {
          weekValue = null
        } else if (g === 'weekly') {
          weekValue = weekRaw
        } else {
          ytd += weekRaw
          weekValue = ytd
        }
        for (const c of col) c.value = c.iso <= todayISO ? weekValue : null
      }
    }
    setCells(cols)
    setLoading(false)
  }, [])

  useEffect(() => {
    load(granularity)
  }, [granularity, load, weekStart])

  useEffect(() => {
    if (!inTauri) return
    let off: (() => void) | null = null
    void events
      .onUsageChanged(() => {
        load(granularity)
      })
      .then((unlisten) => {
        off = unlisten
      })
    return () => {
      off?.()
    }
  }, [granularity, load])

  // Responsive cell size: fills the card interior （chrome 与月标签均为 hover
  // 浮层,不占布局位). Deductions must mirror yearMatrix.css padding
  // （见 CARD_INSET_X/Y；clientWidth/Height 已不含 border,勿双扣)。
  useEffect(() => {
    const el = wrapRef.current
    if (!el) return
    const compute = () => {
      const w = el.clientWidth - CARD_INSET_X
      const h = el.clientHeight - CARD_INSET_Y
      const cellW = (w - 52 * GAP) / 53
      const cellH = (h - 6 * GAP) / 7
      const cell = Math.max(CELL_MIN, Math.min(CELL_MAX, Math.floor(Math.min(cellW, cellH))))
      setCellSize(cell)
    }
    compute()
    const ro = new ResizeObserver(compute)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  // Month labels: show under the column where a new month starts （GitHub style).
  // 存月份下标（数据），文案在渲染时经 t 取——切换语言无需重建。
  const monthLabels = useMemo(() => {
    const labels: (number | null)[] = []
    let prevMonth = -1
    for (const col of cells) {
      const d = col[0]?.date
      if (!d) {
        labels.push(null)
        continue
      }
      // 列首日进入新月份时在该列标注；月份从列中间开始时，标签落在下一列
      // （近似 GitHub 的锚定方式）。
      if (d.getMonth() !== prevMonth) {
        labels.push(d.getMonth())
        prevMonth = d.getMonth()
      } else {
        labels.push(null)
      }
    }
    return labels
  }, [cells])

  const globalCap = useMemo(() => {
    const nz: number[] = []
    for (const col of cells) for (const c of col) if (c.value !== null && c.value > 0) nz.push(c.value)
    if (nz.length === 0) return 0
    nz.sort((a, b) => a - b)
    return nz[Math.min(nz.length - 1, Math.floor(nz.length * 0.95))]
  }, [cells])

  // Lock state: design pref （localStorage). 锁定 = 只读极简态：静止时仅热力图
  // （chrome/月标签隐藏），hover 唤出 chrome 但仅解锁钮可点；拖动随锁定一并禁用。
  const toggleLock = useCallback(() => {
    setDesignPrefs({ locked: !getDesignPrefs().locked })
  }, [])

  // 粒度 = 单钮循环：daily → weekly → cumulative → daily。单钮代替分段控制，
  // 顶部只剩一排图标钮，减少对格子的遮挡；title 显示「当前 → 下一档」。
  const nextGranularity =
    GRANULARITY_CYCLE[(GRANULARITY_CYCLE.indexOf(granularity) + 1) % GRANULARITY_CYCLE.length]
  const cycleGranularity = useCallback(() => {
    setGranularity((g) => GRANULARITY_CYCLE[(GRANULARITY_CYCLE.indexOf(g) + 1) % GRANULARITY_CYCLE.length])
  }, [])

  // 展开 = 打开主窗口（两窗口共存，挂件保持显示）。
  const expandFull = useCallback(() => {
    windowService.showMain().catch(console.error)
  }, [])

  // Restore the current preset's designed size after manual resizes.
  const resetSize = useCallback(() => {
    const { w, h } = SIZE_PRESETS[getDesignPrefs().sizePreset]
    windowService.setWidgetSize(w, h).catch(console.error)
  }, [])

  // Aspect-ratio lock: when the user drags a window edge, snap the height back
  // to the current preset's ratio （guarded against our own SetSize feedback).
  // Honors the "Lock aspect ratio" design preference （default on).
  const lockTimer = useRef<number>(0)
  useEffect(() => {
    if (!inTauri) return
    const onResize = () => {
      if (!getDesignPrefs().lockAspectRatio) return
      window.clearTimeout(lockTimer.current)
      lockTimer.current = window.setTimeout(() => {
        const { w: pw, h: ph } = SIZE_PRESETS[getDesignPrefs().sizePreset]
        const w = window.outerWidth
        const h = window.outerHeight
        if (w < 200 || h < 100) return // minimized / transitional
        const wantH = Math.round((ph / pw) * w)
        if (Math.abs(h - wantH) > 2) {
          windowService.setWidgetSize(w, wantH).catch(() => {})
        }
      }, 120)
    }
    window.addEventListener('resize', onResize)
    return () => {
      window.clearTimeout(lockTimer.current)
      window.removeEventListener('resize', onResize)
    }
  }, [])

  // Foreground/background transparency. Background alpha goes into the card's
  // background COLOR （rgba var) — NOT element opacity — so buttons/labels
  // stay fully opaque and the heatmap cells are never dimmed by the container.
  // Foreground is element opacity on .year-rows only. 自定义卡片色与对比度派生由
  // widgetTheme.applyWidgetTheme 统一处理（同一订阅回调内）。
  // 材质开启且失焦时，卡片底 alpha 抬到补偿起点（host backdrop 失焦退化是系统行为，
  // 以 CSS 补偿，materialTheme 单一源），只升不降。
  const focused = useWindowFocus()
  useEffect(() => {
    const apply = (p: DesignPrefs) => {
      const root = document.documentElement
      root.style.setProperty('--widget-card-alpha', String(blurCompensationAlpha(p, focused) ?? p.bgOpacity))
      root.style.setProperty('--widget-fg-opacity', String(p.fgOpacity))
      applyWidgetTheme(p)
    }
    apply(getDesignPrefs())
    return subscribeDesignPrefs(apply)
  }, [focused])

  // Cell tooltip（与主窗口同组件同格式）。
  const [tooltip, setTooltip] = useState<{ anchor: HTMLElement; content: TooltipContent } | null>(null)
  const hideTooltip = useCallback(() => setTooltip(null), [])

  // 吸附落定动效：Rust 仅在量化位移真变时发 widget-snap-landed；
  // 卡片播放一次 ~150ms 微动效（class 挂上 → animationend 摘除，一次性不循环）。
  const [snapLanded, setSnapLanded] = useState(false)
  useEffect(() => {
    if (!inTauri) return
    let off: (() => void) | null = null
    void events.onWidgetSnapLanded(() => {
      setSnapLanded(false)
      // 双帧重挂保证连续两次落定也能重新播放
      requestAnimationFrame(() => setSnapLanded(true))
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])
  const showCellTooltip = useCallback(
    (c: number, r: number, el: HTMLElement) => {
      const col = cells[c]
      const cell = col?.[r]
      if (!cell) return
      // weekly/cumulative：一格 = 一周聚合，标题标注周起点避免与日期混淆
      const dateLabel = granularity === 'daily' ? cell.iso : t('weekOf', { date: col[0].iso })
      // 未来格与零值格同款灰色显示，hover 文案一致：No usage
      const valueLabel =
        cell.value === null || cell.value === 0
          ? t('noUsage')
          : t('tokensValue', { n: fmt.number(cell.value) })
      // 挂件走紧凑单行（MatrixTooltip compact）：「日期 · 读数」，不带
      // 「Token activity」前缀——两行浮层在矮窗里会压住邻行。
      setTooltip({ anchor: el, content: { title: dateLabel, lines: [valueLabel] } })
    },
    [cells, granularity, t],
  )

  const dragAttr = locked ? undefined : true

  // 热力图格子（useMemo）：hover 换格只重渲染 tooltip，不重排 371 个格子。
  // 收提示只挂在 .year-rows（整片网格）的 onMouseLeave 上：若每格各自收，滑动换格
  // 或穿过格间缝隙时会先隐藏再重绘，闪一帧。入格即换内容，离开整片网格才收。
  const gridRows = useMemo(
    () => (
      <div
        className="year-rows"
        style={{ gap: GAP }}
        data-tauri-drag-region={dragAttr}
        onMouseLeave={hideTooltip}
      >
        {cells.map((col, c) => (
          <div key={c} className="year-col" style={{ gap: GAP }} data-tauri-drag-region={dragAttr}>
            {col.map((cell, r) => {
              // value === null ⇔ future （CSS 默认零值灰); 0 ⇔ real zero →
              // card-tinted gray via opts.zeroBg （pass the 0 through!)
              const vis = cell.value === null
                ? null
                : cellVisual(cell.value, globalCap, { zeroBg: 'var(--widget-zero-bg)' })
              return (
                <div
                  key={r}
                  className={`year-cell${cell.value === null ? ' is-future' : ''}`}
                  style={{
                    width: cellSize,
                    height: cellSize,
                    background: vis ? vis.background : undefined,
                  }}
                  role="gridcell"
                  aria-label={`${cell.iso}: ${cell.value === null ? t('notAvailable') : cell.value === 0 ? t('noUsage') : fmt.number(cell.value)}`}
                  onMouseEnter={(e) => showCellTooltip(c, r, e.currentTarget)}
                />
              )
            })}
          </div>
        ))}
      </div>
    ),
    [cells, cellSize, globalCap, showCellTooltip, dragAttr, hideTooltip, t],
  )

  return (
    <div
      className={`year-matrix${locked ? ' is-locked' : ''}`}
      ref={wrapRef}
      data-tauri-drag-region={dragAttr}
    >
      <div className={`year-card${snapLanded ? ' snap-landed' : ''}`} data-tauri-drag-region={dragAttr}>
        <div className="year-grid" role="grid" aria-label={t('yearGridAria')} data-tauri-drag-region={dragAttr}>
          {gridRows}
        </div>

        {/* 月标签浮层：hover 渐变带，不占布局位——格子占满卡片，静止态上下边距对称。
            与 chrome 平级定位。*/}
        {SIZE_PRESETS[sizePreset].labels && (
          <div className="year-month-labels" style={{ gap: GAP }}>
            {monthLabels.map((label, i) => (
              <span key={i} className="year-month-label" style={{ width: cellSize }}>
                {label === null ? '' : monthShort(t, label)}
              </span>
            ))}
          </div>
        )}

        {/* Chrome 浮层：右上角一排图标钮——粒度循环 / 锁定 / 展开 / 重置。
            浮层不带常驻底色，按钮之外的顶部格子全部可见、可 hover。
            锁定时仅解锁钮可点。悬停文案只留最短标签。*/}
        <div className="year-chrome" data-tauri-drag-region={dragAttr}>
          <div className="year-actions">
            <button
              className="year-action"
              onClick={cycleGranularity}
              disabled={loading}
              title={`${t(GRANULARITY_LABEL[granularity])} → ${t(GRANULARITY_LABEL[nextGranularity])}`}
              aria-label={t('viewAria', { cur: t(GRANULARITY_LABEL[granularity]), next: t(GRANULARITY_LABEL[nextGranularity]) })}
            >
              <SwitchViewIcon />
            </button>
            <button
              className={`year-action is-lock${locked ? ' is-active' : ''}`}
              onClick={toggleLock}
              title={locked ? t('unlock') : t('lock')}
              aria-label={locked ? t('unlockWidget') : t('lockWidget')}
            >
              {locked ? <LockClosedIcon /> : <LockOpenIcon />}
            </button>
            <button className="year-action" onClick={expandFull} title={t('mainWindow')} aria-label={t('openMainWindow')}>
              <ExpandIcon />
            </button>
            <button className="year-action" onClick={resetSize} title={t('resetSize')} aria-label={t('resetSize')}>
              <ResetIcon />
            </button>
          </div>
        </div>
      </div>

      {/* topReserve = chrome 条带底边（= 卡片顶部内边距带 12px）：上方落位不得压到
          hover 唤出的按钮排。compact = 单行变体：挂件窗矮，两行浮层（≈52px）会压住
          邻行；单行（≈24px）时浮层恒落在锚点矩形之外，hover 的那一行任何档位下都不被遮挡。*/}
      <MatrixTooltip
        content={tooltip?.content ?? null}
        anchor={tooltip?.anchor ?? null}
        topReserve={12}
        compact
      />
    </div>
  )
}

/** 全部 chrome 图标：12px / strokeWidth 2.2（12px 下等效 ~1.1px，压在格子上
 * 仍可辨）。12px 是硬约束——按钮高度 = 图标高度 = 卡片顶部内边距带，
 * 整条浮层不压任何格子。 */
const CHROME_ICON = {
  width: 12,
  height: 12,
  viewBox: '0 0 24 24',
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 2.2,
  strokeLinecap: 'round',
  strokeLinejoin: 'round',
  'aria-hidden': true,
} as const

/** 视图切换图标：⇄ 双箭头 = 「切换 / 循环」的通用语汇，比按档位换形状更能读出可点性。
 * 当前档位不由图标承载：热力图形态本身可辨（weekly 整列同色、cumulative 逐列递增），
 * 悬停 title 另有「当前 → 下一档」。 */
function SwitchViewIcon() {
  return (
    <svg {...CHROME_ICON}>
      <path d="M3 8h14" />
      <path d="m13 4 4 4-4 4" />
      <path d="M21 16H7" />
      <path d="m11 12-4 4 4 4" />
    </svg>
  )
}

/** Lock-open icon （lucide lock-open): shown while the widget is interactive
 * — click to lock into the minimal read-only form. */
function LockOpenIcon() {
  return (
    <svg {...CHROME_ICON}>
      <rect width="18" height="11" x="3" y="11" rx="2" ry="2" />
      <path d="M7 11V7a5 5 0 0 1 9.9-1" />
    </svg>
  )
}

/** Lock-closed icon （lucide lock): shown while locked （active state) —
 * hover 唤出 chrome 后唯一可点元素，点击解锁。 */
function LockClosedIcon() {
  return (
    <svg {...CHROME_ICON}>
      <rect width="18" height="11" x="3" y="11" rx="2" ry="2" />
      <path d="M7 11V7a5 5 0 0 1 10 0v4" />
    </svg>
  )
}

/** Expand icon: two right angles, bottom-left + top-right （double-corner). */
function ExpandIcon() {
  return (
    <svg {...CHROME_ICON}>
      <polyline points="15 3 21 3 21 9" />
      <polyline points="9 21 3 21 3 15" />
      <line x1="21" y1="3" x2="14" y2="10" />
      <line x1="3" y1="21" x2="10" y2="14" />
    </svg>
  )
}

/** Reset-size icon （lucide rotate-ccw style). */
function ResetIcon() {
  return (
    <svg {...CHROME_ICON}>
      <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
      <path d="M3 3v5h5" />
    </svg>
  )
}
