// 线上契约形状：与 src-tauri/src/commands.rs 的 serde 定义严格对齐（snake_case）。
// 冻结此契约，采集器按此实现真实数据源。
// 装配层消费的驼峰形状见 types.ts，由各 service 做一次映射。

export interface MatrixQuery {
  month: string
  group_by: 'agent' | 'model'
  bucket: 'day' // 契约保留位：后端仅支持 day，week/cumulative 由前端聚合
  metric: 'total' | 'input' | 'output'
  normalization: 'global' | 'perRow'
}

export interface MatrixRow {
  key: string
  label: string
  /** 下标 0 = 1 号；null = 未来日期（≠0）。 */
  values: (number | null)[]
  /** 契约扩展：与 values 平行的请求/对话数（未来日为 0）。 */
  message_counts?: number[]
  month_total: number
}

export interface MatrixResult {
  month: string
  days_in_month: number
  generated_at: number
  rows: MatrixRow[]
}

export interface BreakdownSlice {
  key: string
  label: string
  tokens: number
}

export interface BreakdownDay {
  day: string
  slices: BreakdownSlice[] | null
}

export interface SourceSummaryContract {
  id: string
  adapter_id: string
  adapter_name: string
  location: string
  kind: string
  probe_status: string
  schema_fingerprint?: string
  last_success_at: string | null
  last_attempt_at: string | null
  last_error_code: string | null
  last_error_message: string | null
  events_collected: number
  stale: boolean
}

export interface ExportResultContract {
  path: string
  rows: number
  format: string
}

/** usage:changed 载荷（前端目前只当刷新触发器用）。 */
export interface ChangedKeys {
  months: string[]
  agent_keys: string[]
  model_keys: string[]
  revision: number
}

// ---- 数据洞察----

/** credit 月报（request_model 对账表聚合;口径见 store.credit_summary）。
 * has_data=false = 该月无导入数据（≠0）,UI 走引导文案不渲染数值。 */
export interface CreditModelSliceContract {
  key: string
  label: string
  credit: number
  requests: number
}

export interface CreditDayPointContract {
  day: string
  credit: number
}

/** credit 按模型×日序列（双组图;与 commands.rs CreditModelDaySlice 对齐）。 */
export interface CreditModelDaySliceContract {
  key: string
  label: string
  by_day: CreditDayPointContract[]
}

export interface CreditSummaryContract {
  month: string
  has_data: boolean
  total_credit: number
  total_requests: number
  by_model: CreditModelSliceContract[]
  by_day: CreditDayPointContract[]
  by_model_day: CreditModelDaySliceContract[]
}

/** 时间范围序列查询（与 commands.rs RangeSeriesQuery 对齐）。 */
export interface RangeSeriesQueryContract {
  start_day: string
  end_day: string
  bucket: 'day' | 'hour'
  dimension: 'agent' | 'model' | 'total'
  metric: 'total' | 'input' | 'output'
  filter_dimension?: 'agent' | 'model'
  filter_key?: string
}

export interface RangeSeriesPointContract {
  /** day 粒度 = "YYYY-MM-DD";hour 粒度 = "YYYY-MM-DD HH"。 */
  bucket: string
  values: number[]
}

export interface RangeSeriesResultContract {
  series_keys: string[]
  series_labels: string[]
  points: RangeSeriesPointContract[]
}
