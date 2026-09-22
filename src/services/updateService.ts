// 应用更新（设置·About）：检查更新 / 下载 / 安装 / 公开仓入口。
// 更新源 = 公开仓 Release 的 latest.json（tauri.conf.json 的 plugins.updater）；
// 安装包签名校验由 updater 插件按配置里的 pubkey 执行——校验不通过的包不会进入安装步骤。
// 非 Tauri 环境（浏览器布局调试）一律降级为 unavailable，不抛错。
//
// 下载与安装分两步：启动时的自动检查只**预下载**安装包并发系统通知（autoCheckAndDownload），
// 安装永远由用户在设置·About 点按钮触发（installReadyUpdate / available.install）。
// 下载与安装都是进程内单飞：自动预下载与手动安装共用同一份安装包、同一个安装器——
// 两个安装器并行会在覆盖正在运行的 tokencalendar.exe 时弹「Error opening file for writing」。
//
// dev 构建不提供安装：dev 与安装版 identifier 不同（tw 窗口类/数据根都隔离），
// dev 里执行安装会把正式版装进系统——只允许检查。

import type { Update } from '@tauri-apps/plugin-updater'
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
      /** 下载（已预下载则跳过）并安装（签名校验在插件内部完成）。Windows 上安装阶段应用会退出。 */
      install(onProgress: (p: UpdateProgress) => void): Promise<void>
    }
  | { status: 'error'; message: string }

/** 已下载完、等用户点安装的新版本。 */
export interface ReadyUpdate {
  version: string
  notes: string | null
}

let ready: { info: ReadyUpdate; update: Update } | null = null
const readyListeners = new Set<(r: ReadyUpdate | null) => void>()
let downloading: { version: string; promise: Promise<Update>; listeners: Set<(p: UpdateProgress) => void> } | null = null
let installing: Promise<void> | null = null

function errText(e: unknown): string {
  const s = e instanceof Error ? e.message : String(e)
  // 常见首跑态：公开仓还没有带 latest.json 的 Release——给可读文案。
  if (/404|not found|Could not fetch|valid release json/i.test(s)) {
    return 'No published release found.'
  }
  if (/network|dns|connect|timed? ?out/i.test(s)) {
    return 'Network error while checking for updates.'
  }
  // 安装包验签失败（如 tauri.conf.json 的 pubkey 与签名私钥不配套，下载到 100% 后在此抛错）：
  // 自动安装没有出路，文案直接指向手动下载。
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

/** 已预下载、待安装的新版本（无则 null）。 */
export function getReadyUpdate(): ReadyUpdate | null {
  return ready?.info ?? null
}

/** 订阅「新版本已下载就绪」状态变化;返回退订函数。 */
export function subscribeReadyUpdate(fn: (r: ReadyUpdate | null) => void): () => void {
  readyListeners.add(fn)
  return () => {
    readyListeners.delete(fn)
  }
}

/** 下载单飞：同一版本已下载完直接复用,正在下载则挂上进度一起等。 */
function downloadOnce(update: Update, onProgress?: (p: UpdateProgress) => void): Promise<Update> {
  if (ready?.info.version === update.version) return Promise.resolve(ready.update)
  if (downloading?.version === update.version) {
    if (onProgress) downloading.listeners.add(onProgress)
    return downloading.promise
  }
  const listeners = new Set<(p: UpdateProgress) => void>()
  if (onProgress) listeners.add(onProgress)
  const emit = (p: UpdateProgress) => listeners.forEach((fn) => fn(p))
  let downloaded = 0
  let total = 0
  const promise = update
    .download((e) => {
      if (e.event === 'Started') {
        total = e.data.contentLength ?? 0
        emit({ downloaded, total, finished: false })
      } else if (e.event === 'Progress') {
        downloaded += e.data.chunkLength
        emit({ downloaded, total, finished: false })
      } else {
        emit({ downloaded, total, finished: true })
      }
    })
    .then(() => {
      ready = { info: { version: update.version, notes: update.body ?? null }, update }
      readyListeners.forEach((fn) => fn(ready!.info))
      return update
    })
    .finally(() => {
      downloading = null
    })
  downloading = { version: update.version, promise, listeners }
  return promise
}

/** 安装单飞：Windows 上安装器接管后进程即退出,走不到 finally;失败才放行下一次。 */
function installOnce(update: Update): Promise<void> {
  if (!installing) {
    installing = update.install().finally(() => {
      installing = null
    })
  }
  return installing
}

/** 安装已预下载的新版本（设置·About 的 Install 按钮）。 */
export async function installReadyUpdate(): Promise<void> {
  if (!ready) throw new Error('No downloaded update to install.')
  if (IS_DEV) throw new Error('Dev build: install is disabled.')
  await installOnce(ready.update)
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
        const u = await downloadOnce(update, onProgress)
        await installOnce(u)
      },
    }
  } catch (e) {
    return { status: 'error', message: errText(e) }
  }
}

/** 系统通知「新版本已下载好」;通知失败只记日志,不影响就绪状态。 */
async function notifyReady(info: ReadyUpdate): Promise<void> {
  try {
    const { isPermissionGranted, requestPermission, sendNotification } = await import('@tauri-apps/plugin-notification')
    const granted = (await isPermissionGranted()) || (await requestPermission()) === 'granted'
    if (!granted) return
    sendNotification({
      title: `TokenCalendar ${info.version} is ready`,
      body: 'The update has been downloaded. Open Settings → About and click Install to update.',
    })
  } catch (e) {
    console.error('[update] notification failed:', e)
  }
}

/**
 * 启动时的自动检查：有新版就在后台下载安装包,下载完发系统通知,**不安装**。
 * dev 构建整段跳过（安装本就禁用,预下载无意义）。
 */
export async function autoCheckAndDownload(): Promise<ReadyUpdate | null> {
  if (!inTauri || IS_DEV) return null
  const { check } = await import('@tauri-apps/plugin-updater')
  const update = await check()
  if (!update) return null
  await downloadOnce(update)
  const info = getReadyUpdate()
  if (info) await notifyReady(info)
  return info
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
