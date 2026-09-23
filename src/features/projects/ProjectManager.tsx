// 项目管理面板:Settings·Projects tab 与主窗口内弹出层（ProjectManagerModal）共用。
// 结构:规则区（自动折叠开关 + 两个阈值 + Unknown 归 Scratch + Scratch 显隐）/ 列表区（筛选 · 排序 · 批量条 · 表格）。
// 时间轴监测:名称列左侧 pin 图标切换;置顶行在任何排序下都额外提前（按置顶先后）;筛选条 All 与 Active 之间的
// pin 按钮 = 只看置顶。
// 数据:list_project_meta（每目录键一行,状态按当前规则解析）;写操作经 projectService,成功后后端发 usage:changed,
// 本面板与各分析视图经既有监听重取。active = false 时不取数（弹出层常驻 DOM,关闭时只停取数）。
// 行存在性语义见 Rust project_meta.rs:Rename / Hide / Unhide / Keep 都让键脱离自动规则;Reset 回到自动态。
// 状态徽章:Active / Scratch 徽章可点击切换——Active → Scratch = 合并进 Scratch 伪项目（手动归入）;
// Scratch → Active = 手动归入的先取消合并,再 Keep（留 meta 行,规则不再作用）。Hidden / Merged 徽章不可点。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { events, projectService } from '../../services'
import type { ProjectMetaList, ProjectMetaRow, ProjectStatus, ScratchRuleInfo, WriteResult } from '../../services'
import { formatCompact, formatFull } from '../matrix/matrixScale'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import { localizeProjectLabel } from '../../services/projectLabels'
import { fmt, getT, useT, type MessageKey } from '../../lib/i18n'
import './projects.css'

type StatusFilter = 'all' | 'pinned' | ProjectStatus
type SortKey = 'recent' | 'sessions'

type ProjectsKey = MessageKey<'projects'>

/** 筛选段:label / hint 存字典键,渲染时取文（模块顶层不拼文案）。pinned 段显示图标,无 label。 */
const STATUS_FILTERS: { v: StatusFilter; label: ProjectsKey | null; hint: ProjectsKey }[] = [
  { v: 'all', label: 'filterAll', hint: 'hintAll' },
  { v: 'pinned', label: null, hint: 'hintPinned' },
  { v: 'active', label: 'filterActive', hint: 'hintActive' },
  { v: 'hidden', label: 'filterHidden', hint: 'hintHidden' },
  { v: 'merged', label: 'filterMerged', hint: 'hintMerged' },
  { v: 'scratch', label: 'scratch', hint: 'hintScratch' },
]

/** 短日期（显示用,按当前语言）:`Sep 5` / `9月5日`;跨年补年份。 */
function dayLabel(day: string | null): string {
  if (!day) return '—'
  const [y, m, d] = day.split('-').map(Number)
  const withYear = y !== new Date().getFullYear()
  return fmt.date(new Date(y, m - 1, d), withYear ? { year: 'numeric', month: 'short', day: 'numeric' } : { month: 'short', day: 'numeric' })
}

function statusText(r: ProjectMetaRow): string {
  const t = getT('projects')
  switch (r.status) {
    case 'hidden':
      return t('filterHidden')
    case 'merged':
      return t('statusMerged', { target: r.mergedLabel ? localizeProjectLabel(r.mergedInto ?? '', r.mergedLabel) : r.mergedInto ?? '' })
    case 'scratch':
      return t('scratch')
    default:
      return t('filterActive')
  }
}

/** 合并目标下拉项的状态后缀（非 active 才显示）。 */
function optionStatus(status: ProjectStatus): string {
  const t = getT('projects')
  const word = status === 'hidden' ? t('optStatusHidden') : status === 'merged' ? t('optStatusMerged') : status === 'scratch' ? t('optStatusScratch') : status
  return t('optStatusWrap', { status: word })
}

function statusHint(r: ProjectMetaRow, scratchHidden: boolean): string {
  const t = getT('projects')
  const lines: string[] = []
  if (r.status === 'merged' && r.mergedInto) lines.push(t('hintCountedUnder', { key: r.mergedInto }))
  if (r.status === 'active') lines.push(t('hintClickToScratch'))
  if (r.status === 'scratch') lines.push(t('hintClickToKeep'))
  if (r.status === 'scratch') {
    const how = r.mergedInto === projectService.SCRATCH_KEY ? t('hintMovedByHand') : t('hintCollapsedByRule')
    lines.push(scratchHidden ? t('hintScratchHidden', { how }) : how)
  }
  if (r.status !== 'hidden' && r.effectiveKey === null && r.status !== 'scratch') lines.push(t('hintNotVisible'))
  if (r.managed && r.status === 'active') lines.push(t('hintManaged'))
  return lines.join('\n')
}

export default function ProjectManager({ active }: { active: boolean }) {
  const t = useT('projects')
  const [list, setList] = useState<ProjectMetaList | null | undefined>(undefined)
  const [ruleInfo, setRuleInfo] = useState<ScratchRuleInfo | null>(null)
  const [refreshTick, setRefreshTick] = useState(0)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const [filter, setFilter] = useState<StatusFilter>('all')
  const [sort, setSort] = useState<SortKey>('recent')
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [renaming, setRenamingState] = useState<{ key: string; value: string } | null>(null)
  // 编辑态的同步镜像:Enter 保存后输入框卸载可能再触发一次 blur（旧闭包仍持有编辑态）,以 ref 判重防止同一改名写两次
  const renamingRef = useRef(renaming)
  const setRenaming = (next: { key: string; value: string } | null) => {
    renamingRef.current = next
    setRenamingState(next)
  }
  /** 合并选择器:sources = 待合并的键;target = 选中的目标键。 */
  const [merging, setMerging] = useState<{ sources: string[]; target: string } | null>(null)

  // 规则阈值输入草稿（失焦 / 回车才提交;非法值回退）
  const [sessionsDraft, setSessionsDraft] = useState('')
  const [turnsDraft, setTurnsDraft] = useState('')

  // 时间轴置顶（prefs timelinePinnedKeys,存原始目录键,置顶先后即时间轴列序;storage 桥跨窗口同步——这里改,
  // timeline 窗口即时跟随）。置顶集 = 时间轴监测的项目集,只在这里管理;
  // 为空时时间轴回退为按窗口容量显示最近项目。
  const [pins, setPins] = useState<string[]>(() => getDesignPrefs().timelinePinnedKeys ?? [])
  useEffect(() => subscribeDesignPrefs((p) => setPins(p.timelinePinnedKeys ?? [])), [])
  const togglePin = (key: string) => {
    const cur = getDesignPrefs().timelinePinnedKeys ?? []
    setDesignPrefs({ timelinePinnedKeys: cur.includes(key) ? cur.filter((k) => k !== key) : [...cur, key] })
  }

  useEffect(() => {
    if (!active) return
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
  }, [active])

  useEffect(() => {
    if (!active) return
    let cancelled = false
    void Promise.all([projectService.listProjectMeta(), projectService.getScratchRule()]).then(([l, r]) => {
      if (cancelled) return
      setList(l)
      setRuleInfo(r)
      if (r) {
        setSessionsDraft(String(r.rule.minSessions))
        setTurnsDraft(String(r.rule.minTurns))
      }
      // 刷新后已不存在的选中键剔除
      if (l) setSelected((prev) => new Set([...prev].filter((k) => l.rows.some((row) => row.key === k))))
    })
    return () => {
      cancelled = true
    }
  }, [active, refreshTick])

  // 关闭弹出层时收起编辑态
  useEffect(() => {
    if (active) return
    renamingRef.current = null
    setRenamingState(null)
    setMerging(null)
    setError(null)
  }, [active])

  const run = useCallback(async <T,>(op: () => Promise<WriteResult<T>>): Promise<boolean> => {
    setBusy(true)
    setError(null)
    const res = await op()
    setBusy(false)
    if (!res.ok) {
      setError(res.error)
      return false
    }
    // usage:changed 会触发重取;这里立即再取一次,避免去抖窗口内的旧状态闪回
    setRefreshTick((t) => t + 1)
    return true
  }, [])

  // 内置伪项目的后端默认名（Unknown project）按当前语言显示;别名 / 目录名原样
  const rows = useMemo(() => (list?.rows ?? []).map((r) => ({ ...r, label: localizeProjectLabel(r.key, r.label, t) })), [list, t])
  const byKey = useMemo(() => new Map(rows.map((r) => [r.key, r])), [rows])
  const counts = useMemo(() => {
    const c: Record<StatusFilter, number> = { all: rows.length, pinned: 0, active: 0, hidden: 0, merged: 0, scratch: 0 }
    for (const r of rows) {
      c[r.status] += 1
      if (pins.includes(r.key)) c.pinned += 1
    }
    return c
  }, [rows, pins])
  /** 被他人合并进来的键（不能再作合并源）。 */
  const targets = useMemo(() => new Set(rows.map((r) => r.mergedInto).filter((k): k is string => !!k)), [rows])

  const visible = useMemo(() => {
    const q = query.trim().toLowerCase()
    const out = rows.filter(
      (r) =>
        (filter === 'all' || (filter === 'pinned' ? pins.includes(r.key) : r.status === filter)) &&
        (!q || r.key.toLowerCase().includes(q) || r.label.toLowerCase().includes(q) || (r.alias ?? '').toLowerCase().includes(q)),
    )
    if (sort === 'sessions') {
      out.sort((a, b) => b.sessions - a.sessions || b.turns - a.turns || fmt.compare(a.label, b.label))
    } else {
      out.sort((a, b) => (b.lastDay ?? '').localeCompare(a.lastDay ?? '') || b.sessions - a.sessions || fmt.compare(a.label, b.label))
    }
    // 置顶行额外提前,按置顶先后（= 时间轴列序）;其余保持上面的排序
    const pinRank = (r: ProjectMetaRow) => {
      const i = pins.indexOf(r.key)
      return i < 0 ? Number.MAX_SAFE_INTEGER : i
    }
    return out.map((r, i) => ({ r, i })).sort((a, b) => pinRank(a.r) - pinRank(b.r) || a.i - b.i).map((x) => x.r)
  }, [rows, filter, sort, query, pins, t])

  const toggleSelect = (key: string) =>
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })
  const allVisibleSelected = visible.length > 0 && visible.every((r) => selected.has(r.key))
  const toggleSelectAll = () =>
    setSelected((prev) => {
      const next = new Set(prev)
      if (allVisibleSelected) visible.forEach((r) => next.delete(r.key))
      else visible.forEach((r) => next.add(r.key))
      return next
    })

  // ---- 行操作 ----

  const setHidden = (r: ProjectMetaRow, hidden: boolean) => run(() => projectService.setProjectMeta(r.key, { alias: r.alias, hidden, note: r.note }))
  /** 状态徽章切换：Active ⇄ Scratch。 */
  const toggleScratch = async (r: ProjectMetaRow) => {
    if (r.status === 'active') {
      await run(() => projectService.mergeProjects([r.key], projectService.SCRATCH_KEY))
    } else if (r.status === 'scratch') {
      if (r.mergedInto === projectService.SCRATCH_KEY && !(await run(() => projectService.unmergeProjects([r.key])))) return
      await setHidden(r, false)
    }
  }

  const saveRename = async () => {
    const editing = renamingRef.current
    if (!editing) return
    setRenaming(null)
    const r = byKey.get(editing.key)
    if (!r) return
    const alias = editing.value.trim()
    if (alias === (r.alias ?? '')) return
    await run(() => projectService.setProjectMeta(r.key, { alias: alias || null, hidden: r.hidden, note: r.note }))
  }

  const batchHide = async () => {
    const keys = [...selected]
    for (const k of keys) {
      const r = byKey.get(k)
      if (!r || r.hidden) continue
      if (!(await run(() => projectService.setProjectMeta(r.key, { alias: r.alias, hidden: true, note: r.note })))) return
    }
    setSelected(new Set())
  }

  const mergeCandidates = useMemo(() => {
    if (!merging) return []
    const src = new Set(merging.sources)
    return rows.filter((r) => !src.has(r.key) && r.mergedInto === null).sort((a, b) => fmt.compare(a.label, b.label))
  }, [merging, rows, t])
  const blockedSources = merging ? merging.sources.filter((k) => targets.has(k)) : []

  const confirmMerge = async () => {
    if (!merging || !merging.target) return
    if (await run(() => projectService.mergeProjects(merging.sources, merging.target))) {
      setMerging(null)
      setSelected(new Set())
    }
  }

  // ---- 规则 ----

  const applyRule = async (patch: Partial<ScratchRuleInfo['rule']>) => {
    if (!ruleInfo) return
    const next = { ...ruleInfo.rule, ...patch }
    setBusy(true)
    setError(null)
    const res = await projectService.setScratchRule(next)
    setBusy(false)
    if (!res.ok) {
      setError(res.error)
      setSessionsDraft(String(ruleInfo.rule.minSessions))
      setTurnsDraft(String(ruleInfo.rule.minTurns))
      return
    }
    setRuleInfo(res.value)
    const r = res.value.rule
    setDesignPrefs({ scratchRuleEnabled: r.enabled, scratchMinSessions: r.minSessions, scratchMinTurns: r.minTurns, scratchUnknown: r.unknownAsScratch })
    setRefreshTick((t) => t + 1)
  }

  const commitNumber = (field: 'minSessions' | 'minTurns', draft: string) => {
    if (!ruleInfo) return
    const [lo, hi] = field === 'minSessions' ? ruleInfo.minSessionsBounds : ruleInfo.minTurnsBounds
    const n = Number(draft)
    const reset = () => (field === 'minSessions' ? setSessionsDraft(String(ruleInfo.rule.minSessions)) : setTurnsDraft(String(ruleInfo.rule.minTurns)))
    if (!Number.isInteger(n) || n < lo || n > hi) {
      setError(t(field === 'minSessions' ? 'sessionsMustBe' : 'turnsMustBe', { lo, hi }))
      reset()
      return
    }
    if (n === ruleInfo.rule[field]) return
    void applyRule({ [field]: n })
  }

  const toggleScratchHidden = (hidden: boolean) =>
    run(() => projectService.setProjectMeta(projectService.SCRATCH_KEY, { alias: list?.scratchAlias ?? null, hidden, note: null }))

  const rule = ruleInfo?.rule
  const selectedCount = selected.size

  return (
    <div className="pm">
      <div className="setting-section">{t('scratchRule')}</div>
      <div className="setting-block">
        <Toggle
          label={t('collapseSmall')}
          title={t('collapseSmallHint')}
          checked={rule?.enabled ?? true}
          disabled={!rule || busy}
          onChange={(v) => void applyRule({ enabled: v })}
        />
        <div className="setting-row">
          <span title={t('collapseWhenHint')}>{t('collapseWhen')}</span>
          <div className="pm-rule-nums">
            <span className="setting-unit">{t('fewerThan')}</span>
            <input
              className="setting-num"
              type="number"
              aria-label={t('sessionThreshold')}
              value={sessionsDraft}
              min={ruleInfo?.minSessionsBounds[0]}
              max={ruleInfo?.minSessionsBounds[1]}
              disabled={!rule || !rule.enabled || busy}
              onChange={(e) => setSessionsDraft(e.target.value)}
              onBlur={() => commitNumber('minSessions', sessionsDraft)}
              onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
            />
            <span className="setting-unit">{t('sessionsAndFewerThan')}</span>
            <input
              className="setting-num"
              type="number"
              aria-label={t('turnThreshold')}
              value={turnsDraft}
              min={ruleInfo?.minTurnsBounds[0]}
              max={ruleInfo?.minTurnsBounds[1]}
              disabled={!rule || !rule.enabled || busy}
              onChange={(e) => setTurnsDraft(e.target.value)}
              onBlur={() => commitNumber('minTurns', turnsDraft)}
              onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
            />
            <span className="setting-unit">{t('turnsUnit')}</span>
          </div>
        </div>
        <Toggle
          label={t('unknownToScratch')}
          title={t('unknownToScratchHint')}
          checked={rule?.unknownAsScratch ?? true}
          disabled={!rule || busy}
          onChange={(v) => void applyRule({ unknownAsScratch: v })}
        />
        <Toggle
          label={t('hideScratch')}
          title={t('hideScratchHint')}
          checked={list?.scratchHidden ?? false}
          disabled={!list || busy}
          onChange={(v) => void toggleScratchHidden(v)}
        />
        {ruleInfo && (
          <div className="setting-note">
            {t('ruleDefault', { sessions: ruleInfo.defaults.minSessions, turns: ruleInfo.defaults.minTurns })}
          </div>
        )}
      </div>

      <div className="setting-section">{t('folders')}</div>
      <div className="setting-block pm-list-block">
        <div className="pm-toolbar">
          <div className="setting-seg" role="group" aria-label={t('statusFilterAria')}>
            {STATUS_FILTERS.map((f) => (
              <button
                key={f.v}
                type="button"
                className={`setting-seg-btn${filter === f.v ? ' is-active' : ''}`}
                title={t(f.hint)}
                onClick={() => setFilter(f.v)}
              >
                {f.label === null ? <PinIcon /> : t(f.label)} <span className="pm-count">{counts[f.v]}</span>
              </button>
            ))}
          </div>
          <div className="setting-seg" role="group" aria-label={t('sortAria')}>
            <button type="button" className={`setting-seg-btn${sort === 'recent' ? ' is-active' : ''}`} title={t('sortRecentHint')} onClick={() => setSort('recent')}>
              {t('sortRecent')}
            </button>
            <button type="button" className={`setting-seg-btn${sort === 'sessions' ? ' is-active' : ''}`} title={t('sortSessionsHint')} onClick={() => setSort('sessions')}>
              {t('sortSessions')}
            </button>
          </div>
          <input className="setting-input pm-search" type="search" placeholder={t('searchPlaceholder')} value={query} onChange={(e) => setQuery(e.target.value)} />
        </div>

        {selectedCount > 0 && !merging && (
          <div className="pm-batchbar">
            <span>{t('selectedCount', { n: selectedCount })}</span>
            <button className="setting-btn" disabled={busy} onClick={() => void batchHide()} title={t('hideSelectedHint')}>
              {t('hide')}
            </button>
            <button className="setting-btn" disabled={busy} onClick={() => setMerging({ sources: [...selected], target: '' })} title={t('mergeSelectedHint')}>
              {t('mergeInto')}
            </button>
            <button className="setting-btn" onClick={() => setSelected(new Set())}>
              {t('clear')}
            </button>
          </div>
        )}

        {merging && (
          <div className="pm-batchbar pm-mergebar">
            <span>
              {merging.sources.length === 1
                ? t('mergeOneInto', { name: byKey.get(merging.sources[0])?.label ?? merging.sources[0] })
                : t('mergeManyInto', { n: merging.sources.length })}
            </span>
            <select
              className="matrix-sort pm-target"
              value={merging.target}
              aria-label={t('mergeTarget')}
              onChange={(e) => setMerging({ ...merging, target: e.target.value })}
            >
              <option value="">{t('chooseProject')}</option>
              {mergeCandidates.map((r) => (
                <option key={r.key} value={r.key} title={r.key}>
                  {r.label}
                  {r.status !== 'active' ? optionStatus(r.status) : ''}
                </option>
              ))}
            </select>
            <button className="setting-btn is-active" disabled={busy || !merging.target || blockedSources.length > 0} onClick={() => void confirmMerge()}>
              {t('merge')}
            </button>
            <button className="setting-btn" onClick={() => setMerging(null)}>
              {t('cancel')}
            </button>
            {blockedSources.length > 0 && (
              <span className="pm-error">
                {t(blockedSources.length === 1 ? 'blockedOne' : 'blockedMany', { names: blockedSources.map((k) => byKey.get(k)?.label ?? k).join(', ') })}
              </span>
            )}
          </div>
        )}

        {error && (
          <div className="pm-error" role="alert">
            {error}
          </div>
        )}

        {list === undefined ? (
          <div className="setting-note">{t('loading')}</div>
        ) : list === null ? (
          <div className="setting-note">{t('dataUnavailable')}</div>
        ) : visible.length === 0 ? (
          <div className="setting-note">{rows.length === 0 ? t('noFoldersYet') : t('noFoldersMatch')}</div>
        ) : (
          <div className="pm-table-wrap">
            <table className="pm-table">
              <thead>
                <tr>
                  <th className="pm-col-check">
                    <input type="checkbox" aria-label={t('selectAllShown')} checked={allVisibleSelected} onChange={toggleSelectAll} />
                  </th>
                  <th className="is-left">{t('colName')}</th>
                  <th className="is-left">{t('colAgents')}</th>
                  <th title={t('colSessionsHint')}>{t('colSessions')}</th>
                  <th title={t('colTurnsHint')}>{t('colTurns')}</th>
                  <th title={t('colTokensHint')}>{t('colTokens')}</th>
                  <th className="is-left" title={t('colActiveHint')}>{t('colActive')}</th>
                  <th className="is-left">{t('colStatus')}</th>
                  <th className="is-left">{t('colActions')}</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((r) => {
                  const isRenaming = renaming?.key === r.key
                  const hint = statusHint(r, list.scratchHidden)
                  return (
                    <tr key={r.key} className={`pm-row is-${r.status}${selected.has(r.key) ? ' is-selected' : ''}`}>
                      <td className="pm-col-check">
                        <input type="checkbox" aria-label={t('selectRow', { label: r.label })} checked={selected.has(r.key)} onChange={() => toggleSelect(r.key)} />
                      </td>
                      <td className="is-left pm-name" title={r.alias ? `${r.alias}\n${r.key}` : r.key}>
                        {isRenaming ? (
                          <input
                            className="setting-input pm-rename"
                            autoFocus
                            maxLength={projectService.META_TEXT_MAX}
                            placeholder={r.key.split('/').filter(Boolean).pop() ?? r.key}
                            value={renaming.value}
                            onChange={(e) => setRenaming({ key: r.key, value: e.target.value })}
                            onKeyDown={(e) => {
                              if (e.key === 'Enter') void saveRename()
                              if (e.key === 'Escape') setRenaming(null)
                            }}
                            onBlur={() => void saveRename()}
                          />
                        ) : (
                          <div className="pm-name-wrap">
                            {r.effectiveKey !== null || pins.includes(r.key) ? (
                              <button
                                type="button"
                                className={`pm-pin-btn${pins.includes(r.key) ? ' is-active' : ''}`}
                                title={pins.includes(r.key) ? t('unpinHint') : t('pinHint')}
                                aria-pressed={pins.includes(r.key)}
                                onClick={() => togglePin(r.key)}
                              >
                                <PinIcon />
                              </button>
                            ) : (
                              <span className="pm-pin-btn is-placeholder" />
                            )}
                            <div className="pm-name-text">
                              <span className="pm-label">{r.label}</span>
                              <span className="pm-path">{r.key}</span>
                            </div>
                          </div>
                        )}
                      </td>
                      <td className="is-left pm-agents" title={r.agents.join(', ')}>
                        {r.agents.join(', ') || '—'}
                      </td>
                      <td>{formatFull(r.sessions)}</td>
                      <td>{formatFull(r.turns)}</td>
                      <td title={formatFull(r.tokens)}>{formatCompact(r.tokens)}</td>
                      <td className="is-left pm-days">
                        {r.firstDay === r.lastDay ? dayLabel(r.firstDay) : `${dayLabel(r.firstDay)} – ${dayLabel(r.lastDay)}`}
                      </td>
                      <td className="is-left" title={hint || undefined}>
                        {r.status === 'active' || r.status === 'scratch' ? (
                          <button
                            type="button"
                            className={`pm-status is-toggle is-${r.status}${r.effectiveKey === null ? ' is-invisible' : ''}`}
                            disabled={busy || (r.status === 'active' && targets.has(r.key))}
                            onClick={() => void toggleScratch(r)}
                          >
                            {statusText(r)}
                          </button>
                        ) : (
                          <span className={`pm-status is-${r.status}${r.effectiveKey === null && r.status !== 'hidden' ? ' is-invisible' : ''}`}>{statusText(r)}</span>
                        )}
                      </td>
                      <td className="is-left">
                        <div className="pm-actions">
                        <button className="pm-act" disabled={busy} title={t('renameHint')} onClick={() => setRenaming({ key: r.key, value: r.alias ?? '' })}>
                          {t('rename')}
                        </button>
                        {r.hidden ? (
                          <button className="pm-act" disabled={busy} title={t('unhideHint')} onClick={() => void setHidden(r, false)}>
                            {t('unhide')}
                          </button>
                        ) : (
                          <button className="pm-act" disabled={busy} title={t('hideHint')} onClick={() => void setHidden(r, true)}>
                            {t('hide')}
                          </button>
                        )}
                        {r.mergedInto && r.mergedInto !== projectService.SCRATCH_KEY ? (
                          <button className="pm-act" disabled={busy} title={t('unmergeHint', { target: r.mergedLabel ?? r.mergedInto })} onClick={() => void run(() => projectService.unmergeProjects([r.key]))}>
                            {t('unmerge')}
                          </button>
                        ) : (
                          <button
                            className="pm-act"
                            disabled={busy || targets.has(r.key)}
                            title={targets.has(r.key) ? t('mergeRowBlocked') : t('mergeRowHint')}
                            onClick={() => setMerging({ sources: [r.key], target: '' })}
                          >
                            {t('mergeRow')}
                          </button>
                        )}
                        {r.folderExists && (
                          <button className="pm-act" title={t('openInExplorer')} onClick={() => void projectService.openProjectFolder(r.key).then((res) => !res.ok && setError(res.error))}>
                            {t('open')}
                          </button>
                        )}
                        {r.managed && (
                          <button className="pm-act is-muted" disabled={busy} title={t('resetHint')} onClick={() => void run(() => projectService.resetProjectMeta(r.key))}>
                            {t('reset')}
                          </button>
                        )}
                        </div>
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  )
}

function PinIcon() {
  return (
    <svg width="11" height="11" viewBox="0 0 12 12" fill="currentColor" aria-hidden="true">
      <path d="M7.5 1 11 4.5 9.6 5.9 8.9 5.2 6.8 7.3l.4 2.4L6 10.9 3.9 8.8 1.5 11.2l-.7-.7 2.4-2.4L1.1 6l1.2-1.2 2.4.4 2.1-2.1-.7-.7L7.5 1Z" />
    </svg>
  )
}

/** Off / On 两段开关（与设置页 ToggleRow 同一视觉语言;该组件未导出,这里按同一类名复刻）。 */
function Toggle({ label, title, checked, disabled = false, onChange }: { label: string; title?: string; checked: boolean; disabled?: boolean; onChange(v: boolean): void }) {
  const t = useT('projects')
  return (
    <div className="setting-row">
      <span title={title}>{label}</span>
      <div className="setting-seg" role="group" aria-label={label}>
        <button type="button" className={`setting-seg-btn${!checked ? ' is-active' : ''}`} disabled={disabled} aria-pressed={!checked} onClick={() => onChange(false)}>
          {t('off')}
        </button>
        <button type="button" className={`setting-seg-btn${checked ? ' is-active' : ''}`} disabled={disabled} aria-pressed={checked} onClick={() => onChange(true)}>
          {t('on')}
        </button>
      </div>
    </div>
  )
}
