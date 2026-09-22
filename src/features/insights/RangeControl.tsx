// 范围控件:7d / 30d / 90d / All / Custom 分段 + 自定义起止日期 +
// 解析后的区间文字;选定单个项目时,非项目生命周期模式下给「Project span」按钮。
// 状态与规则在 useRangeSelection,本组件只渲染与转发。
import { useEffect, useState } from 'react'
import { Seg } from './Seg'
import { formatSpan, todayYmd, validateCustom, type PresetDays } from './range'
import type { RangeSelection } from './useRangeSelection'

type SegKey = '7' | '30' | '90' | 'all' | 'custom'

export default function RangeControl({ selection, noun, note }: {
  selection: RangeSelection
  /** 预设按钮 hover 文案的主语（"Turns" / "Usage"）。 */
  noun: string
  /** 范围条末尾的口径说明（同页多个口径并列时写明,不隐藏差异）。 */
  note?: string
}) {
  const { sel, range, projectSpan, project, choose, applyProjectSpan } = selection
  const segValue: SegKey | '' = sel.kind === 'preset' ? (String(sel.days) as SegKey) : sel.kind === 'project' ? '' : sel.kind

  // 自定义日期草稿:合法才提交;切到 Custom 时以当前解析区间起步
  const [draft, setDraft] = useState({ startDay: range.startDay, endDay: range.endDay })
  useEffect(() => {
    if (sel.kind === 'custom') setDraft({ startDay: sel.startDay, endDay: sel.endDay })
  }, [sel])
  const error = sel.kind === 'custom' ? validateCustom(draft.startDay, draft.endDay) : null

  const editDraft = (patch: Partial<typeof draft>) => {
    const next = { ...draft, ...patch }
    setDraft(next)
    if (!validateCustom(next.startDay, next.endDay)) choose({ kind: 'custom', ...next })
  }

  const today = todayYmd()
  const spanText = formatSpan(range)

  return (
    <div className="range-control">
      <Seg<SegKey | ''>
        value={segValue}
        options={[
          ...([7, 30, 90] as PresetDays[]).map((d) => ({ v: String(d) as SegKey, label: `${d}d`, hint: `${noun} in the last ${d} days` })),
          { v: 'all' as SegKey, label: 'All', hint: `${noun} across all recorded days` },
          { v: 'custom' as SegKey, label: 'Custom', hint: 'Pick start and end dates' },
        ]}
        onChange={(v) => {
          if (v === 'all') choose({ kind: 'all' })
          else if (v === 'custom') choose({ kind: 'custom', startDay: range.startDay, endDay: range.endDay })
          else if (v) choose({ kind: 'preset', days: Number(v) as PresetDays })
        }}
      />
      {sel.kind === 'custom' && (
        <span className="range-custom">
          <input
            type="date"
            className="range-date"
            value={draft.startDay}
            max={today}
            aria-label="Start date"
            aria-invalid={Boolean(error) || undefined}
            onChange={(e) => editDraft({ startDay: e.target.value })}
          />
          <span className="range-dash">–</span>
          <input
            type="date"
            className="range-date"
            value={draft.endDay}
            max={today}
            aria-label="End date"
            aria-invalid={Boolean(error) || undefined}
            onChange={(e) => editDraft({ endDay: e.target.value })}
          />
        </span>
      )}
      {error ? (
        <span className="range-error">{error}</span>
      ) : (
        <span className={`range-span${sel.kind === 'project' ? ' is-project' : ''}`} title={sel.kind === 'project' ? 'Lifecycle of the selected project: first to last active day. Change the range to stop following it.' : spanText}>
          {sel.kind === 'project' ? `Project span · ${spanText}` : spanText}
        </span>
      )}
      {project && sel.kind !== 'project' && projectSpan && (
        <button
          className="seg range-project-btn"
          title={`Use the selected project's lifecycle (${formatSpan({ startDay: projectSpan.firstDay, endDay: projectSpan.lastDay })}) and follow it when the project changes`}
          onClick={applyProjectSpan}
        >
          Project span
        </button>
      )}
      {note && <span className="range-note">{note}</span>}
    </div>
  )
}
