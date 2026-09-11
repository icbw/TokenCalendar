// 数据管理契约封装。
// 全部经 tryInvoke 容错:非 Tauri 环境（纯前端 dev）返回 null,UI 走降级文案。

import { tryInvoke } from './tauri'

export interface DataInfo {
  root: string
  custom_root: string | null
  default_root: string
  fell_back: boolean
  db_bytes: number
  imports_count: number
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

export interface ImportFileResult {
  /** 转存进 imports 目录后的文件名（同名自动 -1/-2 后缀）。 */
  stored_as: string
}

/** 手动导入 CodeBuddy 官网导出 xlsx：校验→转存 imports→唤醒采集线程即时入库。
 *  与 tryInvoke 不同:失败不吞错,把后端错误串（如"not an export file"）带回 UI。 */
export async function importCodebuddyFile(
  sourcePath: string,
): Promise<ImportFileResult | { error: string } | null> {
  if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) return null
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return await invoke<ImportFileResult>('import_codebuddy_file', { sourcePath })
  } catch (e) {
    return { error: typeof e === 'string' ? e : String(e) }
  }
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

/** 系统文件选择器：选一份 CodeBuddy 官网导出 xlsx（取消返回 null）。 */
export async function pickXlsxFile(): Promise<string | null> {
  if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) return null
  try {
    const { open } = await import('@tauri-apps/plugin-dialog')
    const picked = await open({
      multiple: false,
      title: 'Choose a CodeBuddy credit export (.xlsx)',
      filters: [{ name: 'CodeBuddy credit export', extensions: ['xlsx'] }],
    })
    return typeof picked === 'string' ? picked : null
  } catch (e) {
    console.error('[dialog] pickXlsxFile failed:', e)
    return null
  }
}
