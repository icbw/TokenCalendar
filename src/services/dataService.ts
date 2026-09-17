// 数据管理契约封装。
// 全部经 tryInvoke 容错:非 Tauri 环境（纯前端 dev）返回 null,UI 走降级文案。

import { tryInvoke } from './tauri'

export interface DataInfo {
  root: string
  custom_root: string | null
  default_root: string
  fell_back: boolean
  db_bytes: number
  exports_count: number
}

export function getDataInfo(): Promise<DataInfo | null> {
  return tryInvoke<DataInfo>('get_data_info')
}

export function migrateDataRoot(newRoot: string): Promise<string | null> {
  return tryInvoke<string>('migrate_data_root', { newRoot })
}

export function backupData(destDir: string): Promise<{ path: string } | null> {
  return tryInvoke<{ path: string; rows: number; format: string }>('backup_data', { destDir })
}

export function restoreData(backupDir: string, snapshot?: string): Promise<string | null> {
  return tryInvoke<string>('restore_data', { backupDir, snapshot: snapshot ?? null })
}

export function openDataDir(): Promise<null> {
  return tryInvoke<null>('open_data_dir')
}

/** 系统目录选择器（dialog 插件;取消返回 null）。 */
export async function pickDirectory(title: string): Promise<string | null> {
  if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) return null
  try {
    const { open } = await import('@tauri-apps/plugin-dialog')
    const picked = await open({ directory: true, multiple: false, title })
    return typeof picked === 'string' ? picked : null
  } catch (e) {
    console.error('[dialog] pickDirectory failed:', e)
    return null
  }
}

