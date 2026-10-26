import { useEffect, useState } from 'react'
import { alignmentService, events } from '../../services'
import type { QuotaAlignmentResult, QuotaAlignmentTurn, QuotaDiscontinuityReason, SubscriptionPlatform } from '../../services'
import { fmt, useT, type Translator } from '../../lib/i18n'
import { Seg } from '../insights/Seg'
import { formatCompact } from '../matrix/matrixScale'
import { formatDuration } from '../insights/analytics'
import { usd2 } from './taskFormat'

const FIVE_HOURS = 5 * 3600
const GAP_SECONDS = 30 * 60
const VISIBLE_TASKS = 12
const WIDTH = 1000
const LEFT = 190
const RIGHT = 20
const TOP = 10
const QUOTA_HEIGHT = 140
const AXIS_Y = 170
const LANES_Y = 190
const ROW_HEIGHT = 28
const BOTTOM = 28

const fullTime = (seconds: number) =>
  fmt.dateTime(seconds * 1000, { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit' })
const tickTime = (seconds: number) => fmt.dateTime(seconds * 1000, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
const clamp = (value: number, lo: number, hi: number) => Math.max(lo, Math.min(value, hi))
const clipped = (value: string, max = 24) => {
  const chars = Array.from(value)
  return chars.length > max ? `${chars.slice(0, max - 1).join('')}…` : value
}
const sourceLabel = (source: string) =>
  source === 'api' ? 'API' : source === 'desktop' ? 'Desktop' : source === 'rollout' ? 'Rollout' : source

function reasonText(t: Translator<'tasks'>, reason: QuotaDiscontinuityReason): string {
  switch (reason) {
    case 'reset': return t('alignReset')
    case 'window_started': return t('alignWindowStarted')
    case 'plan_changed': return t('alignPlanChanged')
    case 'tail_regressed': return t('alignTailRegressed')
    case 'tail_changed': return t('alignTailChanged')
    case 'evidence_missing': return t('alignEvidenceMissing')
  }
}

function turnTitle(turn: QuotaAlignmentTurn, t: Translator<'tasks'>): string {
  const lines = [
    t('alignTurnRange', {
      seq: turn.turn_seq,
      from: fullTime(turn.started_at / 1000),
      to: turn.ended_at === null ? t('alignOpenEnd') : fullTime(turn.ended_at / 1000),
    }),
    `${turn.project_label}\n${turn.project_raw}`,
    t('turnModel', { model: turn.model }),
    t('stepsTools', { steps: fmt.number(turn.steps), tools: fmt.number(turn.tool_calls) }),
    t('turnTokens', { n: formatCompact(turn.total_tokens) }),
  ]
  lines.push(turn.wall_ms === null
    ? t('noTiming')
    : t('turnWait', { wall: formatDuration(turn.wall_ms), model: formatDuration(turn.model_ms), tool: formatDuration(turn.tool_ms) }))
  if (turn.aborted) lines.push(t('turnAborted'))
  if (turn.usd !== undefined) lines.push(t('turnCostUsd', { usd: usd2(turn.usd) }))
  for (const model of turn.usd_by_model ?? []) lines.push(t('turnModelCost', { model: model.model_key, usd: usd2(model.usd) }))
  return lines.join('\n')
}

function AlignmentChart({ data, showAll, onToggleTasks }: {
  data: QuotaAlignmentResult
  showAll: boolean
  onToggleTasks: () => void
}) {
  const t = useT('tasks')
  const { from, to } = data
  const plotWidth = WIDTH - LEFT - RIGHT
  const x = (seconds: number) => LEFT + ((clamp(seconds, from, to) - from) / (to - from)) * plotWidth
  const y = (used: number) => TOP + (1 - clamp(used, 0, 100) / 100) * QUOTA_HEIGHT
  const tasks = showAll ? data.tasks.tasks : data.tasks.tasks.slice(0, VISIBLE_TASKS)
  const height = LANES_Y + Math.max(tasks.length, 1) * ROW_HEIGHT + BOTTOM
  const lineBottom = height - BOTTOM

  const lines: { key: string; points: string }[] = []
  const gaps: { key: string; from: number; to: number }[] = []
  for (const window of data.quota.windows) {
    let run: typeof window.readings = []
    const flush = () => {
      if (run.length > 1) lines.push({ key: `${window.id}-${run[0].t}-${run[run.length - 1].t}`, points: run.map((r) => `${x(r.t)},${y(r.used_percent)}`).join(' ') })
      run = []
    }
    for (let index = 0; index < window.readings.length; index += 1) {
      const reading = window.readings[index]
      const previous = window.readings[index - 1]
      if (previous && reading.t - previous.t > GAP_SECONDS) {
        gaps.push({ key: `${window.id}-${previous.t}-${reading.t}`, from: previous.t, to: reading.t })
        flush()
      }
      run.push(reading)
    }
    flush()
  }

  return (
    <>
      <div className="quota-align-scroll">
        <svg className="quota-align-chart" viewBox={`0 0 ${WIDTH} ${height}`} role="img" aria-label={t('alignTitle')}>
          {data.quota.windows.map((window) => {
            if (window.start_at === null || window.end_at === null) return null
            const left = x(Math.max(window.start_at, from))
            const right = x(Math.min(window.end_at, to))
            if (right <= left) return null
            return (
              <rect key={`window-${window.id}`} x={left} y={TOP} width={right - left} height={QUOTA_HEIGHT} className="quota-align-window">
                <title>{`${t('alignWindowDerived')}\n${fullTime(window.start_at)}–${fullTime(window.end_at)}`}</title>
              </rect>
            )
          })}
          {gaps.map((gap) => (
            <rect key={gap.key} x={x(gap.from)} y={TOP} width={Math.max(1, x(gap.to) - x(gap.from))} height={QUOTA_HEIGHT} className="quota-align-gap">
              <title>{t('alignGap', { duration: formatDuration((gap.to - gap.from) * 1000) })}</title>
            </rect>
          ))}
          {[0, 25, 50, 75, 100].map((tick) => (
            <g key={tick}>
              <line x1={LEFT} x2={WIDTH - RIGHT} y1={y(tick)} y2={y(tick)} className="quota-align-grid" />
              <text x={LEFT - 8} y={y(tick) + 3} textAnchor="end" className="quota-align-tick">{tick}%</text>
            </g>
          ))}
          {[0, 1, 2, 3, 4].map((index) => {
            const at = from + ((to - from) * index) / 4
            return (
              <text key={at} x={x(at)} y={AXIS_Y} textAnchor={index === 0 ? 'start' : index === 4 ? 'end' : 'middle'} className="quota-align-tick">
                {tickTime(at)}
              </text>
            )
          })}
          <text x={8} y={TOP + 10} className="quota-align-axis-title">{t('alignQuotaAxis')}</text>

          {data.quota.discontinuities.map((item, index) => {
            const title = `${reasonText(t, item.reason)}\n${fullTime(item.from_t)}–${fullTime(item.to_t)}`
            if (item.at !== null && item.at >= from && item.at <= to) {
              return (
                <line key={`disc-${index}`} x1={x(item.at)} x2={x(item.at)} y1={TOP} y2={lineBottom} className={`quota-align-disc${item.reason === 'reset' ? ' is-reset' : ''}`}>
                  <title>{`${title}\n${fullTime(item.at)}`}</title>
                </line>
              )
            }
            const left = x(item.from_t)
            const right = x(item.to_t)
            return (
              <rect key={`disc-${index}`} x={left} y={TOP} width={Math.max(2, right - left)} height={lineBottom - TOP} className="quota-align-disc-band">
                <title>{title}</title>
              </rect>
            )
          })}
          {data.quota.windows.flatMap((window) => {
            const boundaries = [
              { at: window.start_at, hint: t('alignWindowDerived') },
              { at: window.end_at, hint: t('alignWindowAnnounced') },
            ].filter((item): item is { at: number; hint: string } => item.at !== null && item.at >= from && item.at <= to)
            return boundaries.map(({ at, hint }) => (
              <line key={`boundary-${window.id}-${at}`} x1={x(at)} x2={x(at)} y1={TOP} y2={lineBottom} className="quota-align-boundary">
                <title>{`${hint}\n${fullTime(at)}`}</title>
              </line>
            ))
          })}
          {data.quota.windows.filter((window) => window.start_at === null || window.end_at === null).map((window) => (
            <text key={`unknown-${window.id}`} x={x(window.first_reading_at)} y={TOP + 13} textAnchor="middle" className="quota-align-unknown">
              ?<title>{t('alignWindowUnknown')}</title>
            </text>
          ))}
          {lines.map((line) => (
            <polyline key={line.key} points={line.points} className="quota-align-line" />
          ))}
          {data.quota.windows.flatMap((window) =>
            window.readings.map((reading) => {
              const pointX = x(reading.t)
              const pointY = y(reading.used_percent)
              const nearRight = pointX > WIDTH - RIGHT - 110
              const anchor = nearRight ? 'end' : 'start'
              return (
                <g key={`${window.id}-${reading.t}-${reading.source}`}>
                  <circle cx={pointX} cy={pointY} r={2.4} className="quota-align-point">
                    <title>{[
                      fullTime(reading.t),
                      t('alignUsed', { pct: fmt.number(reading.used_percent, { maximumFractionDigits: 1 }) }),
                      t('alignSource', { source: sourceLabel(reading.source) }),
                      reading.plan_type ? t('alignPlan', { plan: reading.plan_type }) : '',
                      reading.resets_at === null ? t('alignNoReset') : t('alignResetAt', { time: fullTime(reading.resets_at) }),
                    ].filter(Boolean).join('\n')}</title>
                  </circle>
                  {data.quota.raw_readings <= 5 && (
                    <text x={pointX + (nearRight ? -6 : 6)} y={Math.max(TOP + 11, pointY - 7)} textAnchor={anchor} className="quota-align-reading-label">
                      {`${sourceLabel(reading.source)} · ${fmt.number(reading.used_percent, { maximumFractionDigits: 1 })}%`}
                    </text>
                  )}
                </g>
              )
            }),
          )}
          {data.quota.raw_readings === 0 && (
            <text x={LEFT + plotWidth / 2} y={TOP + QUOTA_HEIGHT / 2} textAnchor="middle" className="quota-align-empty">{t('alignNoReadings')}</text>
          )}

          <text x={8} y={LANES_Y - 8} className="quota-align-axis-title">{t('alignTasksAxis')}</text>
          {tasks.length === 0 && <text x={LEFT + plotWidth / 2} y={LANES_Y + 20} textAnchor="middle" className="quota-align-empty">{t('alignNoTasks')}</text>}
          {tasks.map((task, lane) => {
            const laneY = LANES_Y + lane * ROW_HEIGHT
            const laneTitle = `${task.project_label}\n${task.project_raw}\n${t('alignSessionStart', { time: fullTime(task.started_at / 1000) })}\n${t('turnsN', { n: task.turns.length })}`
            return (
              <g key={`${task.agent}-${task.session_id}`}>
                <line x1={LEFT} x2={WIDTH - RIGHT} y1={laneY + ROW_HEIGHT} y2={laneY + ROW_HEIGHT} className="quota-align-lane-line" />
                <text x={8} y={laneY + 18} className="quota-align-task-label">
                  {clipped(`${task.project_label} · ${task.turns.length}`, 28)}
                  <title>{laneTitle}</title>
                </text>
                {task.turns.map((turn) => {
                  const started = turn.started_at / 1000
                  const ended = turn.ended_at === null ? null : turn.ended_at / 1000
                  const left = x(started)
                  const right = ended === null ? left : x(ended)
                  const width = ended === null ? 4 : Math.max(2, right - left)
                  const title = turnTitle(turn, t)
                  return (
                    <rect
                      key={`${task.session_id}-${turn.turn_seq}-${turn.started_at}`}
                      x={clamp(left - (ended === null ? 2 : 0), LEFT, WIDTH - RIGHT - width)}
                      y={laneY + 7}
                      width={width}
                      height={13}
                      rx={ended === null ? 2 : 3}
                      className={`quota-align-turn${turn.aborted ? ' is-aborted' : ''}${ended === null ? ' is-open' : ''}`}
                    >
                      <title>{title}</title>
                    </rect>
                  )
                })}
              </g>
            )
          })}
        </svg>
      </div>
      {data.tasks.tasks.length > VISIBLE_TASKS && (
        <button className="seg quota-align-show-all" onClick={onToggleTasks}>
          {showAll ? t('alignShowFewer') : t('alignShowAll')}
        </button>
      )}
    </>
  )
}

function EvidenceNote({ data }: { data: QuotaAlignmentResult }) {
  const t = useT('tasks')
  const raw = data.quota.raw_readings
  const sparse = raw > 0 && raw <= 5
  const latestWindow = data.quota.windows.length === 0
    ? undefined
    : data.quota.windows.reduce((a, b) => (a.last_reading_at > b.last_reading_at ? a : b))
  const latest = latestWindow?.readings[latestWindow.readings.length - 1]
  const latestUnknown = latestWindow !== undefined && (latestWindow.start_at === null || latestWindow.end_at === null)
  if (!sparse && !latestUnknown) return null

  const counts = new Map<string, number>()
  if (sparse) {
    for (const window of data.quota.windows) {
      for (const reading of window.readings) counts.set(reading.source, (counts.get(reading.source) ?? 0) + 1)
    }
  }
  const sources = Array.from(counts, ([source, count]) => `${sourceLabel(source)} ×${count}`).join(' · ')

  return (
    <div className={`quota-align-evidence${sparse ? ' is-sparse' : ''}`} role="note">
      {sparse && <span>{t('alignSparseReadings', { n: fmt.number(raw), sources })}</span>}
      {latestUnknown && (
        <span>
          {latest?.resets_at === null
            ? t('alignLatestNoTail', { source: sourceLabel(latest.source) })
            : t('alignLatestUnknown')}
        </span>
      )}
    </div>
  )
}

export default function QuotaAlignmentCard({ project }: { project: string }) {
  const t = useT('tasks')
  const [platform, setPlatform] = useState<SubscriptionPlatform>('codex')
  const [to, setTo] = useState(() => Math.floor(Date.now() / 1000))
  const [followLatest, setFollowLatest] = useState(true)
  const [refreshTick, setRefreshTick] = useState(0)
  const [data, setData] = useState<QuotaAlignmentResult | null | undefined>(undefined)
  const [loading, setLoading] = useState(false)
  const [showAll, setShowAll] = useState(false)

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    void alignmentService.getQuotaAlignment(platform, to - FIVE_HOURS, to, project).then((result) => {
      if (cancelled) return
      setData(result)
      setLoading(false)
    })
    return () => {
      cancelled = true
    }
  }, [platform, to, project, refreshTick])

  useEffect(() => setShowAll(false), [platform, project])

  useEffect(() => {
    if (!followLatest) return
    let timer = 0
    let cancelled = false
    let subscriptions: (() => void)[] = []
    const schedule = () => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => {
        setTo(Math.floor(Date.now() / 1000))
        setRefreshTick((tick) => tick + 1)
      }, 300)
    }
    void events.onSubscriptionChanged(schedule).then((off) => {
      if (cancelled) off()
      else subscriptions.push(off)
    })
    void events.onUsageChanged(schedule).then((off) => {
      if (cancelled) off()
      else subscriptions.push(off)
    })
    return () => {
      cancelled = true
      window.clearTimeout(timer)
      for (const off of subscriptions) off()
      subscriptions = []
    }
  }, [followLatest])

  const latest = () => {
    setFollowLatest(true)
    setTo(Math.floor(Date.now() / 1000))
    setRefreshTick((tick) => tick + 1)
  }

  return (
    <section className="insight-card quota-align-card">
      <header className="insight-card-header quota-align-header">
        <div className="quota-align-heading">
          <span className="insight-card-title">{t('alignTitle')}</span>
          <span className="insight-card-sub">{t('alignSubtitle')}</span>
        </div>
        <div className="quota-align-controls">
          <Seg value={platform} options={[{ v: 'codex', label: 'Codex' }, { v: 'claude', label: 'Claude' }]} onChange={setPlatform} />
          <div className="toolbar-group">
            <button className="seg" title={t('alignPrevHint')} onClick={() => { setFollowLatest(false); setTo((value) => value - FIVE_HOURS) }}>
              {t('alignPrev')}
            </button>
            <button
              className={`seg${followLatest ? ' is-disabled' : ''}`}
              aria-disabled={followLatest || undefined}
              title={t('alignNextHint')}
              onClick={() => {
                if (followLatest) return
                const next = to + FIVE_HOURS
                if (next >= Math.floor(Date.now() / 1000)) latest()
                else setTo(next)
              }}
            >
              {t('alignNext')}
            </button>
            <button className={`seg${followLatest ? ' is-active' : ''}`} title={t('alignLatestHint')} onClick={latest}>
              {t('alignLatest')}
            </button>
          </div>
          {loading && <span className="matrix-loading">{t('loading')}</span>}
        </div>
      </header>
      {data === undefined ? (
        <div className="insight-empty">{t('loading')}</div>
      ) : data === null ? (
        <div className="insight-empty">{t('alignUnavailable')}</div>
      ) : (
        <>
          <div className="quota-align-range">{fullTime(data.from)} — {fullTime(data.to)}</div>
          <EvidenceNote data={data} />
          <AlignmentChart data={data} showAll={showAll} onToggleTasks={() => setShowAll((value) => !value)} />
          <footer className="quota-align-footer">
            <span>{t('alignTasksShown', { shown: data.tasks.returned_tasks, total: data.tasks.total_tasks })}</span>
            {data.quota.downsampled && <span>{t('alignDownsampled', { shown: data.quota.returned_readings, raw: data.quota.raw_readings })}</span>}
            {data.tasks.truncated && <span className="quota-align-warning">{t('alignTruncated', { tasks: data.tasks.returned_tasks, total: data.tasks.total_tasks, turns: data.tasks.returned_turns })}</span>}
          </footer>
        </>
      )}
    </section>
  )
}
