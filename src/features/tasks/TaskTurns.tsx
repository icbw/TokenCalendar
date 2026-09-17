// 逐轮条形（Tasks 行展开）:每轮一条,长度 = wall_ms（按本任务最长轮归一）,
// 内部分段 model / tool（两者之和超过 wall 时按比例压回——JSONL 族为估算值）,
// 右侧标 steps 与 tool 数;中止轮（aborted,S4-R 起与错误分列）以错误色描边标记,
// 带 API / 工具错误的轮在右侧计数后追加错误数。
// 只读 get_task_turns 的数值列,不含任何内容列。
import type { TaskTurn } from '../../services'
import { formatCompact } from '../matrix/matrixScale'
import { formatDuration } from '../insights/analytics'

const MONTH_ABBR = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']
const pad2 = (n: number) => String(n).padStart(2, '0')

function clock(ms: number): string {
  const d = new Date(ms)
  return `${MONTH_ABBR[d.getMonth()]} ${d.getDate()} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`
}

function turnHint(t: TaskTurn): string {
  const parts = [
    `Turn ${t.turnSeq} · ${clock(t.startedAt)}${t.aborted ? ' · aborted' : ''}`,
    `Model ${t.model}`,
    `Wait ${formatDuration(t.wallMs)} (model ${formatDuration(t.modelMs)} · tool ${formatDuration(t.toolMs)})`,
    `${t.steps} steps · ${t.toolCalls} tools`,
  ]
  if (t.subagentCalls > 0) parts.push(`${t.subagentCount} subagents · ${t.subagentCalls} subagent calls`)
  if (t.ttftMs !== null) parts.push(`First token ${(t.ttftMs / 1000).toFixed(1)}s`)
  if (t.gapMs !== null) parts.push(`Gap before ${formatDuration(t.gapMs)}`)
  parts.push(`${formatCompact(t.totalTokens)} tokens`)
  if (t.errorCount > 0) parts.push(`Errors: ${t.errorCount}`)
  if (t.retryCount > 0) parts.push(`Retries: ${t.retryCount}`)
  return parts.join('\n')
}

export default function TaskTurns({ turns }: { turns: TaskTurn[] | null | undefined }) {
  if (turns === undefined) return <div className="insight-empty">Loading turns…</div>
  if (turns === null) return <div className="insight-empty">Turns unavailable (service not running)</div>
  if (turns.length === 0) return <div className="insight-empty">No turns recorded for this task</div>

  const maxWall = Math.max(1, ...turns.map((t) => t.wallMs ?? 0))
  const anyTiming = turns.some((t) => t.wallMs !== null)
  const aborted = turns.filter((t) => t.aborted).length

  return (
    <div className="turns">
      <div className="turns-head">
        <span>{turns.length} turns{aborted > 0 ? ` · ${aborted} aborted` : ''}</span>
        {anyTiming && <span>Longest {formatDuration(maxWall)}</span>}
        <span className="turns-legend">
          <span className="legend-item"><span className="legend-swatch turn-seg-model" />Model</span>
          <span className="legend-item"><span className="legend-swatch turn-seg-tool" />Tool</span>
          <span className="legend-item"><span className="legend-swatch turn-seg-rest" />Other</span>
          <span className="legend-item"><span className="legend-swatch turn-swatch-error" />Aborted</span>
        </span>
      </div>
      <div className="turns-list">
        {turns.map((t) => {
          const wall = t.wallMs
          let modelPct = 0
          let toolPct = 0
          if (wall !== null && wall > 0) {
            const m = Math.max(0, t.modelMs ?? 0)
            const tl = Math.max(0, t.toolMs ?? 0)
            const scale = m + tl > wall ? wall / (m + tl) : 1
            modelPct = ((m * scale) / wall) * 100
            toolPct = ((tl * scale) / wall) * 100
          }
          return (
            <div key={t.turnSeq} className={`turn-row${t.aborted ? ' is-aborted' : ''}`} title={turnHint(t)}>
              <span className="turn-seq">#{t.turnSeq}</span>
              <div className="turn-track">
                {wall === null ? (
                  <span className="turn-na">no timing</span>
                ) : (
                  <div className="turn-bar" style={{ width: `${Math.max(0.6, (wall / maxWall) * 100)}%` }}>
                    <span className="turn-seg-model" style={{ width: `${modelPct}%` }} />
                    <span className="turn-seg-tool" style={{ width: `${toolPct}%` }} />
                  </div>
                )}
              </div>
              <span className="turn-meta">
                {t.steps} steps · {t.toolCalls} tools
                {t.errorCount > 0 && <span className="turn-errors"> · {t.errorCount} {t.errorCount === 1 ? 'error' : 'errors'}</span>}
              </span>
              <span className="turn-wall">{formatDuration(wall)}</span>
            </div>
          )
        })}
      </div>
    </div>
  )
}
