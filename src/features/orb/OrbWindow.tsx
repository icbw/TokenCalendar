// OrbWindow：悬浮球窗口（两态）。
// 独立根类 .orb-shell：不进 .shell 体系（主窗口样式只属于 .is-expanded、挂件样式只属于
// .is-widget），orb 颜色变量按窗口拆分 --orb-*。
//
// 两态：
// - 收起态 = 竖向外轮廓贴片条：外轮廓 = 周额度（水位随消耗向下退,「外包内」）,
//   内条 = 5 小时额度;双击展开;
// - 展开态 = 周额度环 + 中央 5h 表盘（110px 正圆）：表盘内 = 5h 剩余大数（只显示剩余,
//   不带总量）/ 5h 重置时刻（HH:MM）;底部两行 = 周重置倒计时（上）与平台 + 套餐名（下）。
//   功能按钮在表盘右侧排成一列悬挂按钮。
//   尺寸切换走 set_orb_size（窗口 resizable=false,程序化是唯一入口）。
//
// 读数口径（单侧,全窗口一致）：所有数字与图形都表示**剩余量**——表盘大数 = 5h 剩余、
// 环长 = 各自剩余占比、收起态竖条 = 剩余。
//
// 数据：get_subscription_snapshots 初查 + subscription:changed 事件重查（Rust 轮询 daemon 发）
// + 5 分钟一次漏事件兜底。状态文案见 statusHint。
// 主题：复用挂件族 sync hook（卡片色/WCAG 派生）映射到 --orb-* 变量；不装配材质 hook
// （orb 无毛玻璃材质档）。
import { useCallback, useEffect, useRef, useState } from 'react'
import { useLayoutEffect } from 'react'
import { events, subscriptionService, windowService, type OrbDockState, type SubscriptionPlatform, type SubscriptionSnapshot } from '../../services'
import { getDesignPrefs, orbIdleEnabled, setDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import { deriveWidgetTheme } from '../settings/widgetTheme'
import { useShowOnLoad } from '../window/useShowOnLoad'
import { useRadiusSchemeSync } from '../settings/radiusTheme'
import './orb.css'

/** 挂件派生主题 → --orb-* 变量镜像（orb.css 消费 --orb-* 键,颜色变量按窗口拆分,
 * 不复用 --widget-card-*。映射在此处单点完成,widgetTheme 不动）。scheme 内置色
 * （未自定义卡片色）时变量缺省,由 orb.css 的蓝白透明兜底值生效。 */
function applyOrbThemeVars(): void {
  const root = document.documentElement
  const theme = deriveWidgetTheme(getDesignPrefs())
  const pairs: [string, string | null][] = theme
    ? [
        ['--orb-card-bg', `rgba(${theme.cardBg},0.92)`],
        ['--orb-card-border', `rgba(${theme.cardBorder},0.25)`],
        ['--orb-card-text', `rgb(${theme.text})`],
        ['--orb-card-text-muted', `rgb(${theme.textMuted})`],
      ]
    : []
  for (const [k, v] of pairs) {
    if (v === null) root.style.removeProperty(k)
    else root.style.setProperty(k, v)
  }
  if (!theme) {
    for (const k of ['--orb-card-bg', '--orb-card-border', '--orb-card-text', '--orb-card-text-muted']) {
      root.style.removeProperty(k)
    }
  }
}

function useOrbThemeSync(): void {
  applyOrbThemeVars()
  useEffect(() => subscribeDesignPrefs(applyOrbThemeVars), [])
}

/** 两态窗口尺寸（逻辑像素;tauri.conf.json 初始值 = COLLAPSED）。
 * 收起态窗口 = 竖条本体 24×84 + 每边 16px 透明呼吸位,否则竖条投影与描边发光被窗口
 * 边界硬切出直边。与 Rust 侧 PILL_W/H_LOGICAL 同源;贴边归位按「本体距屏缘 4px」反算、
 * 透明边距出屏。 */
const COLLAPSED_SIZE = { w: 56, h: 116 }
/** 展开态窗口尺寸（逻辑像素）。窗口不紧贴内容：多出的透明区是 hover 提示的落点空间
 * （提示最宽约 188,跟随光标右下 12px;窗口太小时提示会在两个槽位间来回翻）。主体之外的
 * 透明区点击穿透（WM_NCHITTEST → HTTRANSPARENT）,画布不抢鼠标。
 * 容器绕表盘视觉中心对称：Windows 按窗口矩形中心判定窗口属于哪台显示器（DPI 切换、
 * 跨屏判断都用这个中心）;不对称时拖到双屏接缝附近,窗口中心先于表盘过缝,系统提前
 * 把窗口判给另一侧屏,主体会瞬间缩放。
 * ⇒ 内容偏移 = （235, 100),即 padL + 55 = W/2、padT + 55 = H/2（表盘 110×110,
 * 视觉中心在内容块左上 （55,55) 处）。右侧画布 213（按钮列提示放得下）。
 * ⚠ 与 Rust 的 EXPANDED_W/H_LOGICAL、EXPANDED_PAD_L/T_LOGICAL 及 orb.css 的
 * .orb-shell.is-expanded .orb-orb 必须同源。 */
const EXPANDED_SIZE = { w: 580, h: 310 }

/** 展开态交互主体尺寸（内容区 132×110）——与 Rust 穿透命中区/边界钳制同源
 * （EXPANDED_INNER_W/H_LOGICAL）。透明呼吸位不接收鼠标（WM_NCHITTEST → HTTRANSPARENT），
 * 右键菜单的位置必须 clamp 在主体内,否则弹到透明边距上点不到。 */
const EXPANDED_INNER = { w: 132, h: 110 }
/** 内容在窗口内的左上偏移：左右/上下都围绕表盘视觉中心对称（见 EXPANDED_SIZE）——
 * 235 + 55 = 580/2,100 + 55 = 310/2。与 Rust `EXPANDED_PAD_L/T_LOGICAL` 同源（CSS 里同值一并改）。 */
const EXPANDED_PAD = { l: 235, t: 100 }

/** 表盘几何（viewBox 150×150 定值,外层 CSS 缩放到 110px）：
 * 壳体 r74 / 周额度环 r70（内缩 4px 留出一圈玻璃边）/ 中央 5h 表盘 60px。 */
const DIAL_VIEW = 150
const DIAL_R = 74
const RING_R = 70

/** 竖条水位跨度：外轮廓纵向 0〜84（描边线宽 3 居中在 y=1.5/82.5）,取 85
 * 作「全空」终点——水位线 y = SPAN − （SPAN + FADE)×剩余比例。 */
const PILL_LEVEL_SPAN = 85
/** 水位线淡出高度：水位线往上 FADE 像素内由实色渐隐,灰轨道与实色之间是软过渡而非截断。
 * 水位线的行程补上这一段（100% 时线抬到 −FADE,淡出段落在轮廓上缘之外——否则满额度时
 * 顶边会被淡掉一块）。值不宜大：余量很低时两侧只剩一小截,淡出会把它整段吃掉。 */
const PILL_LEVEL_FADE = 6
/** 水位遮罩矩形的高度（只影响淡出在渐变里的归一化位置,见 PILL_LEVEL_FADE）：
 * 矩形只需盖住水位线以下,取一个远大于轮廓高度的定值即可。 */
const PILL_LEVEL_RECT_H = 200

/** 秒表节拍（30s）：周重置倒计时 / 5h 重置时刻是渲染时按 `Date.now` 算的,
 * 没有新快照也要重算,否则读数不动时倒计时会冻住。**只触发重渲染,不查后端**。 */
const TICK_MS = 30_000
/** 每多少个节拍补查一次快照（= 5 分钟）：快照的即时性由 `subscription:changed`
 * 负责,这条只是「万一漏了一次事件」的兜底。 */
const RESNAP_EVERY_TICKS = 10

/** 手动刷新的反馈时长：下限 = 扫掠弧至少走完一轮（看得）;上限兜住
 * 「收不到完成信号」的情形——`fetched_at` 只在**成功**取数时推进,所以网络/
 * 凭据失败那几轮（快照保留旧值）只能靠上限收束,上限不宜大。 */
const REFRESH_MIN_MS = 1200
const REFRESH_MAX_MS = 4000

/** hover 提示显示延迟：指针在同一区域停满这段时间才出提示——扫过/路过不出。
 * 比系统工具提示（≈1s）快一档。 */
const HOVER_DELAY_MS = 350
/** 提示收尾宽限：指针离开一个提示区时不立刻收——相邻区域横移途中要
 * 经过不产出提示的空隙（环 → 悬挂按钮、文本行 → 环）,立刻收会闪一帧并
 * 重等一轮延迟。留这一小段等下一区接手,没人接才真收。 */
const HOVER_GRACE_MS = 140
/** 拖动抑制时长：窗口被拖动过后,这一段内不出提示,超时自动恢复。
 * 不用「指针离开窗口」复位——拖动完光标往往还停在球上（窗口跟着光标走）,
 * 等它离开等于永远不恢复。1.5s 够让拖完的瞬间不弹提示,又不至于像提示坏了。 */
const HOVER_MUTE_MS = 1500

/** 刷新反馈三态：waiting = 扫掠 + 压暗（取数中）→ landing = 新值落库、
 * 摘掉压暗让涨跌走位 → idle。两段各自的下限/上限见上。 */
type RefreshPhase = 'idle' | 'waiting' | 'landing'

/** 首字母大写（平台名/套餐名归一——上游大小写不定,曾见 "codex Plus"）。 */
function capitalize(s: string): string {
  return s ? s.charAt(0).toUpperCase() + s.slice(1) : s
}

/** 平台显示名：codex → 「GPT」（界面显示「GPT Plus」）;其它平台首字母大写。 */
function platformLabel(platform: string): string {
  return platform.toLowerCase() === 'codex' ? 'GPT' : capitalize(platform)
}

function pctText(v: number | null): string {
  return v === null ? '—' : String(Math.round(v))
}

/** 单平台窗口语义序：5h 主显（收起态内条）,7d 为周额度（收起态外轮廓）。 */
function windowOf(snap: SubscriptionSnapshot | undefined, kind: string) {
  return snap?.windows.find((w) => w.kind === kind)
}

/** 窗口「未使用」统一判据：窗口存在但零消耗。两平台对「还没开始用」的表示不一致——
 * Codex 给「当前 + 窗口长度」的滚动 resets_at（每次取数都往后漂的假窗口尾）,
 * Claude 给 resets_at = null。判据只取 used_percent（与 resets_at 无关）⇒ 表盘 / 周行 /
 * 提示统一切到 idle 读数,首次消耗后自动回到真实读数。 */
function windowIdle(w: { used_percent: number } | undefined): boolean {
  return w != null && w.used_percent <= 0
}

/** 状态文案（auth_failed 与 plan_inactive 语义勿混）：
 * auth_failed = 凭据失效,需要用户重新登录 agent CLI;
 * plan_inactive = 凭据仍有效但订阅过期/降级,续费后自动恢复,用户零操作。 */
function statusHint(status: string): { text: string; level: 'ok' | 'warn' | 'error' | 'muted' } {
  switch (status) {
    case 'ok':
      return { text: 'Active', level: 'ok' }
    case 'plan_inactive':
      return { text: 'Subscription inactive — resumes after renewal', level: 'warn' }
    case 'auth_failed':
      return { text: 'Credentials expired — run the agent CLI to refresh', level: 'error' }
    case 'rate_limited':
      return { text: 'Rate limited — retrying automatically', level: 'warn' }
    case 'network_failed':
      return { text: 'Network error — showing last known data', level: 'warn' }
    case 'parse_failed':
      return { text: 'Upstream response unrecognized', level: 'warn' }
    default:
      return { text: 'Not bound — manage in Settings', level: 'muted' }
  }
}

/** 重置时刻（unix 秒 → 本地「HH:MM」;None/已过 → null）。
 * 5h 窗口重置在数小时内,显示具体时刻比「剩余时长」更直接,表盘内直接把时刻当标签。 */
function clockAt(resetsAt: number | null | undefined): string | null {
  if (!resetsAt) return null
  const t = resetsAt * 1000
  if (t <= Date.now()) return null
  const d = new Date(t)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

/** 重置倒计时（unix 秒 → 「6d 21h 45m」天/时/分三段;None/已过 → null）。 */
function countdownDetailed(resetsAt: number | null | undefined): string | null {
  if (!resetsAt) return null
  const diff = resetsAt * 1000 - Date.now()
  if (diff <= 0) return null
  const totalMin = Math.floor(diff / 60_000)
  const d = Math.floor(totalMin / 1440)
  const h = Math.floor((totalMin % 1440) / 60)
  const m = totalMin % 60
  if (d > 0) return `${d}d ${h}h ${m}m`
  if (h > 0) return `${h}h ${m}m`
  return `${m}m`
}

/** hover 提示内容：自绘玻璃浮层（不用原生 title——系统方角样式与玻璃族不搭,且与光标
 * 所指位置无关）,按指针所在位置只给对应的那一条口径：周额度环 / 中央 5h 表盘 /
 * 套餐行（状态）/ 周重置行（绝对时刻）/ 悬挂按钮。
 * 排版两级 = 小字标签（口径名）+ 主读数 + 补充行。 */
interface TipContent {
  /** 口径名（小字全大写淡色;动作类提示可省）。 */
  label?: string
  /** 读数行：第一条 = 主读数,其余 = 补充口径（权重递减）。 */
  lines: string[]
  /** 四态色（仅状态类提示用;值 = statusHint 的 level）。 */
  level?: 'ok' | 'warn' | 'error' | 'muted'
}

/** 重置绝对时刻（周重置行 hover:可见文字是倒计时,提示补「几号几点」——
 * 与可见读数互补而非重复;固定 en-US 短格式,与全英文界面一致）。 */
function stampAt(resetsAt: number | null | undefined): string | null {
  if (!resetsAt) return null
  const t = resetsAt * 1000
  if (t <= Date.now()) return null
  return new Date(t).toLocaleString('en-US', {
    month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false,
  })
}

/** 提示落点边距（逻辑像素）：距光标 12px,距可用区边界 4px。 */
const TIP_GAP = 12
const TIP_EDGE = 4

/** 可用落点区间（窗口内坐标,已含边距）= 窗口矩形 ∩ 当前显示器工作区,再按提示
 * 自身尺寸收边。两者都要看：
 * - 窗口边界：出窗即被 WebView 裁掉,没有第二块画布（这也是收起态不做提示的
 * 原因:56px 宽放不下任何可读提示）;
 * - 工作区：展开态窗口四周是提示位画布,**允许挂出屏外**（贴近屏边时会被
 * 边界钳制推出去）,提示落在屏外那一半就白搭——所以取交集,可用区比窗口小。
 * 返回值保证 max ≥ min（窗口比工作区还小时退化为贴左上）。 */
function tipSpan(w: number, h: number): { minX: number; minY: number; maxX: number; maxY: number } {
  // 窗口内边界（提示必须整块留在窗内——出窗即被 WebView 裁掉）
  const minX = TIP_EDGE
  const minY = TIP_EDGE
  const winMaxX = Math.max(TIP_EDGE, window.innerWidth - w - TIP_EDGE)
  const winMaxY = Math.max(TIP_EDGE, window.innerHeight - h - TIP_EDGE)
  // 再与当前显示器工作区求交（窗口左上在屏幕上的位置 + 工作区左上）
  const sx = window.screenX
  const sy = window.screenY
  const sc = window.screen as Screen & { availLeft?: number; availTop?: number }
  if (!sc || !(sc.availWidth > 0) || !(sc.availHeight > 0)) {
    return { minX, minY, maxX: winMaxX, maxY: winMaxY }
  }
  // availLeft/availTop 是 Chromium 的非标准属性（类型定义里没有,运行时在）
  const availL = typeof sc.availLeft === 'number' ? sc.availLeft : 0
  const availT = typeof sc.availTop === 'number' ? sc.availTop : 0
  const sMinX = Math.max(minX, availL - sx + TIP_EDGE)
  const sMinY = Math.max(minY, availT - sy + TIP_EDGE)
  const sMaxX = Math.min(winMaxX, availL + sc.availWidth - sx - w - TIP_EDGE)
  const sMaxY = Math.min(winMaxY, availT + sc.availHeight - sy - h - TIP_EDGE)
  // 交集退化（窗口多半在屏外,或屏幕坐标系对不上）→ 该轴退回窗口内钳制,
  // 至少保证提示落在窗口里而不是飞到不可见处
  return {
    minX: sMaxX < sMinX ? minX : sMinX,
    minY: sMaxY < sMinY ? minY : sMinY,
    maxX: sMaxX < sMinX ? winMaxX : sMaxX,
    maxY: sMaxY < sMinY ? winMaxY : sMaxY,
  }
}

/** 提示落点：光标右下 12px 起手,放不下才翻到光标另一侧（左上）,最后钳进可用区。
 * 窗口给提示留了画布（见 EXPANDED_SIZE）,绝大多数光标位置下「右下」都放得下。 */
function placeTip(x: number, y: number, w: number, h: number): { left: number; top: number } {
  const s = tipSpan(w, h)
  let left = x + TIP_GAP
  let top = y + TIP_GAP
  if (left > s.maxX) left = x - TIP_GAP - w
  if (top > s.maxY) top = y - TIP_GAP - h
  return {
    left: Math.max(s.minX, Math.min(left, s.maxX)),
    top: Math.max(s.minY, Math.min(top, s.maxY)),
  }
}

export default function OrbWindow() {
  // 启动形态就位（get_orb_form 返回）前不请求显示——否则首帧可能是猜测形态
  const [formReady, setFormReady] = useState(false)
  useShowOnLoad(formReady)
  useOrbThemeSync()
  // 圆角方案档（与挂件/主面板同档对齐 → --widget-card-radius，orb 卡片复用该档位值）。
  useRadiusSchemeSync('widget')

  // 悬浮球窗口全程透明：宿主层不得有不透明默认背景
  useLayoutEffect(() => {
    document.documentElement.classList.add('mode-widget')
  }, [])

  // 两态：启动时贴边则恢复竖条,未贴边一律表盘。初值 = 表盘占位,挂载即以 Rust 权威形态
  // （get_orb_form）校准——**不按 window.innerWidth 猜**：页面可能早于 Rust restore 归位加载,
  // 那时窗口还是配置初始尺寸 56×116,猜成竖条后挂载对齐会把刚恢复的表盘缩回竖条。
  const [expanded, setExpanded] = useState(true)
  // 贴边停靠：docked = Rust 侧判定结果（拖动松手贴缘 dock / 离缘 undock 广播,重启由
  // get_orb_dock 恢复）;边缘由位置承担,前端只跟形态。
  const [dock, setDock] = useState<OrbDockState | null>(null)
  // 挂载恢复：停靠态 + 形态取 Rust 权威值（几何 Rust restore 已归位;IPC 经启动闸门,
  // 必在 restore 之后应答）,再按该形态做一次幂等尺寸对齐（内容锚定,位置不动）。
  // dock 广播先到（用户已拖动）则以广播为准,不拿启动快照覆盖。
  const formTouched = useRef(false)
  useLayoutEffect(() => {
    void windowService.getOrbForm().then((f) => {
      if (f && !formTouched.current) {
        setDock(f.dock)
        setExpanded(f.expanded)
        const size = f.expanded ? EXPANDED_SIZE : COLLAPSED_SIZE
        windowService.setOrbSize(size.w, size.h).catch(console.error)
      }
      setFormReady(true)
    })
  }, [])
  // 拖动松手的 dock/undock 广播（Rust orb_dock 子类化线程发）。
  // 形态切换的**几何**（尺寸 + 位置 + 内容锚定 + 显示器选择）全部在 Rust：
  // dock 走 place_docked、undock 走 orb_undock,两者都在同一次调用里原子完成。
  // 前端只切 React 形态,不再跟手调 set_orb_size——两次位置补偿叠加会让卡片横窜。
  useEffect(() => {
    let off: (() => void) | null = null
    // 挂载期竞态（StrictMode 双挂载）：`.then` 回填 unlisten 时清理函数可能已经
    // 跑过（off 仍为 null）,导致第一个监听器永不注销、事件回调执行两次。disposed
    // 标记兜住：卸载后才到达的 unlisten 立即执行。
    let disposed = false
    void events.onOrbDockChanged((p) => {
      formTouched.current = true
      if (p.docked && p.edge) {
        // dock：形态收到竖条。贴边几何（含收起态尺寸）Rust place_docked 已归位;
        // 此处 work 只是占位（Rust 侧停靠状态才是权威）。
        setDock({ edge: p.edge, anchor_y_ratio: 0.5, work: [0, 0, 0, 0] })
        setExpanded(false)
      } else {
        // undock（拖离边缘）：形态回展开卡片。尺寸与位置归位由 orb_undock 原子
        // 完成——设展开尺寸 + 内容原地长大 + 按内容所在显示器钳进工作区。
        setDock(null)
        setExpanded(true)
        windowService.orbUndock(p.edge, EXPANDED_SIZE.w, EXPANDED_SIZE.h).catch(console.error)
        // 拖离边缘展开 = 用户主动操作 → 退出待机（noteAttention 在下方定义,
        // 回调在挂载后才执行,不撞 TDZ;依赖为空的稳定回调,闭包不陈旧）
        noteAttention()
      }
    }).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [])

  const [snapshots, setSnapshots] = useState<SubscriptionSnapshot[]>([])
  // 本窗口 = orb；绑定多平台时显示上次选中的平台（启动恢复）。按平台 id 记——绑定集合
  // 变化时下标会错位;持久化在 designPrefs.orbPlatform,跟随订阅（prefs.json 异步校准 /
  // 跨窗口广播）。
  const [activePlatformId, setActivePlatformId] = useState<SubscriptionPlatform | undefined>(
    () => getDesignPrefs().orbPlatform,
  )
  useEffect(() => subscribeDesignPrefs((p) => setActivePlatformId(p.orbPlatform)), [])

  // 数据消费：初查 + subscription:changed 即时重查（Rust 侧只在读数真的变了才发,
  // 见 subscription/mod.rs）+ 5 分钟一次的漏事件兜底。
  const refresh = useCallback(() => {
    subscriptionService.getSnapshots().then((s) => {
      if (s) setSnapshots(s)
    }).catch(() => {})
  }, [])

  useEffect(() => {
    let off: (() => void) | null = null
    let disposed = false
    // 先订阅、注册完成后再初查（与待机那条 effect 同款）：初查若与注册并行,
    // 注册窗口期内的变更事件不会重放——读数会陈旧到下一轮。
    void events.onSubscriptionChanged(refresh).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
      if (!disposed) refresh()
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [refresh])

  // 秒表节拍：倒计时按 `Date.now` 渲染,没有新快照也要重算;顺带每 10 拍补查
  // 一次快照兜住漏掉的事件（见 TICK_MS / RESNAP_EVERY_TICKS）。
  const [, setTick] = useState(0)
  const ticks = useRef(0)
  useEffect(() => {
    const timer = window.setInterval(() => {
      ticks.current += 1
      setTick(ticks.current) // 只为触发重渲染——倒计时是渲染时按 Date.now() 算的
      if (ticks.current % RESNAP_EVERY_TICKS === 0) refresh()
    }, TICK_MS)
    return () => window.clearInterval(timer)
  }, [refresh])

  // 待机监控：Rust 侧待机态翻转（进入/退出）时发 subscription:idle,这里重查;
  // 待机态在 Rust 内存,重启即全亮无需恢复。
  const [idle, setIdle] = useState(false)
  useEffect(() => {
    const query = () => {
      subscriptionService.getIdle().then((v) => v !== null && setIdle(v)).catch(() => {})
    }
    let off: (() => void) | null = null
    let disposed = false
    // 先订阅、注册完成后再补查：初查若与监听注册并行,注册窗口期内的翻转事件
    // 不会重放——亮度状态会陈旧到下次翻转。
    void events.onSubscriptionIdle(query).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
      if (!disposed) query()
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [])
  // 待机开关恢复与跟随：orb 是常驻窗口,由它承担运行时值恢复（设置页改动经
  // storage 桥到这里）。Rust 侧 set 会唤醒兜底取数,只在值真变化时下发——
  // 无关 designPrefs 广播被 React state 同值 bail-out 挡住,不触发无谓刷新。
  const [idleOn, setIdleOn] = useState(() => orbIdleEnabled(getDesignPrefs()))
  useEffect(() => subscribeDesignPrefs((p) => setIdleOn(orbIdleEnabled(p))), [])
  useEffect(() => {
    subscriptionService.applyIdleEnabled(idleOn)
  }, [idleOn])
  // 用户注意（手动刷新 / 展开 / 切换平台 = 用户注意到悬浮球 → 退出待机）。本地先摘掉
  // 待机态立即恢复亮度,不等 Rust 翻转事件往返;Rust 侧清零安静计数后广播
  // subscription:idle,重查结果为权威。
  const clearStandbyLocally = useCallback(() => setIdle(false), [])
  const noteAttention = useCallback(() => {
    clearStandbyLocally()
    subscriptionService.noteAttention().catch(() => {})
  }, [clearStandbyLocally])

  // 有快照的平台序列（未绑定平台 idle 占位跳过——orb 只显示已绑定平台）
  const boundSnaps = snapshots.filter((s) => s.status !== 'idle')
  // 记住的平台未绑定 → 回落第一个（键不动,重绑后仍恢复该平台）
  const activePlatform = Math.max(0, boundSnaps.findIndex((s) => s.platform === activePlatformId))

  const snap = boundSnaps[activePlatform]
  // 读数单一来源：快照只走 get_subscription_snapshots + subscription:changed。
  // 取数由本地 token 探针驱动（本机一有新 token 就取）+ 兜底间隔覆盖网页用量,
  // 两条触发源在 Rust 侧汇成同一份快照,前端不做多通道择新。
  // 待机（standby,判据「安静起点距今满 10 分钟」）：安静起点 = 最近一次本地 agent 活动 /
  // 用户注意,Rust 侧是**全局**一个布尔（idle.rs）——待机即整体减淡 50%（.is-standby,
  // orb.css）;不分平台,切换平台不会亮度跳变。
  const standby = idleOn && boundSnaps.length > 0 && idle
  const w5h = windowOf(snap, '5h')
  const w7d = windowOf(snap, '7d') ?? windowOf(snap, '7d_opus')
  // 「剩余 = 100 − 已用」换算（口径单侧）。
  const remain7d = w7d ? Math.max(0, Math.min(100, 100 - w7d.used_percent)) : null
  const remain5h = w5h ? Math.max(0, Math.min(100, 100 - w5h.used_percent)) : null
  // 未使用态（5h 表盘 / 7d 周行）：判据与显示口径见 windowIdle。
  const fiveIdle = windowIdle(w5h)
  const weekIdle = windowIdle(w7d)
  const hint = statusHint(snap?.status ?? 'idle')
  // 竖条外轮廓水位线（从顶部往下、两侧一起退）：周额度剩余 ↦ 遮罩矩形的纵向位移——
  // 100% → 线抬到 −FADE（整圈含顶边全亮）,0% → 落在 SPAN（连淡出段一起沉到轮廓下方 = 全灭）。
  const pillLevelY =
    PILL_LEVEL_SPAN - (PILL_LEVEL_SPAN + PILL_LEVEL_FADE) * ((remain7d ?? 0) / 100)

  // 两态切换：程序化 set_orb_size（orb 不参与格网吸附）;
  // dock 态展开 = undock（离开贴边语义,清 Rust 状态,屏内生长归位在 Rust）。
  const applySize = useCallback((target: 'expanded' | 'collapsed') => {
    const size = target === 'expanded' ? EXPANDED_SIZE : COLLAPSED_SIZE
    windowService.setOrbSize(size.w, size.h).catch(console.error)
  }, [])

  const expand = useCallback(() => {
    setExpanded(true)
    noteAttention()
    if (dock) {
      // dock 态双击（无拖动）：竖条贴在缘上,直接设展开尺寸会把卡片推出屏外——
      // 展开尺寸 + 屏内归位由 orb_undock 原子完成（工作区/DPI 物理像素只在 Rust 可得）;
      // 前端不调 set_orb_size（否则位置补偿算两遍）。
      const edge = dock.edge
      setDock(null)
      windowService.orbUndock(edge, EXPANDED_SIZE.w, EXPANDED_SIZE.h).catch(console.error)
      return
    }
    // 自由态（含「自由收起」的竖条）：纯形态切换,内容锚定在 Rust 的 set_orb_size
    applySize('expanded')
  }, [applySize, dock, noteAttention])

  const collapse = useCallback(() => {
    setExpanded(false)
    applySize('collapsed')
  }, [applySize])

  // 右键菜单只留「隐藏悬浮球」一项：展开/刷新/打开设置都有既有入口（双击、刷新钮、
  // 订阅设置钮）;隐藏后从托盘/设置页/顶栏 Orbit 钮可再开。
  // 弹出位置按菜单实际尺寸 clamp 到交互主体内;收起态放不下菜单。处理器在刷新段之后
  // （收起态右键 = 刷新,要用到 refreshNow——提前定义会撞 TDZ）。
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)
  // 菜单尺寸（首次弹出时测量正;缺省按典型值钳制避免首帧越界）
  const menuRef = useRef<HTMLDivElement | null>(null)
  const [menuSize, setMenuSize] = useState({ w: 120, h: 30 })
  useLayoutEffect(() => {
    if (menu && menuRef.current) {
      const r = menuRef.current.getBoundingClientRect()
      setMenuSize((s) => (s.w === r.width && s.h === r.height ? s : { w: r.width, h: r.height }))
    }
  }, [menu])
  // 菜单钳制范围 = 交互主体：透明区不接收鼠标,菜单越出主体就点不到。主体位置用显式
  // 偏移 EXPANDED_PAD（与 Rust 命中区同源）,不按「（窗口 − 主体)/2」推算。
  const menuStyle = menu
    ? {
        left: Math.max(
          EXPANDED_PAD.l,
          Math.min(menu.x, EXPANDED_PAD.l + EXPANDED_INNER.w - menuSize.w - 2),
        ),
        top: Math.max(
          EXPANDED_PAD.t,
          Math.min(menu.y, EXPANDED_PAD.t + EXPANDED_INNER.h - menuSize.h - 2),
        ),
      }
    : undefined
  // 浮层 DOM 常驻不卸载：隐藏 = 移出视口 + visibility,禁止条件渲染
  const closeMenu = useCallback(() => setMenu(null), [])
  useEffect(() => {
    if (!menu) return
    // mousedown 在菜单内部时**不关**——否则按钮 click 落空（pointer-events 已 none）
    const onDown = (e: MouseEvent) => {
      if (!(e.target instanceof Element) || !e.target.closest('.orb-menu')) closeMenu()
    }
    const onBlur = () => closeMenu()
    window.addEventListener('mousedown', onDown)
    window.addEventListener('blur', onBlur)
    return () => {
      window.removeEventListener('mousedown', onDown)
      window.removeEventListener('blur', onBlur)
    }
  }, [menu, closeMenu])

  const menuHide = useCallback(() => {
    closeMenu()
    windowService.hideOrb().catch(console.error)
  }, [closeMenu])
  // 手动刷新：周额度环与中央 5h 表盘同时进入扫描态——各跑一条白光短弧,余量弧压暗
  // （**读数不清零**,数字/弧长仍是真值）。
  // 时长取真时刻：Rust `refresh_subscriptions_now` 只 wake + 立刻转发事件
  // （抓取在 daemon 线程,落地后另发一次）,所以「数据已落库」只能靠
  // fetched_at 越过点击时刻判定。
  //
  // 三态：
  //   waiting = 已发起、还没见新数据 → 扫掠弧 + 余量弧压暗（取数中）;
  //   landing = 新数据已落库 → **立刻摘掉压暗**,让「涨/跌」在正常亮度下走位
  //             （余量弧自己的 CSS 过渡负责把差值滑出来）,扫掠弧继续留到下限再收;
  //   idle = 全停。
  // 数据始终不来（失败时 `fetched_at` 不推进）→ 由上限直接回 idle。
  const [phase, setPhase] = useState<RefreshPhase>('idle')
  const refreshing = phase !== 'idle' // 扫掠弧 + 图标自转
  const waiting = phase === 'waiting' // 余量弧压暗（仅第一段）
  const refreshTimer = useRef<number>(0)
  const refreshStart = useRef(0) // 点击时刻（ms;与 fetched_at×1000 同尺度比较）
  const refreshNow = useCallback(() => {
    // 手动刷新 = 用户注意:Rust refresh 命令先退出待机再取数,这里只做本地即时恢复亮度
    clearStandbyLocally()
    subscriptionService.refreshNow().catch(console.error)
    refreshStart.current = Date.now()
    setPhase('waiting')
  }, [clearStandbyLocally])
  useEffect(() => () => window.clearTimeout(refreshTimer.current), [])
  // 三态推进。所有截止时刻都从 refreshStart 绝对推算（而非相对上一次定时器）,
  // 所以快照频繁更新引起的重排不会累积漂移;判据用「≥ 点击时刻」而非严格大于：
  // fetched_at 只到秒,取严会让「同一秒内抓完」的快速刷新一直跑到上限。
  useEffect(() => {
    if (phase === 'idle') return
    const elapsed = Date.now() - refreshStart.current
    const rest = (ms: number) => Math.max(0, ms - elapsed)
    window.clearTimeout(refreshTimer.current)
    if (phase === 'waiting') {
      const landed = snapshots.some((s) => (s.fetched_at ?? 0) * 1000 >= refreshStart.current)
      if (landed) {
        // 新值已落库：立刻交棒 landing（扫掠弧的下限由 landing 段继续持有）
        setPhase(elapsed >= REFRESH_MIN_MS ? 'idle' : 'landing')
      } else {
        // 还没等到数据：上限兜底（失败/无变化那几轮没有完成信号）
        refreshTimer.current = window.setTimeout(() => setPhase('idle'), rest(REFRESH_MAX_MS))
      }
      return
    }
    // landing：新值的滑移已在跑,扫掠弧留满下限即收（不留半圈残态）
    refreshTimer.current = window.setTimeout(() => setPhase('idle'), rest(REFRESH_MIN_MS))
  }, [snapshots, phase])
  // 右键：展开态 = 「Hide orb」菜单;收起态 = **刷新**。
  // 竖条的左键已是拖动（drag-region deep）+ 双击展开,再挂单击刷新就得靠延迟跟双击
  // 抢判据;而菜单在竖条窗口里放不下。故收起态右键 = 刷新：语义单义、零延迟。
  const onContextMenu = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault()
      if (!expanded) {
        refreshNow()
        return
      }
      setMenu({ x: e.clientX, y: e.clientY })
    },
    [expanded, refreshNow],
  )

  // 切换订阅（多绑定时点悬挂按钮循环;单平台时按钮禁用）。选中平台落 designPrefs
  // （启动恢复）;点击本身 = 用户注意,退出待机。
  const cyclePlatform = useCallback(() => {
    noteAttention()
    if (!boundSnaps.length) return
    const next = boundSnaps[(activePlatform + 1) % boundSnaps.length].platform
    setActivePlatformId(next)
    setDesignPrefs({ orbPlatform: next })
  }, [boundSnaps, activePlatform, noteAttention])

  // ---- hover 提示浮层（DOM 常驻） ----
  // 位置：环/表盘这类大面积区域跟光标（12px 偏移,越界翻转+钳制）;16px 悬挂
  // 按钮锚按钮中心（光标贴边时提示会跟着抖）。提示本体 pointer-events:none
  // 在 CSS——跟随光标的浮层若挡住光标会立刻触发 mouseleave,提示闪成一片。
  const [tip, setTip] = useState<{ content: TipContent; x: number; y: number; on: boolean } | null>(null)
  const tipRef = useRef<HTMLDivElement | null>(null)
  const [tipSize, setTipSize] = useState({ w: 150, h: 30 })
  // 内容变化才量尺寸（量在 layout 相位 → 与位置正同帧提交,不会闪一帧错位）
  const tipKey = tip && tip.on ? `${tip.content.label ?? ''}|${tip.content.lines.join('|')}` : ''
  useLayoutEffect(() => {
    if (!tipKey || !tipRef.current) return
    const r = tipRef.current.getBoundingClientRect()
    setTipSize((s) => (s.w === r.width && s.h === r.height ? s : { w: r.width, h: r.height }))
  }, [tipKey])
  // 显示时机状态机：悬停延迟 + 拖动抑制。运行态全部走 ref——它们只驱动定时器与判定,
  // 不进渲染（内容/位置仍走 state 的 tip,单点触发重渲染）。
  const tipShowTimer = useRef(0) // 待显示定时器
  const tipHideTimer = useRef(0) // 待收尾定时器（宽限,见 HOVER_GRACE_MS）
  const tipTarget = useRef('') // 指针所在提示区键（'' = 不在任何提示区）
  const tipShown = useRef(false) // 提示当前是否可见
  const tipPending = useRef<{ key: string; content: TipContent } | null>(null)
  const tipPos = useRef({ x: 0, y: 0 }) // 最新光标位置（延迟到点后据此落位）
  const tipMuted = useRef(false) // 拖动抑制（窗口被拖动过 → 见 HOVER_MUTE_MS）
  const tipMuteTimer = useRef(0) // 抑制的自动恢复定时器

  const cancelTipTimers = useCallback(() => {
    window.clearTimeout(tipShowTimer.current)
    window.clearTimeout(tipHideTimer.current)
    tipShowTimer.current = 0
    tipHideTimer.current = 0
  }, [])
  useEffect(
    () => () => {
      cancelTipTimers()
      window.clearTimeout(tipMuteTimer.current)
    },
    [cancelTipTimers],
  )

  /** 立即收提示（按下 / 拖动 / 两态切换）：清目标 + 清定时器,不给宽限。 */
  const clearTip = useCallback(() => {
    cancelTipTimers()
    tipTarget.current = ''
    tipPending.current = null
    tipShown.current = false
    setTip((t) => (t && t.on ? { ...t, on: false } : t))
  }, [cancelTipTimers])

  /** 指针离开一个提示区（宽限收尾,见 HOVER_GRACE_MS）。 */
  const leaveTip = useCallback(() => {
    window.clearTimeout(tipShowTimer.current)
    tipShowTimer.current = 0
    tipTarget.current = ''
    tipPending.current = null
    if (!tipShown.current) return
    window.clearTimeout(tipHideTimer.current)
    tipHideTimer.current = window.setTimeout(() => {
      tipHideTimer.current = 0
      tipShown.current = false
      setTip((t) => (t && t.on ? { ...t, on: false } : t))
    }, HOVER_GRACE_MS)
  }, [])

  /** 指针落在某个提示区（key 唯一标识区域,内容与位置随之更新）：
   * 同区内移动不重新计时（停下来才出）;已显示时换区即时换内容（环 ↔ 表盘 ↔
   * 文本行 ↔ 悬挂按钮横移不闪、不重等）;未显示时换区重新计时;
   * 拖动抑制期间一律不出。 */
  const hoverTip = useCallback((key: string, content: TipContent, x: number, y: number) => {
    if (tipMuted.current) return
    window.clearTimeout(tipHideTimer.current) // 有新区域接手 → 撤掉宽限收尾
    tipHideTimer.current = 0
    tipPos.current = { x, y }
    const sameTarget = tipTarget.current === key
    tipTarget.current = key
    tipPending.current = { key, content }
    if (tipShown.current) {
      // 已显示：同区只跟位置,换区即时换内容（内容可能随数据刷新而变,顺手更新）
      setTip({ content, x, y, on: true })
      return
    }
    if (sameTarget) return
    window.clearTimeout(tipShowTimer.current)
    tipShowTimer.current = window.setTimeout(() => {
      tipShowTimer.current = 0
      const p = tipPending.current
      // 到点时指针已换区（或已被按下/拖动清掉）→ 本次不出
      if (!p || p.key !== tipTarget.current || tipMuted.current) return
      tipShown.current = true
      setTip({ content: p.content, x: tipPos.current.x, y: tipPos.current.y, on: true })
    }, HOVER_DELAY_MS)
  }, [])

  /** 悬挂按钮（16px 小钮）：锚按钮中心而非光标——贴着按钮边缘时提示会跟着抖。 */
  const hangTip = useCallback(
    (key: string, content: TipContent) => (e: React.MouseEvent<HTMLButtonElement>) => {
      const r = e.currentTarget.getBoundingClientRect()
      hoverTip(key, content, r.left + r.width / 2, r.top + r.height / 2)
    },
    [hoverTip],
  )
  // 拖动抑制：按下 / 按住拖动 / 拖动结束三条路径都汇入 muteTips——
  // HOVER_MUTE_MS 内不出提示,超时自动恢复,不必再靠 mouseleave 复位。
  // ⚠ 拖动走系统模态移动循环：webview 收不到期间的 mousemove,鼠标抬起事件也不会送达。
  // 所以抑制不能靠 mouseup 收尾,改由三条自洽信号维持：
  //  按下（mousedown）即抑制; 移动循环结束由 Rust 判定真实位移后广播
  // orb-dragged 再续一段; 拖动中万一有 mousemove 漏进来,事件自带的 buttons≠0
  // 也会被拦下（见 onDialMove 与两行文本）。
  const muteTips = useCallback(() => {
    tipMuted.current = true
    window.clearTimeout(tipMuteTimer.current)
    tipMuteTimer.current = window.setTimeout(() => {
      tipMuteTimer.current = 0
      tipMuted.current = false
    }, HOVER_MUTE_MS)
    clearTip()
  }, [clearTip])
  useEffect(() => {
    let off: (() => void) | null = null
    let disposed = false
    void events.onOrbDragged(muteTips).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [muteTips])
  // 指针让出：光标离开交互主体 → Rust 把整窗对鼠标透明（画布不再挡下层
  // 程序的 hover / 点击）。穿透期间 webview 收不到 mouseleave,提示会"冻"在屏上
  // ——收到让出信号立刻清掉。
  useEffect(() => {
    let off: (() => void) | null = null
    let disposed = false
    void events.onOrbPointerPass((passed) => {
      if (passed) clearTip()
    }).then((unlisten) => {
      if (disposed) unlisten()
      else off = unlisten
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [clearTip])
  // 两态切换/停靠变化时元素被移出视口（left:-9999px）收不到 mouseleave,
  // 不主动清会留一块「僵尸提示」挂在窗口里
  useEffect(() => {
    clearTip()
  }, [expanded, dock, clearTip])
  const tipStyle = tip?.on ? placeTip(tip.x, tip.y, tipSize.w, tipSize.h) : undefined

  // 环内读数：表盘 = 5h 剩余 + 5h 重置时刻（HH:MM）;下方 = 周重置倒计时（三段格式）。
  // fiveIdle 时表盘与提示都走 idle 分支,reset5h 不参与显示——Codex 在
  // 未使用时给的是不断后漂的假窗口尾,显示它等于报一个永远「5 小时后」的时刻。
  const reset5h = clockAt(w5h?.resets_at)
  const weekReset = countdownDetailed(w7d?.resets_at)
  const weekStamp = stampAt(w7d?.resets_at)

  // plan 行 = 平台名 + 套餐名（codex 显示名映射为 GPT;大小写显式归一,上游曾
  // "codex Plus"）。
  const planLabel = snap?.plan_type && snap.plan_type !== 'unknown' ? snap.plan_type : boundSnaps.length ? 'Subscription' : 'Not bound'
  const planTitle = snap?.platform
    ? `${platformLabel(snap.platform)} ${capitalize(planLabel)}`
    : capitalize(planLabel)

  // 分位置提示内容：每条只给该位置对应的口径。
  // 该窗口无数据（未绑定/凭据失效）→ 退化为状态提示,正好解释读数为什么是「—」。
  const statusTip: TipContent = {
    label: snap ? planTitle : undefined,
    lines: [hint.text],
    level: hint.level,
  }
  const weeklyTip: TipContent = weekIdle
    ? { label: 'Weekly quota', lines: ['idle — updates after first use'] }
    : w7d
      ? {
          label: 'Weekly quota',
          lines: [
            `${pctText(remain7d)} / 100 left`,
            ...(weekReset ? [`resets in ${weekReset}`] : []),
          ],
        }
      : statusTip
  const fiveTip: TipContent = fiveIdle
    ? { label: '5-hour quota', lines: ['idle — updates after first use'] }
    : w5h
      ? {
          label: '5-hour quota',
          lines: [`${pctText(remain5h)} / 100 left`, ...(reset5h ? [`resets at ${reset5h}`] : [])],
        }
      : statusTip
  const resetTip: TipContent = weekIdle
    ? { label: 'Weekly reset', lines: ['idle — updates after first use'] }
    : weekStamp
      ? { label: 'Weekly reset', lines: [weekStamp] }
      : statusTip
  // 提示动作（悬挂按钮：锚按钮中心,16px 小钮上跟光标会抖）
  const btnSettingsTip: TipContent = { lines: ['Subscription settings'] }
  const btnRefreshTip: TipContent = { lines: ['Refresh now'] }
  const btnCollapseTip: TipContent = { lines: ['Collapse to strip'] }
  const btnSwitchTip: TipContent =
    boundSnaps.length > 1
      ? { label: 'Subscriptions', lines: [`Switch · ${activePlatform + 1} / ${boundSnaps.length}`] }
      : { lines: ['Only one subscription bound'] }

  // 表盘区域判定：环与中央表盘各有提示,用几何判定而非 DOM 命中——
  // 表盘 SVG 与 gauge 都是 pointer-events:none,鼠标事件一律落在容器上;底部
  // 两行/套餐行是真实元素（自带提示）,事件冒泡到容器时让行,勿覆盖。
  const dialRef = useRef<HTMLDivElement | null>(null)
  const gaugeRef = useRef<HTMLDivElement | null>(null)
  const onDialMove = useCallback(
    (e: React.MouseEvent) => {
      // 按住拖动中（buttons≠0）：一律不出提示,并续抑制（见 muteTips 注释——
      // 拖动期 webview 收不到 mouseup,只能靠 mousedown 抑制 + 这里的按钮态兜底）
      if (e.buttons !== 0) {
        muteTips()
        return
      }
      const target = e.target as Element | null
      if (target?.closest('.orb-orb-plan, .orb-orb-reset')) return
      const dial = dialRef.current
      if (!dial) return
      const d = dial.getBoundingClientRect()
      const dist = Math.hypot(e.clientX - (d.left + d.width / 2), e.clientY - (d.top + d.height / 2))
      // 容器方角（壳体圆外）不提示——那里不是任何读数
      if (dist > DIAL_R) {
        leaveTip()
        return
      }
      const g = gaugeRef.current?.getBoundingClientRect()
      // 命中半径取 min（宽, 高)/2：gauge-wrap 是整幅宽的居中容器,
      // 真正的表盘圆只有 60px（wrap 高度随内容 → 高度即直径,随 CSS 自适应）
      const gr = g ? Math.min(g.width, g.height) / 2 : 0
      const inGauge =
        !!g &&
        Math.hypot(e.clientX - (g.left + g.width / 2), e.clientY - (g.top + g.height / 2)) <= gr
      if (inGauge) hoverTip('five', fiveTip, e.clientX, e.clientY)
      else hoverTip('weekly', weeklyTip, e.clientX, e.clientY)
    },
    [fiveTip, weeklyTip, hoverTip, leaveTip, muteTips],
  )

  return (
    <div
      className={`orb-shell${expanded ? ' is-expanded' : ''}${standby ? ' is-standby' : ''}`}
      // 右键 / 双击只挂在**内容容器**（.orb-pill / .orb-orb）上：shell 是整个窗口
      // （100vw×100vh),挂它等于"透明画布也响应右键"。OS 侧由 Rust 指针让出
      // 解决（光标离开主体 → 整窗对鼠标透明）,这里是前端纵深——真漏进来一块
      // 画布区右键也不响应。
      // 提示收尾两处兜底： 按下即收 + 抑制——拖动走 OS 移动循环,期间 webview
      // 收不到 mousemove、也收不到 mouseup,只靠「拖动结束」的 orb-dragged 兜不住
      // （拖动中若有 mousemove 漏进来就会重新冒提示）;按钮豁免,点按钮不抑制
      //  指针离开窗口即收
      onMouseDownCapture={(e) => {
        if ((e.target as Element | null)?.closest('button')) return
        muteTips()
      }}
      onMouseLeave={clearTip}
    >
      {/* ---- 收起态：竖向外轮廓贴片条（外轮廓=周额度褪色,内条=5h） ----*/}
      {/* drag-region 判定器只认 HTMLElement,SVG 子树整体跳过（点在描边环上拖不动）。
          故容器挂 "deep"（tauri ≥2.11：子树内任意点触发拖动,交互元素 button 天然豁免,
          值=false 可再挖洞）,子元素不挂 drag 属性。*/}
      {/* 刷新状态类挂在本容器上,驱动扫描态（扫掠弧 + 压暗）与落位过渡。*/}
      <div
        className={`orb-pill${expanded ? ' is-hidden' : ''}${refreshing ? ' is-refreshing' : ''}${waiting ? ' is-waiting' : ''}`}
        data-tauri-drag-region="deep"
        // 右键（收起态 = 刷新）与双击展开只挂在竖条本体上,不由 shell 整窗承接
        // （is-hidden 时 pointer-events:none,天然互斥）。
        onContextMenu={onContextMenu}
        onDoubleClick={expand}
      >
        <div className="orb-pill-track">
          {/* 外轮廓 = 周额度（与展开态表盘外环同源）：轨道 rect（淡）+ 整圈
              进度轮廓（青 → 蓝 → 紫三段渐变）——stop 类与表盘外环共用
              （orb-weekly-stop-*,颜色单点定义在 CSS）。
              进度语义是「水位」：整圈轮廓常亮,由 orbPillLevelMask 遮罩切掉水位线以上的部分,
              余量减少时**两侧竖边一起向下退**,与内条自下而上的线性消耗读法对齐。
              ⚠ 几何约束：rect 不能加 rotate（21×81 竖向 rect 转 90° 会超出 24×84 的 viewBox
              被裁）;弧长统一用 pathLength=100 归一,不硬编码周长（圆角矩形真实周长 ≈ 188.6）。*/}
          <svg className="orb-pill-outline" viewBox="0 0 24 84" aria-hidden="true">
            <defs>
              <linearGradient id="orbOutlineFade" x1="0" y1="0" x2="0" y2="1">
                <stop className="orb-weekly-stop-hi" offset="0" />
                <stop className="orb-weekly-stop-mid" offset="0.5" />
                <stop className="orb-weekly-stop-lo" offset="1" />
              </linearGradient>
              {/* 水位遮罩：矩形整体纵向平移 = 水位线,线以上被遮住。
                  矩形在宽高上都留了余量,只靠 transform 移动——动画走 CSS（transform 过渡最稳）,
                  不依赖任何 SVG 几何属性的过渡。遮罩内容是**上黑下白的竖向渐变**（objectBoundingBox,
                  跟着矩形一起平移）——水位线往上 PILL_LEVEL_FADE 像素内渐隐,灰轨道到实色之间不一刀切。*/}
              <linearGradient id="orbPillLevelFade" x1="0" y1="0" x2="0" y2="1">
                <stop offset="0" stopColor="#000" />
                <stop offset={PILL_LEVEL_FADE / PILL_LEVEL_RECT_H} stopColor="#fff" />
              </linearGradient>
              <mask id="orbPillLevelMask" maskUnits="userSpaceOnUse" x="0" y="0" width="24" height="84">
                <rect
                  className="orb-pill-level" x="-4" y="0" width="32" height={PILL_LEVEL_RECT_H}
                  fill="url(#orbPillLevelFade)"
                  style={{ transform: `translateY(${pillLevelY}px)` }}
                />
              </mask>
            </defs>
            <rect x="1.5" y="1.5" width="21" height="81" rx="9" fill="none" strokeWidth="3"
              className="orb-outline-base" />
            <g className="orb-pill-fade">
              {/* 整圈轮廓常亮（闭合路径 = 无端点）;遮罩负责切水位。描边线宽与
                  外轮廓轨道一致（3）*/}
              <path
                className="orb-outline-fade"
                d="M10.5 1.5H13.5A9 9 0 0 1 22.5 10.5V73.5A9 9 0 0 1 13.5 82.5H10.5A9 9 0 0 1 1.5 73.5V10.5A9 9 0 0 1 10.5 1.5Z"
                fill="none"
                stroke="url(#orbOutlineFade)"
                strokeWidth="3"
                mask="url(#orbPillLevelMask)"
              />
            </g>
            {/* 刷新扫掠弧（与展开态表盘同族）：常态全透明,刷新时沿轮廓
                奔跑（不受水位遮罩影响,扫描是「取数中」而非读数）*/}
            <rect x="1.5" y="1.5" width="21" height="81" rx="9" fill="none" strokeWidth="3"
              className="orb-pill-sweep" pathLength="100" />
          </svg>
          {/* 内条：5h 额度（自下而上填充,消耗越多条越短）*/}
          <div className="orb-pill-inner">
            <div className="orb-pill-inner-fill" style={{ height: `${remain5h ?? 0}%` }} />
          </div>
        </div>
      </div>

      {/* ---- 展开态：周额度环 + 中央 5h 表盘 ----*/}
      {/* 形态基线：外圈 = 周额度进度环（壳体 110px 正圆,viewBox 150 等比缩放）。
          环内四件内容：5h 剩余大数（只显示剩余,不带总量）/ 5h 重置时刻 HH:MM /
          平台 + 套餐名一行（无状态点;状态文案落 hover 提示）/ 周重置倒计时（纯文字）。
          完整口径（剩余/总量/重置）与状态文案由环长本身分位置 hover 提示承载
          （零视觉占用但不丢数据）,环内不再加浮标。
          功能按钮在外圈外：右侧一列悬挂按钮,从上到下 = 订阅设置（启动主界面并直达
          Settings·Subscriptions tab）/ 刷新 / 缩小 / 切换订阅（多订阅时可用）。
          容器 deep 子树拖动,按钮豁免。*/}
      <div
        className={`orb-orb${expanded ? '' : ' is-hidden'}`}
        data-tauri-drag-region="deep"
        // 右键（展开态 = Hide orb 菜单）只挂在内容块上,画布区不响应。
        onContextMenu={onContextMenu}
      >
        <div
          className={`orb-orb-dial${refreshing ? ' is-refreshing' : ''}${waiting ? ' is-waiting' : ''}`}
          ref={dialRef}
          onMouseMove={onDialMove}
          onMouseLeave={leaveTip}
        >
          {/* 表盘 SVG：玻璃底 + 外缘高光 + 周额度环（轨道/进度,正圆）*/}
          <svg className="orb-orb-shell" viewBox={`0 0 ${DIAL_VIEW} ${DIAL_VIEW}`} aria-hidden="true">
            <defs>
              <radialGradient id="orbOrbGlass" cx="0.42" cy="0.32" r="0.86">
                <stop offset="0" stopColor="#ffffff" stopOpacity="0.62" />
                <stop offset="0.45" stopColor="#ebf2ff" stopOpacity="0.36" />
                <stop offset="0.72" stopColor="#d3e0ff" stopOpacity="0.24" />
                <stop offset="1" stopColor="#c3d2ff" stopOpacity="0.18" />
              </radialGradient>
              <linearGradient id="orbWeeklyGrad" x1="0" y1="0" x2="1" y2="1">
                <stop className="orb-weekly-stop-hi" offset="0" />
                <stop className="orb-weekly-stop-mid" offset="0.45" />
                <stop className="orb-weekly-stop-lo" offset="1" />
              </linearGradient>
            </defs>
            <circle className="orb-orb-glass" cx="75" cy="75" r={DIAL_R} fill="url(#orbOrbGlass)" />
            <circle className="orb-orb-rim" cx="75" cy="75" r={DIAL_R} fill="none" />
            <circle className="orb-orb-track" cx="75" cy="75" r={RING_R} pathLength="100" fill="none" />
            <circle
              className="orb-orb-fill" cx="75" cy="75" r={RING_R} pathLength="100" fill="none"
              style={{ strokeDasharray: `${remain7d ?? 0} 100` }}
            />
            {/* 刷新扫掠弧：常态全透明,`.is-refreshing` 时沿环奔跑
                （与余量弧同径同宽的白光短弧——余量弧压暗,读数不清零）*/}
            <circle
              className="orb-orb-sweep" cx="75" cy="75" r={RING_R} pathLength="100" fill="none"
            />
          </svg>

          {/* 中央 5h 主表盘（唯一读数：剩余大数 + 重置时刻;未使用 → 5h / idle）*/}
          <div className="orb-orb-gauge-wrap" ref={gaugeRef}>
            <FiveGauge pct={remain5h} resetAt={reset5h} idle={fiveIdle} />
          </div>

          {/* 底部两行：上排 = 周重置倒计时（较宽的读数）,下排 = 平台 + 套餐名（不加粗 / 小字号;
              不带状态点——连接状态文案在 hover 提示里）。两行位置由 CSS 的 bottom 决定,
              与 DOM 顺序无关（这里按「套餐行 → 重置行」书写）。
              两张文本各自挂提示：套餐行 → 状态文案;重置行 → 绝对时刻。
              不挂 onMouseLeave：行内 → 环/按钮横移时若先收再显会闪一帧,
              交给 dial 的判定与宽限收尾（leaveTip 由容器统一兜底）。*/}
          <div
            className="orb-orb-plan"
            onMouseMove={(e) => {
              if (e.buttons !== 0) {
                muteTips()
                return
              }
              hoverTip('plan', statusTip, e.clientX, e.clientY)
            }}
          >
            <span className="orb-orb-plan-name">{planTitle}</span>
          </div>
          <div
            className="orb-orb-reset"
            onMouseMove={(e) => {
              if (e.buttons !== 0) {
                muteTips()
                return
              }
              hoverTip('reset', resetTip, e.clientX, e.clientY)
            }}
          >
            {weekIdle ? '7d idle' : weekReset ?? '—'}
          </div>
        </div>

        {/* 外挂按钮列（外圈外部;从上到下：管理订阅 / 刷新 / 缩小 / 切换订阅）。
            提示一律走自绘浮层,不用原生 title。*/}
        <div className="orb-orb-hang">
          <button
            className="orb-hang-btn"
            onClick={() => windowService.openMainAtView('settings', 'subscriptions').catch(console.error)}
            onMouseEnter={hangTip('btn:settings', btnSettingsTip)}
            onMouseLeave={leaveTip}
            aria-label="Subscription settings"
          >
            <SettingsIcon />
          </button>
          <button
            className="orb-hang-btn"
            onClick={refreshNow}
            onMouseEnter={hangTip('btn:refresh', btnRefreshTip)}
            onMouseLeave={leaveTip}
            aria-label="Refresh now"
          >
            <RefreshIcon spinning={refreshing} />
          </button>
          <button
            className="orb-hang-btn"
            onClick={collapse}
            onMouseEnter={hangTip('btn:collapse', btnCollapseTip)}
            onMouseLeave={leaveTip}
            aria-label="Collapse"
          >
            <CollapseIcon />
          </button>
          <button
            className="orb-hang-btn"
            onClick={cyclePlatform}
            disabled={boundSnaps.length <= 1}
            onMouseEnter={hangTip('btn:switch', btnSwitchTip)}
            onMouseLeave={leaveTip}
            aria-label="Switch subscription"
          >
            <SwitchIcon />
          </button>
        </div>
      </div>

      {/* hover 提示浮层（DOM 常驻——透明 WebView2 上条件卸载留脏像素）。
          两级排版：小字标签 = 口径名,主读数 + 补充行;状态类提示行首带四态点。
          pointer-events:none 见 CSS。*/}
      <div
        ref={tipRef}
        className={`orb-tip${tip?.on ? '' : ' is-hidden'}`}
        style={tipStyle}
        role="tooltip"
        aria-hidden={!tip?.on}
      >
        {tip?.content.label ? <div className="orb-tip-label">{tip.content.label}</div> : null}
        <div className="orb-tip-primary">
          {tip?.content.level ? (
            <span className={`orb-card-dot is-${tip.content.level}`} aria-hidden="true" />
          ) : null}
          <span className="orb-tip-line is-primary">{tip?.content.lines[0] ?? ''}</span>
        </div>
        {(tip?.content.lines.slice(1) ?? []).map((l) => (
          <div key={l} className="orb-tip-line">{l}</div>
        ))}
      </div>

      {/* 右键菜单（只有隐藏项;DOM 常驻浮层;位置钳在交互主体内,收起态不弹）*/}
      <div
        ref={menuRef}
        className={`orb-menu${menu ? '' : ' is-hidden'}`}
        style={menuStyle}
      >
        <button onClick={menuHide}>Hide orb</button>
      </div>
    </div>
  )
}

/** 中央 5h 主表盘：环形进度 + 剩余大数 + 重置时刻。
 * 口径单侧——环长与数字都是「剩余量」（与收起态竖条同向）;总量与重置口径在 hover 提示里。
 * 窗口存在但零消耗（idle）→ 主读数「5h」+ 副行「idle」（满环不变）,两平台「未使用」
 * 的读数形态由此统一。 */
function FiveGauge({ pct, resetAt, idle }: { pct: number | null; resetAt: string | null; idle: boolean }) {
  const r = 46 // viewBox 100 固定半径,外层 CSS 缩放到 60px
  const circ = 2 * Math.PI * r
  const valid = pct !== null
  const shown = pct ?? 0
  return (
    <div className={`orb-gauge${valid ? '' : ' is-empty'}`}>
      <svg viewBox="0 0 100 100" aria-hidden="true">
        <defs>
          <linearGradient id="orbFiveGrad" x1="0" y1="0" x2="1" y2="1">
            <stop className="orb-gauge-stop-hi" offset="0" />
            <stop className="orb-gauge-stop-lo" offset="1" />
          </linearGradient>
        </defs>
        <circle cx="50" cy="50" r={r} className="orb-gauge-track" strokeWidth="6.5" fill="none" />
        {/* 动画值写 inline style 而不是 SVG 属性：只有 CSS 属性才能可靠
            触发 .orb-gauge-fill 上的 transition（presentation attribute → CSS
            的映射各实现不一致）*/}
        <circle
          cx="50" cy="50" r={r}
          className="orb-gauge-fill"
          strokeWidth="6.5" fill="none"
          style={{ strokeDasharray: circ, strokeDashoffset: circ * (1 - shown / 100) }}
          transform="rotate(-90 50 50)"
        />
        {/* 刷新扫掠弧：与余量弧同起点（rotate（-90) → 12 点）+ 同宽,
            pathLength 归一到 100 供 CSS 的 dashoffset 动画使用*/}
        <circle
          cx="50" cy="50" r={r}
          className="orb-gauge-sweep"
          strokeWidth="6.5" fill="none"
          pathLength="100"
          transform="rotate(-90 50 50)"
        />
      </svg>
      <div className="orb-gauge-center">
        <span className="orb-gauge-num">{idle ? '5h' : valid ? Math.round(shown) : '—'}</span>
        <span className="orb-gauge-reset">{idle ? 'idle' : resetAt ?? '—'}</span>
      </div>
    </div>
  )
}

/** 悬挂按钮：管理订阅（滑块式设置图标,跳订阅设置页）。 */
function SettingsIcon() {
  return (
    <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor"
      strokeWidth="1.4" strokeLinecap="round" aria-hidden="true">
      <path d="M2.4 5.2h11.2M2.4 10.8h11.2" />
      <circle cx="6.1" cy="5.2" r="1.75" />
      <circle cx="9.9" cy="10.8" r="1.75" />
    </svg>
  )
}

/** 悬挂按钮：切换订阅（双向箭头;多订阅绑定时可点）。 */
function SwitchIcon() {
  return (
    <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor"
      strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M2.6 5.6h10.2M10.5 3.2l2.4 2.4-2.4 2.4" />
      <path d="M13.4 10.4H3.2M5.5 8l-2.4 2.4 2.4 2.4" />
    </svg>
  )
}

function CollapseIcon() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
      <path d="M2 4 L5 1 L8 4 M2 6 L5 9 L8 6" fill="none" stroke="currentColor" strokeWidth="1.2" />
    </svg>
  )
}

function RefreshIcon({ spinning }: { spinning: boolean }) {
  return (
    <svg
      className={spinning ? 'is-spinning' : ''}
      width="11" height="11" viewBox="0 0 12 12" aria-hidden="true"
    >
      <path
        d="M10.5 6 A4.5 4.5 0 1 1 8.6 2.45 M8.4 0.9 L8.7 2.6 L7 2.9"
        fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"
      />
    </svg>
  )
}
