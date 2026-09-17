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
// - 视图只有一个：项目管理视图（项目为列、时间向下,行高随内容,超出看板区可滚动）。横向日程视图
//   用户多次后删除（不符合时间线需求,无使用价值）;prefs timelineOrientation 随之退役。
// - 每格显示哪些会话：过去的日期每天 timelinePastSessions 条（默认 1）,今天
//   timelineTodaySessions 条（默认 5）;选取按 timelinePick（latest 最新 / earliest 最早 / longest
//   tokens 最多）;格内与日期一律按时间从旧到新自上而下,timelineReverse 整体反转。
//   以上是格子的「设置状态」;会话数超出时走格内滚动：今天格内
//   上滑翻看当天全部会话,过去日点击展开到 5 条可滚动,格子失焦自动折回设置状态。
// - 项目集：设置·Projects 的 Timeline projects 组（prefs
//   timelinePinnedKeys,原始键经 effective_key 解析,顺序即列序）非空时**只显示组内项目**,全部显示不随窗口
//   拉伸裁掉（放不下横向滚动）,组内项目在时间窗内无活动也保留空列;组为空时回退为按 last_day 倒序、按窗口
//   容量显示最近项目。时间轴窗口不再有 pin 按钮,设置上限 timelineMaxProjects 退役。
// - 格子尺寸有范围（timelineConfig）：窗口在范围内拉伸时等分,到最小尺寸后不再缩小
//   （容量减少 / 出滚动条）,超过最大尺寸留白。
// - 项目行 hover 状态卡：常驻 DOM 只切可见性（浮层铁律：透明 WebView2 条件卸载留残影）。
// - 顶栏：项目名表头独立成一张圆角半透明卡片,与下方面板卡片之间
//   留透明间隙;右侧放折条按钮;去掉 Timeline 标题、日期范围与 `+N more`。顶栏表头与面板是两张同列模板
//   的网格,面板右侧留 BAR_ACTIONS_PX 使两者等宽列对齐,横向滚动由面板同步到顶栏。
//   左侧日期列全透明「悬挂」在面板背景上;面板 / 顶栏 / 会话格三档背景 alpha 进设置。
// - 窗口风格（prefs timelineWindowStyle）：shadow（默认）= 主窗口同款 DWM 阴影 + 外缘 8px 透明呼吸位;
//   flat = 无系统阴影、卡片全出血。边缘组合由 Rust set_timeline_style 施加,条态恒 flat。
// - 拖动只在顶栏（data-tauri-drag-region="deep" 逐元素挂载,可点击的亮起行头除外）;看板区是交互区。
//
// S3 注意力:get_attention 会话级快照,挂载查一次 + timeline:attention 事件重查
// （Rust 采集线程每轮派生,有变化才发）。按原始目录键 → effective_key 折叠到项目行:任一未确认
// waiting → 亮起（缓慢呼吸点 + 淡底,弱提示不弹窗）;仅未确认 tool_pending → 弱亮（次色静态点）;
// 点击项目行头 = 确认该项目全部未确认等待（同一会话下一段等待自动复位）。running 不提示,只进 hover 卡。
//
// S4 条态:形态单一源在 Rust（get_timeline_form + timeline-form-changed）,前端只发意图
// set_timeline_form——尺寸 / 位置 / 置顶由 Rust 原子执行,这里不补偿位置。条态层常驻 DOM（看板态
// visibility:hidden,仍参与布局）,ResizeObserver 量出条内容的 CSS 宽随意图传给 Rust。条上 = 项目名
// （看板同序的监测项目,另补上亮起但不在其中的项目）;亮起项目名呼吸闪烁、可点击确认,
// 其余区域可拖动（Rust 钉顶缘横向滑动）;双击 / 末端按钮展开。看板失焦 timelineAutoStripSecs 秒后
// 自动折条（0 = 关;指针仍在窗口上时不计时）。
//
// 遮挡 / 窥视（时间轴不进任务栏,被遮住就难召回）：看板被全屏 / 最大化的前台窗口
// 覆盖时 Rust 立即折条（遮挡检测线程,见 timeline_form.rs）。条态分两档——完整条态;无操作 PEEK_DELAY_MS
// （指针不在、没有亮起项目）后收成 peek 几像素近透明细边,不挡全屏窗口。指针移入细边 / 出现亮起项目 →
// 回完整条态,方便及时跳转。有亮起项目时不收。
//
// S5 桌面窗口聚焦:亮起的项目行头 / 条上项目名点击 = focus_agent_window（该项目最早的未确认
// waiting,没有则 tool_pending）→ Rust 按 agent（+ host）登记表找进程的可见顶层窗口前置并确认;同项目其余
// 未确认条目随之确认。found=false（agent 已关,条目已被 Rust 移除）→ 该项目本地标记 stale:灰色「上次停在
// 这里」,点击回退打开目录并清标记;项目再次亮起也清标记。
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
import { getDesignPrefs, subscribeDesignPrefs, type DesignPrefs } from '../settings/designPrefs'
import { formatCompact, formatFull } from '../matrix/matrixScale'
import { useShowOnLoad } from '../window/useShowOnLoad'
import { useWindowFocus } from '../settings/materialTheme'
import { useWidgetThemeSync } from '../settings/widgetTheme'
import { useRadiusSchemeSync } from '../settings/radiusTheme'
import {
  COL_MAX_PX,
  COL_MIN_PX,
  BAR_HEAD_PX,
  DAY_LABEL_COL_PX,
  EMPTY_DAY_MIN_PX,
  HOVER_DELAY_MS,
  HOVER_GRACE_MS,
  INACTIVE_BADGE_DAYS,
  ITEM_MIN_PX,
  MONTH_ABBR,
  PEEK_DELAY_MS,
  STRIP_SHADOW_PAD_PX,
  BAR_ACTIONS_PX,
  TIMELINE_BAR_ALPHA,
  TIMELINE_BG_ALPHA,
  TIMELINE_CELL_ALPHA,
  TIMELINE_EXPANDED_SESSIONS,
  TIMELINE_FLAT_HEAT,
  TIMELINE_FUTURE_DAYS,
  TIMELINE_PAST_DAYS,
  TIMELINE_PAST_SESSIONS,
  TIMELINE_TODAY_SESSIONS,
  addDays,
  daysBetween,
  clockLabel,
  dayParts,
  localDay,
  shortDay,
} from './timelineConfig'
import type { TimelineForm } from '../../services/windowService'
import './timeline.css'

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
  return `${itemLabel(it)}\n${clockLabel(it.startedAt)} – ${clockLabel(it.lastActiveAt)} · ${it.agent}\n${it.turns} turns · ${formatFull(it.tokens)} tokens\nDouble-click to open in ${it.agent}`
}

interface SessionCellProps {
  cell: TimelineCell
  cls: string
  /** 设置状态下的可见会话数（今天 timelineTodaySessions / 过去 timelinePastSessions）。 */
  limit: number
  isToday: boolean
  pick: PickRule
  reverse: boolean
  heatOf: (tokens: number) => number
}

/** 有会话的日格。会话数超过设置值时：
 * - 今天：格高固定为设置的条数,格内列出当天全部会话（时间旧→新,最新在下;reverse 反转）,初始停在
 * pick 规则代表的一端（latest → 最新端,earliest → 最早端）,上滑翻看其余会话,到头即止。
 * pick = longest 选出的会话在时间上不连续,今天保持设置状态,与过去日同走点击展开。
 * - 过去日：设置状态显示 pick 选出的几条 + `+N`;点击展开到 TIMELINE_EXPANDED_SESSIONS 条可见并可滚动。
 * - 格子失焦（点到别处 / 窗口失焦 / Esc）折回设置状态,滚动位置回到初始端。
 * 行高（跟随 Windows 文本大小,不硬编码）。
 *
 * 双击会话条 = 跳转到该会话的 agent（open_agent_session：窗口在 → 前置;不在 → 启动宿主;找不到 → 打开目录）。
 * 跳转对象取**第一下按下时**指着的会话：双击的第一下可能先把格子展开,第二下落点已是另一条。 */
function SessionCell({ cell, cls, limit, isToday, pick, reverse, heatOf }: SessionCellProps) {
  const ref = useRef<HTMLDivElement | null>(null)
  const hovered = useRef(false)
  const [expanded, setExpanded] = useState(false)
  const firstDown = useRef<TimelineSession | null>(null)
  const [opening, setOpening] = useState<string | null>(null)
  const openingTimer = useRef(0)
  useEffect(() => () => window.clearTimeout(openingTimer.current), [])
  const openSession = () => {
    const it = firstDown.current
    if (!it || !it.agentKey) return
    // 启动宿主要几秒：会话条短暂描边作为「已受理」的反馈
    setOpening(`${it.agent}|${it.sessionId}`)
    window.clearTimeout(openingTimer.current)
    openingTimer.current = window.setTimeout(() => setOpening(null), OPEN_FEEDBACK_MS)
    void timelineService.openAgentSession(it.agentKey, it.sessionId)
  }
  const overflow = cell.items.length > limit
  const expandedRows = Math.max(limit, TIMELINE_EXPANDED_SESSIONS)
  const rows = expanded ? expandedRows : limit
  const scroll = overflow && (expanded || (isToday && pick !== 'longest'))
  const canExpand = overflow && !expanded && (!scroll || expandedRows > limit)
  // 初始端：最新端在下（reverse 时在上）;earliest 取另一端
  const anchorBottom = (pick !== 'earliest') !== reverse
  const items = useMemo(
    () =>
      scroll
        ? [...cell.items].sort((a, b) => (reverse ? b.lastActiveAt - a.lastActiveAt : a.lastActiveAt - b.lastActiveAt))
        : pickItems(cell.items, pick, limit, reverse),
    [scroll, cell, pick, limit, reverse],
  )
  const extra = scroll ? 0 : cell.items.length - items.length

  // 滚动态格高 = rows 条会话的高度
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const first = scroll ? (el.firstElementChild as HTMLElement | null) : null
    if (!first) {
      el.style.maxHeight = ''
      return
    }
    const gap = parseFloat(getComputedStyle(el).rowGap) || 0
    el.style.maxHeight = `${rows * first.offsetHeight + (rows - 1) * gap}px`
  })
  const anchor = useCallback(() => {
    const el = ref.current
    if (el) el.scrollTop = anchorBottom ? el.scrollHeight : 0
  }, [anchorBottom])
  // 形态变化（展开 / 折回 / 设置改动）必回初始端;数据刷新只在没人翻看时回（不打断正在上滑的用户）
  const modeKey = `${scroll}|${rows}|${anchorBottom}`
  const lastMode = useRef('')
  useLayoutEffect(() => {
    const el = ref.current
    const modeChanged = lastMode.current !== modeKey
    lastMode.current = modeKey
    if (!el || !scroll) return
    if (modeChanged || !(hovered.current || document.activeElement === el)) anchor()
    // 展开后格子变高,底部的格子可能伸出看板可见区
    if (modeChanged && expanded) el.scrollIntoView({ block: 'nearest' })
  }, [modeKey, cell, scroll, expanded, anchor])

  return (
    <div
      ref={ref}
      className={`${cls}${scroll ? ' is-scroll' : ''}${expanded ? ' is-expanded' : ''}${canExpand ? ' is-expandable' : ''}`}
      style={scroll ? undefined : { minHeight: ITEM_MIN_PX * items.length }}
      tabIndex={overflow ? -1 : undefined}
      onClick={canExpand ? () => setExpanded(true) : undefined}
      onDoubleClick={openSession}
      onBlur={() => {
        setExpanded(false)
        if (!hovered.current) anchor()
      }}
      onKeyDown={(e) => {
        if (e.key === 'Escape') ref.current?.blur()
      }}
      onMouseEnter={() => {
        hovered.current = true
      }}
      onMouseLeave={() => {
        hovered.current = false
        if (document.activeElement !== ref.current) anchor()
      }}
    >
      {items.map((it, i) => (
        <div
          key={`${it.agent}|${it.sessionId}`}
          className={`tl-item${opening === `${it.agent}|${it.sessionId}` ? ' is-opening' : ''}`}
          title={itemTip(it)}
          style={{ '--tl-heat': heatOf(it.tokens).toFixed(3) } as CSSProperties}
          onMouseDown={(e) => {
            if (e.detail === 1) firstDown.current = it
          }}
        >
          <span className="tl-item-title">{itemLabel(it)}</span>
          <span className="tl-item-sub">
            {itemSub(it)}
            {extra > 0 && i === items.length - 1 ? ` +${extra}` : ''}
          </span>
        </div>
      ))}
    </div>
  )
}

/** 日轴表头：日号;首列与每月 1 号带月份缩写。 */
function dayHeadLabel(day: string, first: boolean): string {
  const { m, d } = dayParts(day)
  return first || d === 1 ? `${MONTH_ABBR[m - 1]} ${d}` : String(d)
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

interface HoverState {
  key: string
  left: number
  top: number
}

/** 双击跳转后会话条「已受理」描边的持续时间。 */
const OPEN_FEEDBACK_MS = 1200

const HOVER_CARD_W = 240
const HOVER_CARD_H = 150
/** hover 卡每条注意力行的高度（卡片钳位用）。 */
const HOVER_ROW_H = 16
/** hover 卡最多列出的等待会话数。 */
const HOVER_ATTENTION_MAX = 3

type AttentionLevel = 'waiting' | 'pending' | 'running' | null

interface ProjectAttention {
  level: AttentionLevel
  /** 未确认、未暂压的 waiting / tool_pending（亮起与点击确认的对象）。 */
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
  pastDays: number
  futureDays: number
  pastSessions: number
  todaySessions: number
  pick: PickRule
  reverse: boolean
  autoStripSecs: number
  floating: boolean
  heat: boolean
  accent: string | undefined
  bgAlpha: number
  barAlpha: number
  cellAlpha: number
}

function readPrefs(p: DesignPrefs): TimelinePrefs {
  return {
    pins: p.timelinePinnedKeys ?? [],
    pastDays: p.timelinePastDays ?? TIMELINE_PAST_DAYS,
    futureDays: p.timelineFutureDays ?? TIMELINE_FUTURE_DAYS,
    pastSessions: p.timelinePastSessions ?? TIMELINE_PAST_SESSIONS,
    todaySessions: p.timelineTodaySessions ?? TIMELINE_TODAY_SESSIONS,
    pick: p.timelinePick ?? 'latest',
    reverse: p.timelineReverse ?? false,
    autoStripSecs: p.timelineAutoStripSecs ?? 0,
    floating: (p.timelineWindowStyle ?? 'shadow') === 'shadow',
    heat: p.timelineHeat ?? true,
    accent: p.timelineAccent,
    bgAlpha: p.timelineBgAlpha ?? TIMELINE_BG_ALPHA,
    barAlpha: p.timelineBarAlpha ?? TIMELINE_BAR_ALPHA,
    cellAlpha: p.timelineCellAlpha ?? TIMELINE_CELL_ALPHA,
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
  const { pins, pastDays, futureDays, pastSessions, todaySessions, pick, reverse, autoStripSecs, floating, heat, accent, bgAlpha, barAlpha, cellAlpha } = prefs

  // ---- 窗口风格：挂载与 prefs 变化时下发 Rust（边缘组合在 Rust 施加,条态恒 flat） ----
  useEffect(() => {
    void windowService.setTimelineStyle(floating)
  }, [floating])

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
  const peek = form === 'peek'
  const strip = form === 'strip' || peek

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
  const barScrollRef = useRef<HTMLDivElement | null>(null)
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
  const capacity = useMemo(() => {
    const byWindow = Math.floor((bodySize.w - DAY_LABEL_COL_PX) / (COL_MIN_PX + 2))
    return Math.max(1, byWindow)
  }, [bodySize])

  // ---- 项目集：监测组（原始键 → 有效键,组序）;组空回退按后端 last_day 倒序裁到容量 ----
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
  const metaByKey = useMemo(() => new Map(meta.map((r) => [r.key, r])), [meta])
  const pinnedEff = useMemo(() => {
    const out: string[] = []
    for (const k of pins) {
      const row = metaByKey.get(k)
      // 有 meta 行但不可见（隐藏 / 合并目标隐藏）→ 不上时间轴
      if (row && row.effectiveKey === null) continue
      const eff = rawToEff.get(k) ?? k
      if (!out.includes(eff)) out.push(eff)
    }
    return out
  }, [pins, rawToEff, metaByKey])
  const ordered = useMemo(() => data?.projects ?? [], [data])
  const todayForMeta = data?.today ?? today
  const visible = useMemo(() => {
    if (pinnedEff.length === 0) return ordered.slice(0, capacity)
    const byKey = new Map(ordered.map((p) => [p.key, p]))
    // 时间窗内无活动的监测项目：按 meta 补一个空列（标签 / 最近活动日 / 未动天数）
    return pinnedEff.map((k): TimelineProject => {
      const hit = byKey.get(k)
      if (hit) return hit
      const row = metaByKey.get(k)
      return {
        key: k,
        label: row?.label ?? k.split('/').filter(Boolean).pop() ?? k,
        agents: row?.agents ?? [],
        firstDay: row?.firstDay ?? null,
        lastDay: row?.lastDay ?? null,
        inactiveDays: row?.lastDay ? daysBetween(row.lastDay, todayForMeta) : null,
        cells: [],
      }
    })
  }, [pinnedEff, ordered, capacity, metaByKey, todayForMeta])
  // 注意力按有效键折叠（原始键经 effective_key 解析,与 pin 同口径）
  const attentionByProject = useMemo(() => {
    const m = new Map<string, ProjectAttention>()
    for (const it of attention) {
      const eff = rawToEff.get(it.projectKey) ?? it.projectKey
      const pa = m.get(eff) ?? { level: null, unacked: [], waiting: [], running: 0 }
      if (it.state === 'running') pa.running += 1
      else {
        pa.waiting.push(it)
        if (!it.acked && !it.held) pa.unacked.push(it)
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
  // S5:聚焦失败（宿主窗口不存在）的项目,灰色降级直到点击回退或再次亮起
  const [stale, setStale] = useState<Set<string>>(() => new Set())
  useEffect(() => {
    setStale((prev) => {
      if (prev.size === 0) return prev
      const next = new Set([...prev].filter((k) => attentionByProject.get(k)?.level == null || attentionByProject.get(k)?.level === 'running'))
      return next.size === prev.size ? prev : next
    })
  }, [attentionByProject])
  const activateProject = (key: string) => {
    const pa = attentionByProject.get(key)
    if (!pa || pa.unacked.length === 0) return
    // 聚焦对象 = 最早的未确认 waiting（unacked 已按 since 先后）,没有则 tool_pending
    const target = pa.unacked.find((i) => i.state === 'waiting') ?? pa.unacked[0]
    // 乐观更新：本地先熄灭,Rust 确认后事件重查兜底
    const acked = new Set(pa.unacked.map(itemKey))
    setAttention((prev) => prev.map((i) => (acked.has(itemKey(i)) ? { ...i, acked: true } : i)))
    for (const it of pa.unacked) {
      if (it !== target) void timelineService.ackAttention(it.agent, it.sessionId)
    }
    void timelineService.focusAgentWindow(target.agent, target.sessionId).then((found) => {
      if (found === null) {
        // 命令不可用（非 Tauri / IPC 失败）→ 退回 S3 行为:只确认
        void timelineService.ackAttention(target.agent, target.sessionId)
        return
      }
      if (!found) {
        // Rust 已移除该条目（伪等待）;本地同步去掉,项目标 stale
        const gone = itemKey(target)
        setAttention((prev) => prev.filter((i) => itemKey(i) !== gone))
        setStale((prev) => new Set(prev).add(key))
      }
    })
  }
  const openStaleProject = (key: string) => {
    setStale((prev) => {
      const next = new Set(prev)
      next.delete(key)
      return next
    })
    if (folderByKey.get(key)) void projectService.openProjectFolder(key)
  }

  // ---- S4 条态：项目名列表（看板同序的监测项目 + 补上亮起的组外项目）/ 量宽 / 折叠 ----
  const stripProjects = useMemo(() => {
    const inHead = new Set(visible.map((p) => p.key))
    const lit = ordered.filter((p) => {
      const lv = attentionByProject.get(p.key)?.level
      return (lv === 'waiting' || lv === 'pending' || stale.has(p.key)) && !inHead.has(p.key)
    })
    return [...visible, ...lit]
  }, [visible, ordered, attentionByProject, stale])
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
      // + 左右 1px 边框（inner 绝对定位在 padding box 内,窗口宽要含边框）+ 左右阴影透明边
      const w = Math.ceil(el.getBoundingClientRect().width) + 2 + 2 * STRIP_SHADOW_PAD_PX
      stripWidth.current = w
      // 条态下内容变宽 / 变窄（项目增减、亮起补位、字体加载）→ 重发意图,Rust 保持左缘重施
      // 带当前形态名重发（peek 下更新宽度不能把细边撑回完整条）
      const cur = formRef.current
      if ((cur === 'strip' || cur === 'peek') && w > 0 && Math.abs(w - sentWidth.current) >= 1) {
        sentWidth.current = w
        void windowService.setTimelineForm(cur, w)
      }
    })
    ro.observe(el)
    return () => ro.disconnect()
  }, [])
  // 切入条态时补发一次当前宽度（启动恢复条态 / 事件驱动的折条,Rust 手里可能是旧宽）
  useEffect(() => {
    if (strip && stripWidth.current > 0 && stripWidth.current !== sentWidth.current) {
      sentWidth.current = stripWidth.current
      void windowService.setTimelineForm(formRef.current, stripWidth.current)
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

  // ---- 窥视态：完整条态无操作 PEEK_DELAY_MS 收细边;指针移入 / 有亮起项目回完整条 ----
  // 形态切换时窗口在指针下变形,mouseleave 不一定触发:换形态先复位,真在窗口里由 mousemove 补回
  useEffect(() => setPointerInside(false), [form])
  const stripLit = useMemo(
    () => stripProjects.some((p) => {
      const lv = attentionByProject.get(p.key)?.level
      return lv === 'waiting' || lv === 'pending'
    }),
    [stripProjects, attentionByProject],
  )
  useEffect(() => {
    if (form !== 'strip' || !shown || pointerInside || stripLit) return
    const id = window.setTimeout(() => void windowService.setTimelineForm('peek', stripWidth.current || undefined), PEEK_DELAY_MS)
    return () => window.clearTimeout(id)
  }, [form, shown, pointerInside, stripLit])
  useEffect(() => {
    if (peek && stripLit) void windowService.setTimelineForm('strip', stripWidth.current || undefined)
  }, [peek, stripLit])
  const onPointerEnter = () => {
    setPointerInside(true)
    if (formRef.current === 'peek') void windowService.setTimelineForm('strip', stripWidth.current || undefined)
  }

  // ---- 格子索引 / 热力 / 紧凑档 ----
  const cellIndex = useMemo(() => {
    const m = new Map<string, Map<string, TimelineCell>>()
    for (const p of visible) m.set(p.key, new Map(p.cells.map((c) => [c.day, c])))
    return m
  }, [visible])
  // 日期轴：默认旧在上、新在下（今天与未来在底部）;reverse 整体反转（两种视图同口径）
  const days = useMemo(() => (reverse ? [...(data?.days ?? [])].reverse() : data?.days ?? []), [data, reverse])
  const todayKey = data?.today ?? today
  // 热力底：会话 tokens 相对可见范围最大值开方归一（小格子也看得）,0.14〜0.5;
  // 设置关掉用量着色（timelineHeat = false）时所有会话格统一 TIMELINE_FLAT_HEAT
  const maxTokens = useMemo(
    () => visible.reduce((m, p) => p.cells.reduce((mm, c) => c.items.reduce((mmm, it) => Math.max(mmm, it.tokens), mm), m), 0),
    [visible],
  )
  const heatOf = (tokens: number) => (!heat ? TIMELINE_FLAT_HEAT : maxTokens > 0 ? 0.14 + 0.36 * Math.sqrt(Math.max(0, tokens) / maxTokens) : 0.14)

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
      let left = r.left - b.left
      let top = r.bottom - b.top + 4
      const rows = Math.min(HOVER_ATTENTION_MAX, attentionByProject.get(key)?.waiting.length ?? 0) + 1
      left = Math.max(4, Math.min(left, b.width - HOVER_CARD_W - 4))
      top = Math.max(4, Math.min(top, b.height - HOVER_CARD_H - rows * HOVER_ROW_H - 4))
      // 卡片绝对定位在滚动容器内容坐标系：加回滚动偏移（顶栏行头的卡片落在看板区可见区顶部）
      setHover({ key, left: left + body.scrollLeft, top: top + body.scrollTop })
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
  // 项目列 minmax（COL_MIN, COL_MAX)、日行随内容（auto）,面板可滚动。
  // 顶栏表头与面板是两张网格,列模板同一份（两容器等宽 → 列对齐）
  const gridColumns = `${DAY_LABEL_COL_PX}px repeat(${visible.length}, minmax(${COL_MIN_PX}px, ${COL_MAX_PX}px))`
  const gridStyle: CSSProperties = { gridTemplateColumns: gridColumns, gridTemplateRows: `repeat(${days.length}, auto)` }
  const headGridStyle: CSSProperties = { gridTemplateColumns: gridColumns, gridTemplateRows: `${BAR_HEAD_PX}px` }
  const shellStyle = {
    ...(accent ? { '--tl-accent': accent } : {}),
    '--tl-bg-alpha': bgAlpha,
    '--tl-bar-alpha': barAlpha,
    '--tl-cell-alpha': cellAlpha,
    '--tl-actions-w': `${BAR_ACTIONS_PX}px`,
  } as CSSProperties
  // 看板区横向滚动 → 顶栏表头跟随（顶栏 overflow:hidden,只由这里驱动）
  const syncBarScroll = () => {
    const body = bodyRef.current
    const bar = barScrollRef.current
    if (body && bar && bar.scrollLeft !== body.scrollLeft) bar.scrollLeft = body.scrollLeft
  }
  /** 顶栏内的非交互元素挂拖动（deep 只作用 mousedown target,逐元素挂）。 */
  const dragProps = (on: boolean) => (on ? { 'data-tauri-drag-region': 'deep' } : {})
  const dayClass = (day: string) => (day === todayKey ? 'is-today' : day > todayKey ? 'is-future' : 'is-past')

  const projectHead = (p: TimelineProject, inBar: boolean) => {
    const badge = p.inactiveDays != null && p.inactiveDays >= INACTIVE_BADGE_DAYS ? `${p.inactiveDays}d` : null
    const level = attentionByProject.get(p.key)?.level ?? null
    const lit = level === 'waiting' || level === 'pending'
    const isStale = !lit && stale.has(p.key)
    const tip =
      level === 'waiting'
        ? `${p.key}\nAn agent is waiting for your reply (click to bring its window to the front)`
        : level === 'pending'
          ? `${p.key}\nA tool call has been pending for a while, maybe an approval (click to bring its window to the front)`
          : isStale
            ? `${p.key}\nLast stopped here: the agent window is gone (click to open the folder)`
            : p.key
    const drag = dragProps(inBar && !lit && !isStale)
    return (
      <div
        key={`h:${p.key}`}
        {...drag}
        className={`tl-proj${lit ? ` is-${level}` : ''}${isStale ? ' is-stale' : ''}`}
        title={tip}
        onMouseEnter={(e) => onProjectEnter(p.key, e.currentTarget)}
        onMouseLeave={onProjectLeave}
        onClick={lit ? () => activateProject(p.key) : isStale ? () => openStaleProject(p.key) : undefined}
      >
        {(lit || isStale) && <span className="tl-dot" aria-label={level === 'waiting' ? 'Waiting for reply' : level === 'pending' ? 'Tool pending' : 'Last stopped here'} />}
        <span className="tl-proj-label" {...drag}>
          {p.label}
        </span>
        {badge && (
          <span className="tl-badge" {...drag} title={`No activity for ${p.inactiveDays} days`}>
            ⚠ {badge}
          </span>
        )}
      </div>
    )
  }

  const cellNode = (p: TimelineProject, day: string) => {
    const c = cellIndex.get(p.key)?.get(day)
    const cls = `tl-cell ${dayClass(day)}${c && c.items.length ? '' : ' is-empty'}`
    if (!c || c.items.length === 0) return <div key={`${p.key}|${day}`} className={cls} style={{ minHeight: EMPTY_DAY_MIN_PX }} />
    // 过去每天 pastSessions 条、今天 todaySessions 条（设置状态）;超出部分走格内滚动 / 点击展开（SessionCell）
    const isToday = day === todayKey
    return <SessionCell key={`${p.key}|${day}`} cell={c} cls={cls} limit={isToday ? todaySessions : pastSessions} isToday={isToday} pick={pick} reverse={reverse} heatOf={heatOf} />
  }

  const dayHead = (day: string, i: number, inBar: boolean) => (
    <div key={`d:${day}`} className={`tl-day ${dayClass(day)}`} title={shortDay(day)} {...dragProps(inBar)}>
      {dayHeadLabel(day, i === 0 || i === days.length - 1)}
    </div>
  )

  const head: ReactElement[] = [<div key="corner" className="tl-corner" {...dragProps(true)} />]
  const grid: ReactElement[] = []
  for (const p of visible) head.push(projectHead(p, true))
  days.forEach((day, i) => {
    grid.push(dayHead(day, i, false))
    for (const p of visible) grid.push(cellNode(p, day))
  })

  const empty = data !== null && ordered.length === 0

  return (
    <div
      className={`timeline-shell${strip ? ' is-strip' : ' is-board'}${peek ? ' is-peek' : ''}${floating ? ' is-floating' : ' is-flat'}`}
      style={shellStyle}
      onMouseEnter={onPointerEnter}
      onMouseMove={() => !pointerInside && onPointerEnter()}
      onMouseLeave={() => setPointerInside(false)}
    >
      <div className="timeline-card" aria-hidden={strip}>
        {/* 顶栏：表头网格（随看板区横向滚动）+ 右侧按钮;非按钮区域可拖动窗口*/}
        <div className="tl-bar" {...dragProps(true)}>
          <div className="tl-bar-scroll" ref={barScrollRef} {...dragProps(true)}>
            {data !== null && !empty && (
              <div className="tl-grid tl-head-grid" style={headGridStyle} {...dragProps(true)}>
                {head}
              </div>
            )}
          </div>
          <div className="tl-bar-actions" {...dragProps(true)}>
            <button type="button" className="tl-icon" onClick={foldToStrip} title="Fold into a strip at the top of the screen (double-click the strip to expand)">
              <FoldIcon />
            </button>
          </div>
        </div>
        <div className="timeline-body" ref={bodyRef} onScroll={syncBarScroll}>
          {failed && data === null ? (
            <div className="tl-empty">Service not running</div>
          ) : empty ? (
            <div className="tl-empty">No project activity yet</div>
          ) : (
            <div className="tl-grid" style={gridStyle}>
              {grid}
            </div>
          )}
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
                    <div key={itemKey(it)} className={`tl-hover-row tl-hover-session${it.acked || it.held ? ' is-acked' : ''}`}>
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
                  title={`${p.key}\n${level === 'waiting' ? 'An agent is waiting for your reply' : 'A tool call has been pending for a while'} (click to bring its window to the front)`}
                  onClick={() => activateProject(p.key)}
                  onDoubleClick={(e) => e.stopPropagation()}
                >
                  {p.label}
                </button>
              )
            }
            if (stale.has(p.key)) {
              return (
                <button
                  key={p.key}
                  type="button"
                  className="tl-strip-name is-stale"
                  title={`${p.key}\nLast stopped here: the agent window is gone (click to open the folder)`}
                  onClick={() => openStaleProject(p.key)}
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
