// YearMatrix: tokscale-style card widget （自前代项目移植).
// 53 week-columns x 7 day-rows in a rounded card. 形态精：
// - chrome（标题/分段控制/操作按钮）改为 hover 浮层（不占常驻空间，热力图
//   静止时占满卡片，三档面积占比均 ≥ 2/3）；
// - 尺寸预设三档（large/medium/small，单一源 SIZE_PRESETS），切换即重设
//   窗口尺寸（Rust 侧几何落盘兜底）；
// - 格子提示统一用主窗口的 MatrixTooltip 组件与格式（替换原生 title）。
// 取数层：Tauri invoke（services），聚合逻辑保持不变。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { events, inTauri, usageService, windowService } from '../../services'
import { cellVisual } from './matrixScale'
import MatrixTooltip, { type TooltipContent } from './MatrixTooltip'
import {
  getDesignPrefs,
  setDesignPrefs,
  subscribeDesignPrefs,
  SIZE_PRESETS,
  type DesignPrefs,
  type SizePreset,
  type WeekStart,
} from '../settings/designPrefs'
import { applyWidgetTheme } from '../settings/widgetTheme'
import { blurCompensationAlpha, useWindowFocus } from '../settings/materialTheme'
import './yearMatrix.css'

type Granularity = 'daily' | 'weekly' | 'cumulative'

const MONTH_SHORT = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']
const GAP = 4
const CELL_MIN = 3
const CELL_MAX = 18
// Card chrome deduction for cell fitting （must mirror yearMatrix.css):
// clientWidth/Height 已不含 border（1px 每侧），扣减只算 padding——横向
// 8×2=16、纵向 12×2=24（月标签浮层化后不再占布局位，无标签扣减；
//。原实现把 border 双扣 2px，格子比设计值小一档，已正）。
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

/** 网格日期范围：最右列 = 今天所在周（进行中，未来日
 * 以零值灰格显示），列边界整周对齐（列 = 起始日…起始日前一天）；最左列 =
 * 53 周前的列首（≈ 去年同期最近整周）——53 列恒为完整周列，无截断列。
 * 注意 start 必须回退到「列首」weekday：end 是列尾，-6 天才是本周起始日，
 * 再减 52 整周（bug 教训：只减 364 会得到列尾 weekday，首列收在第一天）。 */
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
  const [granularity, setGranularity] = useState<Granularity>('daily')
  const [cells, setCells] = useState<Cell[][]>([]) // 53 columns x 7 rows
  const [loading, setLoading] = useState(true)
  const [cellSize, setCellSize] = useState(12)
  const wrapRef = useRef<HTMLDivElement>(null)

  // 尺寸预设：档位来自 designPrefs（设置抽屉是写入口，本组件是
  // 「应用窗口尺寸」的唯一执行者——抽屉只写 pref，这里监听变化调 set_widget_size）。
  const [sizePreset, setSizePreset] = useState<SizePreset>(() => getDesignPrefs().sizePreset)
  const [locked, setLocked] = useState(() => getDesignPrefs().locked)
  const [weekStart, setWeekStart] = useState<WeekStart>(() => getDesignPrefs().weekStart ?? 'sunday')
  useEffect(
    () =>
      subscribeDesignPrefs((p) => {
        setLocked(p.locked)
        setSizePreset(p.sizePreset)
        setWeekStart(p.weekStart ?? 'sunday')
      }),
    [],
  )

  // 档位变化 → 立即重设窗口尺寸（几何落盘由 Rust Resized 节流持久化兜底）。
  // 挂载时不应用：窗口几何以 window-state.json 恢复为准（尊重手动调整过的尺寸）。
  // snapAnchor=true——停靠顶点有效时切档位以右上角顶点为锚
  // （贴边后切档位锚吸附位置缩放，不锚左上角漂移）。
  const appliedPreset = useRef<SizePreset>(getDesignPrefs().sizePreset)
  useEffect(() => {
    if (appliedPreset.current === sizePreset) return
    appliedPreset.current = sizePreset
    const { w, h } = SIZE_PRESETS[sizePreset]
    windowService.setWidgetSize(w, h, true).catch(console.error)
  }, [sizePreset])

  // Load monthly data covering the grid, build day-value map, then derive cells.
  // 周起始（General 可调）在 load 内即时读取 pref：单一来源，切换档位经
  // weekStart state 触发本 effect 重载（行序与列边界随之整体平移）。
  const load = useCallback(async (g: Granularity) => {
    const weekStart = getDesignPrefs().weekStart ?? 'sunday'
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

    // Build the week grid first （Sunday-first, 53 columns), then aggregate per
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
      // Future days are transparent （no data yet); past days default to 0.
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
      // transparent （null); future weeks stay null too.
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

  // Refresh on usage:changed events.
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
  // 浮层,不占布局位). Deductions must mirror yearMatrix.css padding:
  // 横向 8×2=16、纵向 12×2=24（clientWidth/Height 已不含 border,勿双扣;
  // 正后格子达到设计值 15/12/6)。
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
  const monthLabels = useMemo(() => {
    const labels: (string | null)[] = []
    let prevMonth = -1
    for (const col of cells) {
      const d = col[0]?.date
      if (!d) {
        labels.push(null)
        continue
      }
      // Mid-month columns: the month "changes" on the first day that belongs to
      // the new month, but the label anchors to the column of its first Sunday.
      if (d.getMonth() !== prevMonth) {
        labels.push(MONTH_SHORT[d.getMonth()])
        prevMonth = d.getMonth()
      } else {
        // If the month starts mid-column, GitHub anchors the label to the
        // column containing the 1st; approximate: label when day <= 7 gap.
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

  // Lock state: design pref （localStorage). 语义：锁定 = 只读极简态，
  // 静止时仅热力图（chrome/月标签隐藏），hover 唤出完整 chrome 但仅解锁钮
  // 可点；拖动把手随锁定一并移除（位置固定）。解锁路径 = hover 后点锁钮。
  const toggleLock = useCallback(() => {
    setDesignPrefs({ locked: !getDesignPrefs().locked })
  }, [])

  // 双窗口：展开 = 打开主窗口（两窗口共存，挂件保持显示）。
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
  // background COLOR （rgba var) — NOT element opacity — so title/buttons/labels
  // stay fully opaque and the heatmap cells are never dimmed by the container.
  // Foreground is element opacity on .year-rows only. ：自定义卡片色与
  // 对比度派生由 widgetTheme.applyWidgetTheme 统一处理（同一订阅回调内）。
  // 材质开启且失焦时，卡片底 alpha 抬到补偿起点（host backdrop
  // 失焦退化是系统行为，B 方案 CSS 补偿，materialTheme 单一源），只升不降。
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

  // Cell tooltip （与主窗口同组件同格式，替换原生 title)。
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
      const dateLabel = granularity === 'daily' ? cell.iso : `Week of ${col[0].iso}`
      // 未来格与零值格同款灰色显示，hover 一致：No usage
      const lines =
        cell.value === null || cell.value === 0
          ? ['No usage']
          : [`Tokens: ${cell.value.toLocaleString('en-US')}`]
      setTooltip({ anchor: el, content: { title: `Token activity · ${dateLabel}`, lines } })
    },
    [cells, granularity],
  )

  const dragAttr = locked ? undefined : true

  return (
    <div
      className={`year-matrix${locked ? ' is-locked' : ''}`}
      ref={wrapRef}
      data-tauri-drag-region={dragAttr}
    >
      <div className={`year-card${snapLanded ? ' snap-landed' : ''}`} data-tauri-drag-region={dragAttr}>
        <div className="year-grid" role="grid" aria-label="Year token activity" data-tauri-drag-region={dragAttr}>
          <div className="year-rows" style={{ gap: GAP }} data-tauri-drag-region={dragAttr}>
            {cells.map((col, c) => (
              <div key={c} className="year-col" style={{ gap: GAP }} data-tauri-drag-region={dragAttr}>
                {col.map((cell, r) => {
                  // value === null ⇔ future （transparent); 0 ⇔ real zero →
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
                      aria-label={`${cell.iso}: ${cell.value === null ? 'N/A' : cell.value === 0 ? 'No usage' : cell.value.toLocaleString('en-US')}`}
                      onMouseEnter={(e) => showCellTooltip(c, r, e.currentTarget)}
                      onMouseLeave={hideTooltip}
                    />
                  )
                })}
              </div>
            ))}
          </div>
        </div>

        {/* 月标签浮层：hover 渐变带，不占布局位——
            格子占满卡片，静止态上下边距严格对称。与 chrome 平级定位。*/}
        {SIZE_PRESETS[sizePreset].labels && (
          <div className="year-month-labels" style={{ gap: GAP }}>
            {monthLabels.map((label, i) => (
              <span key={i} className="year-month-label" style={{ width: cellSize }}>
                {label ?? ''}
              </span>
            ))}
          </div>
        )}

        {/* Chrome 浮层：标题/分段控制/操作按钮悬停唤出，平时隐藏，
            不占常驻空间。锁定时仅解锁钮可点。*/}
        <div className="year-chrome" data-tauri-drag-region={dragAttr}>
          <div className="year-header">
            <span className="year-title" data-tauri-drag-region={dragAttr}>
              Token activity
            </span>
            <div className="year-header-right">
              <div className="year-toggle">
                {(['daily', 'weekly', 'cumulative'] as Granularity[]).map((g) => (
                  <button
                    key={g}
                    className={`year-seg${granularity === g ? ' is-active' : ''}`}
                    onClick={() => setGranularity(g)}
                    disabled={loading}
                  >
                    {g === 'daily' ? 'Daily' : g === 'weekly' ? 'Weekly' : 'Cumulative'}
                  </button>
                ))}
              </div>
              <div className="year-actions">
                <button
                  className={`year-action is-lock${locked ? ' is-active' : ''}`}
                  onClick={toggleLock}
                  title={locked ? 'Unlock widget (restore interaction)' : 'Lock widget (minimal read-only form)'}
                  aria-label={locked ? 'Unlock widget' : 'Lock widget'}
                >
                  {locked ? <LockClosedIcon /> : <LockOpenIcon />}
                </button>
                <button className="year-action" onClick={expandFull} title="Open main window" aria-label="Open main window">
                  <ExpandIcon />
                </button>
                <button className="year-action" onClick={resetSize} title="Reset size" aria-label="Reset size">
                  <ResetIcon />
                </button>
              </div>
            </div>
          </div>
        </div>
      </div>

      {/* topReserve = chrome 浮层高度（padding 4 + header 24 + 呼吸 8）：
          第一行格子的 tooltip 翻到下方，不与 hover 弹出的 chrome 重叠
。*/}
      <MatrixTooltip
        content={tooltip?.content ?? null}
        anchor={tooltip?.anchor ?? null}
        topReserve={36}
      />
    </div>
  )
}

/** Lock-open icon （lucide lock-open, 14px): shown while the widget is
 * interactive — click to lock into the minimal read-only form. */
function LockOpenIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <rect width="18" height="11" x="3" y="11" rx="2" ry="2" />
      <path d="M7 11V7a5 5 0 0 1 9.9-1" />
    </svg>
  )
}

/** Lock-closed icon （lucide lock, 14px): shown while locked （active state) —
 * hover 唤出 chrome 后唯一可点元素，点击解锁。 */
function LockClosedIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <rect width="18" height="11" x="3" y="11" rx="2" ry="2" />
      <path d="M7 11V7a5 5 0 0 1 10 0v4" />
    </svg>
  )
}

/** Expand icon: two right angles, bottom-left + top-right （double-corner). */
function ExpandIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
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
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
      <path d="M3 3v5h5" />
    </svg>
  )
}
