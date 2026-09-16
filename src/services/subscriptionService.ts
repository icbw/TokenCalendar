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

/** 设置轮询间隔（秒；仅改运行时值，持久化由 designPrefs.subscriptionPollSecs 承担）。 */
export async function setPollSecs(secs: number): Promise<void> {
  await tryInvoke<null>('set_subscription_poll_secs', { secs })
}

/** 恢复轮询间隔运行时值（前端装载 designPrefs 后调用；非 Tauri 环境静默跳过）。 */
export async function applyPollSecs(secs: number): Promise<void> {
  if (!inTauri) return
  await setPollSecs(secs).catch(() => {})
}

/** boost 监控配置（Rust subscription/boost.rs BoostConfig 同形,snake_case 契约）。
 * 触发条件 OR（任一命中即进入）:spike = 相邻主轮询 used_5h 差 ≥ 阈值;
 * low = 5h 剩余 ≤ 阈值。退出 = 滚动 5 样本窗口内所有已启用条件不再成立。 */
export interface BoostConfig {
  enabled: boolean
  spike_enabled: boolean
  spike_threshold_pct: number
  low_enabled: boolean
  low_threshold_pct: number
  interval_secs: number
}

/** 当前 boost 快照（仅激活中平台有值;boost 结果不落库,独立于主快照通道）。 */
export async function getBoost(): Promise<SubscriptionSnapshot[] | null> {
  return tryInvoke<SubscriptionSnapshot[]>('get_subscription_boost')
}

/** 下发 boost 配置（持久化由 designPrefs 承担,这里只改运行时值）。 */
export async function setBoostConfig(config: BoostConfig): Promise<void> {
  await tryInvoke<null>('set_subscription_boost', { config })
}

/** 恢复 boost 配置运行时值（orb 窗口装载时调用;非 Tauri 环境静默跳过）。 */
export async function applyBoostConfig(config: BoostConfig): Promise<void> {
  if (!inTauri) return
  await setBoostConfig(config).catch(() => {})
}

/** 待机监控状态（Rust subscription/idle.rs 出口形状,snake_case 契约）。
 * idle = 该平台已进入待机（连续 3 轮无变化且安静 ≥ 10 分钟,检测放慢中）;
 * interval_secs = 当前档位。 */
export interface PlatformIdleState {
  platform: SubscriptionPlatform
  idle: boolean
  interval_secs: number
}

/** 当前待机态（orb 窗口初查口;事件 subscription:idle 翻转后重查）。 */
export async function getIdle(): Promise<PlatformIdleState[] | null> {
  return tryInvoke<PlatformIdleState[]>('get_subscription_idle')
}

/** 下发待机开关（持久化由 designPrefs 承担,这里只改运行时值;
 * Rust 侧会唤醒主轮询——调用方应只在值变化时调用）。 */
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
