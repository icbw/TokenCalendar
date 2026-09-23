// 逐轮条形（Tasks 行展开）:每轮一条,长度 = wall_ms（按本任务最长轮归一）,
// 内部分段 model / tool（两者之和超过 wall 时按比例压回——JSONL 族为估算值）,
// 右侧标 steps 与 tool 数;中止轮（aborted,与错误分列）以错误色描边标记,
// 带 API / 工具错误的轮在右侧计数后追加错误数。
// 只读 get_task_turns 的数值列,不含任何内容列。
import type { TaskTurn } from '../../services'
import { formatCompact } from '../matrix/matrixScale'
import { formatDuration } from '../insights/analytics'
import { useT, type Translator } from '../../lib/i18n'
import { clockLabel } from './taskFormat'

function turnHint(tn: TaskTurn, t: Translator<'tasks'>): string {
  const parts = [
    `${t('turnHead', { seq: tn.turnSeq, time: clockLabel(tn.startedAt) })}${tn.aborted ? ` · ${t('turnAborted')}` : ''}`,
    t('turnModel', { model: tn.model }),
    t('turnWait', { wall: formatDuration(tn.wallMs), model: formatDuration(tn.modelMs), tool: formatDuration(tn.toolMs) }),
    t('stepsTools', { steps: tn.steps, tools: tn.toolCalls }),
  ]
  if (tn.subagentCalls > 0) parts.push(t('turnSubagents', { n: tn.subagentCount, calls: tn.subagentCalls }))
  if (tn.ttftMs !== null) parts.push(t('turnFirstToken', { s: (tn.ttftMs / 1000).toFixed(1) }))
  if (tn.gapMs !== null) parts.push(t('turnGap', { d: formatDuration(tn.gapMs) }))
  parts.push(t('turnTokens', { n: formatCompact(tn.totalTokens) }))
  if (tn.errorCount > 0) parts.push(t('turnErrors', { n: tn.errorCount }))
  if (tn.retryCount > 0) parts.push(t('turnRetries', { n: tn.retryCount }))
  return parts.join('\n')
}

export default function TaskTurns({ turns }: { turns: TaskTurn[] | null | undefined }) {
  const t = useT('tasks')
  if (turns === undefined) return <div className="insight-empty">{t('loadingTurns')}</div>
  if (turns === null) return <div className="insight-empty">{t('turnsUnavailable')}</div>
  if (turns.length === 0) return <div className="insight-empty">{t('noTurnsTask')}</div>

  const maxWall = Math.max(1, ...turns.map((tn) => tn.wallMs ?? 0))
  const anyTiming = turns.some((tn) => tn.wallMs !== null)
  const aborted = turns.filter((tn) => tn.aborted).length

  return (
    <div className="turns">
      <div className="turns-head">
        <span>{t('turnsN', { n: turns.length })}{aborted > 0 ? ` · ${t('abortedN', { n: aborted })}` : ''}</span>
        {anyTiming && <span>{t('longest', { d: formatDuration(maxWall) })}</span>}
        <span className="turns-legend">
          <span className="legend-item"><span className="legend-swatch turn-seg-model" />{t('legendModel')}</span>
          <span className="legend-item"><span className="legend-swatch turn-seg-tool" />{t('legendTool')}</span>
          <span className="legend-item"><span className="legend-swatch turn-seg-rest" />{t('legendOther')}</span>
          <span className="legend-item"><span className="legend-swatch turn-swatch-error" />{t('legendAborted')}</span>
        </span>
      </div>
      <div className="turns-list">
        {turns.map((tn) => {
          const wall = tn.wallMs
          let modelPct = 0
          let toolPct = 0
          if (wall !== null && wall > 0) {
            const m = Math.max(0, tn.modelMs ?? 0)
            const tl = Math.max(0, tn.toolMs ?? 0)
            const scale = m + tl > wall ? wall / (m + tl) : 1
            modelPct = ((m * scale) / wall) * 100
            toolPct = ((tl * scale) / wall) * 100
          }
          return (
            <div key={tn.turnSeq} className={`turn-row${tn.aborted ? ' is-aborted' : ''}`} title={turnHint(tn, t)}>
              <span className="turn-seq">#{tn.turnSeq}</span>
              <div className="turn-track">
                {wall === null ? (
                  <span className="turn-na">{t('noTiming')}</span>
                ) : (
                  <div className="turn-bar" style={{ width: `${Math.max(0.6, (wall / maxWall) * 100)}%` }}>
                    <span className="turn-seg-model" style={{ width: `${modelPct}%` }} />
                    <span className="turn-seg-tool" style={{ width: `${toolPct}%` }} />
                  </div>
                )}
              </div>
              <span className="turn-meta">
                {t('stepsTools', { steps: tn.steps, tools: tn.toolCalls })}
                {tn.errorCount > 0 && (
                  <span className="turn-errors"> · {t(tn.errorCount === 1 ? 'errorsN_one' : 'errorsN_other', { n: tn.errorCount })}</span>
                )}
              </span>
              <span className="turn-wall">{formatDuration(wall)}</span>
            </div>
          )
        })}
      </div>
    </div>
  )
}
