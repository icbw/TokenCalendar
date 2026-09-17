// 导出契约封装（对齐旧 ExportService）。

import type { ExportResultContract } from './contract'
import type { ExportResult } from './types'
import { tryInvoke } from './tauri'

async function exportMonth(cmd: 'export_month_csv' | 'export_month_json', month: string): Promise<ExportResult | null> {
  const res = await tryInvoke<ExportResultContract>(cmd, { month })
  return res ? { path: res.path, rows: res.rows, format: res.format } : null
}

export function exportMonthCSV(month: string): Promise<ExportResult | null> {
  return exportMonth('export_month_csv', month)
}

export function exportMonthJSON(month: string): Promise<ExportResult | null> {
  return exportMonth('export_month_json', month)
}
