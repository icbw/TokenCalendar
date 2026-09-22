// 线上契约形状：与 src-tauri/src/commands.rs 的 serde 定义严格对齐（snake_case）。
// 装配层消费的驼峰形状见 types.ts，由各 service 做一次映射。

/** token 指标:四个分项互斥,total = input + cache_write + cache_read + output（collector v16 口径;
 * Codex 少量源事件只报 total、无分项,这部分不归属任何分项）。input = 未命中缓存的输入。 */
export type TokenMetric = 'total' | 'input' | 'cache_write' | 'cache_read' | 'output'

export interface MatrixQuery {
  month: string
  group_by: 'agent' | 'model'
  bucket: 'day' // 契约保留位：后端仅支持 day，week/cumulative 由前端聚合
  metric: TokenMetric
  normalization: 'global' | 'perRow'
}

export interface MatrixRow {
  key: string
  label: string
  /** 下标 0 = 1 号；null = 未来日期（≠0）。 */
  values: (number | null)[]
  /** 与 values 平行的请求/对话数（未来日为 0）。 */
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

/** get_collect_status 返回 / `collector:status` 事件载荷（采集轮进度）。 */
export interface CollectStatusContract {
  source: string | null
  round_started_at: number | null
  first_round_done: boolean
  last_round_at: number | null
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

// ---- 数据洞察 ----

/** credit 月报（daily_usage 源本地积分聚合;口径见 store.credit_summary）。
 * has_data=false = 该月无积分数据（≠0）,UI 走空态文案不渲染数值。 */
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

/** credit 按模型×日序列（与 commands.rs CreditModelDaySlice 对齐）。 */
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
  metric: TokenMetric
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

// ---- 项目维与任务分析（与 commands.rs / collector/task_query.rs 对齐）----
// 调用约定:get_project_month_rows / get_task_turns 参数名 snake_case（rename_all），其余单词参数。

/** 项目维 metric:token 指标 + 计数 + 时间成本（wait = Σ wall_ms 等待,human = Σ idle_ms 人工,毫秒,并列不相加）。 */
export type ProjectMetric = TokenMetric | 'turns' | 'model_calls' | 'tool_calls' | 'wait' | 'human'
export type ProjectGroupBy = 'project' | 'agent' | 'model'

/** get_project_month_rows 参数（返回 MatrixResult;message_counts = Σ turns）。 */
export interface ProjectMonthRowsArgs {
  month: string
  group_by: ProjectGroupBy
  metric: ProjectMetric
}

/** get_project_breakdown 参数（返回 BreakdownDay[],tokens = total）:
 * kind=project → 每日 Agent 构成;kind=agent|model → 每日项目构成。 */
export interface ProjectBreakdownArgs {
  kind: ProjectGroupBy
  key: string
  month: string
}

/** 本地日闭区间（"YYYY-MM-DD"）。 */
export interface DayRangeContract {
  start_day: string
  end_day: string
}

export interface TaskFiltersContract {
  agent?: string
  project?: string
}

export type TaskSortField =
  | 'started_at'
  | 'turns'
  | 'steps'
  | 'tool_calls'
  | 'wall_ms'
  | 'model_ms'
  | 'tool_ms'
  | 'error_count'
  | 'aborted_count'
  | 'subagent_count'
  | 'subagent_calls'
  | 'total_tokens'

export interface TaskSortContract {
  field: TaskSortField
  direction: 'asc' | 'desc'
}

/** limit 1..500（缺省 50）。 */
export interface TaskPageReqContract {
  offset: number
  limit: number
}

/** get_task_list 参数:filters / sort / page 可省（不过滤 / started_at 降序 / 前 50 条）。 */
export interface TaskListArgs {
  range: DayRangeContract
  filters?: TaskFiltersContract
  sort?: TaskSortContract
  page?: TaskPageReqContract
}

/** 任务 = 有轮的根会话（子会话已并入,不单独出现）。时间字段毫秒;null = 源无该值（CodeBuddy）,
 * JSONL 族 model_ms / tool_ms 为估算。title 是内容列:仅本地展示,不得进入导出。 */
export interface TaskRowContract {
  agent: string
  session_id: string
  /** 解析后的有效项目键（合并目标 / __scratch / 原键）。 */
  project: string
  /** 有效项目展示名（alias 优先）。 */
  project_label: string
  /** 会话首轮的原始目录键。 */
  project_raw: string
  started_at: number
  ended_at: number | null
  title: string | null
  turns: number
  /** = model_calls（含子代理）。 */
  steps: number
  tool_calls: number
  wall_ms: number | null
  model_ms: number | null
  tool_ms: number | null
  /** API / 工具错误（不含用户中止）。 */
  error_count: number
  /** 用户中止的轮数。 */
  aborted_count: number
  subagent_count: number
  subagent_calls: number
  total_tokens: number
}

export interface TaskPageContract {
  /** 过滤后总数（分页前）。 */
  total: number
  rows: TaskRowContract[]
}

/** get_task_turns 参数（子会话 id → 空列表）。 */
export interface TaskTurnsArgs {
  agent: string
  session_id: string
}

export interface TaskTurnContract {
  /** 按开始时间 1..n。 */
  turn_seq: number
  day: string
  project: string
  model: string
  started_at: number
  ended_at: number | null
  wall_ms: number | null
  model_ms: number | null
  tool_ms: number | null
  /** 仅 ZCode 有值。 */
  ttft_ms: number | null
  /** 原始轮间空档（首轮 null,不截断）。 */
  gap_ms: number | null
  steps: number
  tool_calls: number
  subagent_count: number
  subagent_calls: number
  error_count: number
  retry_count: number
  /** 用户中止（与 error_count 分列）。 */
  aborted: boolean
  input_tokens: number
  output_tokens: number
  total_tokens: number
}

/** get_effort_series 参数（返回 RangeSeriesResultContract;bucket 仅 day）。 */
export interface EffortSeriesArgs {
  range: DayRangeContract
  bucket: 'day'
  dimension: ProjectGroupBy | 'total'
  metric: ProjectMetric
  filter?: { dimension: ProjectGroupBy; key: string }
}

export interface GapBucketContract {
  lo_ms: number
  /** null = 开放上界（最后一桶）。 */
  hi_ms: number | null
  count: number
}

/** get_gap_histogram（range) 返回:gap_ms 对数分桶（[0,1s) + 1s×10^（k/4),k=0..24,共 26 桶）+ 当前阈值两侧合计。 */
export interface GapHistogramContract {
  threshold_ms: number
  buckets: GapBucketContract[]
  total: number
  /** gap ≤ 阈值:计入 idle（= 范围内 Σ daily_project.idle_ms）。 */
  within_count: number
  within_ms: number
  beyond_count: number
  beyond_ms: number
}

/** get_project_span（project?) 返回:给定项目 = 生命周期（daily_project 首末日）;
 * 省略 = 全部数据首末日（All 范围起点）;无数据 = null。 */
export interface DaySpanContract {
  first_day: string
  last_day: string
}

// ---- 项目推进时间轴（与 commands.rs / collector/task_query.rs 对齐）----

/** get_project_timeline（from, to):含端点,YYYY-MM-DD 本地日。 */
export interface TimelineQueryArgs {
  from: string
  to: string
}

export interface TimelineCellContract {
  day: string
  turns: number
  tokens: number
  wall_ms: number
  idle_ms: number
  sessions: number
  agents: string[]
  /** 当日各根会话,最新开始的在前。 */
  items: TimelineSessionContract[]
}

export interface TimelineSessionContract {
  /** agent 展示名。 */
  agent: string
  /** agent 键（open_agent_session 入参）。 */
  agent_key: string
  session_id: string
  /** 【内容列】空 / null 前端回退 started_at 的时刻。 */
  title: string | null
  started_at: number
  /** 当日最后活动时刻（末轮 ended_at）;排序依据。 */
  last_active_at: number
  turns: number
  tokens: number
  wall_ms: number
}

export interface TimelineProjectContract {
  key: string
  label: string
  agents: string[]
  first_day: string | null
  last_day: string | null
  inactive_days: number | null
  cells: TimelineCellContract[]
}

export interface TimelineResultContract {
  today: string
  days: string[]
  projects: TimelineProjectContract[]
}

// ---- 注意力状态机（与 collector/attention.rs 对齐）----

/** get_attention 数组元素:会话级现状,子会话不列出,按 since 先后。 */
export interface AttentionItemContract {
  agent: string
  agent_label: string
  session_id: string
  /** 原始目录键（前端经 effective_key 折叠到项目行）。 */
  project_key: string
  /** 【内容列】只供 timeline 窗口本地渲染。 */
  title: string | null
  /** 桌面宿主线索（Claude Code 的 entrypoint;其余源 null）,前端只透传。 */
  host: string | null
  state: 'running' | 'waiting' | 'tool_pending'
  /** running = 最近事件;waiting = 模型答完时刻;tool_pending = 工具静默开始时刻（ms）。 */
  since: number
  last_event: number
  /** 已确认（仅 waiting / tool_pending;同一会话进入新一段等待自动复位）。 */
  acked: boolean
  /** 暂压（仅 waiting）:宿主窗口在前台期间新答完的会话,先不亮;留在窗口里继续操作 → 确认,离开窗口 → 亮起。 */
  held: boolean
}

/** ack_attention（agent, session_id) → 是否有变化。 */
export interface AckAttentionArgs {
  agent: string
  session_id: string
}

/** focus_agent_window（agent, session_id)（agent_focus.rs）:找到 → 前置 + 确认;
 * 找不到 → 条目已从注意力表移除（伪等待）,前端降级为「上次停在这里」。 */
export interface FocusAgentWindowArgs {
  agent: string
  session_id: string
}
export interface FocusResultContract {
  found: boolean
}

/** open_agent_session（agent, session_id)（agent_focus.rs）:时间轴会话条双击 → 到该会话的 agent。
 * agent = agent 键;项目目录与宿主线索由 Rust 从库里取,前端不传路径。 */
export interface OpenAgentSessionArgs {
  agent: string
  session_id: string
}
export interface OpenResultContract {
  /** focused 已前置窗口 / launched 已启动宿主 / folder 回退打开了项目目录 / none 无事可做。 */
  outcome: 'focused' | 'launched' | 'folder' | 'none'
}

/** get_idle_threshold 返回。 */
export interface IdleThresholdInfoContract {
  minutes: number
  default_minutes: number
  min_minutes: number
  max_minutes: number
}

/** set_idle_threshold（minutes) 返回（写 prefs.json idleThresholdMin + 同步重算 daily_project + emit usage:changed）。 */
export interface IdleThresholdAppliedContract {
  minutes: number
  recomputed_days: number
  elapsed_ms: number
}

// ---- 项目管理（与 commands.rs / collector/project_meta.rs 对齐）----

export type ProjectStatus = 'active' | 'hidden' | 'merged' | 'scratch'

/** list_project_meta 每个目录键一行。 */
export interface ProjectMetaRowContract {
  key: string
  label: string
  alias: string | null
  hidden: boolean
  merged_into: string | null
  merged_label: string | null
  note: string | null
  status: ProjectStatus
  /** 有 meta 行（显式管理过,自动规则不作用）。 */
  managed: boolean
  /** 分析视图里的有效键;null = 不可见（自身隐藏,或合并目标 / Scratch 被隐藏）。 */
  effective_key: string | null
  agents: string[]
  sessions: number
  turns: number
  tokens: number
  first_day: string | null
  last_day: string | null
  updated_at: number | null
  folder_exists: boolean
}

export interface ProjectMetaListContract {
  rows: ProjectMetaRowContract[]
  scratch_hidden: boolean
  scratch_alias: string | null
}

/** set_project_meta（input):单条 upsert（alias / note 空 = 清除,≤ 120 字符）;reset = 删行回到自动态。 */
export interface ProjectMetaInputContract {
  project_key: string
  alias: string | null
  hidden: boolean
  note: string | null
  reset: boolean
}

export interface ScratchRuleContract {
  enabled: boolean
  min_sessions: number
  min_turns: number
  unknown_as_scratch: boolean
}

export interface ScratchRuleInfoContract {
  rule: ScratchRuleContract
  defaults: ScratchRuleContract
  min_sessions_bounds: [number, number]
  min_turns_bounds: [number, number]
}
