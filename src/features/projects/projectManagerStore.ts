// 项目管理弹出层开合:Tasks / Insights 的「Manage projects…」入口与
// FullWindow 常驻的 ProjectManagerModal 之间的模块级开关（不新开 Tauri 窗口,不走 props 穿透）。

let open = false
const listeners = new Set<(open: boolean) => void>()

function emit(next: boolean): void {
  if (open === next) return
  open = next
  for (const fn of listeners) fn(open)
}

export function openProjectManager(): void {
  emit(true)
}

export function closeProjectManager(): void {
  emit(false)
}

export function isProjectManagerOpen(): boolean {
  return open
}

export function subscribeProjectManager(fn: (open: boolean) => void): () => void {
  listeners.add(fn)
  return () => listeners.delete(fn)
}
