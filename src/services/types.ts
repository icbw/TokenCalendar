// 装配层内部形状：沿用旧项目 bindings 的驼峰命名，把移植 diff 压到最小。
// 线上 snake_case 契约（contract.ts）→ 此处驼峰形状的映射在各 service 内完成。

export interface UsageRow {
  key: string
  label: string
  values: (number | null)[] | null
  /** 契约扩展：与 values 平行的请求/对话数。 */
  messageCounts?: number[]
  monthTotal: number
}

export interface UsageResult {
  month: string
  daysInMonth: number
  generatedAt: number
  rows: UsageRow[] | null
}

export interface SourceSummary {
  id: string
  adapterId: string
  adapterName: string
  location: string
  kind: string
  probeStatus: string
  schemaFingerprint?: string
  lastSuccessAt?: string
  lastAttemptAt?: string
  lastErrorCode?: string
  lastErrorMessage?: string
  eventsCollected: number
  stale: boolean
}

export interface ExportResult {
  path: string
  rows: number
  format: string
}

// WindowMode（expanded|widget 互斥模式）随 get_mode/set_mode 退役，
// 窗口身份由入口装配决定（main.tsx → FullWindow / widget.tsx → WidgetWindow），
// 可见性状态在 windowService.WindowVisibility + 事件广播。

export interface QueryOptions {
  month: string
  groupBy: 'agent' | 'model'
  bucket: 'day'
  metric: 'total' | 'input' | 'output'
  normalization: 'global' | 'perRow'
}
