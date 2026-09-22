// 范围选择状态:Tasks 与 Insights 各自持有一份,规则共用。
// - 数据跨度（All 起点）与选定项目的生命周期经 get_project_span 取,随 usage:changed 刷新;
// - 选定单个项目且自动开关开（designPrefs.projectAutoRange,缺省开）→ 范围切到项目生命周期;
// - 在项目生命周期模式下手改范围 → 关自动（写 designPrefs,两视图共用）;「Project span」按钮套用并重开;
// - 取消项目选择 → 从项目生命周期回到进入前的范围。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { taskService } from '../../services'
import type { DayRange, DaySpan } from '../../services'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import { DEFAULT_RANGE, resolveRange, type RangeSel } from './range'

export interface RangeSelection {
  sel: RangeSel
  range: DayRange
  dataSpan: DaySpan | null
  /** 当前选定项目的生命周期（未选项目 / 取数中 / 无数据 = null）。 */
  projectSpan: DaySpan | null
  /** 选定了单个项目（控件据此显示「Project span」按钮）。 */
  project: string
  /** 用户手动改范围（预设 / All / Custom）。 */
  choose: (next: RangeSel) => void
  /** 套用项目生命周期并重开自动切换。 */
  applyProjectSpan: () => void
}

export function useRangeSelection(project: string, refreshTick: number): RangeSelection {
  const [sel, setSel] = useState<RangeSel>(DEFAULT_RANGE)
  // 进入项目生命周期模式前的范围（取消项目时回到这里）
  const plainRef = useRef<RangeSel>(DEFAULT_RANGE)
  const [auto, setAuto] = useState(() => getDesignPrefs().projectAutoRange ?? true)
  useEffect(() => subscribeDesignPrefs((p) => setAuto(p.projectAutoRange ?? true)), [])

  const [dataSpan, setDataSpan] = useState<DaySpan | null>(null)
  const [projectSpan, setProjectSpan] = useState<{ project: string; span: DaySpan | null } | null>(null)

  useEffect(() => {
    let cancelled = false
    void taskService.getProjectSpan().then((res) => {
      if (!cancelled) setDataSpan(res)
    })
    return () => {
      cancelled = true
    }
  }, [refreshTick])

  useEffect(() => {
    if (!project) {
      setProjectSpan(null)
      return
    }
    let cancelled = false
    void taskService.getProjectSpan(project).then((res) => {
      if (!cancelled) setProjectSpan({ project, span: res })
    })
    return () => {
      cancelled = true
    }
  }, [project, refreshTick])

  const span = projectSpan && projectSpan.project === project ? projectSpan.span : null

  useEffect(() => {
    if (!project) {
      setSel((cur) => (cur.kind === 'project' ? plainRef.current : cur))
      return
    }
    setSel((cur) => {
      if (auto && span) return { kind: 'project', project, startDay: span.firstDay, endDay: span.lastDay }
      // 自动关:换了项目 → 旧项目的生命周期不再适用,回到进入前的范围
      if (cur.kind === 'project' && cur.project !== project) return plainRef.current
      return cur
    })
  }, [project, span, auto])

  const choose = useCallback(
    (next: RangeSel) => {
      if (sel.kind === 'project') setDesignPrefs({ projectAutoRange: false })
      plainRef.current = next
      setSel(next)
    },
    [sel.kind],
  )

  const applyProjectSpan = useCallback(() => {
    setDesignPrefs({ projectAutoRange: true })
    if (project && span) setSel({ kind: 'project', project, startDay: span.firstDay, endDay: span.lastDay })
  }, [project, span])

  // refreshTick:跨日后刷新时预设范围随今天前移
  const range = useMemo(
    () => resolveRange(sel, dataSpan),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [sel, dataSpan, refreshTick],
  )

  return { sel, range, dataSpan, projectSpan: span, project, choose, applyProjectSpan }
}
