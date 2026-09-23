// Widget design preferences shared across components （settings drawer writes,
// YearMatrix reads). Module-level store: tiny, no provider needed.
// 持久化单一源 = 数据根 prefs.json（Rust 命令 get_prefs_raw/set_prefs_raw 原子读写）;
// localStorage 仅作旧数据一次性迁移来源（首启检测 prefs.json 缺失而 localStorage 有 → 迁入后清除旧键）,
// 跨窗口实时互通走 'storage' 事件桥（写入方同时写 localStorage 中转键）。

/** 尺寸预设三档。窗口尺寸单一源：SIZE_PRESETS（YearMatrix.tsx），
 * designPrefs 只存档位名；档位切换即 set_widget_size，几何落盘兜底。 */
export type SizePreset = 'large' | 'medium' | 'small'

import type { SubscriptionPlatform } from '../../services/subscriptionService'
import { setFetchPolicy } from '../../services/subscriptionService'
import { systemLocale } from '../../lib/i18n/system'

/** 「按预计消耗取数」阈值的合法域（与 Rust clamp 同源：0.5〜10.0，步进 0.5；
 * default = 距上次读数预计消耗这么多百分点就取一次）。设置页的 min/max/step 取这里。 */
// 默认 5 与 Rust 侧 demand:DEFAULT_THRESHOLD_PCT 同源。
// 阈值的单位是「5h 窗口的百分点」，而两个平台满窗的 API 等价用量差约一个数量级
// （Claude Max ≈ $110、Codex ≈ $11〜14），阈值过低会让 Codex 很少的用量就触发一次请求。
// Codex 的读数新鲜度不靠它——rollout 会零请求推进快照。
export const SUBSCRIPTION_FETCH_PCT = { min: 0.5, max: 10, step: 0.5, default: 5 } as const

/** 毛玻璃材质档（undefined = 关闭）。两窗口各自独立一个键。
 * 版本门槛：Mica 仅 Win11（22000+，apply 失败自动回退关闭）；Acrylic
 * Win10 1809+（Win11 ≥22523 走 host backdrop，染色参数被忽略）。 */
export type MaterialEffect = 'mica' | 'acrylic'

/** 圆角方案三档：全局圆角统一为主题参数。
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
 * （padding 12×2 + border 2；月标签是浮层不占布局位，上下边距严格对称）。
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
   * clickable. The widget is a glanceable desktop calendar, not always-on-top. */
  locked: boolean
  /** Widget window size preset （large/medium/small); switching applies the
   * preset's window size immediately via set_widget_size. */
  sizePreset: SizePreset
  /** CodeBuddy 积分卡显隐。默认关——没用过 CodeBuddy 的用户
   * 不应看到常驻空引导卡;开启后 chart 视图尾部渲染该卡。 */
  insightsCredit: boolean
  /** 主窗口矩阵最大行数（同时限制 agent/model 视图行数,
   * 超出截断为「+N more」摘要行）。默认 15;0 = 不限。热力图是主体,行数
   * 上限保证小窗下热力图完整可见、不被图表面板挤压。 */
  matrixMaxRows: number
  /** 订阅**兜底取数间隔**（秒）。undefined = 1800（30 分钟,默认值见 SettingsPage 的 POLL_DEFAULT_SECS）;设置页下拉 5/10/15/30 分钟。
   * 读数主路径是本地 token 探针（采集器一发现本机新 token 就立即取数）,本键只管
   * 「本地留不下痕迹」那部分用量（网页 / 在线会话）的兜底节奏。持久化在此键,
   * 运行时值经 set_subscription_poll_secs 下发（窗口装载时恢复）。 */
  subscriptionPollSecs?: number
  /** 取数阈值（百分点;undefined = SUBSCRIPTION_FETCH_PCT.default）：本地 token 按模型加权折算成「距上次读数大约
   * 消耗了百分之几」,达到本值就取一次读数——这是取数主路径,subscriptionPollSecs
   * 只剩兜底。合法域见 SUBSCRIPTION_FETCH_PCT（0.5〜10,0.5 的整数倍;Rust 侧同域 clamp）。
   * 运行时值经 set_subscription_fetch_policy 下发（与 subscriptionTightenLow 合成一次调用）。 */
  subscriptionFetchPct?: number
  /** 低余量收紧（undefined = 开）：5h 窗口剩余 ≤ 20% 时把上面的阈值减半,
   * 额度见底那段时间读数更密。与 subscriptionFetchPct 同一条下发命令。 */
  subscriptionTightenLow?: boolean
  /** 用户自己那一档的订阅月费（美元,按平台;缺键 = 未填）。**不维护官方月费表**——只用于
   * Insights 价格面板的「回本倍数」行:区间美元当量 ÷（月费 × 区间天数 / 月均天数）。
   * 未填的平台面板不显示该行。 */
  subscriptionMonthlyUsd?: Partial<Record<SubscriptionPlatform, number>>
  /** 待机（Rust subscription/idle.rs）：本地 agent 静默满「离开」时长即进入待机,悬浮球整体减淡;
   * 新 token / 用户注意（手动刷新等）立即退出。只影响视觉,不改变取数频次。
   * **默认开**（undefined = 开）。运行时经 set_subscription_idle_enabled 下发,orb 窗口装载时恢复。 */
  orbIdleEnabled?: boolean
  /** 悬浮球上次显示的订阅平台：启动恢复上次的平台,而非固定第一个。
   * 按平台 id 记（绑定集合变化时下标会错位）;该平台未绑定 → 回落第一个已绑定平台,
   * 键保持不动直到用户再切换。undefined = 第一个。 */
  orbPlatform?: SubscriptionPlatform
  /** 悬浮球 5h 表盘 hover 里显示剩余消息数的模型（按平台,值 = collector 模型键,按此顺序印）。
   * 缺键 = 自动（只显示主力模型 = 最近 7 天用户轮最多的那个）;空数组 = 不显示。
   * 选了但近 30 天样本不够的模型静默略过。上限 ORB_MESSAGE_MODELS_MAX 个。 */
  orbMessageModels?: Partial<Record<SubscriptionPlatform, string[]>>
  /** 应用更新：每次启动后自动检查,有新版就在后台预下载并发系统通知;**从不自动安装**,
   * 安装由用户在设置·About 点 Install。默认关（启动即联网属显式授权行为）。关闭时
   * 仅「Check for updates」手动触发。更新源与签名校验见 services/updateService.ts（仅安装版生效）。 */
  autoUpdate: boolean
  /** 离开阈值（分钟,1〜1440;undefined = 30）:轮间空档 ≤ 阈值才计入「人工时间」。
   * **写入方是 Rust `set_idle_threshold`**（合并写 prefs.json 并同步重算 daily_project）;
   * 前端改阈值时须同时 setDesignPrefs 本键,否则 persist 的旧快照会把它覆盖回去。 */
  idleThresholdMin?: number
  /** 采集频率（秒,30 / 60 / 120 / 180 / 300;undefined = 30）。**写入方是 Rust `set_collect_interval`**
   * （合并写 prefs.json 并下发运行时值）;前端改档时须同时 setDesignPrefs 本键,否则 persist 的旧快照会覆盖回去。 */
  collectIntervalSecs?: number
  /** Tasks 列表标签:time（开始时间,默认）⇄ title（会话标题,空时回退 time）。
   * undefined = time。title 是内容列,只在 Tasks 列表渲染。 */
  taskLabelMode?: 'time' | 'title'
  /** Tasks 时间卡模式:gaps（空档直方图,默认）⇄ spent（时间统计）。undefined = gaps。 */
  taskTimeMode?: 'gaps' | 'spent'
  /** 时间统计维度（undefined = project）。选定单个项目时界面自动切到 task,不写回本键。 */
  taskTimeGroup?: 'project' | 'task' | 'day'
  /** 时间统计指标:total（Task + Human 堆叠,默认）/ task / human。 */
  taskTimeMetric?: 'total' | 'task' | 'human'
  /** 选定单个项目时,Tasks / Insights 的范围自动切到该项目生命周期（undefined = 开）。
   * 用户在项目生命周期模式下手改范围即写 false;范围控件的「Project span」按钮写回 true。 */
  projectAutoRange?: boolean
  /** 项目固定配色:project_key → 色板槽位（projectColors.ts 的 PROJECT_PALETTE 下标）。
   * 首次出图时分配、此后不变,跨会话 / 跨范围 / 跨视图同色;由 projectColors.ts 写入。 */
  projectColors?: Record<string, number>
  /** 项目自动折叠规则（设置·Projects）:根会话数 < scratchMinSessions 且总轮数 < scratchMinTurns 的
   * 目录折叠进内置 Scratch 项目;scratchUnknown = 无目录源（unknown）归 Scratch。undefined = 开 / 2 / 5 / 开。
   * **写入方是 Rust `set_scratch_rule`**（合并写 prefs.json 并下发运行时值）;前端改规则时须同时 setDesignPrefs
   * 四键,否则 persist 的旧快照会把它们覆盖回去（同 idleThresholdMin）。 */
  scratchRuleEnabled?: boolean
  scratchMinSessions?: number
  scratchMinTurns?: number
  scratchUnknown?: boolean
  /** 时间轴监测项目组（时间轴只显示这些项目,顺序即列序;设置·Projects 管理）:**原始目录键**列表（读时经 list_project_meta 的 effective_key 解析,
   * merge 后不孤儿;匹配不到的键静默忽略不删除）。undefined = 无置顶。主窗口 ProjectManager 与
   * timeline 窗口共享本键,经 storage 桥跨窗口即时同步。 */
  timelinePinnedKeys?: string[]
  /** 时间轴过去 / 未来天数（0〜30;undefined = 7 / 7）。设置·General Timeline 段。 */
  timelinePastDays?: number
  timelineFutureDays?: number
  /** 过去的日期每天只显示 timelinePastSessions 条会话（1〜10,undefined = 1）,
   * 今天显示 timelineTodaySessions 条（1〜20,undefined = 5）;选哪几条按 timelinePick：latest = 最新的代表
   * 一天（默认）/ earliest = 最早的几条代表一天 / longest = tokens 最多的代表一天。
   * 格内与日期一律按时间从旧到新自上而下;timelineReverse = true 整体反转（最新在上）。 */
  timelinePastSessions?: number
  timelineTodaySessions?: number
  timelinePick?: 'latest' | 'earliest' | 'longest'
  timelineReverse?: boolean
  /** 看板失焦 N 秒后自动折成条态（0〜3600;undefined / 0 = 关）。第二屏挂着不获焦就不触发。 */
  timelineAutoStripSecs?: number
  /** 时间轴外观（设置·Appearance Timeline 组）：面板背景 alpha（0〜1,undefined = 0.55）、
   * 顶栏 alpha（0.2〜1,undefined = 0.85）、会话格 alpha（0.2〜1,undefined = 0.8）。都是背景色 alpha,
   * 不是元素 opacity——文字与按钮始终全不透明。与挂件 bgOpacity / 主界面 titlebarAlpha 零关联。 */
  timelineBgAlpha?: number
  /** 时间轴看板窗口风格：shadow = 主窗口同款 DWM 阴影 + 外缘透明呼吸位（undefined 即此档）/ flat = 无阴影全出血。
   * Rust set_timeline_style 施加边缘组合,条态恒 flat。 */
  timelineWindowStyle?: 'shadow' | 'flat'
  /** 时间轴主题色（#rrggbb;undefined = 跟随全局 accent）：会话格热力、今天、亮起、pin 等强调色,不影响顶栏 / 面板底色。 */
  timelineAccent?: string
  /** 时间轴会话格按用量（tokens）着色深浅（undefined = 开）;关 = 所有会话格统一浅色。 */
  timelineHeat?: boolean
  timelineBarAlpha?: number
  timelineCellAlpha?: number
  /** 界面语言（undefined = 跟随系统:中文系统 → zh-CN,其余 → en）。消费方一律经
   * src/lib/i18n 的 effectiveLocale / useT;Rust 托盘启动读本键,运行时经 set_ui_locale 同步。 */
  locale?: 'en' | 'zh-CN'
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
// （主界面三色 + 顶栏 alpha 与卡片色同口径）。
function sanitize(p: Partial<DesignPrefs>): Partial<DesignPrefs> {
  const hexKeys = ['widgetCardBg', 'titlebarBg', 'panelBg', 'borderColor', 'timelineAccent'] as const
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
  // 兜底取数间隔只认设置页四个档（秒;Rust 侧受理域仍是 60〜1800,
  // 前端收窄到下拉可选值,越界或旧档视为未设置 → 回默认）。
  if (p.subscriptionPollSecs !== undefined && ![300, 600, 900, 1800].includes(p.subscriptionPollSecs as number)) {
    delete p.subscriptionPollSecs
  }
  // 取数阈值：只认 0.5〜10 且是 0.5 的整数倍（与 Rust clamp / 步进同域;
  // 0.5 的倍数在二进制里精确可表示,除以步进取整判断无误差）。越界 / 非法步进视为未设置 → 回默认值。
  if (
    p.subscriptionFetchPct !== undefined &&
    !(
      typeof p.subscriptionFetchPct === 'number' &&
      Number.isFinite(p.subscriptionFetchPct) &&
      p.subscriptionFetchPct >= SUBSCRIPTION_FETCH_PCT.min &&
      p.subscriptionFetchPct <= SUBSCRIPTION_FETCH_PCT.max &&
      Number.isInteger(p.subscriptionFetchPct / SUBSCRIPTION_FETCH_PCT.step)
    )
  ) {
    delete p.subscriptionFetchPct
  }
  // 低余量收紧开关：非布尔视为未设置（回默认开）。
  if (p.subscriptionTightenLow !== undefined && typeof p.subscriptionTightenLow !== 'boolean') delete p.subscriptionTightenLow
  // 订阅月费:只留 codex / claude 两键里 （0, MONTHLY_USD_MAX] 的有限数,其余键 / 非法值丢弃。
  if (p.subscriptionMonthlyUsd !== undefined) {
    const src = p.subscriptionMonthlyUsd as Record<string, unknown> | null
    const clean: Partial<Record<SubscriptionPlatform, number>> = {}
    if (src && typeof src === 'object' && !Array.isArray(src)) {
      for (const k of ['codex', 'claude'] as SubscriptionPlatform[]) {
        const v = src[k]
        if (typeof v === 'number' && Number.isFinite(v) && v > 0 && v <= MONTHLY_USD_MAX) clean[k] = v
      }
    }
    if (Object.keys(clean).length > 0) p.subscriptionMonthlyUsd = clean
    else delete p.subscriptionMonthlyUsd
  }
  // 待机退档开关：非布尔视为未设置（回默认开）。
  if (p.orbIdleEnabled !== undefined && typeof p.orbIdleEnabled !== 'boolean') delete p.orbIdleEnabled
  if (p.orbPlatform !== undefined && p.orbPlatform !== 'codex' && p.orbPlatform !== 'claude') {
    delete p.orbPlatform
  }
  // 剩余消息数的模型选择:只留 codex / claude 两键里的字符串数组（去重、截到上限）。
  if (p.orbMessageModels !== undefined) {
    const src = p.orbMessageModels as Record<string, unknown> | null
    const clean: Partial<Record<SubscriptionPlatform, string[]>> = {}
    if (src && typeof src === 'object' && !Array.isArray(src)) {
      for (const k of ['codex', 'claude'] as SubscriptionPlatform[]) {
        const v = src[k]
        if (Array.isArray(v)) {
          clean[k] = [...new Set(v.filter((m): m is string => typeof m === 'string' && m.length > 0))].slice(
            0,
            ORB_MESSAGE_MODELS_MAX,
          )
        }
      }
    }
    if (Object.keys(clean).length > 0) p.orbMessageModels = clean
    else delete p.orbMessageModels
  }
  // 退役键清理：提频档位 orbBoost* 与悬浮球总开关 orbEnabled 已不再使用（可见性单一源在
  // Rust visibility.rs,持久化在 window-state.json 的 orb_visible）。旧 prefs.json 里的
  // 残值静默清掉——不读、不回写、不报错。**只认这两条前缀**：命名空间通配会把
  // 将来新增的 orb 前缀键一并吞掉。
  for (const k of Object.keys(p)) {
    if (k.startsWith('orbBoost') || k === 'orbEnabled') delete (p as Record<string, unknown>)[k]
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
  if (p.taskTimeMode !== undefined && p.taskTimeMode !== 'gaps' && p.taskTimeMode !== 'spent') {
    delete p.taskTimeMode
  }
  if (p.taskTimeGroup !== undefined && !['project', 'task', 'day'].includes(p.taskTimeGroup)) {
    delete p.taskTimeGroup
  }
  if (p.taskTimeMetric !== undefined && !['total', 'task', 'human'].includes(p.taskTimeMetric)) {
    delete p.taskTimeMetric
  }
  if (p.projectAutoRange !== undefined && typeof p.projectAutoRange !== 'boolean') {
    delete p.projectAutoRange
  }
  if (p.projectColors !== undefined) {
    const ok = typeof p.projectColors === 'object' && p.projectColors !== null && !Array.isArray(p.projectColors)
    if (!ok) {
      delete p.projectColors
    } else {
      const clean: Record<string, number> = {}
      for (const [k, v] of Object.entries(p.projectColors)) {
        if (typeof v === 'number' && Number.isInteger(v) && v >= 0 && v < 64) clean[k] = v
      }
      p.projectColors = clean
    }
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
  // 时间轴置顶键 = 非空字符串数组（去重,单键 ≤ 1024 字符,最多 200 项）。
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
  // 退役键 timelineOrientation（看板只有纵向项目视图）:旧 prefs 残值清掉
  delete (p as Record<string, unknown>).timelineOrientation
  if (p.timelineHeat !== undefined && typeof p.timelineHeat !== 'boolean') delete p.timelineHeat
  if (p.timelineWindowStyle !== undefined && p.timelineWindowStyle !== 'shadow' && p.timelineWindowStyle !== 'flat') {
    delete p.timelineWindowStyle
  }
  // 退役键 timelineMaxProjects（时间轴改为固定监测项目组,见 timelinePinnedKeys）:清残值
  delete (p as Record<string, unknown>).timelineMaxProjects
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
  for (const [k, min] of [['timelineBgAlpha', 0], ['timelineBarAlpha', 0.2], ['timelineCellAlpha', 0.2]] as const) {
    const v = p[k]
    if (v !== undefined && !(typeof v === 'number' && v >= min && v <= 1)) delete p[k]
  }
  if (p.timelineAutoStripSecs !== undefined && !(typeof p.timelineAutoStripSecs === 'number' && Number.isInteger(p.timelineAutoStripSecs) && p.timelineAutoStripSecs >= 0 && p.timelineAutoStripSecs <= 3600)) {
    delete p.timelineAutoStripSecs
  }
  if (p.locale !== undefined && p.locale !== 'en' && p.locale !== 'zh-CN') delete p.locale
  // 应用更新开关：非布尔值视为未设置（回落默认关）。
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
  ready = true
  for (const fn of listeners) fn(prefs)
}

/** prefs.json 已载入（此前的快照只来自 localStorage 桥,不宜由非用户操作触发写盘）。 */
let ready = false
export function designPrefsReady(): boolean {
  return ready
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

/** designPrefs → 热力图周起始。未设置时按界面语言:中文默认周一（国内日历习惯）,英文默认周日（GitHub 式）。 */
export function weekStartOf(p: DesignPrefs): WeekStart {
  return p.weekStart ?? ((p.locale ?? systemLocale()) === 'zh-CN' ? 'monday' : 'sunday')
}

/** designPrefs → 待机开关（undefined = 开;与 Rust ENABLED 初值对齐）。 */
export function orbIdleEnabled(p: DesignPrefs): boolean {
  return p.orbIdleEnabled ?? true
}

/** designPrefs → 取数阈值（百分点;undefined = SUBSCRIPTION_FETCH_PCT.default）。 */
/** 订阅月费输入上限（美元）:防手滑多打几个零,远高于任何现行个人档。 */
export const MONTHLY_USD_MAX = 10_000

export function subscriptionFetchPct(p: DesignPrefs): number {
  return p.subscriptionFetchPct ?? SUBSCRIPTION_FETCH_PCT.default
}

/** designPrefs → 低余量收紧（undefined = 开）。 */
export function subscriptionTightenLow(p: DesignPrefs): boolean {
  return p.subscriptionTightenLow ?? true
}

/** 恢复 / 下发取数策略运行时值：两键合成一次 set_subscription_fetch_policy
 * （与 applyPollSecs 同款——持久化在本模块,运行时值归 Rust;非 Tauri 环境与
 * Rust 缺命令时静默跳过,不打断调用方）。 */
export async function applySubscriptionFetchPolicy(p: DesignPrefs): Promise<void> {
  await setFetchPolicy(subscriptionFetchPct(p), subscriptionTightenLow(p)).catch(() => {})
}

// 跨窗口实时互通：各窗口共享同源 localStorage。
// 'storage' 事件只在「其他」窗口触发（写入方收不到），收到后重读并通知
// 本窗口订阅者——主窗口抽屉拖滑条，挂件即时生效；反之亦然。
// 桥键与旧键都监听（兼容仍写旧键的旧版本窗口共存）。
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

/** 悬浮球 hover 里剩余消息数最多列几个模型（hover 要简短）。 */
export const ORB_MESSAGE_MODELS_MAX = 4
