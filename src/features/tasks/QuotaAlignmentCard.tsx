import { useEffect, useState } from 'react'
import { alignmentService, events } from '../../services'
import type { QuotaAlignmentResult, QuotaAlignmentTurn, QuotaDiscontinuityReason, SubscriptionPlatform } from '../../services'
import { fmt, useT, type Translator } from '../../lib/i18n'
import { Seg } from '../insights/Seg'
import { formatCompact } from '../matrix/matrixScale'
import { formatDuration } from '../insights/analytics'
import { usd2 } from './taskFormat'

const VIEWPORT_HOURS = [5, 12, 24] as const
type ViewportHours = (typeof VIEWPORT_HOURS)[number]
const GAP_SECONDS = 30 * 60
const VISIBLE_TASKS = 12
const WIDTH = 1000
const LEFT = 190
const RIGHT = 20
const TOP = 10
const QUOTA_HEIGHT = 120
const AXIS_Y = TOP + QUOTA_HEIGHT + 20
const LANES_Y = AXIS_Y + 20
const ROW_HEIGHT = 24
const BOTTOM = 24

const fullTime = (seconds: number) =>
  fmt.dateTime(seconds * 1000, { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit' })
const tickTime = (seconds: number) => fmt.dateTime(seconds * 1000, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
const timeOnly = (seconds: number) => fmt.dateTime(seconds * 1000, { hour: '2-digit', minute: '2-digit' })
const clamp = (value: number, lo: number, hi: number) => Math.max(lo, Math.min(value, hi))
const clipped = (value: string, max = 24) => {
  const chars = Array.from(value)
  return chars.length > max ? `${chars.slice(0, max - 1).join('')}…` : value
}
const sourceLabel = (source: string) =>
  source === 'api' ? 'API' : source === 'desktop' ? 'Desktop' : source === 'rollout' ? 'Rollout' : source

type LabelAnchor = 'start' | 'middle' | 'end'
interface ReadingLabelInput {
  key: string
  text: string
  shortText: string
  x: number
  y: number
}
interface ReadingLabel {
  key: string
  text: string
  x: number
  y: number
  anchor: LabelAnchor
}
interface LabelBox {
  left: number
  right: number
  top: number
  bottom: number
}

function layoutReadingLabels(inputs: ReadingLabelInput[]): ReadingLabel[] {
  const labels: ReadingLabel[] = []
  const boxes: LabelBox[] = []
  for (const input of inputs) {
    let match: { label: ReadingLabel; box: LabelBox } | undefined
    const texts = input.text === input.shortText ? [input.text] : [input.text, input.shortText]
    for (const text of texts) {
      const width = Array.from(text).reduce((n, char) => n + (char.charCodeAt(0) > 255 ? 10 : 5.8), 4)
      const candidates: { dx: number; dy: number; anchor: LabelAnchor }[] = []
      const sides = input.x + width + 12 > WIDTH - RIGHT ? [-1, 1] : [1, -1]
      for (const side of sides) {
        for (const dy of [-7, 17, -21, 31]) {
          candidates.push({ dx: side * 7, dy, anchor: side < 0 ? 'end' : 'start' })
        }
      }
      candidates.push({ dx: 0, dy: -21, anchor: 'middle' }, { dx: 0, dy: 29, anchor: 'middle' })
      for (const candidate of candidates) {
        const labelX = input.x + candidate.dx
        const baseline = clamp(input.y + candidate.dy, TOP + 11, TOP + QUOTA_HEIGHT - 3)
        const left = candidate.anchor === 'start' ? labelX : candidate.anchor === 'end' ? labelX - width : labelX - width / 2
        const box = { left, right: left + width, top: baseline - 10, bottom: baseline + 2 }
        if (box.left < LEFT || box.right > WIDTH - RIGHT || box.top < TOP || box.bottom > TOP + QUOTA_HEIGHT) continue
        if (boxes.some((other) => box.left < other.right + 3 && box.right + 3 > other.left && box.top < other.bottom + 3 && box.bottom + 3 > other.top)) continue
        match = {
          label: { key: input.key, text, x: labelX, y: baseline, anchor: candidate.anchor },
          box,
        }
        break
      }
      if (match) break
    }
    if (match) {
      labels.push(match.label)
      boxes.push(match.box)
    }
  }
  return labels
}

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

function AlignmentChart({ data, showAll }: {
  data: QuotaAlignmentResult
  showAll: boolean
}) {
  const t = useT('tasks')
  const { from, to } = data
  const sameDay = new Date(from * 1000).toDateString() === new Date(to * 1000).toDateString()
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

  const readingGroups = new Map<string, { t: number; used: number; sources: Set<string> }>()
  if (data.quota.raw_readings <= 5) {
    for (const window of data.quota.windows) {
      for (const reading of window.readings) {
        const key = `${reading.t}:${reading.used_percent}`
        const group = readingGroups.get(key)
        if (group) group.sources.add(sourceLabel(reading.source))
        else readingGroups.set(key, { t: reading.t, used: reading.used_percent, sources: new Set([sourceLabel(reading.source)]) })
      }
    }
  }
  const readingLabels = layoutReadingLabels(Array.from(readingGroups, ([key, group]) => {
    const pct = fmt.number(group.used, { maximumFractionDigits: 1 })
    return {
      key,
      text: `${Array.from(group.sources).join(' + ')} · ${pct}%`,
      shortText: `${pct}%`,
      x: x(group.t),
      y: y(group.used),
    }
  }))

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
            const label = sameDay && index > 0 && index < 4 ? timeOnly(at) : tickTime(at)
            return (
              <text key={at} x={x(at)} y={AXIS_Y} textAnchor={index === 0 ? 'start' : index === 4 ? 'end' : 'middle'} className="quota-align-tick">
                {label}
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
            window.readings.map((reading) => (
              <circle key={`${window.id}-${reading.t}-${reading.source}`} cx={x(reading.t)} cy={y(reading.used_percent)} r={2.4} className="quota-align-point">
                <title>{[
                  fullTime(reading.t),
                  t('alignUsed', { pct: fmt.number(reading.used_percent, { maximumFractionDigits: 1 }) }),
                  t('alignSource', { source: sourceLabel(reading.source) }),
                  reading.plan_type ? t('alignPlan', { plan: reading.plan_type }) : '',
                  reading.resets_at === null ? t('alignNoReset') : t('alignResetAt', { time: fullTime(reading.resets_at) }),
                ].filter(Boolean).join('\n')}</title>
              </circle>
            )),
          )}
          {readingLabels.map((label) => (
            <text key={label.key} x={label.x} y={label.y} textAnchor={label.anchor} className="quota-align-reading-label">
              {label.text}
            </text>
          ))}
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
                <text x={8} y={laneY + 16} className="quota-align-task-label">
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
                      y={laneY + 5}
                      width={width}
                      height={14}
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
    <span className="quota-align-evidence" role="note">
      {sparse && <span>{t('alignSparseReadings', { n: fmt.number(raw), sources })}</span>}
      {latestUnknown && (
        <span className="quota-align-warning">
          {latest?.resets_at === null
            ? t('alignLatestNoTail', { source: sourceLabel(latest.source) })
            : t('alignLatestUnknown')}
        </span>
      )}
    </span>
  )
}

export default function QuotaAlignmentCard({ project }: { project: string }) {
  const t = useT('tasks')
  const [platform, setPlatform] = useState<SubscriptionPlatform>('codex')
  const [viewportHours, setViewportHours] = useState<ViewportHours>(24)
  const [to, setTo] = useState(() => Math.floor(Date.now() / 1000))
  const [followLatest, setFollowLatest] = useState(true)
  const [refreshTick, setRefreshTick] = useState(0)
  const [data, setData] = useState<QuotaAlignmentResult | null | undefined>(undefined)
  const [loading, setLoading] = useState(false)
  const [showAll, setShowAll] = useState(false)
  const viewportSeconds = viewportHours * 3600

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    void alignmentService.getQuotaAlignment(platform, to - viewportSeconds, to, project).then((result) => {
      if (cancelled) return
      setData(result)
      setLoading(false)
    })
    return () => {
      cancelled = true
    }
  }, [platform, to, project, refreshTick, viewportSeconds])

  useEffect(() => setShowAll(false), [platform, project, viewportHours])

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
          <span className="insight-card-title" title={t('alignSubtitle')}>{t('alignTitle')}</span>
        </div>
        <div className="quota-align-controls">
          <Seg value={platform} options={[{ v: 'codex', label: 'Codex' }, { v: 'claude', label: 'Claude' }]} onChange={setPlatform} />
          <Seg<ViewportHours>
            value={viewportHours}
            options={VIEWPORT_HOURS.map((hours) => ({ v: hours, label: `${hours}h`, hint: t('alignViewportHint', { hours }) }))}
            onChange={setViewportHours}
          />
          <div className="toolbar-group">
            <button
              className="seg quota-align-nav"
              aria-label={t('alignPrev', { hours: viewportHours })}
              title={t('alignPrevHint', { hours: viewportHours })}
              onClick={() => { setFollowLatest(false); setTo((value) => value - viewportSeconds) }}
            >
              ←
            </button>
            <button
              className={`seg quota-align-nav${followLatest ? ' is-disabled' : ''}`}
              aria-label={t('alignNext', { hours: viewportHours })}
              aria-disabled={followLatest || undefined}
              title={t('alignNextHint', { hours: viewportHours })}
              onClick={() => {
                if (followLatest) return
                const next = to + viewportSeconds
                if (next >= Math.floor(Date.now() / 1000)) latest()
                else setTo(next)
              }}
            >
              →
            </button>
            <button className={`seg${followLatest ? ' is-active' : ''}`} title={t('alignLatestHint', { hours: viewportHours })} onClick={latest}>
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
          <div className="quota-align-meta">
            <span className="quota-align-range" title={`${fullTime(data.from)} — ${fullTime(data.to)}`}>
              {tickTime(data.from)} – {tickTime(data.to)}
            </span>
            <span>{t('alignSubtitle')}</span>
            <EvidenceNote data={data} />
          </div>
          <AlignmentChart data={data} showAll={showAll} />
          <footer className="quota-align-footer">
            <span>{t('alignTasksShown', { shown: data.tasks.returned_tasks, total: data.tasks.total_tasks })}</span>
            {data.quota.downsampled && <span>{t('alignDownsampled', { shown: data.quota.returned_readings, raw: data.quota.raw_readings })}</span>}
            {data.tasks.truncated && <span className="quota-align-warning">{t('alignTruncated', { tasks: data.tasks.returned_tasks, total: data.tasks.total_tasks, turns: data.tasks.returned_turns })}</span>}
            {data.tasks.tasks.length > VISIBLE_TASKS && (
              <button className="seg quota-align-show-all" onClick={() => setShowAll((value) => !value)}>
                {showAll ? t('alignShowFewer') : t('alignShowAll')}
              </button>
            )}
          </footer>
        </>
      )}
    </section>
  )
}
