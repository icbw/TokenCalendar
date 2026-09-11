// Tauri 运行环境探测 + invoke 容错封装。
// 失败统一返回 null，保持旧项目「取数失败 → 降级 mock/空态」的路径不变。

export const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

export async function tryInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (!inTauri) return null
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return await invoke<T>(cmd, args)
  } catch (e) {
    console.error(`[invoke] ${cmd} failed:`, e)
    return null
  }
}
