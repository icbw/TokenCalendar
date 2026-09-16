// Widget design preferences shared across components （settings drawer writes,
// YearMatrix reads). Module-level store: tiny, no provider needed.
// 发布数据架构：持久化单一源 = 数据根 prefs.json（Rust
// 命令 get_prefs_raw/set_prefs_raw 原子读写）;localStorage 仅作旧数据一次性
// 迁移来源（首启检测 prefs.json 缺失而 localStorage 有 → 迁入后清除旧键）,
// 跨窗口实时互通仍走 'storage' 事件桥（写入方同时写 localStorage 中转键）。

/** 尺寸预设三档。窗口尺寸单一源：SIZE_PRESETS（YearMatrix.tsx），
 * designPrefs 只存档位名；档位切换即 set_widget_size，几何落盘兜底。 */
export type SizePreset = 'large' | 'medium' | 'small'

import type { BoostConfig, SubscriptionPlatform } from '../../services/subscriptionService'

/** 毛玻璃材质档（undefined = 关闭）。两窗口各自独立一个键。
 * 版本门槛：Mica 仅 Win11（22000+，apply 失败自动回退关闭）；Acrylic
 * Win10 1809+（Win11 ≥22523 走 host backdrop，染色参数被忽略）。 */
export type MaterialEffect = 'mica' | 'acrylic'

/** 圆角方案三档（全局圆角统一为主题参数）。
 * 每档拆分主面板 panel 与挂件 widget 两个值（当前同档对齐相等，结构
 * 拆分留独立演化空间）。**材质态窗口圆角由 DWM 裁切（系统 ~8px），不
 * 受本方案控制**——需要与材质态一致请选 small。undefined = large。 */
export type RadiusScheme = 'small' | 'medium' | 'large'

export const RADIUS_PRESETS: Record<RadiusScheme, { panel: number; widget: number }> = {
  small: { panel: 8, widget: 8 },
  medium: { panel: 12, widget: 12 },
  large: { panel: 16, widget: 16 },
}

/** 热力图周起始（General 可调）：行序与列边界都从起始日开始
 * （sunday = 周日在第一行、列为周日~周六；monday 则整体平移）。
 * undefined = sunday（GitHub 式默认）。 */
export type WeekStart = 'sunday' | 'monday'

/** 三档窗口尺寸（单一源，YearMatrix 与 SettingsPage 共用）。
 * 数值由格位精确反推：53 列 × cell（15/12/6) + 52×gap4 网格 + 卡片
 * padding 8×2 + border 2（横向 +38）；纵向 = 7×cell + 6×gap4 + 26
 * （padding 12×2 + border 2；月标签浮层化不再占布局位，
 * 上下边距严格对称，格子尺寸不变）。
 * chrome（header/标签）为 hover 浮层不占常驻空间，三档热力图面积占比
 * ≈81% / 78% / 71%，满足设计约束 ≥ 2/3。 */
export const SIZE_PRESETS: Record<SizePreset, { w: number; h: number; labels: boolean }> = {
  large: { w: 1041, h: 155, labels: true },
  medium: { w: 882, h: 134, labels: true },
  small: { w: 564, h: 92, labels: false },
}

export interface DesignPrefs {
  /** Card background alpha 0.2..1. Applied to the card's background COLOR
   * （rgba) — NOT element opacity — so text/buttons stay fully readable and
   * the heatmap cells are never dimmed by their container. */
  bgOpacity: number
  /** Heatmap cells opacity （element opacity on .year-rows). */
  fgOpacity: number
  /** 挂件卡片背景色（hex）。undefined = 跟随亮/暗 scheme 内置值
   * （恢复默认即删键）；自定义色视为显式意图，覆盖两套 scheme。 */
  widgetCardBg?: string
  /** 主界面顶栏底色（hex）。undefined = 跟随 scheme。
   * 仅主窗口消费（--shell-bg 覆写），与挂件参数零关联。 */
  titlebarBg?: string
  /** 主界面顶栏不透明度 0.2..1（默认 0.8）。仅作用于标题栏
   * alpha；最大化态转实底（alpha 无效化），与挂件 bgOpacity 零关联。 */
  titlebarAlpha?: number
  /** 主界面主体卡片底色（hex）→ --panel 覆写（仅主窗口入口应用）。 */
  panelBg?: string
  /** 主界面边框色（hex）→ --border 覆写（仅主窗口入口应用）。 */
  borderColor?: string
  /** 挂件毛玻璃材质。undefined = 关闭；由挂件窗口自己消费
   * （useMaterialSync（'widget')），设置页只写键经 storage 桥广播。 */
  widgetMaterial?: MaterialEffect
  /** 主窗口毛玻璃材质。undefined = 关闭；生效时 Rust 联动
   * DWM ROUND（CSS 圆角转 DWM 单源），关闭恢复 DONOTROUND。 */
  mainMaterial?: MaterialEffect
  /** 圆角方案档（undefined = large）。主窗口覆写 --radius-card（panel 值），
   * 挂件覆写 --widget-card-radius（widget 值），同档两值对齐。 */
  radiusScheme?: RadiusScheme
  /** 热力图周起始（undefined = sunday）。仅挂件矩阵消费（列边界+行序）。 */
  weekStart?: WeekStart
  /** Keep W:H at the current preset's ratio when the widget is resized. */
  lockAspectRatio: boolean
  /** Locked widget: the card content ignores the mouse （read-only minimal
   * form); hovering calls the chrome back and the unlock button stays
   * clickable. Replaces the old always-on-top pin — the widget is a
   * glanceable desktop calendar. */
  locked: boolean
  /** Widget window size preset （large/medium/small); switching applies the
   * preset's window size immediately via set_widget_size. */
  sizePreset: SizePreset
  /** v3.1:CodeBuddy 积分卡显隐。默认关——没用过 CodeBuddy 的用户
   * 不应看到常驻空引导卡;开启后 chart 视图尾部渲染该卡。 */
  insightsCredit: boolean
  /** v3.2:主窗口矩阵最大行数（同时限制 agent/model 视图行数,
   * 超出截断为「+N more」摘要行）。默认 15;0 = 不限。热力图是主体,行数
   * 上限保证小窗下热力图完整可见、不被图表面板挤压。 */
  matrixMaxRows: number
  /** ：订阅轮询间隔（秒）。默认 300（5 分钟）;设置页下拉 5/10/15/30 分钟。
   * 持久化在此键,运行时值经 set_subscription_poll_secs 下发（窗口装载时恢复）。 */
  subscriptionPollSecs?: number
  /** boost 监控：悬浮球订阅 boost 补充路由（Rust
   * subscription/boost.rs）。总开关**默认关**（提频联网属显式授权,与 autoUpdate
   * 同口径）;两触发条件 OR——spike（相邻主轮询 used_5h 差 ≥ 阈值,缺省 = 开）
   * 与 low（5h 剩余 ≤ 阈值,缺省 = 关）;boost 间隔默认 60s（可 120/180/自定义）。
   * 运行时经 set_subscription_boost 下发,orb 窗口装载时恢复;boost 状态本身
   * 不持久化（重启按主快照重评）。 */
  orbBoostEnabled?: boolean
  orbBoostSpikeEnabled?: boolean
  orbBoostSpikePct?: number
  orbBoostLowEnabled?: boolean
  orbBoostLowPct?: number
  orbBoostIntervalSecs?: number
  /** 待机监控：主轮询自适应退档（Rust subscription/idle.rs）。
   * 读数无变化逐档放慢（封顶 30 分钟）,任何变化立即回设置档;待机中悬浮球整体
   * 减淡 50%。**默认开**（undefined = 开）——退档是收敛行为,与 boost「提频需
   * 显式授权」的口径相反。运行时经 set_subscription_idle_enabled 下发,orb 窗口
   * 装载时恢复;档位状态本身不持久化（重启回基础档重评）。 */
  orbIdleEnabled?: boolean
  /** 悬浮球上次显示的订阅平台（启动恢复上次的平台,而非固定第一个）。
   * 按平台 id 记（绑定集合变化时下标会错位）;该平台未绑定 → 回落第一个已绑定平台,
   * 键保持不动直到用户再切换。undefined = 第一个。 */
  orbPlatform?: SubscriptionPlatform
  /** 应用更新：启动后自动检查并安装新版本（**默认关**——联网自动装包属显式
   * 授权行为,由用户在设置·About 主动开启）。关闭时主窗口加载不自动检查,
   * 仅「Check for updates」手动触发。更新源与签名校验
   * services/updateService.ts（仅安装版生效）。 */
  autoUpdate: boolean
  /** 离开阈值（分钟,1〜1440;undefined = 30）:轮间空档 ≤ 阈值才计入「人工时间」。
   * **写入方是 Rust `set_idle_threshold`**（合并写 prefs.json 并同步重算 daily_project）;
   * 前端改阈值时须同时 setDesignPrefs 本键,否则 persist 的旧快照会把它覆盖回去（S4 接线）。 */
  idleThresholdMin?: number
  /** 采集频率（秒,30 / 60 / 120 / 180 / 300;undefined = 30）。**写入方是 Rust `set_collect_interval`**
   * （合并写 prefs.json 并下发运行时值）;前端改档时须同时 setDesignPrefs 本键,否则 persist 的旧快照会覆盖回去。 */
  collectIntervalSecs?: number
  /** Tasks 列表标签:time（开始时间,默认）⇄ title（会话标题,空时回退 time）。
   * undefined = time。title 是内容列,只在 Tasks 列表渲染。 */
  taskLabelMode?: 'time' | 'title'
  /** -R 选定单个项目时,Tasks / Insights 的范围自动切到该项目生命周期（undefined = 开）。
   * 用户在项目生命周期模式下手改范围即写 false;范围控件的「Project span」按钮写回 true。 */
  projectAutoRange?: boolean
  /** 项目自动折叠规则（设置·Projects）:根会话数 < scratchMinSessions 且总轮数 < scratchMinTurns 的
   * 目录折叠进内置 Scratch 项目;scratchUnknown = 无目录源（unknown）归 Scratch。undefined = 开 / 2 / 5 / 开。
   * **写入方是 Rust `set_scratch_rule`**（合并写 prefs.json 并下发运行时值）;前端改规则时须同时 setDesignPrefs
   * 四键,否则 persist 的旧快照会把它们覆盖回去（同 idleThresholdMin）。 */
  scratchRuleEnabled?: boolean
  scratchMinSessions?: number
  scratchMinTurns?: number
  scratchUnknown?: boolean
  /** 时间轴置顶项目:**原始目录键**列表（读时经 list_project_meta 的 effective_key 解析,
   * merge 后不孤儿;匹配不到的键静默忽略不删除）。undefined = 无置顶。主窗口 ProjectManager 与
   * timeline 窗口共享本键,经 storage 桥跨窗口即时同步。 */
  timelinePinnedKeys?: string[]
  /** 看板方向:horizontal（时间为 X、项目为行,默认）⇄ vertical（项目为列,时间向下）。 */
  timelineOrientation?: 'horizontal' | 'vertical'
  /** ：最多显示的项目数（1〜50,0 = 不限只按窗口容量;undefined = 8）、
   * 过去 / 未来天数（0〜30;undefined = 7 / 7）。设置·General Timeline 段。 */
  timelineMaxProjects?: number
  timelinePastDays?: number
  timelineFutureDays?: number
  /** ：过去的日期每天只显示 timelinePastSessions 条会话（1〜10,undefined = 1）,
   * 今天显示 timelineTodaySessions 条（1〜20,undefined = 5）;选哪几条按 timelinePick：latest = 最新的代表
   * 一天（默认）/ earliest = 最早的几条代表一天 / longest = tokens 最多的代表一天。
   * 格内与日期一律按时间从旧到新自上而下;timelineReverse = true 整体反转（最新在上）。 */
  timelinePastSessions?: number
  timelineTodaySessions?: number
  timelinePick?: 'latest' | 'earliest' | 'longest'
  timelineReverse?: boolean
  /** ：看板失焦 N 秒后自动折成条态（0〜3600;undefined / 0 = 关）。第二屏挂着不获焦就不触发。 */
  timelineAutoStripSecs?: number
}

const KEY = 'tokencalendar.design'
/** 跨窗口实时互通的中转键（'storage' 事件只在「其他」窗口触发;本键内容
 * 与 prefs.json 同步,仅作事件载体与「其他窗口」的快速读取源）。 */
const BRIDGE_KEY = 'tokencalendar.design.bridge'

const DEFAULTS: DesignPrefs = {
  bgOpacity: 1,
  fgOpacity: 1,
  lockAspectRatio: true,
  locked: false,
  sizePreset: 'large',
  insightsCredit: false,
  matrixMaxRows: 15,
  autoUpdate: false,
}

// 载入清洗：未知键（如已退役字段）不落 prefs；非法色值/越界 alpha 视为未自定义
// （P1 主界面三色 + 顶栏 alpha 与 P0 卡片色同口径）。
function sanitize(p: Partial<DesignPrefs>): Partial<DesignPrefs> {
  const hexKeys = ['widgetCardBg', 'titlebarBg', 'panelBg', 'borderColor'] as const
  for (const k of hexKeys) {
    if (p[k] !== undefined && !/^#[0-9a-f]{3}([0-9a-f]{3})?$/i.test(p[k] as string)) {
      delete p[k]
    }
  }
  if (p.titlebarAlpha !== undefined && !(typeof p.titlebarAlpha === 'number' && p.titlebarAlpha >= 0.2 && p.titlebarAlpha <= 1)) {
    delete p.titlebarAlpha
  }
  for (const k of ['widgetMaterial', 'mainMaterial'] as const) {
    if (p[k] !== undefined && p[k] !== 'mica' && p[k] !== 'acrylic') {
      delete p[k]
    }
  }
  if (p.radiusScheme !== undefined && p.radiusScheme !== 'small' && p.radiusScheme !== 'medium' && p.radiusScheme !== 'large') {
    delete p.radiusScheme
  }
  if (p.weekStart !== undefined && p.weekStart !== 'sunday' && p.weekStart !== 'monday') {
    delete p.weekStart
  }
  if (p.matrixMaxRows !== undefined && !(typeof p.matrixMaxRows === 'number' && Number.isInteger(p.matrixMaxRows) && p.matrixMaxRows >= 0 && p.matrixMaxRows <= 100)) {
    delete p.matrixMaxRows
  }
  // 订阅轮询间隔（60〜1800 秒整）白名单清洗。
  // （悬浮球总开关的 `orbEnabled` 键已退役：可见性单一源在 Rust visibility.rs,
  //   持久化在 window-state.json 的 orb_visible;设置页直接消费该源,见㉚。）
  if (p.subscriptionPollSecs !== undefined && !(typeof p.subscriptionPollSecs === 'number' && Number.isInteger(p.subscriptionPollSecs) && p.subscriptionPollSecs >= 60 && p.subscriptionPollSecs <= 1800)) {
    delete p.subscriptionPollSecs
  }
  // boost 监控键清洗：布尔 + 整数域（阈值/间隔越界视为未设置 → 回默认档;
  // 与 Rust clamp_cfg 同域,双端兜底）。orbIdleEnabled 同族（待机退档开关）。
  for (const k of ['orbBoostEnabled', 'orbBoostSpikeEnabled', 'orbBoostLowEnabled', 'orbIdleEnabled'] as const) {
    if (p[k] !== undefined && typeof p[k] !== 'boolean') delete p[k]
  }
  if (p.orbPlatform !== undefined && p.orbPlatform !== 'codex' && p.orbPlatform !== 'claude') {
    delete p.orbPlatform
  }
  if (p.orbBoostSpikePct !== undefined && !(typeof p.orbBoostSpikePct === 'number' && Number.isInteger(p.orbBoostSpikePct) && p.orbBoostSpikePct >= 5 && p.orbBoostSpikePct <= 50)) {
    delete p.orbBoostSpikePct
  }
  if (p.orbBoostLowPct !== undefined && !(typeof p.orbBoostLowPct === 'number' && Number.isInteger(p.orbBoostLowPct) && p.orbBoostLowPct >= 10 && p.orbBoostLowPct <= 50)) {
    delete p.orbBoostLowPct
  }
  if (p.orbBoostIntervalSecs !== undefined && !(typeof p.orbBoostIntervalSecs === 'number' && Number.isInteger(p.orbBoostIntervalSecs) && p.orbBoostIntervalSecs >= 30 && p.orbBoostIntervalSecs <= 240)) {
    delete p.orbBoostIntervalSecs
  }
  // 离开阈值:整数分钟 1〜1440（与 Rust task_store 同域）,越界视为未设置。
  if (p.idleThresholdMin !== undefined && !(typeof p.idleThresholdMin === 'number' && Number.isInteger(p.idleThresholdMin) && p.idleThresholdMin >= 1 && p.idleThresholdMin <= 1440)) {
    delete p.idleThresholdMin
  }
  // 采集频率:只认五个档位（与 Rust collector:POLL_INTERVAL_CHOICES_SECS 同域）,其余视为未设置。
  if (p.collectIntervalSecs !== undefined && ![30, 60, 120, 180, 300].includes(p.collectIntervalSecs as number)) {
    delete p.collectIntervalSecs
  }
  if (p.taskLabelMode !== undefined && p.taskLabelMode !== 'time' && p.taskLabelMode !== 'title') {
    delete p.taskLabelMode
  }
  if (p.projectAutoRange !== undefined && typeof p.projectAutoRange !== 'boolean') {
    delete p.projectAutoRange
  }
  // 自动折叠规则:布尔 + 整数域（与 Rust project_meta:SCRATCH_*_BOUNDS 同域）。
  for (const k of ['scratchRuleEnabled', 'scratchUnknown'] as const) {
    if (p[k] !== undefined && typeof p[k] !== 'boolean') delete p[k]
  }
  if (p.scratchMinSessions !== undefined && !(typeof p.scratchMinSessions === 'number' && Number.isInteger(p.scratchMinSessions) && p.scratchMinSessions >= 1 && p.scratchMinSessions <= 50)) {
    delete p.scratchMinSessions
  }
  if (p.scratchMinTurns !== undefined && !(typeof p.scratchMinTurns === 'number' && Number.isInteger(p.scratchMinTurns) && p.scratchMinTurns >= 1 && p.scratchMinTurns <= 500)) {
    delete p.scratchMinTurns
  }
  // 时间轴:置顶键 = 非空字符串数组（去重,单键 ≤ 1024 字符,最多 200 项）;方向只认两值。
  if (p.timelinePinnedKeys !== undefined) {
    if (Array.isArray(p.timelinePinnedKeys)) {
      const seen = new Set<string>()
      p.timelinePinnedKeys = p.timelinePinnedKeys
        .filter((k): k is string => typeof k === 'string' && k.trim().length > 0 && k.length <= 1024)
        .filter((k) => !seen.has(k) && seen.add(k))
        .slice(0, 200)
    } else {
      delete p.timelinePinnedKeys
    }
  }
  if (p.timelineOrientation !== undefined && p.timelineOrientation !== 'horizontal' && p.timelineOrientation !== 'vertical') {
    delete p.timelineOrientation
  }
  if (p.timelineMaxProjects !== undefined && !(typeof p.timelineMaxProjects === 'number' && Number.isInteger(p.timelineMaxProjects) && p.timelineMaxProjects >= 0 && p.timelineMaxProjects <= 50)) {
    delete p.timelineMaxProjects
  }
  for (const k of ['timelinePastDays', 'timelineFutureDays'] as const) {
    if (p[k] !== undefined && !(typeof p[k] === 'number' && Number.isInteger(p[k]) && p[k] >= 0 && p[k] <= 30)) delete p[k]
  }
  if (p.timelinePastSessions !== undefined && !(typeof p.timelinePastSessions === 'number' && Number.isInteger(p.timelinePastSessions) && p.timelinePastSessions >= 1 && p.timelinePastSessions <= 10)) {
    delete p.timelinePastSessions
  }
  if (p.timelineTodaySessions !== undefined && !(typeof p.timelineTodaySessions === 'number' && Number.isInteger(p.timelineTodaySessions) && p.timelineTodaySessions >= 1 && p.timelineTodaySessions <= 20)) {
    delete p.timelineTodaySessions
  }
  if (p.timelinePick !== undefined && !['latest', 'earliest', 'longest'].includes(p.timelinePick as string)) {
    delete p.timelinePick
  }
  if (p.timelineReverse !== undefined && typeof p.timelineReverse !== 'boolean') {
    delete p.timelineReverse
  }
  if (p.timelineAutoStripSecs !== undefined && !(typeof p.timelineAutoStripSecs === 'number' && Number.isInteger(p.timelineAutoStripSecs) && p.timelineAutoStripSecs >= 0 && p.timelineAutoStripSecs <= 3600)) {
    delete p.timelineAutoStripSecs
  }
  // 应用更新开关：非布尔值视为未设置（回落默认开）。
  if (p.autoUpdate !== undefined && typeof p.autoUpdate !== 'boolean') {
    delete p.autoUpdate
  }
  return p
}

let prefs: DesignPrefs = DEFAULTS
const listeners = new Set<(p: DesignPrefs) => void>()

// ---------- prefs.json 桥（Rust 命令异步读写;inTauri 之外退化为纯内存） ----------

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) return null
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return await invoke<T>(cmd, args)
  } catch (e) {
    console.error(`[prefs] ${cmd} failed:`, e)
    return null
  }
}

/** 旧 localStorage → prefs.json 一次性迁移（首启执行一次）。
 * 返回迁移前的 localStorage 内容（null = 无旧数据）。 */
async function migrateLegacyLocalStorage(): Promise<Partial<DesignPrefs> | null> {
  let legacy: Partial<DesignPrefs> | null = null
  try {
    const raw = localStorage.getItem(KEY)
    if (raw) legacy = sanitize(JSON.parse(raw) as Partial<DesignPrefs>)
  } catch {
    legacy = null
  }
  if (legacy && Object.keys(legacy).length > 0) {
    await invoke('set_prefs_raw', { json: JSON.stringify(legacy) })
  }
  try {
    localStorage.removeItem(KEY)
  } catch {
    /* private mode */
  }
  return legacy
}

async function bootstrap(): Promise<void> {
  // prefs.json 优先;缺失且 localStorage 有旧数据 → 迁入
  const remote = await invoke<string | null>('get_prefs_raw')
  let loaded: Partial<DesignPrefs> | null = null
  if (remote) {
    try {
      loaded = sanitize(JSON.parse(remote) as Partial<DesignPrefs>)
    } catch {
      loaded = null
    }
  } else {
    loaded = await migrateLegacyLocalStorage()
  }
  prefs = { ...DEFAULTS, ...(loaded ?? {}) }
  for (const fn of listeners) fn(prefs)
}

/** 写盘节流（滑条高频 setDesignPrefs;600ms 静默窗口合并写）。
 * 同步写 localStorage 中转键 → 其他窗口 storage 事件即时生效。 */
let persistTimer = 0
function persist(): void {
  try {
    localStorage.setItem(BRIDGE_KEY, JSON.stringify(prefs))
  } catch {
    /* private mode */
  }
  window.clearTimeout(persistTimer)
  persistTimer = window.setTimeout(() => {
    void invoke('set_prefs_raw', { json: JSON.stringify(prefs) })
  }, 600)
}

function bootstrapSync(): void {
  // 同步初始化路径：先用 localStorage 桥（或旧键）同步恢复,异步再对 prefs.json 校准——
  // 避免首帧主题闪变（挂件在 window_ready 前就要消费尺寸/透明度档）。
  try {
    const bridge = localStorage.getItem(BRIDGE_KEY)
    const legacy = localStorage.getItem(KEY)
    const raw = bridge ?? legacy
    if (raw) prefs = { ...DEFAULTS, ...sanitize(JSON.parse(raw) as Partial<DesignPrefs>) }
  } catch {
    /* keep defaults */
  }
}
bootstrapSync()
if (typeof window !== 'undefined') {
  void bootstrap()
}

export function getDesignPrefs(): DesignPrefs {
  return prefs
}

export function setDesignPrefs(patch: Partial<DesignPrefs>): void {
  prefs = { ...prefs, ...patch }
  persist()
  for (const fn of listeners) fn(prefs)
}

export function subscribeDesignPrefs(fn: (p: DesignPrefs) => void): () => void {
  listeners.add(fn)
  return () => listeners.delete(fn)
}

/** designPrefs → boost 运行时配置（Rust BoostConfig 形状;缺省档与
 * Rust default_config 对齐：总开关关 / spike 开 10% / low 关 30% / 60s）。
 * 设置页改动与 orb 窗口装载恢复共用,避免两处各写一套缺省合并。 */
export function orbBoostConfig(p: DesignPrefs): BoostConfig {
  return {
    enabled: p.orbBoostEnabled ?? false,
    spike_enabled: p.orbBoostSpikeEnabled ?? true,
    spike_threshold_pct: p.orbBoostSpikePct ?? 10,
    low_enabled: p.orbBoostLowEnabled ?? false,
    low_threshold_pct: p.orbBoostLowPct ?? 30,
    interval_secs: p.orbBoostIntervalSecs ?? 60,
  }
}

/** designPrefs → 待机开关（undefined = 开;与 Rust ENABLED 初值对齐——
 * 退档是收敛行为,无需显式授权）。 */
export function orbIdleEnabled(p: DesignPrefs): boolean {
  return p.orbIdleEnabled ?? true
}

// 跨窗口实时互通：widget / main 两个窗口共享同源 localStorage。
// 'storage' 事件只在「其他」窗口触发（写入方收不到），收到后重读并通知
// 本窗口订阅者——主窗口抽屉拖滑条，挂件即时生效；反之亦然（验收项）。
// 桥键与旧键都监听（迁移期旧版本多窗口共存兼容）。
if (typeof window !== 'undefined') {
  window.addEventListener('storage', (e) => {
    if (e.key !== BRIDGE_KEY && e.key !== KEY && e.key !== null) return
    try {
      const raw = e.newValue ?? localStorage.getItem(BRIDGE_KEY)
      if (raw) prefs = { ...DEFAULTS, ...sanitize(JSON.parse(raw) as Partial<DesignPrefs>) }
    } catch {
      /* keep current */
    }
    for (const fn of listeners) fn(prefs)
  })
}
