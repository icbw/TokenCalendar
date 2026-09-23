// 订阅额度服务：设置页 Subscriptions tab 与 orb 窗口共用的命令封装。
// 数据形状由 Rust subscription/model.rs 归一化（两平台一致）；
// 凭据扫描结果只含存在性/掩码，绝无 token 值。

import { inTauri, tryInvoke } from './tauri'

export type SubscriptionPlatform = 'codex' | 'claude'

/** 归一化额度窗口（kind: "5h" | "7d" | "7d_opus"）。 */
export interface QuotaWindow {
  kind: string
  used_percent: number
  resets_at: number | null
}

/** 单平台订阅快照（Rust get_subscription_snapshots 出口形状）。
 * status: ok / plan_inactive / auth_failed / rate_limited / network_failed /
 * parse_failed / idle（失败态一律保留上一次成功数据）。 */
export interface SubscriptionSnapshot {
  platform: SubscriptionPlatform
  plan_type: string
  windows: QuotaWindow[]
  fetched_at: number | null
  status: string
  /** 读数来源：`api` = 平台 usage 端点；`desktop` = Claude 桌面端采样文件；
   * `rollout` = Codex 会话 rollout 里 token_count 事件带的 rate_limits
   * （与 api 是同一个服务端数字，只是走本地文件到手、不花请求）。 */
  source?: 'api' | 'desktop' | 'rollout'
}

/** 凭据发现项（scan_subscription_credentials 出口；只含掩码）。 */
export interface CredentialInfo {
  platform: SubscriptionPlatform
  present: boolean
  parseable: boolean
  account_hint: string | null
}

export async function getSnapshots(): Promise<SubscriptionSnapshot[] | null> {
  return tryInvoke<SubscriptionSnapshot[]>('get_subscription_snapshots')
}

export async function scanCredentials(): Promise<CredentialInfo[] | null> {
  return tryInvoke<CredentialInfo[]>('scan_subscription_credentials')
}

export async function bind(platform: SubscriptionPlatform): Promise<void> {
  await tryInvoke<null>('bind_subscription', { platform })
}

export async function unbind(platform: SubscriptionPlatform): Promise<void> {
  await tryInvoke<null>('unbind_subscription', { platform })
}

export async function refreshNow(): Promise<void> {
  await tryInvoke<null>('refresh_subscriptions_now')
}

/** 设置兜底取数间隔（秒；仅改运行时值，持久化由 designPrefs.subscriptionPollSecs 承担）。
 * 读数主路径是本地 token 探针（采集器发现本机新 token 即取数），本命令只调节
 * 「本地无痕迹」用量（网页 / 在线会话）的兜底节奏。 */
export async function setPollSecs(secs: number): Promise<void> {
  await tryInvoke<null>('set_subscription_poll_secs', { secs })
}

/** 恢复兜底取数间隔运行时值（前端装载 designPrefs 后调用；非 Tauri 环境静默跳过）。 */
export async function applyPollSecs(secs: number): Promise<void> {
  if (!inTauri) return
  await setPollSecs(secs).catch(() => {})
}

/** 下发「按预计消耗取数」策略（仅改运行时值，持久化由 designPrefs 的
 * subscriptionFetchPct / subscriptionTightenLow 承担）。
 * - pct：距上次读数的预计消耗达到百分之几就取一次读数（Rust 侧 clamp 到 0.5〜10.0、步进 0.5）；
 * - tightenLow：5h 剩余 ≤ 20% 时把阈值减半。 */
export async function setFetchPolicy(pct: number, tightenLow: boolean): Promise<void> {
  // 参数键为 camelCase，经 Tauri 默认映射到 Rust 形参 threshold_pct / tighten_when_low（与 migrate_data_root 同款）
  await tryInvoke<null>('set_subscription_fetch_policy', {
    thresholdPct: pct,
    tightenWhenLow: tightenLow,
  })
}

/** 「本机 agent 解释不了的消耗」证据（Rust bootstrap:ForeignEvidence 出口形状）。
 * 判据 = 相邻两条服务端采样之间**用量涨了而本机一条轮记录都没有**。
 * **它说明不了是谁在用**，只说明不是本机的 agent——可能是另一台电脑、网页版、
 * 本机 Claude 桌面端自己的对话（不走 collector 源却吃同一份配额），或手机 App。
 * 文案一律说「本地 token 解释不了」，不要说成「别的设备」。
 * 它只把盲区量出来摆上台面：取数与读数走的是服务端真值不会错，被污染的是**标定**
 * （样本的涨幅含别处的量、代价只有本机的）。 */
export interface ForeignEvidence {
  /** 统计窗口内可判定的相邻样本区间数（太新的不算——本地轮可能还没采到）。 */
  windows: number
  /** 其中「服务端涨了、本机零痕迹」的区间数。 */
  unexplained: number
  /** 这些区间累计的服务端涨幅（百分点）。 */
  unexplained_pct: number
  /** 统计窗口长度（小时）。 */
  window_hours: number
}

/** 估算器诊断态（Rust get_subscription_estimator 出口形状,snake_case 契约;每个已支持平台一条）。
 * calibrated = 已用数据校准（false = 仍在用出厂预设权重）;pairs = 有效标定样本数;
 * est_pct_since_fetch = 距上次读数的预计消耗（百分点,估计值）。 */
export interface EstimatorState {
  platform: SubscriptionPlatform
  calibrated: boolean
  pairs: number
  est_pct_since_fetch: number
  /** 归一化系数（**百分点 / 美元当量**）：1 美元的官方 API 当量吃掉多少配额。
   * 取倒数 = 「1% 配额 ≈ 多少美元」。旧版 Rust 没有这个字段时为 undefined。 */
  scale?: number
  /** 已收割留存的**本地读数**条数（Claude = 桌面端 plan-usage-history.json 的采样；
   * Codex = 会话 rollout 里 token_count 事件带的 rate_limits；0 = 本机没有这类源）。
   * 两个源自己都会滚掉旧数据，这个数会越过那条线继续涨 = 样本密度在累积。
   * 字段名沿用 desktop_samples 不改——改名要同时动契约与前端，而它只是个计数。 */
  desktop_samples?: number
  /** 本机之外的消耗证据（旧版 Rust 没有这个字段时为 undefined）。 */
  foreign?: ForeignEvidence
}

/** 取一次估算器状态。**诊断只读,不要轮询**：设置页挂载查一次,收到 subscription:changed
 * 再重查即可。旧版 Rust 没有这条命令时 tryInvoke 返回 null（调用方据此整行不显示）。 */
export async function getEstimator(): Promise<EstimatorState[] | null> {
  return tryInvoke<EstimatorState[]>('get_subscription_estimator')
}

/** 当前待机态（orb 窗口初查口;事件 subscription:idle 翻转后重查）。
 * true = 已进入待机（安静起点距今满 10 分钟;安静起点 = 最近一次本地 agent 活动
 * 或用户注意）。**待机是全局视觉态**,不分平台;只管减淡,**不改变取数频次**。 */
export async function getIdle(): Promise<boolean | null> {
  return tryInvoke<boolean>('get_subscription_idle')
}

/** 下发待机开关（持久化由 designPrefs 承担,这里只改运行时值）。
 * Rust 侧幂等早退,且**不唤醒取数**——待机是视觉态,不该改变网络行为。 */
export async function setIdleEnabled(enabled: boolean): Promise<void> {
  await tryInvoke<null>('set_subscription_idle_enabled', { enabled })
}

/** 用户注意到悬浮球（展开 / 切换平台）：Rust 侧全部平台退出待机、清零安静计数,
 * 不额外取数（手动刷新走 refreshNow,同样先退出待机）。 */
export async function noteAttention(): Promise<void> {
  await tryInvoke<null>('note_subscription_attention')
}

/** 恢复待机开关运行时值（orb 窗口装载时调用;非 Tauri 环境静默跳过）。 */
export async function applyIdleEnabled(enabled: boolean): Promise<void> {
  if (!inTauri) return
  await setIdleEnabled(enabled).catch(() => {})
}

// ---------- 价格与读数的只读查询面 ----------
// 五条命令都只读本地库、零网络。形状由 Rust subscription/query.rs 定，snake_case 契约。

/** 一个模型的一段价目生效期（Rust price:PriceRow）。
 * 四项 usd_* 都是**官方公布的 API 单价，USD / Mtok**。绝大多数模型终其生命周期只有
 * 一行；有第二行的那个模型就是被官方降过价的——价格梯度图里的台阶正是这些行。 */
export interface PriceModelRow {
  platform: SubscriptionPlatform
  /** 小写子串模式（'opus' / 'gpt-5-6-sol'…），匹配时最长键优先。 */
  match_key: string
  /** unix 秒；该价目开始生效。 */
  effective_from: number
  display_name: string
  usd_input: number
  usd_output: number
  usd_cache_read: number
  usd_cache_write: number
  /** 出处（这张表是可审计的官方价格快照，要能回答「这个数从哪儿来」）。 */
  source_note: string
}

/** 某平台全部模型的全部生效期（按 match_key、生效期升序）。 */
export async function getPriceModels(
  platform: SubscriptionPlatform,
): Promise<PriceModelRow[] | null> {
  return tryInvoke<PriceModelRow[]>('get_price_models', { platform })
}

export interface PriceAtResult {
  platform: SubscriptionPlatform
  /** 实际取价的时刻（unix 秒；入参省略时是 Rust 侧的「此刻」）。 */
  at: number
  /** 每个 match_key 至多一行。 */
  rows: PriceModelRow[]
}

/** 某时刻各模型的有效价目（官方价目对照表 / 诊断用；at 省略 = 此刻）。 */
export async function getPriceAt(
  platform: SubscriptionPlatform,
  at?: number,
): Promise<PriceAtResult | null> {
  return tryInvoke<PriceAtResult>('get_price_at', { platform, at: at ?? null })
}

/** 一个模型在一段价目生效期内的用量与美元当量。 */
export interface ModelUsageSegment {
  /** 该段价目的起点（null = 没命中任何价目键，单价来自回落常量）。 */
  effective_from: number | null
  /** 实际命中的价目键（null = 回落；codex-auto-review 命中的是它**路由到**的键）。 */
  match_key: string | null
  /** 价目是否可信（回落 / 路由标签 → false）。 */
  known: boolean
  usd_input: number
  usd_output: number
  usd_cache_read: number
  usd_cache_write: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  usd: number
}

export interface ModelUsageRow {
  model_key: string
  display_name: string
  known: boolean
  /** 用户发起的对话轮次。
   * **不按价目段切分**——轮次只有日粒度，摊到两段上就是造数据。 */
  requests: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  total_tokens: number
  /** 美元当量合计（各段各按各自单价算完再相加）。 */
  usd: number
  /** 按价目生效期切开的明细（没被降过价的模型只有一段）。 */
  segments: ModelUsageSegment[]
}

export interface ModelUsageResult {
  platform: SubscriptionPlatform
  /** 实际生效的日区间（回显；入参省略时 = 该平台在 collector 里的全部历史）。 */
  from: string
  to: string
  usd_total: number
  /** 其中价目不可信那部分（回落模型 + codex-auto-review）。提示用，不是误差棒。 */
  usd_unknown: number
  /** 按美元当量降序。 */
  rows: ModelUsageRow[]
}

/** 区间内的分模型用量与代价，按各模型自己的价目生效段切分。
 * from / to = 本地日期 'YYYY-MM-DD' 闭区间；省略 = 该平台的全部历史。
 *
 * **口径红线**：usd 是「这些 token 若按官方 API 单价计费值多少钱」的**当量**，
 * 不是账单——用户付的是固定月费。文案一律说「相当于」。 */
export async function getModelUsage(
  platform: SubscriptionPlatform,
  from?: string,
  to?: string,
): Promise<ModelUsageResult | null> {
  return tryInvoke<ModelUsageResult>('get_model_usage', {
    platform,
    from: from ?? null,
    to: to ?? null,
  })
}

/** 一个模型的每轮额度代价（Rust query:MessageCostRow）。 */
export interface MessageCostRow {
  /** collector 里的模型键，原样。 */
  model_key: string
  /** 命中价目行的展示名（可能是区间名，如「Claude Opus 4.5〜5」；短名见 messageBudget.ts）。 */
  display_name: string
  /** 取样窗口里以它为主的用户轮数（= 中位数的样本数）。 */
  turns: number
  /** 每轮代价的中位数（美元当量）。 */
  median_usd: number
  /** 一轮吃掉 5h 窗口的百分点。剩余条数 = 剩余 % ÷ 它；满窗口条数 = 100 ÷ 它。 */
  pct_per_turn: number
}

/** 分模型的「一轮吃掉多少 5h 额度」（Rust query:MessageBudget）。
 * **只给每轮代价、不给剩余条数**：剩余 % 以调用方手里的快照为准，在那边除——
 * hover 里的条数与表盘百分比必须来自同一时刻。 */
export interface MessageBudget {
  platform: SubscriptionPlatform
  /** 取样窗口起点（本地日，含；近 30 天）。 */
  from: string
  /** 标定系数（百分点 / 美元当量，5h 窗口）。 */
  scale: number
  /** false = 仍是出厂预设系数，估计更粗。 */
  calibrated: boolean
  /** 子会话（子代理 / 自动审查）开销倍率，已乘进 pct_per_turn。 */
  overhead: number
  /** 最近 7 天用户轮最多的模型（null = 没有够样本的模型）。 */
  main_model: string | null
  /** 够样本（≥5 轮）的模型，按轮数降序。 */
  rows: MessageCostRow[]
}

/** 分模型每轮额度代价（剩余消息数的分母）。「消息」= 用户发起的对话轮次。 */
export async function getMessageBudget(platform: SubscriptionPlatform): Promise<MessageBudget | null> {
  return tryInvoke<MessageBudget>('get_message_budget', { platform })
}

/** 归一化读数序列的一行（Rust model:QuotaReading）。 */
export interface QuotaReading {
  /** 读数时刻（unix 秒；语义随 source 不同）。 */
  t: number
  /** 窗口种类，原样透传：'5h' / '7d' / '7d_opus'。 */
  kind: string
  used_percent: number
  /** 窗尾（unix 秒；null = **该来源不提供**，不是「没有窗尾」）。 */
  resets_at: number | null
  plan_type: string
  source: 'api' | 'desktop' | 'rollout'
}

/** 读数序列（闭区间，unix 秒；kind 省略 = 全部窗口种类）。
 * **年度曲线不要走这条**——那是几千行的量，日级汇总已经算好了（getQuotaDays）。 */
export async function getQuotaReadings(
  platform: SubscriptionPlatform,
  from: number,
  to: number,
  kind?: string,
): Promise<QuotaReading[] | null> {
  return tryInvoke<QuotaReading[]>('get_quota_readings', {
    platform,
    kind: kind ?? null,
    from,
    to,
  })
}

/** 日级汇总的一行（Rust model:QuotaDay；纯派生，可从读数层完整重建）。 */
export interface QuotaDay {
  /** 本地日期 'YYYY-MM-DD'。 */
  day: string
  kind: string
  /** 当日读数条数（按时刻去重之后）。 */
  n: number
  t_first: number
  t_last: number
  used_first: number
  used_last: number
  used_max: number
  used_min: number
  /** 当日观测到的额度消耗（相邻读数正向差之和）。
   * **是下界不是账单**——滚动窗口里消耗与过期同时发生。 */
  gain_pct: number
  /** 当日观测到的窗口回收（负向差之和的绝对值）。 */
  drop_pct: number
  /** 当日跨过窗口重置的次数（两端有一端不提供窗尾就判不出来，恒为 0）。 */
  resets: number
  /** 当天第一条读数与它前面那条之间的间隔（秒）。讲「这天用了多少」必须同时看它
   * ——它说明这笔涨幅是跨多久攒出来的。 */
  carry_secs: number
}

/** 日级汇总（闭区间，本地日期 'YYYY-MM-DD'；kind 省略 = 全部种类）。 */
export async function getQuotaDays(
  platform: SubscriptionPlatform,
  from: string,
  to: string,
  kind?: string,
): Promise<QuotaDay[] | null> {
  return tryInvoke<QuotaDay[]>('get_quota_days', { platform, kind: kind ?? null, from, to })
}
