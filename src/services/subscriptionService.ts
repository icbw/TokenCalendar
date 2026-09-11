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
 *  status: ok / plan_inactive / auth_failed / network_failed / parse_failed / idle。 */
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
