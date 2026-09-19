// 订阅额度服务：设置页 Subscriptions tab 与 orb 窗口共用的
// 命令封装。数据形状由 Rust subscription/model.rs 归一化（两平台一致）；
// 凭据扫描结果只含存在性/掩码，绝无 token 值（凭据安全走查口径）。

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
  /** 读数来源：'api' = 平台端点读数（小数精度）；'desktop' = Claude
   * 桌面端本地采样（整数百分比，空闲期的余量恢复靠它零请求正）。旧版 Rust
   * 没有这个字段时为 undefined——前端不据此改显示，只作诊断。 */
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
 * - tightenLow：5h 剩余 ≤ 20% 时把阈值减半。
 * 参数键同时带 camelCase 与 snake_case：Tauri 默认把 JS 的 camelCase 映射到 Rust
 * snake_case 形参，而契约文本写的是 snake_case——两种都带上，Rust 侧无论是否声明
 * rename_all = "snake_case" 都能取到（多余的键被忽略）。 */
export async function setFetchPolicy(pct: number, tightenLow: boolean): Promise<void> {
  // 参数名走 Tauri 默认的 camelCase → snake_case 映射（与 migrate_data_root 同款）
  await tryInvoke<null>('set_subscription_fetch_policy', {
    thresholdPct: pct,
    tightenWhenLow: tightenLow,
  })
}

/** 估算器诊断态（Rust get_subscription_estimator 出口形状,snake_case 契约;每个已支持平台一条）。
 * calibrated = 已用数据校准（false = 仍在用出厂预设权重）;pairs = 有效标定样本数;
 * est_pct_since_fetch = 距上次读数的预计消耗（百分点,估计值）。 */
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

export interface EstimatorState {
  platform: SubscriptionPlatform
  calibrated: boolean
  pairs: number
  est_pct_since_fetch: number
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

/** 待机监控状态（Rust subscription/idle.rs 出口形状,snake_case 契约）。
 * idle = 已进入待机（安静起点距今满 10 分钟;安静起点 = 最近一次本地 agent 活动
 * 或用户注意）。**待机是全局视觉态**,两条记录的 idle 恒相同——按平台给形状只是
 * 让前端「所有已绑定平台都待机才减淡」的判据不必特判。
 * 待机只管减淡,**不改变取数频次**。 */
export interface PlatformIdleState {
  platform: SubscriptionPlatform
  idle: boolean
}

/** 当前待机态（orb 窗口初查口;事件 subscription:idle 翻转后重查）。 */
export async function getIdle(): Promise<PlatformIdleState[] | null> {
  return tryInvoke<PlatformIdleState[]>('get_subscription_idle')
}

/** 下发待机开关（持久化由 designPrefs 承担,这里只改运行时值）。
 * Rust 侧幂等早退,且**不再唤醒取数**——待机是视觉态,不该改变网络行为。 */
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
