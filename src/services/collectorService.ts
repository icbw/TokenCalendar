// Collector 契约封装：来源健康面板 + 暂停开关（对齐旧 CollectorService 的子集：
// 前端 UI 只消费 ListSources/GetPaused/SetPaused）。

import type { SourceSummaryContract } from './contract'
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

export async function getPaused(): Promise<boolean | null> {
  return tryInvoke<boolean>('get_paused')
}

export async function setPaused(paused: boolean): Promise<void> {
  await tryInvoke<null>('set_paused', { paused })
}

// 挂件网格吸附开关（Rust AppState + window-state.json 即时落盘）。
export async function getSnapEnabled(): Promise<boolean | null> {
  return tryInvoke<boolean>('get_snap_enabled')
}

export async function setSnapEnabled(enabled: boolean): Promise<void> {
  await tryInvoke<null>('set_snap_enabled', { enabled })
}
