// 窗口可见性封装（取代退役的 getMode/setMode 互斥模式）。
// 几何/可见性单一源在 Rust 侧（visibility.rs）：前端只发意图
// （show/hide/toggle），状态以 window_ready 后端裁决 + 可见性事件广播为准。
// 浏览器布局调试：直接打开 /widget.html 即挂件入口（多入口装配，无 URL 参数约定）。

import { inTauri, tryInvoke } from './tauri'

export interface WindowVisibility {
  widget: boolean
  main: boolean
  /** 悬浮球（get_visibility 载荷三键齐发）。 */
  orb: boolean
  /** 项目推进时间轴（第四键）。 */
  timeline: boolean
}

/** 首帧提交后调用（useShowOnLoad）：后端按可见性单一源裁决是否 show 本窗口。 */
export async function windowReady(): Promise<void> {
  await tryInvoke<null>('window_ready')
}

export async function getVisibility(): Promise<WindowVisibility | null> {
  return tryInvoke<WindowVisibility>('get_visibility')
}

async function apply(
  label: 'widget' | 'main' | 'orb' | 'timeline',
  action: 'show' | 'hide' | 'toggle',
): Promise<void> {
  await tryInvoke<null>(`${action}_${label}`)
}

export function showWidget(): Promise<void> {
  return apply('widget', 'show')
}
export function hideWidget(): Promise<void> {
  return apply('widget', 'hide')
}
export function toggleWidget(): Promise<void> {
  return apply('widget', 'toggle')
}
export function showMain(): Promise<void> {
  return apply('main', 'show')
}
export function hideMain(): Promise<void> {
  return apply('main', 'hide')
}
export function toggleMain(): Promise<void> {
  return apply('main', 'toggle')
}
// ---- 悬浮球（第三窗口；托盘/设置页/主界面顶栏 Orbit 按钮共用） ----
export function showOrb(): Promise<void> {
  return apply('orb', 'show')
}
export function hideOrb(): Promise<void> {
  return apply('orb', 'hide')
}
/** 顶栏 Orbit 按钮（与 toggleWidget 同语义）；按钮态以 orb-visibility-changed 事件为准。 */
export function toggleOrb(): Promise<void> {
  return apply('orb', 'toggle')
}

// ---- 项目推进时间轴（第四窗口；托盘/设置页共用，状态以 timeline-visibility-changed 为准） ----
export function showTimeline(): Promise<void> {
  return apply('timeline', 'show')
}
export function hideTimeline(): Promise<void> {
  return apply('timeline', 'hide')
}
export function toggleTimeline(): Promise<void> {
  return apply('timeline', 'toggle')
}

/** 时间轴两态（看板 / 条态）。 */
/** board = 看板;strip = 贴顶条;peek = 条态收成几像素细边。 */
export type TimelineForm = 'board' | 'strip' | 'peek'

export async function getTimelineForm(): Promise<TimelineForm | null> {
  return tryInvoke<TimelineForm>('get_timeline_form')
}

/** 形态切换一个执行者：Rust 原子完成尺寸 + 位置 + 置顶。只在用户 / 计时器确有切换意图时调用——
 * 单纯的宽度更新走 setTimelineStripWidth。
 * stripWidth = 条态内容 CSS 宽（前端量出,Rust 按该屏 scale × 文本大小换算并钳到工作区）。 */
export async function setTimelineForm(form: TimelineForm, stripWidth?: number): Promise<TimelineForm | null> {
  return tryInvoke<TimelineForm>('set_timeline_form', { form, stripWidth })
}

/** 条态内容宽更新（量宽回调专用）：不切形态,Rust 在看板态忽略——量宽回调与形态广播先后不定,
 * 迟到的宽度更新不得把刚展开的看板折回条态。 */
export async function setTimelineStripWidth(width: number): Promise<void> {
  await tryInvoke<null>('set_timeline_strip_width', { width })
}

/** 时间轴看板窗口风格：true = Shadow（DWM 阴影 + 透明呼吸位,同主窗口）/ false = Flat。条态恒 Flat。 */
export async function setTimelineStyle(floating: boolean): Promise<void> {
  await tryInvoke<null>('set_timeline_style', { floating })
}

/** 悬浮球两态尺寸切换（窗口 resizable=false,程序化是唯一入口）。 */
export async function setOrbSize(width: number, height: number): Promise<void> {
  await tryInvoke<null>('set_orb_size', { width, height })
}

/** 悬浮球停靠状态。
 * edge: "left" | "right";anchor_y_ratio = 竖条中心相对工作区顶部比例。 */
export interface OrbDockState {
  edge: 'left' | 'right'
  anchor_y_ratio: number
  work: [number, number, number, number]
}

/** 悬浮球启动形态（Rust 权威:restore 后的停靠态 + 形态;OrbWindow 挂载时恢复）。
 * 不要按 window.innerWidth 猜形态——页面可能早于 Rust 归位加载,窗口还是配置初始尺寸。 */
export interface OrbForm {
  dock: OrbDockState | null
  expanded: boolean
}

export async function getOrbForm(): Promise<OrbForm | null> {
  return tryInvoke<OrbForm>('get_orb_form')
}

/** 前端发起的 undock（双击展开/拖离边缘展开）：清 Rust 侧停靠状态,并**原子**
 * 完成展开归位——设展开尺寸 + 内容原地长大 + 按内容所在显示器的 scale/工作区
 * 钳进屏内（无广播,发起方已知新形态）。edge=null 时仅清状态。
 * ⚠ 调用方**不要再跟手调 setOrbSize**：位置补偿已包含在本命令里,再调一次
 * set_orb_size（内容锚定）会把补偿算两遍,卡片横向窜两百像素。
 * expandW/expandH = 展开后目标窗口尺寸（逻辑像素）;缺省用 Rust 侧常量。 */
export async function orbUndock(
  edge?: 'left' | 'right' | null,
  expandW?: number,
  expandH?: number,
): Promise<void> {
  await tryInvoke<null>('orb_undock', {
    edge: edge ?? null,
    expandW: expandW ?? null,
    expandH: expandH ?? null,
  })
}

/** 主窗口导航中转键（openMainAtView 写 / FullWindow 消费）。
 * **必须 localStorage**：orb 与 main 是两个 WebView 窗口,sessionStorage 按
 * 窗口隔离（orb 写入 main 永远读不到——旧版按钮「只唤起不跳转」的根因）;
 * localStorage 跨窗口共享,且其它窗口写入会触发 main 的 storage 事件
 * （窗口已启动只是隐藏时也能即时导航,与 designPrefs 桥同款机制）。 */
export const MAIN_NAV_KEY = 'tokencalendar.main.nav'

/** 打开主窗口并定位指定视图/tab（orb「订阅设置」按钮跳转;
 * 位置经 localStorage 中转,FullWindow 挂载消费一次 + 常驻 storage 事件监听）。 */
export async function openMainAtView(view: string, tab?: string): Promise<void> {
  try {
    localStorage.setItem(MAIN_NAV_KEY, JSON.stringify({ view, tab }))
  } catch {
    /* private mode */
  }
  await apply('main', 'show')
}

/** 恢复设计默认 widget 尺寸（重置按钮 / 宽高比锁回吸共用）。
 * ：snapAnchor=true 时后端在 set_size 后以停靠顶点为锚
 * 重算位置（右上角保持在该顶点）——仅档位切换路径使用；比例锁回写/重置按钮
 * 不传（手动拉伸例外）。 */
export async function setWidgetSize(
  width: number,
  height: number,
  snapAnchor?: boolean,
): Promise<void> {
  await tryInvoke<null>('set_widget_size', { width, height, snapAnchor })
}

// ---- 主窗口控制 ----

export async function mainMinimize(): Promise<void> {
  await tryInvoke<null>('main_minimize')
}

export async function mainToggleMaximize(): Promise<void> {
  await tryInvoke<null>('main_toggle_maximize')
}

/** 关闭 = 隐藏（退出唯一入口在托盘）。 */
export async function mainClose(): Promise<void> {
  await tryInvoke<null>('main_close')
}

// ---- 毛玻璃材质（spike；Rust 侧 effects.rs） ----

/** 应用窗口材质档。effect=null = 关闭。返回是否成功——失败时调用方回退
 * 关闭档（探测失败静默回退哲学，设计文档 ：apply 失败不得拖垮窗口）。 */
export async function setWindowMaterial(
  label: 'widget' | 'main',
  effect: 'mica' | 'acrylic' | null,
  dark: boolean,
): Promise<boolean> {
  if (!inTauri) return false
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    await invoke('set_window_material', { label, effect: effect ?? 'none', dark })
    return true
  } catch (e) {
    console.error(`[material] set_window_material(${label}, ${effect}) failed:`, e)
    return false
  }
}
