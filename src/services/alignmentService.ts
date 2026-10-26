// 双轨只读契约：额度与任务形状均沿用 Rust snake_case，不在前端另造口径。
import { tryInvoke } from './tauri'
import type { QuotaReading, SubscriptionPlatform } from './subscriptionService'

export type QuotaWindowEvidence = 'announced_tail' | 'tail_minus_5h' | 'unknown'
export type QuotaDiscontinuityReason = 'reset' | 'window_started' | 'plan_changed' | 'tail_regressed' | 'tail_changed' | 'evidence_missing'

export interface QuotaDiscontinuity {
  from_t: number
  to_t: number
  at: number | null
  reason: QuotaDiscontinuityReason
}

export interface QuotaWindowSegment {
  id: number
  start_at: number | null
  end_at: number | null
  start_evidence: QuotaWindowEvidence
  end_evidence: QuotaWindowEvidence
  first_reading_at: number
  last_reading_at: number
  continues_left: boolean
  continues_right: boolean
  readings: QuotaReading[]
}

export interface QuotaTimeline {
  windows: QuotaWindowSegment[]
  discontinuities: QuotaDiscontinuity[]
  raw_readings: number
  returned_readings: number
  downsampled: boolean
}

export interface QuotaAlignmentTurn {
  turn_seq: number
  project: string
  project_raw: string
  project_label: string
  model: string
  started_at: number
  ended_at: number | null
  wall_ms: number | null
  model_ms: number | null
  tool_ms: number | null
  steps: number
  tool_calls: number
  total_tokens: number
  aborted: boolean
  usd?: number
  usd_by_model?: { model_key: string; usd: number }[]
}

export interface QuotaAlignmentTask {
  agent: string
  session_id: string
  started_at: number
  ended_at: number | null
  project: string
  project_raw: string
  project_label: string
  turns: QuotaAlignmentTurn[]
}

export interface QuotaAlignmentTasks {
  total_tasks: number
  returned_tasks: number
  returned_turns: number
  truncated: boolean
  tasks: QuotaAlignmentTask[]
}

export interface QuotaAlignmentResult {
  platform: SubscriptionPlatform
  kind: '5h'
  from: number
  to: number
  quota: QuotaTimeline
  tasks: QuotaAlignmentTasks
}

/** 有界双轨查询（from/to 为 unix 秒；首版视口 5h，后端上限 24h）。 */
export async function getQuotaAlignment(
  platform: SubscriptionPlatform,
  from: number,
  to: number,
  project?: string,
): Promise<QuotaAlignmentResult | null> {
  return tryInvoke<QuotaAlignmentResult>('get_quota_alignment', { platform, from, to, project: project || null })
}
