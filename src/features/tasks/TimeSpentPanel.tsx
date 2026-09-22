// Time 卡的「Time spent」模式:按轮的本地日归日的时间统计（get_time_spent）。
// 口径（TASK_TIME_SPENT_DESIGN）:Task = Σ turn.wall_ms（= 任务列表 Wait 列）,Human = Σ gap_ms ≤ 离开阈值,
// Total = 两者堆叠;项目归属与筛选按逐轮的有效项目键。任务列表仍按会话开始时间——两个口径并列,不强行统一。
// 维度:Project / Task = 横向堆叠条（Top 15 + Others,按当前指标降序）;Day = 竖向逐日堆叠柱（StackedBarChart）。
// 选定单个项目时,维度自动切到 Task（每次换项目只切一次;用户在该项目下手选过维度就不再抢）。
// 内容列:Task 维标签在 Title 模式下取 session.title,只在本卡本地渲染,不进导出。
import { useEffect, useMemo, useState } from 'react'
import { taskService } from '../../services'
import type { DayRange, TimeSpent, TimeSpentGroup, TimeSpentRow } from '../../services'
import { formatFull } from '../matrix/matrixScale'
import { formatDuration, projectDisplayName, projectTooltip } from '../insights/analytics'
import { StackedBarChart, type SeriesSpec } from '../insights/charts'
import { projectColor } from '../insights/projectColors'
import { Seg } from '../insights/Seg'
import { getDesignPrefs, setDesignPrefs } from '../settings/designPrefs'

export type TimeMetric = 'total' | 'task' | 'human'

/** 图内三量的名称与说明（任务列表的 Wait 列与 Task 为同一个量）。 */
export const TIME_METRIC_LABELS: Record<TimeMetric, { label: string; hint: string }> = {
  total: { label: 'Total', hint: 'Task + Human, stacked' },
  task: { label: 'Task', hint: 'Task time: sum of turn wall-clock durations (the Wait column of the task list)' },
  human: { label: 'Human', hint: 'Human time: gaps between turns up to the idle threshold' },
}

const GROUP_OPTIONS: { v: TimeSpentGroup; label: string; hint: string }[] = [
  { v: 'project', label: 'Project', hint: 'Time per project (each turn counts toward its own project)' },
  { v: 'task', label: 'Task', hint: 'Time per task, counting only the turns inside the range' },
  { v: 'day', label: 'Day', hint: 'Time per local day of the turn' },
]

const TOP_N = 15
const TASK_COLOR = 'var(--time-task)'
const HUMAN_COLOR = 'var(--time-human)'
const MONTH_ABBR = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']
const pad2 = (n: number) => String(n).padStart(2, '0')

function startedLabel(ms: number): string {
  const d = new Date(ms)
  const year = d.getFullYear() !== new Date().getFullYear() ? `, ${d.getFullYear()}` : ''
  return `${MONTH_ABBR[d.getMonth()]} ${d.getDate()}${year} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`
}

/** 本地 0 点毫秒（范围起点,判定「started before range」）。 */
function localMidnight(day: string): number {
  const [y, m, d] = day.split('-').map(Number)
  return new Date(y, m - 1, d).getTime()
}

interface BarItem {
  key: string
  label: string
  swatch?: string
  task: number
  human: number
  turns: number
  tooltip: string
  badge?: string
  row?: TimeSpentRow
  isOthers?: boolean
}

const valueOf = (it: { task: number; human: number }, metric: TimeMetric) =>
  metric === 'task' ? it.task : metric === 'human' ? it.human : it.task + it.human

function triple(task: number, human: number): string {
  return `Task ${formatDuration(task)} · Human ${formatDuration(human)} · Total ${formatDuration(task + human)}`
}

function HBars({ items, metric, onPick }: { items: BarItem[]; metric: TimeMetric; onPick?: (it: BarItem) => void }) {
  const max = Math.max(1, ...items.map((it) => valueOf(it, metric)))
  return (
    <div className="tspent-bars" role="list">
      {items.map((it) => {
        const v = valueOf(it, metric)
        const tw = metric === 'human' ? 0 : (it.task / max) * 100
        const hw = metric === 'task' ? 0 : (it.human / max) * 100
        const clickable = Boolean(onPick && it.row)
        return (
          <div
            key={it.key}
            role="listitem"
            className={`tspent-row${clickable ? ' is-clickable' : ''}${it.isOthers ? ' is-others' : ''}`}
            title={it.tooltip}
            data-key={it.key}
            onClick={clickable ? () => onPick?.(it) : undefined}
          >
            <span className="tspent-label">
              {it.swatch && <span className="legend-swatch" style={{ background: it.swatch }} />}
              <span className="tspent-name">{it.label}</span>
              {it.badge && <span className="tspent-badge">{it.badge}</span>}
            </span>
            <span className="tspent-track">
              {tw > 0 && <span className="tspent-seg is-task" style={{ width: `${tw}%` }} />}
              {hw > 0 && <span className="tspent-seg is-human" style={{ width: `${hw}%` }} />}
            </span>
            <span className="tspent-value">{formatDuration(v)}</span>
          </div>
        )
      })}
    </div>
  )
}

export default function TimeSpentPanel({ range, agent, project, labelMode, agentLabel, refreshTick, onPickTask }: {
  range: DayRange
  agent: string
  project: string
  labelMode: 'time' | 'title'
  agentLabel: (key: string) => string
  refreshTick: number
  /** Task 维点击:列表当前页有该任务 → 展开并滚到它,返回 true;否则返回 false（只提示,不改列表筛选）。 */
  onPickTask: (agent: string, sessionId: string) => boolean
}) {
  const [userGroup, setUserGroup] = useState<TimeSpentGroup>(() => getDesignPrefs().taskTimeGroup ?? 'project')
  const [metric, setMetric] = useState<TimeMetric>(() => getDesignPrefs().taskTimeMetric ?? 'total')
  // 用户在哪个项目下手选过维度（换项目即失效）:自动切 Task 只在没手选过的那次生效
  const [pickedFor, setPickedFor] = useState<string | null>(null)
  const group: TimeSpentGroup = project && pickedFor !== project && userGroup === 'project' ? 'task' : userGroup

  const [data, setData] = useState<TimeSpent | null | undefined>(undefined)
  const [notice, setNotice] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    void taskService.getTimeSpent(range, group, { agent, project }, TOP_N).then((res) => {
      if (!cancelled) setData(res)
    })
    return () => {
      cancelled = true
    }
  }, [range, group, agent, project, refreshTick])

  useEffect(() => setNotice(null), [range, group, agent, project])

  const pickGroup = (g: TimeSpentGroup) => {
    setUserGroup(g)
    setPickedFor(project || null)
    setDesignPrefs({ taskTimeGroup: g })
  }
  const pickMetric = (m: TimeMetric) => {
    setMetric(m)
    setDesignPrefs({ taskTimeMetric: m })
  }

  const rangeStartMs = useMemo(() => localMidnight(range.startDay), [range.startDay])

  const items = useMemo<BarItem[]>(() => {
    if (!data || group === 'day') return []
    const list: BarItem[] = data.rows.map((r) => {
      if (group === 'project') {
        return {
          key: r.key,
          label: r.label || projectDisplayName(r.key),
          swatch: projectColor(r.key),
          task: r.taskMs,
          human: r.humanMs,
          turns: r.turns,
          tooltip: `${projectTooltip(r.key)}\n${triple(r.taskMs, r.humanMs)}\n${formatFull(r.turns)} turns`,
        }
      }
      const started = r.startedAt !== undefined ? startedLabel(r.startedAt) : ''
      const title = labelMode === 'title' ? r.title?.trim() || '' : ''
      const before = r.startedAt !== undefined && r.startedAt < rangeStartMs
      const lifeTask = r.lifetimeTaskMs ?? r.taskMs
      const lifeHuman = r.lifetimeHumanMs ?? r.humanMs
      const lines = [
        title ? `${title}\n${started}` : started,
        `${agentLabel(r.agent ?? '')}${r.project ? ` · ${projectDisplayName(r.project)}` : ''}`,
        `In range: ${triple(r.taskMs, r.humanMs)} · ${formatFull(r.turns)} turns`,
        `Whole task: ${triple(lifeTask, lifeHuman)}`,
      ]
      if (before) lines.push('Started before the range: only its turns inside the range are counted')
      lines.push('Click to open it in the task list')
      return {
        key: r.key,
        label: title || started,
        task: r.taskMs,
        human: r.humanMs,
        turns: r.turns,
        tooltip: lines.join('\n'),
        badge: before ? 'started before range' : undefined,
        row: r,
      }
    })
    if (data.others) {
      const o = data.others
      list.push({
        key: '__others__',
        label: `Others (${formatFull(o.count)})`,
        swatch: group === 'project' ? 'var(--border-strong)' : undefined,
        task: o.taskMs,
        human: o.humanMs,
        turns: o.turns,
        tooltip: `${formatFull(o.count)} more ${group === 'project' ? 'projects' : 'tasks'}\n${triple(o.taskMs, o.humanMs)}\n${formatFull(o.turns)} turns`,
        isOthers: true,
      })
    }
    // 按当前指标降序（Others 固定末尾）
    const head = list.filter((it) => !it.isOthers).sort((a, b) => valueOf(b, metric) - valueOf(a, metric))
    return [...head, ...list.filter((it) => it.isOthers)]
  }, [data, group, labelMode, metric, rangeStartMs, agentLabel])

  const daySeries = useMemo<SeriesSpec[]>(() => {
    if (!data || group !== 'day') return []
    const task: SeriesSpec = { key: 'task', label: 'Task', values: data.rows.map((r) => r.taskMs), color: TASK_COLOR }
    const human: SeriesSpec = { key: 'human', label: 'Human', values: data.rows.map((r) => r.humanMs), color: HUMAN_COLOR }
    return metric === 'task' ? [task] : metric === 'human' ? [human] : [task, human]
  }, [data, group, metric])

  const pick = (it: BarItem) => {
    const r = it.row
    if (!r?.agent || !r.sessionId) return
    setNotice(onPickTask(r.agent, r.sessionId) ? null : 'This task is not on the current page of the task list (the list shows tasks started in the range)')
  }

  return (
    <>
      <div className="gap-controls tspent-controls">
        <Seg<TimeSpentGroup>
          value={group}
          options={GROUP_OPTIONS.map((o) => ({
            ...o,
            hint: o.v === 'task' && group === 'task' && project && userGroup === 'project' ? 'Switched to tasks because a single project is selected' : o.hint,
          }))}
          onChange={pickGroup}
        />
        <Seg<TimeMetric>
          value={metric}
          options={(['total', 'task', 'human'] as TimeMetric[]).map((m) => ({ v: m, label: TIME_METRIC_LABELS[m].label, hint: TIME_METRIC_LABELS[m].hint }))}
          onChange={pickMetric}
        />
        <span className="tspent-legend">
          {metric !== 'human' && (
            <span className="gap-stat">
              <span className="legend-swatch" style={{ background: TASK_COLOR }} />
              <span className="gap-stat-label">Task</span>
            </span>
          )}
          {metric !== 'task' && (
            <span className="gap-stat">
              <span className="legend-swatch" style={{ background: HUMAN_COLOR }} />
              <span className="gap-stat-label">Human</span>
            </span>
          )}
        </span>
      </div>

      {data === undefined ? (
        <div className="insight-empty">Loading…</div>
      ) : data === null ? (
        <div className="insight-empty">Data unavailable (service not running)</div>
      ) : data.total.turns === 0 ? (
        <div className="insight-empty">No turns in this range</div>
      ) : (
        <>
          {group === 'day' ? (
            <StackedBarChart series={daySeries} buckets={data.rows.map((r) => r.key)} formatValue={formatDuration} ordered />
          ) : (
            <HBars items={items} metric={metric} onPick={group === 'task' ? pick : undefined} />
          )}
          {notice && <div className="gap-status tspent-notice">{notice}</div>}
          <div className="gap-stats">
            <div className="gap-stat">
              <span className="gap-stat-label">Task</span>
              <span className="gap-stat-value">{formatDuration(data.total.taskMs)}</span>
            </div>
            <div className="gap-stat">
              <span className="gap-stat-label">Human</span>
              <span className="gap-stat-value">{formatDuration(data.total.humanMs)}</span>
            </div>
            <div className="gap-stat">
              <span className="gap-stat-label">Total</span>
              <span className="gap-stat-value">
                {formatDuration(data.total.taskMs + data.total.humanMs)} · {formatFull(data.total.turns)} turns
              </span>
            </div>
          </div>
        </>
      )}
    </>
  )
}
