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
}

/** 首帧提交后调用（useShowOnLoad）：后端按可见性单一源裁决是否 show 本窗口。 */
export async function windowReady(): Promise<void> {
  await tryInvoke<null>('window_ready')
}

export async function getVisibility(): Promise<WindowVisibility | null> {
  return tryInvoke<WindowVisibility>('get_visibility')
}

async function apply(
  label: 'widget' | 'main' | 'orb',
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

/** 悬浮球两态尺寸切换（窗口 resizable=false,程序化是唯一入口）。 */
export async function setOrbSize(width: number, height: number): Promise<void> {
  await tryInvoke<null>('set_orb_size', { width, height })
}

/** 悬浮球停靠状态查询（OrbWindow 挂载时恢复 dock 态）。
 *  edge: "left" | "right";anchor_y_ratio = 竖条中心相对工作区顶部比例。 */
export interface OrbDockState {
  edge: 'left' | 'right'
  anchor_y_ratio: number
  work: [number, number, number, number]
}

export async function getOrbDock(): Promise<OrbDockState | null> {
  return tryInvoke<OrbDockState | null>('get_orb_dock')
}

/** 前端发起的 undock（双击展开/拖离边缘展开）：清 Rust 侧停靠状态,并按
 *  origin 侧把窗口位置钳回屏内（expand-ready 归位——dock 态窗口贴死缘,
 *  直接展开会出屏;无广播,发起方已知新形态）。edge=null 时仅清状态。 */
export async function orbUndock(edge?: 'left' | 'right' | null): Promise<void> {
  await tryInvoke<null>('orb_undock', { edge: edge ?? null })
}

/** 打开主窗口并定位指定视图/tab（orb「Manage subscriptions」跳转;
 *  view 位置经 sessionStorage 中转,FullWindow 挂载时消费后清除）。 */
export async function openMainAtView(view: string, tab?: string): Promise<void> {
  try {
    sessionStorage.setItem('tokencalendar.main.nav', JSON.stringify({ view, tab }))
  } catch {
    /* private mode */
  }
  await apply('main', 'show')
}

/** 恢复设计默认 widget 尺寸（重置按钮 / 宽高比锁回吸共用）。
 *  snapAnchor=true 时后端在 set_size 后以停靠顶点为锚
 *  重算位置（右上角保持在该顶点）——仅档位切换路径使用；比例锁回写/重置按钮
 *  不传（手动拉伸例外）。 */
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
 *  关闭档（探测失败静默回退哲学，设计文档 ：apply 失败不得拖垮窗口）。 */
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
