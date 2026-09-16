// TimelineWindow：项目推进时间轴窗口（S1 接线壳 → S2 只读看板）。
// 第四窗口（label=timeline），常驻第二屏的跨项目看板——**独立窗口,不是主窗口
// tab**（主窗口按热力图格子最小不变形尺寸保持形态,任何塞进
// 主窗口的视图都继承该约束;Timeline 尺寸由第二屏决定,与主窗口解耦是需求）。
//
// 装配对齐 WidgetWindow 模式：useShowOnLoad（首帧后由 Rust 按可见性单一源裁决
// show）+ 主题 / 圆角 hook（与挂件同档）+ html.mode-widget 透明类。**不装配材质
// hook**（省一处 DWM 风险面,透明度走 CSS alpha）。
// 独立根类 .timeline-shell,不进 .shell.is-expanded / .is-widget 类名体系
// （GUIDE 红线：颜色变量按窗口拆分,消费 --widget-card-* 派生值但不复用挂件选择器）。
//
// S2 看板：
// - 数据 = get_project_timeline（today−past, today+future) 一条只读命令;挂载查一次,
//   usage:changed 去抖重查,每分钟核对本地日变化（跨零点整窗平移）。
// - 两个视图：**纵向 = 项目管理视图**（项目为列、时间向下,行高随内容,超出看板区可滚动）;
//   横向 = 日程视图（项目为行,每格只放一条 + `+N`,列窄时只显示轮数）。
// - 每格显示哪些会话：过去的日期每天 timelinePastSessions 条（默认 1）,今天
//   timelineTodaySessions 条（默认 5）;选取按 timelinePick（latest 最新 / earliest 最早 / longest
//   tokens 最多）;格内与日期一律按时间从旧到新自上而下,timelineReverse 整体反转。
// - 项目集：pinned（prefs timelinePinnedKeys,原始键经 effective_key 解析）优先,其余按
//   last_day 倒序;显示数 = min（设置上限 timelineMaxProjects, 按最小尺寸算出的容量)。
// - 格子尺寸有范围（timelineConfig）：窗口在范围内拉伸时等分,到最小尺寸后不再缩小
//   （容量减少 / 出滚动条）,超过最大尺寸留白。
// - 项目行 hover 状态卡：常驻 DOM 只切可见性（浮层铁律：透明 WebView2 条件卸载留残影）。
// - 拖动只在 head 区（data-tauri-drag-region="deep" 逐元素挂载）;看板区是交互区。
//
// S3 注意力:get_attention 会话级快照,挂载查一次 + timeline:attention 事件重查
// （Rust 采集线程每轮派生,有变化才发）。按原始目录键 → effective_key 折叠到项目行:任一未确认
// waiting → 亮起（缓慢呼吸点 + 淡底,弱提示不弹窗）;仅未确认 tool_pending → 弱亮（次色静态点）;
// 点击项目行头 = 确认该项目全部未确认等待（同一会话下一段等待自动复位）。running 不提示,只进 hover 卡。
//
// S4 条态:形态单一源在 Rust（get_timeline_form + timeline-form-changed）,前端只发意图
// set_timeline_form——尺寸 / 位置 / 置顶由 Rust 原子执行,这里不补偿位置。条态层常驻 DOM（看板态
// visibility:hidden,仍参与布局）,ResizeObserver 量出条内容的 CSS 宽随意图传给 Rust。条上 = 项目名
// （看板同序取前 maxProjects 个,另补上亮起但不在其中的项目）;亮起项目名呼吸闪烁、可点击确认,
// 其余区域可拖动（Rust 钉顶缘横向滑动）;双击 / 末端按钮展开。看板失焦 timelineAutoStripSecs 秒后
// 自动折条（0 = 关;指针仍在窗口上时不计时）。
// [Focus] 聚焦后续接入。
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type ReactElement } from 'react'
import {
  events,
  projectService,
  timelineService,
  windowService,
  type AttentionItem,
  type ProjectMetaRow,
  type TimelineCell,
  type TimelineProject,
  type TimelineResult,
  type TimelineSession,
} from '../../services'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs, type DesignPrefs } from '../settings/designPrefs'
import { formatCompact, formatFull } from '../matrix/matrixScale'
import { useShowOnLoad } from '../window/useShowOnLoad'
import { useWindowFocus } from '../settings/materialTheme'
import { useWidgetThemeSync } from '../settings/widgetTheme'
import { useRadiusSchemeSync } from '../settings/radiusTheme'
import {
  CELL_NARROW_PX,
  COL_MAX_PX,
  COL_MIN_PX,
  DAY_COL_MIN_PX,
  DAY_HEADER_PX,
  DAY_LABEL_COL_PX,
  EMPTY_DAY_MIN_PX,
  HOVER_DELAY_MS,
  HOVER_GRACE_MS,
  INACTIVE_BADGE_DAYS,
  ITEM_MIN_PX,
  MONTH_ABBR,
  PROJECT_LABEL_COL_PX,
  ROW_MAX_PX,
  ROW_MIN_PX,
  SIDE_ICONS_PX,
  TIMELINE_FUTURE_DAYS,
  TIMELINE_MAX_PROJECTS,
  TIMELINE_PAST_DAYS,
  TIMELINE_PAST_SESSIONS,
  TIMELINE_TODAY_SESSIONS,
  addDays,
  clockLabel,
  dayParts,
  localDay,
  shortDay,
} from './timelineConfig'
import type { TimelineForm } from '../../services/windowService'
import './timeline.css'

type Orientation = 'horizontal' | 'vertical'
type PickRule = 'latest' | 'earliest' | 'longest'

/** ：一格里显示哪几条会话——按规则选出 limit 条,再按时间从旧到新排（reverse = 最新在上）。
 * ：「时间」= 当日最后活动时刻（lastActiveAt）,不是创建时刻——早创建但仍在活跃的会话排在
 * 最新位置,与 Claude app 新消息置顶的原则一致。earliest 仍按首轮开始时刻取「最早的几条」。 */
function pickItems(items: TimelineSession[], rule: PickRule, limit: number, reverse: boolean): TimelineSession[] {
  const sorted = [...items]
  if (rule === 'latest') sorted.sort((a, b) => b.lastActiveAt - a.lastActiveAt)
  else if (rule === 'earliest') sorted.sort((a, b) => a.startedAt - b.startedAt)
  else sorted.sort((a, b) => b.tokens - a.tokens || b.lastActiveAt - a.lastActiveAt)
  const picked = sorted.slice(0, Math.max(1, limit))
  picked.sort((a, b) => (reverse ? b.lastActiveAt - a.lastActiveAt : a.lastActiveAt - b.lastActiveAt))
  return picked
}

/** 会话标签：标题优先,空则回退开始时刻（Tasks 视图 LabelMode='title' 同口径）。 */
function itemLabel(it: TimelineSession): string {
  return it.title?.trim() || clockLabel(it.startedAt)
}

function itemSub(it: TimelineSession): string {
  return `${it.turns} ${it.turns === 1 ? 'turn' : 'turns'} · ${formatCompact(it.tokens)}`
}

function itemTip(it: TimelineSession): string {
  return `${itemLabel(it)}\n${clockLabel(it.startedAt)} – ${clockLabel(it.lastActiveAt)} · ${it.agent}\n${it.turns} turns · ${formatFull(it.tokens)} tokens`
}

/** 日轴表头：日号;首列与每月 1 号带月份缩写。 */
function dayHeadLabel(day: string, first: boolean): string {
  const { m, d } = dayParts(day)
  return first || d === 1 ? `${MONTH_ABBR[m - 1]} ${d}` : String(d)
}

function OrientationIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M2 4.5h7.5M7.5 2l2.5 2.5L7.5 7" />
      <path d="M9.5 12V4.5M12 9.5 9.5 12 7 9.5" />
    </svg>
  )
}

/** 折条：内容收向上缘。 */
function FoldIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M2.5 2.5h9" />
      <path d="M7 12V5.5M4.5 8 7 5.5 9.5 8" />
    </svg>
  )
}

/** 展开：内容自上缘放下。 */
function ExpandIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 14 14" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M2.5 2.5h9" />
      <path d="M7 5.5V12M4.5 9.5 7 12l2.5-2.5" />
    </svg>
  )
}

function PinIcon() {
  return (
    <svg width="11" height="11" viewBox="0 0 12 12" fill="currentColor" aria-hidden="true">
      <path d="M7.5 1 11 4.5 9.6 5.9 8.9 5.2 6.8 7.3l.4 2.4L6 10.9 3.9 8.8 1.5 11.2l-.7-.7 2.4-2.4L1.1 6l1.2-1.2 2.4.4 2.1-2.1-.7-.7L7.5 1Z" />
    </svg>
  )
}

interface HoverState {
  key: string
  left: number
  top: number
}

const HOVER_CARD_W = 240
const HOVER_CARD_H = 150
/** hover 卡每条注意力行的高度（卡片钳位用）。 */
const HOVER_ROW_H = 16
/** hover 卡最多列出的等待会话数。 */
const HOVER_ATTENTION_MAX = 3

type AttentionLevel = 'waiting' | 'pending' | 'running' | null

interface ProjectAttention {
  level: AttentionLevel
  /** 未确认的 waiting / tool_pending（点击确认的对象）。 */
  unacked: AttentionItem[]
  /** 非 running 的全部条目（hover 卡列出,含已确认）。 */
  waiting: AttentionItem[]
  running: number
}

/** 距今时长：<1m / Nm / Nh。 */
function ago(ms: number, now: number): string {
  const m = Math.floor(Math.max(0, now - ms) / 60_000)
  if (m < 1) return '<1m'
  return m < 60 ? `${m}m` : `${Math.floor(m / 60)}h`
}

function itemKey(it: AttentionItem): string {
  return `${it.agent}|${it.sessionId}`
}

interface TimelinePrefs {
  pins: string[]
  orientation: Orientation
  maxProjects: number
  pastDays: number
  futureDays: number
  pastSessions: number
  todaySessions: number
  pick: PickRule
  reverse: boolean
  autoStripSecs: number
}

function readPrefs(p: DesignPrefs): TimelinePrefs {
  return {
    pins: p.timelinePinnedKeys ?? [],
    orientation: p.timelineOrientation ?? 'horizontal',
    maxProjects: p.timelineMaxProjects ?? TIMELINE_MAX_PROJECTS,
    pastDays: p.timelinePastDays ?? TIMELINE_PAST_DAYS,
    futureDays: p.timelineFutureDays ?? TIMELINE_FUTURE_DAYS,
    pastSessions: p.timelinePastSessions ?? TIMELINE_PAST_SESSIONS,
    todaySessions: p.timelineTodaySessions ?? TIMELINE_TODAY_SESSIONS,
    pick: p.timelinePick ?? 'latest',
    reverse: p.timelineReverse ?? false,
    autoStripSecs: p.timelineAutoStripSecs ?? 0,
  }
}

export default function TimelineWindow() {
  useShowOnLoad()
  useWidgetThemeSync()
  useRadiusSchemeSync('widget')

  // 全程透明（透明检查清单）：宿主层不得有不透明默认背景
  useLayoutEffect(() => {
    document.documentElement.classList.add('mode-widget')
  }, [])

  // ---- prefs（置顶 / 方向 / 上限 / 天数）:storage 桥跨窗口同步,主窗口改设置这里即时跟随 ----
  const [prefs, setPrefs] = useState<TimelinePrefs>(() => readPrefs(getDesignPrefs()))
  useEffect(() => subscribeDesignPrefs((p) => setPrefs(readPrefs(p))), [])
  const { pins, orientation, maxProjects, pastDays, futureDays, pastSessions, todaySessions, pick, reverse, autoStripSecs } = prefs

  // ---- S4 形态：Rust 单一源,挂载查一次 + 事件跟随 ----
  const [form, setForm] = useState<TimelineForm>('board')
  useEffect(() => {
    let disposed = false
    let off: (() => void) | null = null
    void windowService.getTimelineForm().then((f) => {
      if (f && !disposed) setForm(f)
    })
    void events.onTimelineFormChanged((f) => setForm(f)).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [])
  const strip = form === 'strip'

  // ---- 数据 ----
  const [today, setToday] = useState(() => localDay())
  const [data, setData] = useState<TimelineResult | null>(null)
  const [meta, setMeta] = useState<ProjectMetaRow[]>([])
  const [failed, setFailed] = useState(false)
  const load = useCallback(
    async (day: string) => {
      const [tl, pm] = await Promise.all([
        timelineService.getProjectTimeline(addDays(day, -pastDays), addDays(day, futureDays)),
        projectService.listProjectMeta(),
      ])
      if (tl) setData(tl)
      setFailed(!tl)
      if (pm) setMeta(pm.rows)
    },
    [pastDays, futureDays],
  )
  useEffect(() => {
    void load(today)
  }, [load, today])
  // usage:changed 去抖重查（与 ProjectManager 同款 300ms）
  useEffect(() => {
    let timer = 0
    let off: (() => void) | null = null
    void events.onUsageChanged(() => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => void load(localDay()), 300)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      window.clearTimeout(timer)
      off?.()
    }
  }, [load])
  // 跨零点：每分钟核对本地日,变了整窗平移重查
  useEffect(() => {
    const id = window.setInterval(() => {
      const now = localDay()
      setToday((prev) => (prev === now ? prev : now))
    }, 60_000)
    return () => window.clearInterval(id)
  }, [])

  // ---- S3 注意力：挂载查一次 + timeline:attention 事件重查 ----
  const [attention, setAttention] = useState<AttentionItem[]>([])
  const loadAttention = useCallback(async () => {
    const items = await timelineService.getAttention()
    if (items) setAttention(items)
  }, [])
  useEffect(() => {
    void loadAttention()
    let off: (() => void) | null = null
    let disposed = false
    void events.onTimelineAttention(() => void loadAttention()).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [loadAttention])

  // ---- 容量：ResizeObserver 量看板区（contentRect 已扣 padding）,按最小尺寸算能放下的项目数 ----
  const bodyRef = useRef<HTMLDivElement | null>(null)
  const [bodySize, setBodySize] = useState({ w: 0, h: 0 })
  useLayoutEffect(() => {
    const el = bodyRef.current
    if (!el) return
    const ro = new ResizeObserver((entries) => {
      const r = entries[0]?.contentRect
      if (r) setBodySize({ w: Math.floor(r.width), h: Math.floor(r.height) })
    })
    ro.observe(el)
    return () => ro.disconnect()
  }, [])
  const horizontal = orientation === 'horizontal'
  const capacity = useMemo(() => {
    const byWindow = horizontal
      ? Math.floor((bodySize.h - DAY_HEADER_PX) / (ROW_MIN_PX + 3))
      : Math.floor((bodySize.w - DAY_LABEL_COL_PX - SIDE_ICONS_PX) / (COL_MIN_PX + 2))
    const cap = Math.max(1, byWindow)
    return maxProjects > 0 ? Math.min(cap, maxProjects) : cap
  }, [horizontal, bodySize, maxProjects])

  // ---- 项目顺序：pinned（原始键 → 有效键）优先,其余按后端 last_day 倒序;裁到容量 ----
  const rawToEff = useMemo(() => {
    const m = new Map<string, string>()
    for (const r of meta) if (r.effectiveKey) m.set(r.key, r.effectiveKey)
    return m
  }, [meta])
  const folderByKey = useMemo(() => {
    const m = new Map<string, boolean>()
    for (const r of meta) m.set(r.key, r.folderExists)
    return m
  }, [meta])
  const pinnedEff = useMemo(() => {
    const out: string[] = []
    for (const k of pins) {
      const eff = rawToEff.get(k) ?? k
      if (!out.includes(eff)) out.push(eff)
    }
    return out
  }, [pins, rawToEff])
  const ordered = useMemo(() => {
    const all = data?.projects ?? []
    const byKey = new Map(all.map((p) => [p.key, p]))
    const pinned = pinnedEff.map((k) => byKey.get(k)).filter((p): p is TimelineProject => !!p)
    const rest = all.filter((p) => !pinnedEff.includes(p.key))
    return [...pinned, ...rest]
  }, [data, pinnedEff])
  const visible = useMemo(() => ordered.slice(0, capacity), [ordered, capacity])
  // 注意力按有效键折叠（原始键经 effective_key 解析,与 pin 同口径）
  const attentionByProject = useMemo(() => {
    const m = new Map<string, ProjectAttention>()
    for (const it of attention) {
      const eff = rawToEff.get(it.projectKey) ?? it.projectKey
      const pa = m.get(eff) ?? { level: null, unacked: [], waiting: [], running: 0 }
      if (it.state === 'running') pa.running += 1
      else {
        pa.waiting.push(it)
        if (!it.acked) pa.unacked.push(it)
      }
      m.set(eff, pa)
    }
    for (const pa of m.values()) {
      if (pa.unacked.some((i) => i.state === 'waiting')) pa.level = 'waiting'
      else if (pa.unacked.length > 0) pa.level = 'pending'
      else if (pa.running > 0) pa.level = 'running'
    }
    return m
  }, [attention, rawToEff])
  const ackProject = (key: string) => {
    const pa = attentionByProject.get(key)
    if (!pa || pa.unacked.length === 0) return
    // 乐观更新：本地先熄灭,Rust 确认后事件重查兜底
    const acked = new Set(pa.unacked.map(itemKey))
    setAttention((prev) => prev.map((i) => (acked.has(itemKey(i)) ? { ...i, acked: true } : i)))
    for (const it of pa.unacked) void timelineService.ackAttention(it.agent, it.sessionId)
  }
  const isPinned = useCallback((key: string) => pinnedEff.includes(key), [pinnedEff])
  const togglePin = (key: string) => {
    const cur = getDesignPrefs().timelinePinnedKeys ?? []
    // 取消：把所有解析到该有效键的原始键一并移除（merge 后的旧 pin 也能取消）
    const next = isPinned(key) ? cur.filter((k) => (rawToEff.get(k) ?? k) !== key) : [...cur, key]
    setDesignPrefs({ timelinePinnedKeys: next })
  }
  const toggleOrientation = () => setDesignPrefs({ timelineOrientation: horizontal ? 'vertical' : 'horizontal' })

  // ---- S4 条态：项目名列表（看板同序取前 maxProjects 个 + 补上亮起的）/ 量宽 / 折叠 ----
  const stripProjects = useMemo(() => {
    const head = maxProjects > 0 ? ordered.slice(0, maxProjects) : ordered
    const lit = ordered.filter((p) => {
      const lv = attentionByProject.get(p.key)?.level
      return (lv === 'waiting' || lv === 'pending') && !head.includes(p)
    })
    return [...head, ...lit]
  }, [ordered, maxProjects, attentionByProject])
  const stripInnerRef = useRef<HTMLDivElement | null>(null)
  const stripWidth = useRef(0)
  const sentWidth = useRef(0)
  const formRef = useRef(form)
  formRef.current = form
  const foldToStrip = useCallback(() => {
    sentWidth.current = stripWidth.current
    void windowService.setTimelineForm('strip', stripWidth.current || undefined)
  }, [])
  const expandToBoard = useCallback(() => {
    void windowService.setTimelineForm('board')
  }, [])
  useLayoutEffect(() => {
    const el = stripInnerRef.current
    if (!el) return
    const ro = new ResizeObserver(() => {
      // + 左右 1px 边框（inner 绝对定位在 padding box 内,窗口宽要含边框）
      const w = Math.ceil(el.getBoundingClientRect().width) + 2
      stripWidth.current = w
      // 条态下内容变宽 / 变窄（项目增减、亮起补位、字体加载）→ 重发意图,Rust 保持左缘重施
      if (formRef.current === 'strip' && w > 0 && Math.abs(w - sentWidth.current) >= 1) {
        sentWidth.current = w
        void windowService.setTimelineForm('strip', w)
      }
    })
    ro.observe(el)
    return () => ro.disconnect()
  }, [])
  // 切入条态时补发一次当前宽度（启动恢复条态 / 事件驱动的折条,Rust 手里可能是旧宽）
  useEffect(() => {
    if (strip && stripWidth.current > 0 && stripWidth.current !== sentWidth.current) {
      sentWidth.current = stripWidth.current
      void windowService.setTimelineForm('strip', stripWidth.current)
    }
  }, [strip])

  // 焦点丢失自动折：只在看板态「获过焦点后失焦」计时;指针仍在窗口上不计时
  const focused = useWindowFocus()
  const everFocused = useRef(false)
  if (focused) everFocused.current = true
  const [pointerInside, setPointerInside] = useState(false)
  // 托盘隐藏看板也会失焦——隐藏期间不计时,否则下次显示莫名成了条
  const [shown, setShown] = useState(true)
  useEffect(() => {
    let disposed = false
    let off: (() => void) | null = null
    void events.onTimelineVisibilityChanged((v) => setShown(v)).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [])
  useEffect(() => {
    if (strip || !shown || focused || pointerInside || autoStripSecs <= 0 || !everFocused.current) return
    const id = window.setTimeout(() => {
      everFocused.current = false
      foldToStrip()
    }, autoStripSecs * 1000)
    return () => window.clearTimeout(id)
  }, [strip, shown, focused, pointerInside, autoStripSecs, foldToStrip])

  // ---- 格子索引 / 热力 / 紧凑档 ----
  const cellIndex = useMemo(() => {
    const m = new Map<string, Map<string, TimelineCell>>()
    for (const p of visible) m.set(p.key, new Map(p.cells.map((c) => [c.day, c])))
    return m
  }, [visible])
  // 日期轴：默认旧在上、新在下（今天与未来在底部）;reverse 整体反转（两种视图同口径）
  const days = useMemo(() => (reverse ? [...(data?.days ?? [])].reverse() : data?.days ?? []), [data, reverse])
  const todayKey = data?.today ?? today
  // 热力底：会话 tokens 相对可见范围最大值开方归一（小格子也看得）,0.14〜0.5
  const maxTokens = useMemo(
    () => visible.reduce((m, p) => p.cells.reduce((mm, c) => c.items.reduce((mmm, it) => Math.max(mmm, it.tokens), mm), m), 0),
    [visible],
  )
  const heatOf = (tokens: number) => (maxTokens > 0 ? 0.14 + 0.36 * Math.sqrt(Math.max(0, tokens) / maxTokens) : 0.14)
  // 横向日列宽（模板 minmax（DAY_COL_MIN, 1fr) 等分）不够 72px 时只显示轮数
  const dayCount = Math.max(1, days.length)
  const cellW = horizontal ? Math.max(DAY_COL_MIN_PX, (bodySize.w - PROJECT_LABEL_COL_PX - 2 * dayCount) / dayCount) : COL_MIN_PX
  const narrow = horizontal && cellW < CELL_NARROW_PX

  // ---- hover 状态卡（常驻 DOM,延迟出、宽限收） ----
  const [hover, setHover] = useState<HoverState | null>(null)
  const hoverTimer = useRef(0)
  const clearHoverTimer = () => window.clearTimeout(hoverTimer.current)
  const onProjectEnter = (key: string, el: HTMLElement) => {
    clearHoverTimer()
    hoverTimer.current = window.setTimeout(() => {
      const body = bodyRef.current
      if (!body) return
      const b = body.getBoundingClientRect()
      const r = el.getBoundingClientRect()
      let left = r.left - b.left + (horizontal ? 8 : 0)
      let top = r.bottom - b.top + 4
      const rows = Math.min(HOVER_ATTENTION_MAX, attentionByProject.get(key)?.waiting.length ?? 0) + 1
      left = Math.max(4, Math.min(left, b.width - HOVER_CARD_W - 4))
      top = Math.max(4, Math.min(top, b.height - HOVER_CARD_H - rows * HOVER_ROW_H - 4))
      setHover({ key, left, top })
    }, HOVER_DELAY_MS)
  }
  const onProjectLeave = () => {
    clearHoverTimer()
    hoverTimer.current = window.setTimeout(() => setHover(null), HOVER_GRACE_MS)
  }
  const onCardEnter = () => clearHoverTimer()
  useEffect(() => () => window.clearTimeout(hoverTimer.current), [])
  const hoverProject = hover ? visible.find((p) => p.key === hover.key) ?? null : null
  const hoverTodayCell = hoverProject ? cellIndex.get(hoverProject.key)?.get(todayKey) ?? null : null
  const hoverWindowSessions = hoverProject ? hoverProject.cells.reduce((s, c) => s + c.sessions, 0) : 0
  const hoverFolder = hoverProject ? folderByKey.get(hoverProject.key) ?? false : false
  const hoverAttention = hoverProject ? attentionByProject.get(hoverProject.key) ?? null : null
  const hoverStatus = !hoverAttention
    ? 'Idle'
    : [
        hoverAttention.waiting.length > 0 ? `${hoverAttention.waiting.length} waiting` : '',
        hoverAttention.running > 0 ? `${hoverAttention.running} running` : '',
      ]
        .filter(Boolean)
        .join(' · ') || 'Idle'
  const nowMs = Date.now()
  const openFolder = () => {
    if (hoverProject && hoverFolder) void projectService.openProjectFolder(hoverProject.key)
  }

  // ---- 渲染 ----
  // 横向：日列 minmax（DAY_COL_MIN, 1fr) 等分、项目行 minmax（ROW_MIN, ROW_MAX);
  // 纵向：项目列 minmax（COL_MIN, COL_MAX)、日行随内容（auto）,看板区可滚动。
  const gridStyle: CSSProperties = horizontal
    ? {
        gridTemplateColumns: `${PROJECT_LABEL_COL_PX}px repeat(${days.length}, minmax(${DAY_COL_MIN_PX}px, 1fr))`,
        gridTemplateRows: `${DAY_HEADER_PX}px repeat(${visible.length}, minmax(${ROW_MIN_PX}px, ${ROW_MAX_PX}px))`,
      }
    : {
        gridTemplateColumns: `${DAY_LABEL_COL_PX}px repeat(${visible.length}, minmax(${COL_MIN_PX}px, ${COL_MAX_PX}px))`,
        gridTemplateRows: `${DAY_HEADER_PX * 2}px repeat(${days.length}, auto)`,
      }
  const dayClass = (day: string) => (day === todayKey ? 'is-today' : day > todayKey ? 'is-future' : 'is-past')

  const projectHead = (p: TimelineProject) => {
    const badge = p.inactiveDays != null && p.inactiveDays >= INACTIVE_BADGE_DAYS ? `${p.inactiveDays}d` : null
    const pinned = isPinned(p.key)
    const level = attentionByProject.get(p.key)?.level ?? null
    const lit = level === 'waiting' || level === 'pending'
    const tip =
      level === 'waiting'
        ? `${p.key}\nAn agent is waiting for your reply (click to dismiss)`
        : level === 'pending'
          ? `${p.key}\nA tool call has been pending for a while, maybe an approval (click to dismiss)`
          : p.key
    return (
      <div
        key={`h:${p.key}`}
        className={`tl-proj${pinned ? ' is-pinned' : ''}${lit ? ` is-${level}` : ''}`}
        title={tip}
        onMouseEnter={(e) => onProjectEnter(p.key, e.currentTarget)}
        onMouseLeave={onProjectLeave}
        onClick={lit ? () => ackProject(p.key) : undefined}
      >
        {lit && <span className="tl-dot" aria-label={level === 'waiting' ? 'Waiting for reply' : 'Tool pending'} />}
        <span className="tl-proj-label">{p.label}</span>
        {badge && (
          <span className="tl-badge" title={`No activity for ${p.inactiveDays} days`}>
            ⚠ {badge}
          </span>
        )}
        <button
          type="button"
          className={`tl-pin${pinned ? ' is-active' : ''}`}
          title={pinned ? 'Unpin from the top' : 'Pin to the top'}
          aria-pressed={pinned}
          onClick={(e) => {
            e.stopPropagation()
            togglePin(p.key)
          }}
        >
          <PinIcon />
        </button>
      </div>
    )
  }

  const itemNode = (it: TimelineSession, extra: number) => (
    <div key={`${it.agent}|${it.sessionId}`} className="tl-item" title={itemTip(it)} style={{ '--tl-heat': heatOf(it.tokens).toFixed(3) } as CSSProperties}>
      <span className="tl-item-title">{narrow ? formatFull(it.turns) : itemLabel(it)}</span>
      <span className="tl-item-sub">
        {itemSub(it)}
        {extra > 0 ? ` +${extra}` : ''}
      </span>
    </div>
  )

  const cellNode = (p: TimelineProject, day: string) => {
    const c = cellIndex.get(p.key)?.get(day)
    const cls = `tl-cell ${dayClass(day)}${c && c.items.length ? '' : ' is-empty'}`
    if (!c || c.items.length === 0) return <div key={`${p.key}|${day}`} className={cls} style={{ minHeight: horizontal ? undefined : EMPTY_DAY_MIN_PX }} />
    // 过去每天 pastSessions 条、今天 todaySessions 条,按 pick 规则选、按时间旧→新排;
    // 横向（日程视图）行高有上限,只放选出的第一条 + `+N`
    const limit = day === todayKey ? todaySessions : pastSessions
    const chosen = pickItems(c.items, pick, limit, reverse)
    const items = horizontal ? chosen.slice(0, 1) : chosen
    const extra = c.items.length - items.length
    return (
      <div key={`${p.key}|${day}`} className={cls} style={{ minHeight: horizontal ? undefined : ITEM_MIN_PX * items.length }}>
        {items.map((it, i) => itemNode(it, i === items.length - 1 ? extra : 0))}
      </div>
    )
  }

  const dayHead = (day: string, i: number) => (
    <div key={`d:${day}`} className={`tl-day ${dayClass(day)}`} title={shortDay(day)}>
      {dayHeadLabel(day, i === 0 || i === days.length - 1)}
    </div>
  )

  const grid: ReactElement[] = [<div key="corner" className="tl-corner" />]
  if (horizontal) {
    days.forEach((day, i) => grid.push(dayHead(day, i)))
    for (const p of visible) {
      grid.push(projectHead(p))
      for (const day of days) grid.push(cellNode(p, day))
    }
  } else {
    for (const p of visible) grid.push(projectHead(p))
    days.forEach((day, i) => {
      grid.push(dayHead(day, i))
      for (const p of visible) grid.push(cellNode(p, day))
    })
  }

  const empty = data !== null && ordered.length === 0
  const hiddenCount = ordered.length - visible.length

  return (
    <div className={`timeline-shell${strip ? ' is-strip' : ' is-board'}`} onMouseEnter={() => setPointerInside(true)} onMouseLeave={() => setPointerInside(false)}>
      <div className="timeline-card" aria-hidden={strip}>
        <div className="timeline-head" data-tauri-drag-region="deep">
          <span className="timeline-title" data-tauri-drag-region="deep">
            Timeline
          </span>
          <span className="timeline-hint" data-tauri-drag-region="deep">
            {failed ? 'Service not running' : `${shortDay(addDays(todayKey, -pastDays))} – ${shortDay(addDays(todayKey, futureDays))}`}
          </span>
          {hiddenCount > 0 && (
            <span className="timeline-hint is-right" data-tauri-drag-region="deep" title="More projects than the window or the project limit allows (Settings · General · Timeline)">
              +{hiddenCount} more
            </span>
          )}
          <button type="button" className={`tl-icon tl-fold${hiddenCount > 0 ? '' : ' is-right'}`} onClick={foldToStrip} title="Fold into a strip at the top of the screen (double-click the strip to expand)">
            <FoldIcon />
          </button>
        </div>
        <div className={`timeline-body${horizontal ? ' is-horizontal' : ' is-vertical'}`} ref={bodyRef}>
          {empty ? (
            <div className="tl-empty">No project activity yet</div>
          ) : (
            <div className={`tl-grid${narrow ? ' is-narrow' : ''}`} style={gridStyle}>
              {grid}
            </div>
          )}
          {/* 右缘图标开关（与矩阵右缘开关同一视觉）:默认横向不高亮,纵向高亮*/}
          <div className="tl-side-icons">
            <button
              type="button"
              className={`tl-icon${horizontal ? '' : ' is-active'}`}
              onClick={toggleOrientation}
              title={horizontal ? 'Schedule view: days across (click for project view)' : 'Project view: projects across, days down (click for schedule view)'}
              aria-pressed={!horizontal}
            >
              <OrientationIcon />
            </button>
          </div>
          {/* hover 状态卡：常驻 DOM,只切可见性*/}
          <div
            className={`tl-hover${hover && hoverProject ? ' is-visible' : ''}`}
            style={{ left: hover?.left ?? 0, top: hover?.top ?? 0 }}
            onMouseEnter={onCardEnter}
            onMouseLeave={onProjectLeave}
            aria-hidden={!hoverProject}
          >
            {hoverProject && (
              <>
                <div className="tl-hover-title" title={hoverProject.key}>
                  {hoverProject.label}
                </div>
                <div className="tl-hover-row">
                  <span>Status</span>
                  <span className={hoverAttention && hoverAttention.unacked.length > 0 ? 'tl-hover-accent' : undefined}>{hoverStatus}</span>
                </div>
                {hoverAttention?.waiting.slice(0, HOVER_ATTENTION_MAX).map((it) => {
                  const label = `${it.title?.trim() || clockLabel(it.since)} · ${it.agentLabel}`
                  return (
                    <div key={itemKey(it)} className={`tl-hover-row tl-hover-session${it.acked ? ' is-acked' : ''}`}>
                      <span>
                        {it.state === 'waiting' ? 'Reply' : 'Tool'} · {ago(it.since, nowMs)}
                      </span>
                      <span title={label}>{label}</span>
                    </div>
                  )
                })}
                <div className="tl-hover-row">
                  <span>Today</span>
                  <span>{hoverTodayCell ? `${hoverTodayCell.turns} turns · ${formatCompact(hoverTodayCell.tokens)}` : 'No activity'}</span>
                </div>
                <div className="tl-hover-row">
                  <span>Last day</span>
                  <span>
                    {shortDay(hoverProject.lastDay)}
                    {hoverProject.inactiveDays != null && hoverProject.inactiveDays > 0 ? ` (${hoverProject.inactiveDays}d ago)` : ''}
                  </span>
                </div>
                <div className="tl-hover-row">
                  <span>Span</span>
                  <span>{hoverProject.firstDay === hoverProject.lastDay ? shortDay(hoverProject.firstDay) : `${shortDay(hoverProject.firstDay)} – ${shortDay(hoverProject.lastDay)}`}</span>
                </div>
                <div className="tl-hover-row">
                  <span>In window</span>
                  <span>
                    {hoverWindowSessions} {hoverWindowSessions === 1 ? 'session' : 'sessions'} · {hoverProject.cells.length} {hoverProject.cells.length === 1 ? 'day' : 'days'}
                  </span>
                </div>
                <div className="tl-hover-row">
                  <span>Agents</span>
                  <span>{hoverProject.agents.join(', ') || '—'}</span>
                </div>
                <div className="tl-hover-actions">
                  <button type="button" className="tl-btn" disabled={!hoverFolder} title={hoverFolder ? 'Open in File Explorer' : 'Folder not found on this machine'} onClick={openFolder}>
                    Open Folder
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      </div>
      {/* S4 条态层：常驻 DOM（看板态 visibility:hidden 仍参与布局,供量宽）。整条可拖动,亮起项目名是按钮
          （按钮天然豁免 drag-region,点击 = 确认）;双击展开（条态不可缩放,tauri 双击最大化不生效）。*/}
      <div className="tl-strip" aria-hidden={!strip} data-tauri-drag-region="deep" onDoubleClick={expandToBoard}>
        <div className="tl-strip-inner" ref={stripInnerRef}>
          {stripProjects.length === 0 && <span className="tl-strip-name is-muted">Timeline</span>}
          {stripProjects.map((p) => {
            const level = attentionByProject.get(p.key)?.level ?? null
            if (level === 'waiting' || level === 'pending') {
              return (
                <button
                  key={p.key}
                  type="button"
                  className={`tl-strip-name is-${level}`}
                  title={`${p.key}\n${level === 'waiting' ? 'An agent is waiting for your reply' : 'A tool call has been pending for a while'} (click to dismiss)`}
                  onClick={() => ackProject(p.key)}
                  onDoubleClick={(e) => e.stopPropagation()}
                >
                  {p.label}
                </button>
              )
            }
            return (
              <span key={p.key} className="tl-strip-name" title={p.key}>
                {p.label}
              </span>
            )
          })}
          <button type="button" className="tl-strip-expand" onClick={expandToBoard} onDoubleClick={(e) => e.stopPropagation()} title="Expand the board">
            <ExpandIcon />
          </button>
        </div>
      </div>
    </div>
  )
}
