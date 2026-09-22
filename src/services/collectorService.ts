// Collector 命令封装：来源健康面板、暂停开关、采集频率、挂件网格吸附开关。

import type { CollectStatusContract, SourceSummaryContract } from './contract'
import type { SourceSummary } from './types'
import { tryInvoke } from './tauri'

function toInternal(s: SourceSummaryContract): SourceSummary {
  return {
    id: s.id,
    adapterId: s.adapter_id,
    adapterName: s.adapter_name,
    location: s.location,
    kind: s.kind,
    probeStatus: s.probe_status,
    schemaFingerprint: s.schema_fingerprint ?? undefined,
    lastSuccessAt: s.last_success_at ?? undefined,
    lastAttemptAt: s.last_attempt_at ?? undefined,
    lastErrorCode: s.last_error_code ?? undefined,
    lastErrorMessage: s.last_error_message ?? undefined,
    eventsCollected: s.events_collected,
    stale: s.stale,
  }
}

export async function listSources(): Promise<SourceSummary[] | null> {
  const res = await tryInvoke<SourceSummaryContract[]>('list_sources')
  return res ? res.map(toInternal) : null
}

/** 采集轮进度：source = 正在采集的源 id（null = 空闲）;firstRoundDone = 本次启动后已跑完一轮。 */
export interface CollectStatus {
  source: string | null
  roundStartedAt: number | null
  firstRoundDone: boolean
  lastRoundAt: number | null
}

export function toCollectStatus(s: CollectStatusContract): CollectStatus {
  return { source: s.source, roundStartedAt: s.round_started_at, firstRoundDone: s.first_round_done, lastRoundAt: s.last_round_at }
}

export async function getCollectStatus(): Promise<CollectStatus | null> {
  const res = await tryInvoke<CollectStatusContract>('get_collect_status')
  return res ? toCollectStatus(res) : null
}

export async function getPaused(): Promise<boolean | null> {
  return tryInvoke<boolean>('get_paused')
}

export async function setPaused(paused: boolean): Promise<void> {
  await tryInvoke<null>('set_paused', { paused })
}

/** 采集频率（秒;五档,默认 30）。运行时值在 Rust,持久化 = prefs.json `collectIntervalSecs`。 */
export interface CollectInterval {
  secs: number
  defaultSecs: number
  choices: number[]
}

interface CollectIntervalContract {
  secs: number
  default_secs: number
  choices: number[]
}

const toCollectInterval = (r: CollectIntervalContract): CollectInterval => ({
  secs: r.secs,
  defaultSecs: r.default_secs,
  choices: r.choices,
})

export async function getCollectInterval(): Promise<CollectInterval | null> {
  const res = await tryInvoke<CollectIntervalContract>('get_collect_interval')
  return res ? toCollectInterval(res) : null
}

/** 写 prefs.json 并即时下发;调用方成功后须再 setDesignPrefs（{ collectIntervalSecs })（同 idleThresholdMin）。 */
export async function setCollectInterval(secs: number): Promise<CollectInterval | null> {
  const res = await tryInvoke<CollectIntervalContract>('set_collect_interval', { secs })
  return res ? toCollectInterval(res) : null
}

// 挂件网格吸附开关（Rust AppState + window-state.json 即时落盘）。
export async function getSnapEnabled(): Promise<boolean | null> {
  return tryInvoke<boolean>('get_snap_enabled')
}

export async function setSnapEnabled(enabled: boolean): Promise<void> {
  await tryInvoke<null>('set_snap_enabled', { enabled })
}
