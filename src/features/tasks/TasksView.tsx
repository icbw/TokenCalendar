// Tasks 视图:主窗口第三个视图按钮（与 Matrix / Insights 三态互斥）。
// 结构:工具栏（固定,洞察页同款 .insight-toolbar + Seg）/ 滚动区 = Time 卡（Idle gaps ⇄ Time spent）、任务列表卡。
// 两个口径并列:Time 卡按轮所在的本地日（这段时间花在哪）,任务列表按会话开始时间（这段时间开始了哪些任务）。
// 数据:get_task_list（分页 50、表头排序、范围 / Agent / 项目过滤全在 SQL）;行展开 get_task_turns;
// 筛选候选取 get_effort_series（turns) 的系列键:Agent = 当前范围内有轮的;项目 = 全部数据跨度内有轮的
// （选项目即切到其生命周期,候选不能被当前范围截掉）。
// 范围:7d/30d/90d/All/Custom + 选定项目时默认切到项目生命周期,见 insights/useRangeSelection。
// Errors 列 = API / 工具错误（不含用户中止）;中止轮数在 Turns 格 hover 与逐轮条形里表达。
// 离群标记:对当前页 steps/turn 与 wall/turn 做 z-score（与异常日同款阈值 |z| ≥ 2、样本 ≥ 7）,
// 只标偏高一侧（偏低不是异常任务）。
// 项目:列表 / 筛选 / 下拉候选全部是解析层的有效项目键（合并目标 / Scratch;隐藏项目不出现）;
// 下拉末尾「Manage projects…」打开主窗口内的项目管理弹出层,不改变当前筛选。
// 内容列约束:TaskRow.title 只在本文件的列表单元格渲染（标签开关 = title 时）,不进入展开、导出或其它视图。
import { Fragment, useCallback, useEffect, useMemo, useState } from 'react'
import { events, taskService, usageService } from '../../services'
import type { TaskPage, TaskRow, TaskSort, TaskSortField, TaskTurn } from '../../services'
import { formatCompact } from '../matrix/matrixScale'
import { fmt, useT, type MessageKey, type Translator } from '../../lib/i18n'
import { startedLabel } from './taskFormat'
import { OUTLIER_Z, formatDuration, projectDisplayName, projectTooltip, zScores } from '../insights/analytics'
import { Seg } from '../insights/Seg'
import RangeControl from '../insights/RangeControl'
import { useRangeSelection } from '../insights/useRangeSelection'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import TaskTurns from './TaskTurns'
import TimeCard from './TimeCard'
import { openProjectManager } from '../projects/projectManagerStore'
import '../insights/insights.css'
import './tasks.css'

type LabelMode = 'time' | 'title'

const PAGE_SIZE = 50
/** 项目下拉里「Manage projects…」的哨兵值（目录键是归一化路径 / unknown / __scratch,不会撞上）。 */
const MANAGE_PROJECTS = '__manage_projects__'
interface Column {
  id: string
  label: MessageKey<'tasks'>
  hint: MessageKey<'tasks'>
  sort?: TaskSortField
  left?: boolean
}

// label / hint 存字典键,渲染时再取文案（语言切换即更新）
const COLUMNS: Column[] = [
  { id: 'agent', label: 'colAgent', hint: 'colAgentHint', left: true },
  { id: 'project', label: 'colProject', hint: 'colProjectHint', left: true },
  { id: 'started', label: 'colStarted', hint: 'colStartedHint', sort: 'started_at', left: true },
  { id: 'turns', label: 'colTurns', hint: 'colTurnsHint', sort: 'turns' },
  { id: 'steps', label: 'colSteps', hint: 'colStepsHint', sort: 'steps' },
  { id: 'tools', label: 'colTools', hint: 'colToolsHint', sort: 'tool_calls' },
  { id: 'wait', label: 'colWait', hint: 'colWaitHint', sort: 'wall_ms' },
  { id: 'model', label: 'colModel', hint: 'colModelHint', sort: 'model_ms' },
  { id: 'tool', label: 'colTool', hint: 'colToolHint', sort: 'tool_ms' },
  { id: 'errors', label: 'colErrors', hint: 'colErrorsHint', sort: 'error_count' },
  { id: 'subagents', label: 'colSubagents', hint: 'colSubagentsHint', sort: 'subagent_count' },
  { id: 'tokens', label: 'colTokens', hint: 'colTokensHint', sort: 'total_tokens' },
]

const taskKey = (r: { agent: string; sessionId: string }) => `${r.agent}${r.sessionId}`

function subagentCell(r: TaskRow): string {
  if (r.subagentCount === 0 && r.subagentCalls === 0) return '—'
  const pct = r.steps > 0 ? (r.subagentCalls / r.steps) * 100 : 0
  const pctText = pct > 0 && pct < 1 ? '<1' : String(Math.round(pct))
  return `${r.subagentCount} · ${pctText}%`
}

interface Outlier {
  steps?: { value: number; z: number }
  wall?: { value: number; z: number }
}

function outliersOf(rows: TaskRow[]): Map<string, Outlier> {
  const out = new Map<string, Outlier>()
  const withTurns = rows.filter((r) => r.turns > 0)
  const stepsZ = zScores(withTurns.map((r) => r.steps / r.turns))
  withTurns.forEach((r, i) => {
    const z = stepsZ?.[i]
    if (z !== undefined && z >= OUTLIER_Z) out.set(taskKey(r), { steps: { value: r.steps / r.turns, z } })
  })
  const timed = withTurns.filter((r) => r.wallMs !== null)
  const wallZ = zScores(timed.map((r) => (r.wallMs ?? 0) / r.turns))
  timed.forEach((r, i) => {
    const z = wallZ?.[i]
    if (z !== undefined && z >= OUTLIER_Z) {
      const k = taskKey(r)
      out.set(k, { ...out.get(k), wall: { value: (r.wallMs ?? 0) / r.turns, z } })
    }
  })
  return out
}

function outlierHint(o: Outlier, t: Translator<'tasks'>): string {
  const lines = [t('outlierTitle')]
  if (o.steps) lines.push(t('outlierSteps', { v: o.steps.value.toFixed(1), z: o.steps.z.toFixed(1) }))
  if (o.wall) lines.push(t('outlierWait', { v: formatDuration(o.wall.value), z: o.wall.z.toFixed(1) }))
  return lines.join('\n')
}

export default function TasksView() {
  const t = useT('tasks')
  const [agent, setAgent] = useState('')
  const [project, setProject] = useState('')
  const [sort, setSort] = useState<TaskSort>({ field: 'started_at', direction: 'desc' })
  const [page, setPage] = useState(0)
  const [labelMode, setLabelMode] = useState<LabelMode>(() => getDesignPrefs().taskLabelMode ?? 'time')
  useEffect(() => subscribeDesignPrefs((p) => setLabelMode(p.taskLabelMode ?? 'time')), [])

  const [data, setData] = useState<TaskPage | null | undefined>(undefined)
  const [loading, setLoading] = useState(false)
  const [options, setOptions] = useState<{ agents: { key: string; label: string }[]; projects: string[] }>({ agents: [], projects: [] })
  const [expanded, setExpanded] = useState<string | null>(null)
  const [turns, setTurns] = useState<{ key: string; rows: TaskTurn[] | null } | null>(null)
  const [refreshTick, setRefreshTick] = useState(0)

  // usage:changed（采集轮 / 改阈值重算）→ 300ms 去抖:列表、筛选候选、直方图、展开明细一并重取
  useEffect(() => {
    let timer = 0
    let off: (() => void) | null = null
    void events.onUsageChanged(() => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => setRefreshTick((t) => t + 1), 300)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      window.clearTimeout(timer)
      off?.()
    }
  }, [])

  const selection = useRangeSelection(project, refreshTick)
  const { range, dataSpan } = selection

  // 范围变化（手改 / 项目生命周期自动切换 / 跨日）→ 回第 1 页并收起展开
  useEffect(() => {
    setPage(0)
    setExpanded(null)
  }, [range.startDay, range.endDay])

  // 筛选候选:Agent = 范围内有轮的;项目 = 全部数据跨度内有轮的（按轮数降序）
  useEffect(() => {
    let cancelled = false
    const all = dataSpan ? { startDay: dataSpan.firstDay, endDay: range.endDay } : range
    void Promise.all([
      usageService.getEffortSeries({ ...range, dimension: 'agent', metric: 'turns' }),
      usageService.getEffortSeries({ ...all, dimension: 'project', metric: 'turns' }),
    ]).then(([a, p]) => {
      if (cancelled) return
      setOptions({
        agents: (a?.seriesKeys ?? []).map((k, i) => ({ key: k, label: a?.seriesLabels[i] ?? k })),
        projects: p?.seriesKeys ?? [],
      })
    })
    return () => {
      cancelled = true
    }
  }, [range, dataSpan])

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    void taskService
      .getTaskList(range, { agent, project }, sort, { offset: page * PAGE_SIZE, limit: PAGE_SIZE })
      .then((res) => {
        if (cancelled) return
        setData(res)
        setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [range, agent, project, sort, page])

  useEffect(() => {
    if (expanded === null) return
    const row = data?.rows.find((r) => taskKey(r) === expanded)
    if (!row) return
    let cancelled = false
    setTurns((prev) => (prev?.key === expanded ? prev : null))
    void taskService.getTaskTurns(row.agent, row.sessionId).then((res) => {
      if (!cancelled) setTurns({ key: expanded, rows: res })
    })
    return () => {
      cancelled = true
    }
    // data 引用在每次列表重取后变化——展开明细随 usage:changed 一起刷新
  }, [expanded, data])

  // 刷新后总数变少、当前页越界 → 退到最后一页
  useEffect(() => {
    if (data && page > 0 && page * PAGE_SIZE >= data.total) setPage(Math.max(0, Math.ceil(data.total / PAGE_SIZE) - 1))
  }, [data, page])

  const agentLabel = useMemo(() => new Map(options.agents.map((a) => [a.key, a.label])), [options.agents])
  const labelOfAgent = useCallback((k: string) => agentLabel.get(k) ?? k, [agentLabel])

  // Time spent 的 Task 维点击:当前页有该任务 → 展开并滚到该行;没有 → 返回 false 由卡片提示（不改列表筛选）
  const pickTask = useCallback(
    (a: string, sid: string): boolean => {
      const k = taskKey({ agent: a, sessionId: sid })
      if (!data?.rows.some((r) => taskKey(r) === k)) return false
      setExpanded(k)
      window.requestAnimationFrame(() => {
        const el = Array.from(document.querySelectorAll<HTMLElement>('.task-row')).find((n) => n.dataset.taskKey === k)
        el?.scrollIntoView({ behavior: 'smooth', block: 'center' })
      })
      return true
    },
    [data],
  )
  const outliers = useMemo(() => outliersOf(data?.rows ?? []), [data])

  const resetPaging = () => {
    setPage(0)
    setExpanded(null)
  }
  const pickSort = (field: TaskSortField) => {
    setSort((prev) => (prev.field === field ? { field, direction: prev.direction === 'desc' ? 'asc' : 'desc' } : { field, direction: 'desc' }))
    resetPaging()
  }
  const pickLabelMode = (m: LabelMode) => {
    setLabelMode(m)
    setDesignPrefs({ taskLabelMode: m })
  }

  // 当前选中值不在候选里（范围切换后）仍保留为可见选项,避免 select 显示空白
  const agentOptions = agent && !agentLabel.has(agent) ? [...options.agents, { key: agent, label: agent }] : options.agents
  const projectOptions = project && !options.projects.includes(project) ? [...options.projects, project] : options.projects

  const total = data?.total ?? 0
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE))
  const from = total === 0 ? 0 : page * PAGE_SIZE + 1
  const to = Math.min(total, (page + 1) * PAGE_SIZE)

  return (
    <div className="insights-view tasks-view">
      <header className="insight-toolbar">
        <span className="insight-card-title">{t('title')}</span>
        <select
          className="matrix-sort"
          value={agent}
          title={t('filterAgentTitle')}
          aria-label={t('filterAgentAria')}
          onChange={(e) => {
            setAgent(e.target.value)
            resetPaging()
          }}
        >
          <option value="">{t('allAgents')}</option>
          {agentOptions.map((a) => (
            <option key={a.key} value={a.key}>{a.label}</option>
          ))}
        </select>
        <select
          className="matrix-sort tasks-project-select"
          value={project}
          title={project ? projectTooltip(project) : t('filterProjectTitle')}
          aria-label={t('filterProjectAria')}
          onChange={(e) => {
            if (e.target.value === MANAGE_PROJECTS) {
              openProjectManager()
              return
            }
            setProject(e.target.value)
            resetPaging()
          }}
        >
          <option value="">{t('allProjects')}</option>
          {projectOptions.map((k) => (
            <option key={k} value={k}>{projectDisplayName(k)}</option>
          ))}
          <option disabled>──────────</option>
          <option value={MANAGE_PROJECTS}>{t('manageProjects')}</option>
        </select>
        <Seg
          value={labelMode}
          options={[
            { v: 'time' as LabelMode, label: t('labelTime'), hint: t('labelTimeHint') },
            { v: 'title' as LabelMode, label: t('labelTitle'), hint: t('labelTitleHint') },
          ]}
          onChange={pickLabelMode}
        />
        {loading && <span className="matrix-loading">{t('loading')}</span>}
      </header>
      <div className="insight-rangebar">
        <RangeControl
          selection={selection}
          noun={t('rangeNoun')}
          note={t('rangeNote')}
        />
      </div>

      <div className="insights-scroll">
        <TimeCard
          range={range}
          agent={agent}
          project={project}
          labelMode={labelMode}
          agentLabel={labelOfAgent}
          refreshTick={refreshTick}
          onPickTask={pickTask}
        />

        <section className="insight-card">
          <header className="insight-card-header">
            <span className="insight-card-title">{t('taskList')}</span>
            <span className="insight-card-sub">
              {data ? t('taskListSub', { n: fmt.number(total) }) : ''}
            </span>
          </header>
          {data === undefined ? (
            <div className="insight-empty">{t('loading')}</div>
          ) : data === null ? (
            <div className="insight-empty">{t('unavailable')}</div>
          ) : data.rows.length === 0 ? (
            <div className="insight-empty">{t('noTasks')}</div>
          ) : (
            <>
              <div className="task-table-wrap">
                <table className="task-table">
                  <thead>
                    <tr>
                      <th className="task-col-flag" aria-label={t('expand')} />
                      {COLUMNS.map((c) => {
                        const active = c.sort !== undefined && sort.field === c.sort
                        const label = t(c.id === 'started' && labelMode === 'title' ? 'colTask' : c.label)
                        const hint = t(c.hint)
                        return (
                          <th key={c.id} className={c.left ? 'is-left' : undefined} aria-sort={active ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined}>
                            {c.sort ? (
                              <button className={`task-th${active ? ' is-active' : ''}`} title={t('sortHint', { hint })} onClick={() => pickSort(c.sort!)}>
                                {label}
                                <span className="task-th-arrow">{active ? (sort.direction === 'asc' ? '▲' : '▼') : ''}</span>
                              </button>
                            ) : (
                              <span className="task-th is-static" title={hint}>{label}</span>
                            )}
                          </th>
                        )
                      })}
                    </tr>
                  </thead>
                  <tbody>
                    {data.rows.map((r) => {
                      const k = taskKey(r)
                      const isOpen = expanded === k
                      const flag = outliers.get(k)
                      const started = startedLabel(r.startedAt)
                      // 内容列唯一渲染点:title 模式且非空才显示,否则回退开始时间
                      const title = labelMode === 'title' ? r.title?.trim() || '' : ''
                      return (
                        <Fragment key={k}>
                          <tr
                            className={`task-row${isOpen ? ' is-expanded' : ''}${flag ? ' is-outlier' : ''}`}
                            data-task-key={k}
                            onClick={() => setExpanded(isOpen ? null : k)}
                            aria-expanded={isOpen}
                          >
                            <td className="task-col-flag">
                              <span className={`task-chevron${isOpen ? ' is-open' : ''}`} aria-hidden="true">›</span>
                              {flag && <span className="task-flag" title={outlierHint(flag, t)}>!</span>}
                            </td>
                            <td className="is-left">{agentLabel.get(r.agent) ?? r.agent}</td>
                            <td
                              className="is-left task-ellipsis"
                              title={r.project === r.projectRaw ? projectTooltip(r.project) : `${r.projectLabel}\n${projectTooltip(r.projectRaw)}`}
                            >
                              {r.projectLabel || projectDisplayName(r.project)}
                            </td>
                            <td className="is-left task-ellipsis task-label" title={title ? `${title}\n${started}` : started}>
                              {title || started}
                            </td>
                            <td title={r.abortedCount > 0 ? t('abortedN', { n: fmt.number(r.abortedCount) }) : undefined}>{fmt.number(r.turns)}</td>
                            <td className={flag?.steps ? 'is-outlier-value' : undefined}>{fmt.number(r.steps)}</td>
                            <td>{fmt.number(r.toolCalls)}</td>
                            <td className={flag?.wall ? 'is-outlier-value' : undefined}>{formatDuration(r.wallMs)}</td>
                            <td className="task-muted">{formatDuration(r.modelMs)}</td>
                            <td className="task-muted">{formatDuration(r.toolMs)}</td>
                            <td className={r.errorCount > 0 ? 'task-errors' : 'task-muted'}>{r.errorCount > 0 ? fmt.number(r.errorCount) : '0'}</td>
                            <td className={r.subagentCount > 0 ? undefined : 'task-muted'}>{subagentCell(r)}</td>
                            <td>{formatCompact(r.totalTokens)}</td>
                          </tr>
                          {isOpen && (
                            <tr className="task-detail">
                              <td colSpan={COLUMNS.length + 1}>
                                <TaskTurns turns={turns?.key === k ? turns.rows : undefined} />
                              </td>
                            </tr>
                          )}
                        </Fragment>
                      )
                    })}
                  </tbody>
                </table>
              </div>
              <footer className="task-pager">
                <span>
                  {t('pagerRange', { from: fmt.number(from), to: fmt.number(to), total: fmt.number(total) })}
                </span>
                <div className="toolbar-group">
                  <button
                    className={`seg${page === 0 ? ' is-disabled' : ''}`}
                    aria-disabled={page === 0 || undefined}
                    title={t('prevPageTitle')}
                    onClick={() => {
                      if (page > 0) {
                        setPage(page - 1)
                        setExpanded(null)
                      }
                    }}
                  >
                    {t('prevPage')}
                  </button>
                  <span className="task-pager-page">
                    {page + 1} / {pageCount}
                  </span>
                  <button
                    className={`seg${page + 1 >= pageCount ? ' is-disabled' : ''}`}
                    aria-disabled={page + 1 >= pageCount || undefined}
                    title={t('nextPageTitle')}
                    onClick={() => {
                      if (page + 1 < pageCount) {
                        setPage(page + 1)
                        setExpanded(null)
                      }
                    }}
                  >
                    {t('nextPage')}
                  </button>
                </div>
              </footer>
            </>
          )}
        </section>
      </div>
    </div>
  )
}
