// 开机自启（设置·General）：**状态单一源 = 系统启动项**（Windows 为 HKCU Run 键，
// Rust 侧 autostart 插件读写），前端不落 prefs.json 镜像——与「可见性单一源」同款
// 口径：勾选态每次挂载现查，系统侧被外部改动（Windows 设置·启动应用）也如实反映。
// dev 构建后端返回 supported=false（会把开发二进制注册成自启），前端据此禁用勾选框。

import { tryInvoke } from './tauri'

export interface AutostartInfo {
  /** 是否已注册开机自启（系统侧被禁用也算未启用）。 */
  enabled: boolean
  /** 当前构建是否允许改动（dev 构建为 false → 勾选框禁用）。 */
  supported: boolean
}

/** 读当前自启状态（非 Tauri 环境 → null，勾选框保持禁用）。 */
export async function getAutostart(): Promise<AutostartInfo | null> {
  return tryInvoke<AutostartInfo>('get_autostart')
}

/** 设为目标态（幂等；后端写完回读）。失败 → null，调用方回读真实状态。 */
export async function setAutostart(enabled: boolean): Promise<AutostartInfo | null> {
  return tryInvoke<AutostartInfo>('set_autostart', { enabled })
}
