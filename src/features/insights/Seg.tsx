// 分段控件（口径控件,样式复用 .seg / .toolbar-group）。
// 自 InsightsView 抽出供 Tasks 视图复用;新增 disabled。
// 禁用态用 aria-disabled + is-disabled 而非原生 disabled:原生禁用按钮不派发鼠标事件,
// hover 提示（title）在 WebView 里不可靠,而「为什么不可选」正是要靠提示说明。

export interface SegOption<T> {
  v: T
  label: string
  /** hover 提示（缺省回落到 label;禁用时说明原因）。 */
  hint?: string
  disabled?: boolean
}

export function Seg<T extends string | number>({ value, options, onChange }: {
  value: T
  options: SegOption<T>[]
  onChange: (v: T) => void
}) {
  return (
    <div className="toolbar-group">
      {options.map((o) => (
        <button
          key={String(o.v)}
          className={`seg${value === o.v ? ' is-active' : ''}${o.disabled ? ' is-disabled' : ''}`}
          title={o.hint ?? o.label}
          aria-disabled={o.disabled || undefined}
          onClick={() => {
            if (!o.disabled) onChange(o.v)
          }}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}
