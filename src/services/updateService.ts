// 应用更新（设置·About）：检查更新 / 下载安装 / 公开仓入口。
// 更新源 = 公开仓 Release 的 latest.json（tauri.conf.json 的 plugins.updater）；
// 安装包签名校验由 updater 插件内置执行（按配置里的 pubkey 验签）——校验不
// 通过的包不会进入安装步骤。非 Tauri 环境（浏览器布局调试）一律降级为
// unavailable，不抛错、不改变既有降级路径。
//
// dev 构建不提供安装：dev 与安装版 identifier 不同（tw 窗口类/数据根都隔离），
// dev 里执行安装会把正式版装进系统，与「开发中」语境不符——只允许检查。

import { inTauri, tryInvoke } from './tauri'

/** 公开仓 Releases 页（版本号按钮落点；手动下载走这里）。 */
export const RELEASES_URL = 'https://github.com/icbw/TokenCalendar/releases'

/** dev 构建：可以检查，但不提供安装动作。 */
const IS_DEV = import.meta.env.DEV

export interface UpdateProgress {
  /** 已下载字节（Started 前为 0）。 */
  downloaded: number
  /** 总字节（服务端未给 Content-Length 时为 0，进度按未知处理）。 */
  total: number
  /** 下载完成、等待安装器接管。 */
  finished: boolean
}

export type UpdateCheck =
  /** 非 Tauri 环境 / 插件不可用。 */
  | { status: 'unavailable' }
  | { status: 'up-to-date'; current: string }
  | {
      status: 'available'
      current: string
      version: string
      notes: string | null
      /** 当前构建是否允许执行安装（dev 构建为 false）。 */
      canInstall: boolean
      /** 下载并安装（签名校验在插件内部完成）。Windows 上安装阶段应用会退出。 */
      install(onProgress: (p: UpdateProgress) => void): Promise<void>
    }
  | { status: 'error'; message: string }

function errText(e: unknown): string {
  const s = e instanceof Error ? e.message : String(e)
  // 常见首跑态：公开仓还没有带 latest.json 的 Release——给可读文案。
  if (/404|not found|Could not fetch|valid release json/i.test(s)) {
    return 'No published release found.'
  }
  if (/network|dns|connect|timed? ?out/i.test(s)) {
    return 'Network error while checking for updates.'
  }
  // 安装包验签失败（公钥与签名不配套等）：自动安装没有出路，直接指向手动下载。
  // 事故背景：tauri.conf.json 的 pubkey 曾与私钥不配套，下载到 100% 后
  // 在此处抛错——此前原文文案不指向任何动作，用户只看到「没有开始安装」。
  if (/signature|minisign|verification failed/i.test(s)) {
    return 'The downloaded package failed signature verification. Download the installer from the releases page instead.'
  }
  return s
}

/** 当前应用版本（Tauri package_info；非 Tauri 返回 null）。 */
export async function currentVersion(): Promise<string | null> {
  if (!inTauri) return null
  try {
    const { getVersion } = await import('@tauri-apps/api/app')
    return await getVersion()
  } catch (e) {
    console.error('[update] getVersion failed:', e)
    return null
  }
}

/**
 * 检查更新：有新版本 → available（install 闭包即下载安装入口）；
 * 无新版本 → up-to-date；网络/解析失败 → error（文案已归一）。
 */
export async function checkForUpdate(): Promise<UpdateCheck> {
  if (!inTauri) return { status: 'unavailable' }
  const current = (await currentVersion()) ?? ''
  try {
    const { check } = await import('@tauri-apps/plugin-updater')
    const update = await check()
    if (!update) return { status: 'up-to-date', current }
    return {
      status: 'available',
      current,
      version: update.version,
      notes: update.body ?? null,
      canInstall: !IS_DEV,
      install: async (onProgress) => {
        let downloaded = 0
        let total = 0
        await update.downloadAndInstall((e) => {
          if (e.event === 'Started') {
            total = e.data.contentLength ?? 0
            onProgress({ downloaded, total, finished: false })
          } else if (e.event === 'Progress') {
            downloaded += e.data.chunkLength
            onProgress({ downloaded, total, finished: false })
          } else {
            onProgress({ downloaded, total, finished: true })
          }
        })
      },
    }
  } catch (e) {
    return { status: 'error', message: errText(e) }
  }
}

/** 打开公开仓 Releases 页（系统浏览器；版本号按钮与手动下载入口共用）。 */
export async function openReleasesPage(): Promise<boolean> {
  if (!inTauri) {
    window.open(RELEASES_URL, '_blank', 'noopener')
    return true
  }
  const r = await tryInvoke<null>('open_external_url', { url: RELEASES_URL })
  return r !== null
}
