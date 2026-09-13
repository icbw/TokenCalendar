// 事件订阅封装：非 Tauri 环境（纯浏览器布局调试）下返回空退订函数。

import { inTauri } from './tauri'
import type { ChangedKeys } from './contract'

export type Unlisten = () => void

async function listen<T>(event: string, cb: (payload: T) => void): Promise<Unlisten> {
  if (!inTauri) return () => {}
  const { listen } = await import('@tauri-apps/api/event')
  return listen<T>(event, (e) => cb(e.payload))
}

export function onUsageChanged(cb: () => void): Promise<Unlisten> {
  return listen<ChangedKeys>('usage:changed', () => cb())
}

// 双窗口可见性广播（Rust visibility.rs 单一源），载荷为 bool。
export function onWidgetVisibilityChanged(cb: (visible: boolean) => void): Promise<Unlisten> {
  return listen<boolean>('widget-visibility-changed', (v) => cb(v))
}

// 悬浮球可见性广播（同族单一源），载荷为 bool。
export function onOrbVisibilityChanged(cb: (visible: boolean) => void): Promise<Unlisten> {
  return listen<boolean>('orb-visibility-changed', (v) => cb(v))
}

export function onMainVisibilityChanged(cb: (visible: boolean) => void): Promise<Unlisten> {
  return listen<boolean>('main-visibility-changed', (v) => cb(v))
}

// 自绘标题栏：主窗口最大化状态广播（Rust Resized 去重后发），载荷为 bool。
export function onMainMaximizedChanged(cb: (maximized: boolean) => void): Promise<Unlisten> {
  return listen<boolean>('main-maximized-changed', (m) => cb(m))
}

// 挂件网格吸附落定（Rust 仅在量化位移真变时发），载荷为停靠顶点索引。
export function onWidgetSnapLanded(
  cb: (vertex: { col: number; row: number }) => void,
): Promise<Unlisten> {
  return listen<{ col: number; row: number }>('widget-snap-landed', (v) => cb(v))
}

// 订阅快照变更（Rust 轮询 daemon 每轮/绑定/解绑/手动刷新后发）。
export function onSubscriptionChanged(cb: () => void): Promise<Unlisten> {
  return listen<boolean>('subscription:changed', () => cb())
}

// 悬浮球贴边停靠（Rust orb_dock 子类化线程在拖动松手后发）。
export function onOrbDockChanged(
  cb: (payload: { docked: boolean; edge: 'left' | 'right' | null }) => void,
): Promise<Unlisten> {
  return listen<{ docked: boolean; edge: 'left' | 'right' | null }>('orb-dock-changed', (p) => cb(p))
}

// ㉝悬浮球被拖动（Rust orb_dock 在移动循环结束时判定窗口确实位移后发）：
// 拖动过的这一次悬停不出 hover 提示（前端抑制,指针离开窗口复位）。
export function onOrbDragged(cb: () => void): Promise<Unlisten> {
  return listen<boolean>('orb-dragged', () => cb())
}

// ㊻悬浮球「指针让出」（Rust orb_dock：光标离开交互主体 → 整窗对鼠标
// 透明,回到主体 → 恢复）。让出期间 webview 收不到 mouseleave/mousemove,提示浮层
// 必须由这条信号主动收起,否则会留一块"冻住"的提示挂在画布上。
export function onOrbPointerPass(cb: (passed: boolean) => void): Promise<Unlisten> {
  return listen<boolean>('orb-pointer-pass', (p) => cb(p))
}
