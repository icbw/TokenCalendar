// 逐轮明细（Tasks 行展开）:**渲染成外层任务表的真表格行**——每轮一行,字段值逐列
// 落在表头同名列的正下方（同一张表决定列宽,对齐是浏览器保证的,不是目测拼的）:
// 轮次列 = 轮号,步数 / 工具调用 / 等待 / 模型耗时 / 工具耗时 / 错误 / 子 Agent / Token /
// 消耗列 = 每轮的值;agent ~ 任务三列合并放逐轮条形（长度 = wall 按本任务最长轮归一,
// 内部分段 model / tool,两者之和超 wall 时按比例压回——JSONL 族为估算值）。
// 成本字段仅订阅平台（claude-code / codex）有:$ 当量 = 后端逐轮取价,子代理切片按
// 时间窗并入父轮;≈% 由 scale换算,系数缺省只显 $。
// 中止轮（aborted,与错误分列）以错误色描边标记;只读 get_task_turns 的数值列,不含内容列。
import { Fragment } from 'react'
import type { TaskTurn } from '../../services'
import { formatCompact } from '../matrix/matrixScale'
import { formatDuration } from '../insights/analytics'
import { fmt, useT, type Translator } from '../../lib/i18n'
import { clockLabel, usd2 } from './taskFormat'

/** ≈百分点 = $ × 系数（百分点 / $）;一位小数,不足 0.1 显 <0.1;系数缺省 → 空串。 */
function pctOf(usd: number, scale: number | null | undefined): string {
  if (!scale || scale <= 0) return ''
  const p = usd * scale
  return p > 0 && p < 0.1 ? '<0.1' : p.toFixed(1)
}

function turnHint(tn: TaskTurn, t: Translator<'tasks'>, scale: number | null | undefined): string {
  const parts = [
    `${t('turnHead', { seq: tn.turnSeq, time: clockLabel(tn.startedAt) })}${tn.aborted ? ` · ${t('turnAborted')}` : ''}`,
    t('turnModel', { model: tn.model }),
    t('turnWait', { wall: formatDuration(tn.wallMs), model: formatDuration(tn.modelMs), tool: formatDuration(tn.toolMs) }),
    t('stepsTools', { steps: tn.steps, tools: tn.toolCalls }),
  ]
  if (tn.usd !== null) {
    const pct = pctOf(tn.usd, scale)
    parts.push(pct ? t('turnCost', { pct, usd: usd2(tn.usd) }) : t('turnCostUsd', { usd: usd2(tn.usd) }))
    for (const m of tn.usdByModel ?? []) parts.push(t('turnModelCost', { model: m.modelKey, usd: usd2(m.usd) }))
  }
  if (tn.subagentCalls > 0) parts.push(t('turnSubagents', { n: tn.subagentCount, calls: tn.subagentCalls }))
  if (tn.ttftMs !== null) parts.push(t('turnFirstToken', { s: (tn.ttftMs / 1000).toFixed(1) }))
  if (tn.gapMs !== null) parts.push(t('turnGap', { d: formatDuration(tn.gapMs) }))
  parts.push(t('turnTokens', { n: formatCompact(tn.totalTokens) }))
  if (tn.errorCount > 0) parts.push(t('turnErrors', { n: tn.errorCount }))
  if (tn.retryCount > 0) parts.push(t('turnRetries', { n: tn.retryCount }))
  return parts.join('\n')
}

export default function TaskTurns({ turns, scale, colCount }: {
  turns: TaskTurn[] | null | undefined
  scale?: number | null
  /** 外层表格总列数（flag 列 + COLUMNS）——汇总行 colSpan 用。 */
  colCount: number
}) {
  const t = useT('tasks')
  if (turns === undefined)
    return <tr className="turns-head-row"><td colSpan={colCount} className="insight-empty">{t('loadingTurns')}</td></tr>
  if (turns === null)
    return <tr className="turns-head-row"><td colSpan={colCount} className="insight-empty">{t('turnsUnavailable')}</td></tr>
  if (turns.length === 0)
    return <tr className="turns-head-row"><td colSpan={colCount} className="insight-empty">{t('noTurnsTask')}</td></tr>

  const maxWall = Math.max(1, ...turns.map((tn) => tn.wallMs ?? 0))
  const anyTiming = turns.some((tn) => tn.wallMs !== null)
  const aborted = turns.filter((tn) => tn.aborted).length
  // 任务合计成本（Σ;仅订阅平台有 usd 字段）
  const sumUsd = turns.reduce((s, tn) => s + (tn.usd ?? 0), 0)
  const hasCost = turns.some((tn) => tn.usd !== null)
  const sumPct = hasCost ? pctOf(sumUsd, scale) : ''

  return (
    <Fragment>
      {/* 汇总行:轮数 / 最长 / Σ 合计 + 分段图例*/}
      <tr className="turns-head-row">
        <td colSpan={colCount}>
          <div className="turns-head">
            <span>{t('turnsN', { n: turns.length })}{aborted > 0 ? ` · ${t('abortedN', { n: aborted })}` : ''}</span>
            {anyTiming && <span>{t('longest', { d: formatDuration(maxWall) })}</span>}
            {hasCost && (
              <span title={t('turnsSpendHint')}>
                {sumPct ? t('turnsSpend', { pct: sumPct, usd: usd2(sumUsd) }) : t('turnsSpendUsd', { usd: usd2(sumUsd) })}
              </span>
            )}
            <span className="turns-legend">
              <span className="legend-item"><span className="legend-swatch turn-seg-model" />{t('legendModel')}</span>
              <span className="legend-item"><span className="legend-swatch turn-seg-tool" />{t('legendTool')}</span>
              <span className="legend-item"><span className="legend-swatch turn-seg-rest" />{t('legendOther')}</span>
              <span className="legend-item"><span className="legend-swatch turn-swatch-error" />{t('legendAborted')}</span>
            </span>
          </div>
        </td>
      </tr>
      {turns.map((tn) => {
        const wall = tn.wallMs
        let modelPct = 0
        let toolPct = 0
        if (wall !== null && wall > 0) {
          const m = Math.max(0, tn.modelMs ?? 0)
          const tl = Math.max(0, tn.toolMs ?? 0)
          const segScale = m + tl > wall ? wall / (m + tl) : 1
          modelPct = ((m * segScale) / wall) * 100
          toolPct = ((tl * segScale) / wall) * 100
        }
        return (
          <tr key={tn.turnSeq} className={`turn-item-row${tn.aborted ? ' is-aborted' : ''}`} title={turnHint(tn, t, scale)}>
            <td className="task-col-flag" />
            {/* agent ~ 任务三列合并放逐轮条形*/}
            <td colSpan={3} className="turn-bar-cell">
              {wall === null ? (
                <span className="turn-na">{t('noTiming')}</span>
              ) : (
                <div className="turn-bar" style={{ width: `${Math.max(0.6, (wall / maxWall) * 100)}%` }}>
                  <span className="turn-seg-model" style={{ width: `${modelPct}%` }} />
                  <span className="turn-seg-tool" style={{ width: `${toolPct}%` }} />
                </div>
              )}
            </td>
            <td className="turn-seq">#{tn.turnSeq}</td>
            <td>{fmt.number(tn.steps)}</td>
            <td>{fmt.number(tn.toolCalls)}</td>
            <td>{formatDuration(wall)}</td>
            <td className="task-muted">{formatDuration(tn.modelMs)}</td>
            <td className="task-muted">{formatDuration(tn.toolMs)}</td>
            <td className={tn.errorCount > 0 ? 'task-errors' : 'task-muted'}>
              {tn.errorCount > 0 ? fmt.number(tn.errorCount) : '0'}
            </td>
            <td className={tn.subagentCount > 0 ? undefined : 'task-muted'}>
              {tn.subagentCount > 0 ? fmt.number(tn.subagentCount) : '—'}
            </td>
            <td>{formatCompact(tn.totalTokens)}</td>
            <td className={tn.usd !== null ? undefined : 'task-muted'}>{tn.usd !== null ? usd2(tn.usd) : '—'}</td>
          </tr>
        )
      })}
    </Fragment>
  )
}
