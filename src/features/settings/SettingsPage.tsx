// SettingsPage：设置页（自右缘窄面板改为内容区全幅独立视图）。
// 子 tab：General（行为）/ Appearance（美化）/ Data（导出）/ Subscriptions
// （订阅额度）/ About（版本与更新）。
// 外观 tab 的挂件美化组：预定义色板 + react-colorful 取色弹层 + hex 输入 +
// 恢复默认；色与透明度分离（调色盘只改色相，alpha 仍走 bgOpacity 滑条）。
// 浮层铁律：取色弹层 DOM 常驻不卸载——隐藏=移出视口+visibility，
// 禁止条件渲染（透明 WebView2 条件卸载留脏像素残影）。
import { useEffect, useRef, useState } from 'react'
// react-colorful 自注入样式（运行时 <style> 注入，无独立 CSS 文件可 import）。
import { HexColorPicker, HexColorInput } from 'react-colorful'
import { autostartService, collectorService, dataService, events, exportService, subscriptionService, updateService, windowService, type AutostartInfo, type CredentialInfo, type ExportResult, type DataInfo, type SubscriptionSnapshot, type UpdateCheck, type UpdateProgress } from '../../services'
import { currentMonth } from '../../lib/time'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs, orbBoostConfig, SIZE_PRESETS, RADIUS_PRESETS, type DesignPrefs, type SizePreset, type WeekStart } from './designPrefs'
import { deriveWidgetTheme } from './widgetTheme'
import './settings.css'

type SettingsTab = 'general' | 'appearance' | 'data' | 'subscriptions' | 'about'

/** 页签（hint = hover 提示：一句话说明本页管什么）。 */
const TABS: { id: SettingsTab; label: string; hint: string }[] = [
  { id: 'general', label: 'General', hint: 'Collection, matrix and widget behavior' },
  { id: 'appearance', label: 'Appearance', hint: 'Colors, glass material and corner radius' },
  { id: 'data', label: 'Data', hint: 'Storage, backup, restore and export' },
  { id: 'subscriptions', label: 'Subscriptions', hint: 'Quota polling, boost monitoring and binding' },
  { id: 'about', label: 'About', hint: 'Version and updates' },
]

/** 策展预定义色板（8 色，点击即用；首项=跟随 scheme 默认）。 */
const WIDGET_SWATCHES: { label: string; hex: string | null }[] = [
  { label: 'Default', hex: null },
  { label: 'Snow', hex: '#f8f9fb' },
  { label: 'Paper', hex: '#f5efe2' },
  { label: 'Mist', hex: '#e8f0f2' },
  { label: 'Graphite', hex: '#3a3f4a' },
  { label: 'Midnight', hex: '#141b2d' },
  { label: 'Navy', hex: '#1e3a5f' },
  { label: 'Forest', hex: '#1f3d2b' },
  { label: 'Plum', hex: '#3b2a4d' },
]

/** 主界面色板：中性系 + 与挂件同款的深色锚点。
 * 首项=跟随 scheme 默认（A 圆点）。 */
const MAIN_SWATCHES: { label: string; hex: string | null }[] = [
  { label: 'Default', hex: null },
  { label: 'Snow', hex: '#f8f9fb' },
  { label: 'Mist', hex: '#e8f0f2' },
  { label: 'Fog', hex: '#dde4ec' },
  { label: 'Graphite', hex: '#3a3f4a' },
  { label: 'Midnight', hex: '#141b2d' },
  { label: 'Slate', hex: '#2b3648' },
  { label: 'Navy', hex: '#1e3a5f' },
  { label: 'Espresso', hex: '#2e2620' },
]

/** 色板圆点内的小徽标色：深色圆点用浅徽标，浅色圆点用深徽标。 */
function swatchMark(hex: string): string {
  const triple = hexToTriple(hex)
  if (!triple) return '#1e293b'
  const [r, g, b] = triple.split(/\s+/).map(Number)
  return 0.2126 * srgb(r) + 0.7152 * srgb(g) + 0.0722 * srgb(b) > 0.5 ? '#334155' : '#e2e8f0'
}

function srgb(v: number): number {
  const c = v / 255
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4
}

function hexToTriple(hex: string): string | null {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim())
  if (!m) return null
  const n = parseInt(m[1], 16)
  return `${(n >> 16) & 255} ${(n >> 8) & 255} ${n & 255}`
}

export default function SettingsPage({ onBack, initialTab }: { onBack(): void; initialTab?: SettingsTab }) {
  const [tab, setTab] = useState<SettingsTab>(initialTab ?? 'general')
  return (
    <div className="settings-page">
      {/* 工具栏内容限宽 720px 与中间面板对齐（返回键在 General 左侧）*/}
      <div className="settings-toolbar">
        <div className="settings-toolbar-inner">
          <button className="settings-back" onClick={onBack} title="Back to matrix" aria-label="Back to matrix">
            <BackIcon />
            <span>Matrix</span>
          </button>
          <div className="settings-tabs" role="tablist" aria-label="Settings sections">
            {TABS.map((t) => (
              <button
                key={t.id}
                role="tab"
                aria-selected={tab === t.id}
                className={`settings-tab${tab === t.id ? ' is-active' : ''}`}
                title={t.hint}
                onClick={() => setTab(t.id)}
              >
                {t.label}
              </button>
            ))}
          </div>
        </div>
      </div>
      <div className="settings-content" role="tabpanel">
        {tab === 'general' && <GeneralTab />}
        {tab === 'appearance' && <AppearanceTab />}
        {tab === 'data' && <DataTab />}
        {tab === 'subscriptions' && <SubscriptionsTab />}
        {tab === 'about' && <AboutTab />}
      </div>
    </div>
  )
}

/* ---------------- General：行为类设置（自右缘面板平移） ---------------- */

function GeneralTab() {
  const [paused, setPaused] = useState(false)
  const [snapEnabled, setSnapEnabled] = useState(false)
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  // 开机自启：状态单一源 = 系统启动项（Rust 侧插件读写），前端不存镜像——
  // 与 paused/snap 同款：每次进 tab 现查，勾选直接写系统。
  const [autostart, setAutostart] = useState<AutostartInfo | null>(null)
  const [autostartNote, setAutostartNote] = useState<string | null>(null)

  // Pull the real backend state every time the tab mounts — the tray
  // checkboxes can change while the page was hidden.
  useEffect(() => {
    collectorService.getPaused().then((p) => p !== null && setPaused(p)).catch(console.error)
    collectorService.getSnapEnabled().then((v) => v !== null && setSnapEnabled(v)).catch(console.error)
    autostartService.getAutostart().then((v) => v !== null && setAutostart(v)).catch(console.error)
  }, [])
  useEffect(() => subscribeDesignPrefs(setDesign), [])

  const resetWidgetSize = () => {
    const { w, h } = SIZE_PRESETS[design.sizePreset]
    windowService.setWidgetSize(w, h).catch(console.error)
  }

  const setSizePreset = (preset: SizePreset) => {
    // 写 pref 即可——YearMatrix 监听变化后执行 set_widget_size（执行者单一）。
    setDesignPrefs({ sizePreset: preset })
  }

  const togglePause = (next: boolean) => {
    setPaused(next)
    collectorService.setPaused(next).catch(console.error)
  }

  const toggleSnap = (next: boolean) => {
    setSnapEnabled(next)
    collectorService.setSnapEnabled(next).catch(console.error)
  }

  /** 自启开关：乐观翻牌 → 写系统启动项；失败回读真实状态，不留假象。 */
  const toggleAutostart = (next: boolean) => {
    if (!autostart || !autostart.supported) return
    setAutostart({ ...autostart, enabled: next })
    setAutostartNote(null)
    autostartService
      .setAutostart(next)
      .then((r) => {
        if (r) {
          setAutostart(r)
        } else {
          setAutostartNote('Could not update the startup entry, see log.')
          autostartService.getAutostart().then((v) => v !== null && setAutostart(v)).catch(console.error)
        }
      })
      .catch(console.error)
  }

  // 设置页改版：分组参照 Appearance 用 setting-block 卡片化；
  // 布尔项复选框全部退役改 ToggleRow（Off/On 分段开关）；解释性长文转为
  // 行 label / 按钮 title hover，仅动态结果与危险警示保留常驻 note。
  // Week starts on 自 Widget 组迁入 Matrix 组（它决定矩阵行列排布口径）。
  return (
    <>
      <div className="setting-section">Startup</div>
      <div className="setting-block">
        <ToggleRow
          label="Launch at login"
          title={
            autostart?.supported
              ? 'Start TokenCalendar automatically when you sign in'
              : 'Only available in installed builds'
          }
          checked={autostart?.enabled ?? false}
          disabled={!autostart?.supported}
          onChange={toggleAutostart}
        />
        {autostartNote ? <div className="setting-note">{autostartNote}</div> : null}
      </div>

      <div className="setting-section">Collection</div>
      <div className="setting-block">
        <ToggleRow
          label="Pause collection"
          title="Paused data is kept; resuming continues incrementally"
          checked={paused}
          onChange={togglePause}
        />
      </div>

      <div className="setting-section">Matrix</div>
      <div className="setting-block">
        <div className="setting-row">
          <span title="Cap rows per view; extras collapse into a summary line">
            Max rows
          </span>
          <div className="setting-seg">
            {[8, 12, 15, 20, 0].map((n) => (
              <button
                key={n}
                className={`setting-seg-btn${design.matrixMaxRows === n ? ' is-active' : ''}`}
                title={n === 0 ? 'No limit' : `Show up to ${n} rows`}
                onClick={() => setDesignPrefs({ matrixMaxRows: n })}
              >
                {n === 0 ? 'All' : n}
              </button>
            ))}
          </div>
        </div>
        <div className="setting-row">
          <span title="Sets the first day of matrix rows and week columns">
            Week starts on
          </span>
          <div className="setting-seg">
            {(['sunday', 'monday'] as WeekStart[]).map((d) => (
              <button
                key={d}
                className={`setting-seg-btn${(design.weekStart ?? 'sunday') === d ? ' is-active' : ''}`}
                title={d === 'sunday' ? 'Weeks start on Sunday' : 'Weeks start on Monday'}
                onClick={() => setDesignPrefs({ weekStart: d })}
              >
                {d === 'sunday' ? 'Sunday' : 'Monday'}
              </button>
            ))}
          </div>
        </div>
      </div>

      <div className="setting-section">Widget</div>
      <div className="setting-block">
        <div className="setting-row">
          <span title="Applies the preset window size immediately">Widget size</span>
          <div className="setting-seg">
            {(['large', 'medium', 'small'] as SizePreset[]).map((p) => (
              <button
                key={p}
                className={`setting-seg-btn${design.sizePreset === p ? ' is-active' : ''}`}
                title={`${SIZE_PRESETS[p].w} × ${SIZE_PRESETS[p].h}`}
                onClick={() => setSizePreset(p)}
              >
                {p[0].toUpperCase() + p.slice(1)}
              </button>
            ))}
          </div>
        </div>
        <ToggleRow
          label="Lock widget"
          title="Content ignores the mouse until unlocked"
          checked={design.locked}
          onChange={(v) => setDesignPrefs({ locked: v })}
        />
        <ToggleRow
          label="Lock aspect ratio"
          title="Resize keeps the preset ratio"
          checked={design.lockAspectRatio}
          onChange={(v) => setDesignPrefs({ lockAspectRatio: v })}
        />
        <ToggleRow
          label="Snap to grid"
          title="Snap to the nearest 10px grid vertex"
          checked={snapEnabled}
          onChange={toggleSnap}
        />
        <div className="setting-actions">
          <button
            className="setting-btn"
            onClick={resetWidgetSize}
            title="Apply the preset size again"
          >
            Reset widget size
          </button>
        </div>
      </div>
    </>
  )
}

/* ---------------- Appearance：挂件美化组 + 主界面美化组 ---------------- */

/** 取色槽位：挂件卡片 / 主界面顶栏 / 主界面主体 / 主界面边框。 */
type ColorSlot = 'card' | 'titlebar' | 'panel' | 'border'

function AppearanceTab() {
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  useEffect(() => subscribeDesignPrefs(setDesign), [])

  // 取色弹层开合（单一弹层服务四个槽位）。浮层 DOM 常驻：open 只驱动
  // 类名/aria，绝不条件渲染卸载（铁律）。
  const [pickerFor, setPickerFor] = useState<ColorSlot | null>(null)
  const colorRowRefs = {
    card: useRef<HTMLDivElement>(null),
    titlebar: useRef<HTMLDivElement>(null),
    panel: useRef<HTMLDivElement>(null),
    border: useRef<HTMLDivElement>(null),
  }
  const popoverRefs = {
    card: useRef<HTMLDivElement>(null),
    titlebar: useRef<HTMLDivElement>(null),
    panel: useRef<HTMLDivElement>(null),
    border: useRef<HTMLDivElement>(null),
  }

  // 点击弹层外关闭。内点 = 色板行 + 取色弹层两者：二者是平级节点，只认色板行
  // 的话，弹层内拖取色盘/点输入框会被误判外点而闪关（react-colorful 不拦
  // mousedown 冒泡，必现）。
  useEffect(() => {
    if (!pickerFor) return
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node
      if (colorRowRefs[pickerFor].current?.contains(t)) return
      if (popoverRefs[pickerFor].current?.contains(t)) return
      setPickerFor(null)
    }
    window.addEventListener('mousedown', onDown)
    return () => window.removeEventListener('mousedown', onDown)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pickerFor])

  const custom = design.widgetCardBg
  const theme = deriveWidgetTheme(design)
  // 预览底色：自定义色直用；默认态跟当前 scheme 内置值（rgba 三元组合成）。
  const previewBg = custom
    ? `rgba(${theme?.cardBg ?? '248 249 251'} / ${design.bgOpacity})`
    : `rgba(var(--widget-card-bg) / ${design.bgOpacity})`

  /** 各槽位当前值/写入器，统一驱动下方的色板行与弹层。 */
  const slots: Record<ColorSlot, {
    value: string | undefined
    set: (hex: string | undefined) => void
    rowRef: React.RefObject<HTMLDivElement>
    popoverRef: React.RefObject<HTMLDivElement>
    fallback: string
  }> = {
    card: { value: custom, set: (h) => setDesignPrefs({ widgetCardBg: h }), rowRef: colorRowRefs.card, popoverRef: popoverRefs.card, fallback: '#f8f9fb' },
    titlebar: { value: design.titlebarBg, set: (h) => setDesignPrefs({ titlebarBg: h }), rowRef: colorRowRefs.titlebar, popoverRef: popoverRefs.titlebar, fallback: '#f8f9fb' },
    panel: { value: design.panelBg, set: (h) => setDesignPrefs({ panelBg: h }), rowRef: colorRowRefs.panel, popoverRef: popoverRefs.panel, fallback: '#ffffff' },
    border: { value: design.borderColor, set: (h) => setDesignPrefs({ borderColor: h }), rowRef: colorRowRefs.border, popoverRef: popoverRefs.border, fallback: '#e2e8f0' },
  }

  /** 色板行（PRESETS + 自定义钮）+ 取色弹层，包在槽位容器内：弹层 absolute
   * 锚定本行正下方（四个槽位互不串位）。浮层 DOM 常驻不卸载（铁律），
   * closed = visibility + pointer-events，open 才恢复交互。 */
  const renderColorField = (slot: ColorSlot, presets: { label: string; hex: string | null }[]) => {
    const s = slots[slot]
    const hasCustom = s.value && !presets.some((p) => p.hex === s.value)
    return (
      <div className="setting-color-field">
        <div className="setting-swatches" ref={s.rowRef}>
          {presets.map((p) => (
            <button
              key={p.label}
              className={`setting-swatch${(s.value ?? null) === p.hex ? ' is-active' : ''}`}
              style={{ background: p.hex ?? 'linear-gradient(135deg, #f8f9fb 50%, #161e2f 50%)' }}
              title={p.label}
              aria-label={p.label}
              onClick={() => {
                s.set(p.hex ?? undefined)
                setPickerFor(null)
              }}
            >
              {p.hex === null ? <span className="setting-swatch-mark">A</span> : <span className="setting-swatch-mark" style={{ color: swatchMark(p.hex) }}>●</span>}
            </button>
          ))}
          <button
            className={`setting-swatch is-custom${hasCustom ? ' is-active' : ''}`}
            style={s.value ? { background: s.value } : undefined}
            title="Custom color"
            aria-label="Custom color"
            aria-expanded={pickerFor === slot}
            onClick={() => setPickerFor((f) => (f === slot ? null : slot))}
          >
            {s.value ? null : <span className="setting-swatch-mark" style={{ color: '#64748b' }}>+</span>}
          </button>
        </div>
        <div ref={s.popoverRef} className={`color-popover${pickerFor === slot ? ' is-open' : ''}`} aria-hidden={pickerFor !== slot}>
          <HexColorPicker
            color={s.value ?? s.fallback}
            onChange={(hex) => {
              const v = normalizeHex(hex)
              if (v) s.set(v)
            }}
          />
          <div className="color-popover-input">
            <HexColorInput
              color={s.value ?? ''}
              onChange={(hex) => {
                const v = normalizeHex(hex)
                if (v) s.set(v)
              }}
              prefixed
            />
            <button className="setting-btn" onClick={() => s.set(undefined)} title="Clear the custom color">
              Auto
            </button>
          </div>
        </div>
      </div>
    )
  }

  return (
    <>
      <div className="setting-section">Corner radius</div>
      <div className="setting-block">
        <div className="setting-row">
          <span title="Shared by panel and widget; glass is clipped to ~8px by the system">
            Radius scheme
          </span>
          <div className="setting-seg">
            {(['small', 'medium', 'large'] as const).map((r) => (
              <button
                key={r}
                className={`setting-seg-btn${(design.radiusScheme ?? 'large') === r ? ' is-active' : ''}`}
                title={`${RADIUS_PRESETS[r].panel}px corners`}
                onClick={() => setDesignPrefs({ radiusScheme: r })}
              >
                {r === 'small' ? `Small (${RADIUS_PRESETS.small.panel}px)` : r === 'medium' ? `Medium (${RADIUS_PRESETS.medium.panel}px)` : `Large (${RADIUS_PRESETS.large.panel}px)`}
              </button>
            ))}
          </div>
        </div>
      </div>

      <div className="setting-section">Widget appearance</div>
      <div className="setting-block">
        <div className="setting-row">
          <span title="Applies to both themes; shades adapt automatically">
            Card color
          </span>
          <span className="setting-value">{custom ? custom.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('card', WIDGET_SWATCHES)}

        <div className="setting-row">
          <span title="Card background only — cells and text stay fully opaque">Background opacity</span>
          <input
            type="range"
            title="Drag to adjust"
            min={0.2}
            max={1}
            step={0.05}
            value={design.bgOpacity}
            onChange={(e) => setDesignPrefs({ bgOpacity: Number(e.target.value) })}
          />
          <span className="setting-value">{Math.round(design.bgOpacity * 100)}%</span>
        </div>
        <div className="setting-row">
          <span title="Opacity of the heatmap cells">Heatmap opacity</span>
          <input
            type="range"
            title="Drag to adjust"
            min={0.2}
            max={1}
            step={0.05}
            value={design.fgOpacity}
            onChange={(e) => setDesignPrefs({ fgOpacity: Number(e.target.value) })}
          />
          <span className="setting-value">{Math.round(design.fgOpacity * 100)}%</span>
        </div>

        <MaterialRow
          value={design.widgetMaterial}
          onChange={(m) => setDesignPrefs({ widgetMaterial: m })}
          title="Experimental — Mica needs Win11; Acrylic may lag while dragging"
        />

        {/* 实时预览卡：所见即所得（含对比度派生效果），不必抬头看挂件。*/}
        <div className="setting-preview" style={{ background: previewBg, borderColor: custom ? theme?.cardBorder : undefined }}>
          <span className="setting-preview-title" style={{ color: custom ? theme?.text : undefined }}>Token activity</span>
          <div className="setting-preview-cells">
            <i style={{ background: custom ? theme?.zeroBg : undefined }} />
            <i className="lv1" />
            <i className="lv2" />
            <i className="lv3" />
            <i className="lv4" />
            <i className="lv5" />
          </div>
        </div>

        <div className="setting-actions">
          <button
            className="setting-btn"
            onClick={() => setDesignPrefs({ widgetCardBg: undefined })}
            disabled={!custom}
            title="Reset card color to auto"
          >
            Restore default colors
          </button>
        </div>
      </div>

      <div className="setting-section">Main interface</div>
      <div className="setting-block">
        <div className="setting-row">
          <span title="Main window only — the widget is not affected">Titlebar color</span>
          <span className="setting-value">{design.titlebarBg ? design.titlebarBg.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('titlebar', MAIN_SWATCHES)}
        <div className="setting-row">
          <span title="Floating titlebar only; maximized turns solid">Titlebar opacity</span>
          <input
            type="range"
            title="Drag to adjust"
            min={0.2}
            max={1}
            step={0.05}
            value={design.titlebarAlpha ?? 0.8}
            onChange={(e) => setDesignPrefs({ titlebarAlpha: Number(e.target.value) })}
          />
          <span className="setting-value">{Math.round((design.titlebarAlpha ?? 0.8) * 100)}%</span>
        </div>

        <div className="setting-row">
          <span title="Main window body background">Main panel color</span>
          <span className="setting-value">{design.panelBg ? design.panelBg.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('panel', MAIN_SWATCHES)}

        <div className="setting-row">
          <span title="Panel and titlebar border">Border color</span>
          <span className="setting-value">{design.borderColor ? design.borderColor.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('border', MAIN_SWATCHES)}

        <MaterialRow
          value={design.mainMaterial}
          onChange={(m) => setDesignPrefs({ mainMaterial: m })}
          title="Experimental — uses system round corners and glass"
        />

        <div className="setting-actions">
          <button
            className="setting-btn"
            disabled={!design.titlebarBg && design.titlebarAlpha === undefined && !design.panelBg && !design.borderColor}
            onClick={() => setDesignPrefs({ titlebarBg: undefined, titlebarAlpha: undefined, panelBg: undefined, borderColor: undefined })}
            title="Reset titlebar, panel and border to auto"
          >
            Restore default colors
          </button>
        </div>
      </div>
    </>
  )
}

/** 宽松归一：#abc → #aabbcc；非法输入原样退回（HexColorInput 中途态）。 */
function normalizeHex(hex: string): string | undefined {
  const t = hex.trim().replace(/^#/, '')
  if (/^[0-9a-f]{3}$/i.test(t)) return `#${t[0]}${t[0]}${t[1]}${t[1]}${t[2]}${t[2]}`.toLowerCase()
  if (/^[0-9a-f]{6}$/i.test(t)) return `#${t.toLowerCase()}`
  return undefined
}

/* ---------------- 材质三档（spike，两窗口共用行控件） ---------------- */

const MATERIAL_OPTIONS: { label: string; value: 'mica' | 'acrylic' | null; hint: string }[] = [
  { label: 'Off', value: null, hint: 'No glass effect' },
  { label: 'Mica', value: 'mica', hint: 'Windows 11 system material' },
  { label: 'Acrylic', value: 'acrylic', hint: 'Live blur; may lag while dragging' },
]

function MaterialRow({
  value,
  onChange,
  title,
}: {
  value: 'mica' | 'acrylic' | undefined
  onChange(m: 'mica' | 'acrylic' | undefined): void
  title?: string
}) {
  return (
    <div className="setting-row">
      <span title={title}>Glass material</span>
      <div className="setting-seg">
        {MATERIAL_OPTIONS.map((o) => (
          <button
            key={o.label}
            className={`setting-seg-btn${(value ?? null) === o.value ? ' is-active' : ''}`}
            title={o.hint}
            onClick={() => onChange(o.value ?? undefined)}
          >
            {o.label}
          </button>
        ))}
      </div>
    </div>
  )
}

/* ---------------- 布尔开关行：Off/On 两段
 * 分段控件，与档位切换同语言。说明文案走 label 的 title hover，
 * 不再常驻 note；disabled 段保持可 hover（title 仍可读）。 ---------------- */

function ToggleRow({
  label,
  title,
  checked,
  disabled = false,
  onChange,
}: {
  label: string
  /** 行说明（hover 提示，替代原常驻 note）。 */
  title?: string
  checked: boolean
  disabled?: boolean
  onChange(next: boolean): void
}) {
  return (
    <div className="setting-row">
      <span title={title}>{label}</span>
      <div className="setting-seg" role="group" aria-label={label}>
        <button
          type="button"
          className={`setting-seg-btn${!checked ? ' is-active' : ''}`}
          title="Turn off"
          disabled={disabled}
          aria-pressed={!checked}
          onClick={() => onChange(false)}
        >
          Off
        </button>
        <button
          type="button"
          className={`setting-seg-btn${checked ? ' is-active' : ''}`}
          title="Turn on"
          disabled={disabled}
          aria-pressed={checked}
          onClick={() => onChange(true)}
        >
          On
        </button>
      </div>
    </div>
  )
}

/* ---------------- Data：月度导出（自右缘面板平移） ---------------- */

function DataTab() {
  const [exporting, setExporting] = useState(false)
  const [exportResult, setExportResult] = useState<string | null>(null)
  const month = currentMonth()
  // 数据管理（发布数据架构）：信息 + 迁移 + 备份/恢复 + 打开目录。
  // 目录输入用手输路径（写路径场景）;目录/文件选择走 dialog 插件。
  const [info, setInfo] = useState<DataInfo | null>(null)
  const [migTarget, setMigTarget] = useState('')
  const [bakTarget, setBakTarget] = useState('')
  const [resTarget, setResTarget] = useState('')
  const [busy, setBusy] = useState<'migrate' | 'backup' | 'restore' | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  // CodeBuddy 官网导入手动入口。
  const [importing, setImporting] = useState(false)
  const [importNote, setImportNote] = useState<string | null>(null)
  // credit 卡开关（insightsCredit）随区块一并移入本 tab。
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  useEffect(() => subscribeDesignPrefs(setDesign), [])

  useEffect(() => {
    let cancelled = false
    void dataService.getDataInfo().then((d) => {
      if (!cancelled) setInfo(d)
    })
    return () => {
      cancelled = true
    }
  }, [notice])

  const doExport = (format: 'csv' | 'json') => {
    setExporting(true)
    setExportResult(null)
    const p =
      format === 'csv' ? exportService.exportMonthCSV(month) : exportService.exportMonthJSON(month)
    p.then((res: ExportResult | null) => {
      if (res) setExportResult(`${format.toUpperCase()} exported ${res.rows} rows → ${res.path}`)
      else setExportResult('Export failed, see log')
    })
      .catch((e: unknown) => {
        console.error('[export]', e)
        setExportResult('Export failed, see log')
      })
      .finally(() => setExporting(false))
  }

  const run = async (kind: 'migrate' | 'backup' | 'restore') => {
    setBusy(kind)
    setNotice(null)
    try {
      if (kind === 'migrate') {
        const r = await dataService.migrateDataRoot(migTarget.trim())
        setNotice(r ? `${r} (restart to apply)` : 'Migration failed, see log')
      } else if (kind === 'backup') {
        const r = await dataService.backupData(bakTarget.trim())
        setNotice(r ? `Backup finished → ${r.path}` : 'Backup failed, see log')
      } else {
        const r = await dataService.restoreData(resTarget.trim())
        setNotice(r ? `${r} (restart to apply)` : 'Restore failed, see log')
      }
    } finally {
      setBusy(null)
    }
  }

  const fmtBytes = (n: number) => (n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : `${(n / 1024).toFixed(0)} KB`)

  /** 系统目录选择器 → 回填输入框（取消不动）。 */
  const pickInto = (set: (v: string) => void, title: string) => {
    void dataService.pickDirectory(title).then((dir) => {
      if (dir) set(dir)
    })
  }
  const dirPick = { migrate: 'Choose migration target folder', backup: 'Choose backup output folder', restore: 'Choose the folder containing the backup' } as const

  /** CodeBuddy 官网导出手动导入：选 xlsx → 校验+转存 imports → 唤醒采集线程
   * 即时入库（毫秒级,无 30s 轮询等待）。成功后刷新 storage 概要。 */
  const doImport = () => {
    setImporting(true)
    setImportNote(null)
    void (async () => {
      try {
        const file = await dataService.pickXlsxFile()
        if (!file) return
        const r = await dataService.importCodebuddyFile(file)
        if (!r) {
          setImportNote('Import failed, see log')
        } else if ('error' in r) {
          setImportNote(`Import failed: ${r.error}`)
        } else {
          setImportNote(`Imported → stored as ${r.stored_as}. Models map within a second.`)
          const d = await dataService.getDataInfo()
          if (d) setInfo(d)
        }
      } catch (e) {
        console.error('[import]', e)
        setImportNote('Import failed, see log')
      } finally {
        setImporting(false)
      }
    })()
  }

  return (
    <>
      <div className="setting-section">Storage</div>
      <div className="setting-block">
        {info ? (
          <div className="setting-note">
            Root: {info.root}
            {info.fell_back ? ' (default root not writable, fell back to AppData)' : ''}
            {info.custom_root ? <><br />Custom: {info.custom_root}</> : null}
            <br />DB {fmtBytes(info.db_bytes)} · imports {info.imports_count} files · exports {info.exports_count} files
          </div>
        ) : (
          <div className="setting-note">Storage info unavailable (non-Tauri environment or backend not ready).</div>
        )}
        <div className="setting-actions">
          <button
            className="setting-btn"
            onClick={() => void dataService.openDataDir()}
            title="Open the data directory in Explorer"
          >
            Open data folder
          </button>
        </div>
      </div>

      <div className="setting-section">CodeBuddy credit import</div>
      <div className="setting-block">
        <div className="setting-note">
          Import a CodeBuddy credit export (.xlsx) to resolve unknown models. Idempotent.
        </div>
        <div className="setting-actions">
          <button
            className="setting-btn"
            disabled={importing}
            onClick={doImport}
            title="Pick a .xlsx credit export; applied at once"
          >
            {importing ? 'Importing…' : 'Import export file…'}
          </button>
        </div>
        {importNote ? <div className="setting-note">{importNote}</div> : null}
        <ToggleRow
          label="Show credit card in Insights"
          title="Tokens × credit card at the bottom of the Chart view"
          checked={design.insightsCredit}
          onChange={(v) => setDesignPrefs({ insightsCredit: v })}
        />
      </div>

      <div className="setting-section">Migrate data root</div>
      <div className="setting-block">
        <div className="setting-note">
          Moves cache, prefs, imports and exports to a new folder (restart to apply).
          Pause collection first.
        </div>
        <div className="setting-actions">
          <input className="setting-input" placeholder="D:\Data\TokenCalendar" value={migTarget} onChange={(e) => setMigTarget(e.target.value)} />
          <button className="setting-btn" onClick={() => void pickInto(setMigTarget, dirPick.migrate)} title="Pick a folder">Browse</button>
          <button
            className="setting-btn"
            disabled={busy !== null || !migTarget.trim()}
            onClick={() => void run('migrate')}
            title="Move data files and write the pointer (restart to apply)"
          >
            {busy === 'migrate' ? 'Migrating…' : 'Migrate'}
          </button>
        </div>
      </div>

      <div className="setting-section">Backup</div>
      <div className="setting-block">
        <div className="setting-note">SQLite snapshot + prefs + import files → target folder.</div>
        <div className="setting-actions">
          <input className="setting-input" placeholder="E:\Backup\TokenCalendar" value={bakTarget} onChange={(e) => setBakTarget(e.target.value)} />
          <button className="setting-btn" onClick={() => void pickInto(setBakTarget, dirPick.backup)} title="Pick a folder">Browse</button>
          <button
            className="setting-btn"
            disabled={busy !== null || !bakTarget.trim()}
            onClick={() => void run('backup')}
            title="Write a consistent snapshot to the target folder"
          >
            {busy === 'backup' ? 'Backing up…' : 'Backup now'}
          </button>
        </div>
      </div>

      <div className="setting-section">Restore</div>
      <div className="setting-block">
        <div className="setting-note">
          Newest snapshot from a backup folder — overwrites current data. Pause collection first.
        </div>
        <div className="setting-actions">
          <input className="setting-input" placeholder="E:\Backup\TokenCalendar" value={resTarget} onChange={(e) => setResTarget(e.target.value)} />
          <button className="setting-btn" onClick={() => void pickInto(setResTarget, dirPick.restore)} title="Pick a folder">Browse</button>
          <button
            className="setting-btn"
            disabled={busy !== null || !resTarget.trim()}
            onClick={() => void run('restore')}
            title="Overwrites current data with the newest backup"
          >
            {busy === 'restore' ? 'Restoring…' : 'Restore'}
          </button>
        </div>
        {notice ? <div className="setting-note">{notice}</div> : null}
      </div>

      <div className="setting-section">Export month ({month})</div>
      <div className="setting-block">
        <div className="setting-actions">
          <button
            className="setting-btn"
            disabled={exporting}
            onClick={() => doExport('csv')}
            title="Day × agent × model; no session paths"
          >
            {exporting ? 'Exporting…' : 'Export CSV'}
          </button>
          <button
            className="setting-btn"
            disabled={exporting}
            onClick={() => doExport('json')}
            title="Day × agent × model; no session paths"
          >
            Export JSON
          </button>
        </div>
        {exportResult ? <div className="setting-note">{exportResult}</div> : null}
      </div>
    </>
  )
}

/* ---------------- Subscriptions：悬浮球总开关 + 平台绑定卡 ---------------- */

const POLL_OPTIONS = [300, 600, 900, 1800] // 秒 → 5/10/15/30 分钟
const BOOST_INTERVAL_OPTIONS = [60, 120, 180] // 秒 → 1/2/3 分钟（超出 = Custom 档）

function SubscriptionsTab() {
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  const [scan, setScan] = useState<CredentialInfo[]>([])
  const [snapshots, setSnapshots] = useState<SubscriptionSnapshot[]>([])
  const [notice, setNotice] = useState<string | null>(null)
  // boost 数字输入（spike 阈值 / 自定义间隔）：draft 态自由输入,blur/Enter 校验
  // 提交（越界回弹显示现值）;Custom 档由「值不在预设档」或显式点击 Custom 展开。
  const [spikeDraft, setSpikeDraft] = useState<string | null>(null)
  const [intervalDraft, setIntervalDraft] = useState<string | null>(null)
  const [customMode, setCustomMode] = useState(false)
  const customInputRef = useRef<HTMLInputElement | null>(null)

  useEffect(() => subscribeDesignPrefs(setDesign), [])

  // 初查 + subscription:changed 跟随（bind/unbind/轮询完成都会汇入同一事件）
  useEffect(() => {
    subscriptionService.scanCredentials().then((s) => s && setScan(s)).catch(console.error)
    subscriptionService.getSnapshots().then((s) => s && setSnapshots(s)).catch(console.error)
    let off: (() => void) | null = null
    void events.onSubscriptionChanged(() => {
      subscriptionService.getSnapshots().then((s) => s && setSnapshots(s)).catch(console.error)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])

  // 轮询间隔运行时值恢复（prefs 装载完成后一次性下发）
  useEffect(() => {
    const secs = design.subscriptionPollSecs
    if (secs) subscriptionService.applyPollSecs(secs)
  }, [design.subscriptionPollSecs])

  // 悬浮球总开关（㉚,「顶栏 Orbit 钮 / 表盘右键关闭后设置页
  // 复选框不跟随,反过来也不 check」）：状态**直接取可见性单一源**——Rust
  // visibility.rs 的 orb_visible,get_visibility 初查 + orb-visibility-changed
  // 广播跟随;顶栏 Orbit 钮、托盘勾选、orb 自身右键「Hide orb」三条路径都汇入
  // 那里（与顶栏按钮完全同款消费方式）。
  // 旧版读 designPrefs.orbEnabled（只在勾选时写）⇒ 别处改了这里看不,是单向的;
  // 该键随之退役——可见性的持久化在 window-state.json 的 orb_visible（Rust 唯一源）,
  // 前端不需要第二份镜像。
  const [orbVisible, setOrbVisible] = useState(false)
  useEffect(() => {
    void windowService
      .getVisibility()
      .then((v) => {
        if (v) setOrbVisible(Boolean(v.orb))
      })
      .catch(() => {})
    let off: (() => void) | null = null
    void events.onOrbVisibilityChanged(setOrbVisible).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])
  const toggleOrb = (next: boolean) => {
    // 开 = showOrb;关 = hideOrb（停轮询与绑定数据不在此处——绑定管理独立于呈现,
    // 轮询始终低频,实施口径:总开关只控呈现,凭据绑定在平台卡卸载）
    windowService[next ? 'showOrb' : 'hideOrb']().catch(console.error)
  }

  const setPoll = (secs: number) => {
    setDesignPrefs({ subscriptionPollSecs: secs })
    subscriptionService.setPollSecs(secs).catch(console.error)
  }

  const doBind = (platform: CredentialInfo['platform']) => {
    setNotice(null)
    subscriptionService.bind(platform).then(() => {
      setNotice(`${platform} bound — first refresh in flight`)
    }).catch((e) => {
      setNotice(String(e))
      console.error(e)
    })
  }

  const doUnbind = (platform: CredentialInfo['platform']) => {
    setNotice(null)
    subscriptionService.unbind(platform).then(() => {
      setNotice(`${platform} unbound`)
    }).catch((e) => {
      setNotice(String(e))
      console.error(e)
    })
  }

  const doRefresh = () => {
    setNotice(null)
    subscriptionService.refreshNow().catch(console.error)
  }

  // boost 配置：designPrefs 持久化 + 运行时下发双写（与 setPoll 同款）。
  // setDesignPrefs 同步生效,随后的 getDesignPrefs 已含本次 patch。
  const setBoost = (patch: Partial<DesignPrefs>) => {
    setDesignPrefs(patch)
    subscriptionService.setBoostConfig(orbBoostConfig(getDesignPrefs())).catch(console.error)
  }
  const boostOn = design.orbBoostEnabled ?? false
  const spikeOn = design.orbBoostSpikeEnabled ?? true
  const lowOn = design.orbBoostLowEnabled ?? false
  const intervalVal = design.orbBoostIntervalSecs ?? 60
  const customInterval = customMode || !BOOST_INTERVAL_OPTIONS.includes(intervalVal)
  const commitSpike = () => {
    if (spikeDraft == null) return
    const v = Number(spikeDraft)
    if (Number.isInteger(v) && v >= 5 && v <= 50) setBoost({ orbBoostSpikePct: v })
    setSpikeDraft(null) // 非法输入回弹显示现值
  }
  const commitInterval = () => {
    if (intervalDraft == null) return
    const v = Number(intervalDraft)
    if (Number.isInteger(v) && v >= 30 && v <= 240) setBoost({ orbBoostIntervalSecs: v })
    setIntervalDraft(null)
  }
  const boostIntervalLabel = customInterval ? `${intervalVal}s` : `${intervalVal / 60}m`

  const snapOf = (platform: string) => snapshots.find((s) => s.platform === platform)
  const boundOf = (platform: string) => {
    const s = snapOf(platform)
    return !!s && s.status !== 'idle'
  }

  return (
    <>
      <div className="setting-section">Floating orb</div>
      <div className="setting-block">
        <ToggleRow
          label="Show floating orb"
          title="Desktop quota orb; togglable from titlebar or tray"
          checked={orbVisible}
          onChange={toggleOrb}
        />
        <div className="setting-row">
          <span title="How often quota is polled">Refresh interval</span>
          <div className="setting-seg">
            {POLL_OPTIONS.map((s) => (
              <button
                key={s}
                className={`setting-seg-btn${(design.subscriptionPollSecs ?? 300) === s ? ' is-active' : ''}`}
                title={`Poll every ${s / 60} minutes`}
                onClick={() => setPoll(s)}
              >
                {s / 60}m
              </button>
            ))}
          </div>
        </div>
        {/* Standby monitoring：主轮询自适应退档——读数无变化逐档
            放慢（封顶 30 分钟）,任何变化立即回设置档;待机中悬浮球整体减淡。
            默认开（退档是收敛行为,与 boost 提频需显式授权相反口径）。*/}
        <ToggleRow
          label="Standby monitoring"
          title="Slow polling (max 30 min) while unchanged; dims the orb"
          checked={design.orbIdleEnabled ?? true}
          onChange={(v) => {
            setDesignPrefs({ orbIdleEnabled: v })
            subscriptionService.setIdleEnabled(v).catch(console.error)
          }}
        />
        <div className="setting-actions">
          <button
            className="setting-btn"
            onClick={doRefresh}
            title="Fetch quota for all bound platforms now"
          >
            Refresh now
          </button>
        </div>
        {notice ? <div className="setting-note">{notice}</div> : null}
      </div>

      {/* Boost monitoring：订阅 boost 补充路由。
          两触发条件 OR;退出 = 滚动 5 样本窗口内所有已启用条件不再成立。
          间隔档 1/2/3 分钟 + Custom（30〜240 秒,与 Rust clamp_cfg 同域）。*/}
      <div className="setting-section">Boost monitoring</div>
      <div className="setting-block">
        <ToggleRow
          label="Enable boost monitoring"
          title="Faster orb refresh while usage spikes or quota runs low"
          checked={boostOn}
          onChange={(v) => setBoost({ orbBoostEnabled: v })}
        />
        <ToggleRow
          label="Trigger: usage spike"
          title="Triggers when a poll interval consumes more than the threshold"
          checked={spikeOn}
          disabled={!boostOn}
          onChange={(v) => setBoost({ orbBoostSpikeEnabled: v })}
        />
        <div className="setting-row">
          <span title="Consumed share of the 5h quota per poll interval">Spike threshold</span>
          <input
            className="setting-num"
            title="Percent consumed per poll interval (5-50)"
            type="number" min={5} max={50} step={1} aria-label="Spike threshold percent"
            value={spikeDraft ?? String(design.orbBoostSpikePct ?? 10)}
            disabled={!boostOn || !spikeOn}
            onChange={(e) => setSpikeDraft(e.target.value)}
            onBlur={commitSpike}
            onKeyDown={(e) => { if (e.key === 'Enter') (e.target as HTMLInputElement).blur() }}
          />
          <span className="setting-unit">%</span>
        </div>
        <ToggleRow
          label="Trigger: low 5h remaining"
          title="Triggers when 5h remaining is at or below the threshold"
          checked={lowOn}
          disabled={!boostOn}
          onChange={(v) => setBoost({ orbBoostLowEnabled: v })}
        />
        <div className="setting-row">
          <span title="5h remaining at or below this value">Low threshold</span>
          <div className="setting-seg">
            {[20, 30, 40].map((v) => (
              <button
                key={v}
                className={`setting-seg-btn${(design.orbBoostLowPct ?? 30) === v ? ' is-active' : ''}`}
                title={`Trigger at ${v}% remaining`}
                disabled={!boostOn || !lowOn}
                onClick={() => setBoost({ orbBoostLowPct: v })}
              >
                {v}%
              </button>
            ))}
          </div>
        </div>
        <div className="setting-row">
          <span title="How often the orb polls while boosting">Boost interval</span>
          <div className="setting-seg">
            {BOOST_INTERVAL_OPTIONS.map((s) => (
              <button
                key={s}
                className={`setting-seg-btn${!customInterval && intervalVal === s ? ' is-active' : ''}`}
                title={`Boost poll every ${s / 60} min`}
                disabled={!boostOn}
                onClick={() => { setCustomMode(false); setBoost({ orbBoostIntervalSecs: s }) }}
              >
                {s / 60}m
              </button>
            ))}
            <button
              className={`setting-seg-btn${customInterval ? ' is-active' : ''}`}
              title="Set a custom interval in seconds"
              disabled={!boostOn}
              onClick={() => { setCustomMode(true); window.setTimeout(() => customInputRef.current?.focus(), 0) }}
            >
              Custom
            </button>
          </div>
        </div>
        {customInterval ? (
          <div className="setting-row">
            <span title="Interval between boost polls">Custom seconds</span>
            <input
              ref={customInputRef}
              className="setting-num"
              title="Seconds between boost polls (30-240)"
              type="number" min={30} max={240} step={1} aria-label="Custom boost interval seconds"
              value={intervalDraft ?? String(intervalVal)}
              disabled={!boostOn}
              onChange={(e) => setIntervalDraft(e.target.value)}
              onBlur={commitInterval}
              onKeyDown={(e) => { if (e.key === 'Enter') (e.target as HTMLInputElement).blur() }}
            />
            <span className="setting-unit">s</span>
          </div>
        ) : null}
        <div className="setting-note">
          While boosting, the orb polls a separate lane every {boostIntervalLabel} and shows the refresh sweep; it exits after 5 samples of low usage.
        </div>
      </div>

      <div className="setting-section">Platforms</div>
      <div className="setting-block">
        {scan.map((info) => {
          const snap = snapOf(info.platform)
          const bound = boundOf(info.platform)
          const statusText =
            snap?.status === 'ok'
              ? `Active — ${snap.windows.map((w) => `${w.kind} ${(100 - w.used_percent).toFixed(0)}% left`).join(', ')}`
              : snap?.status === 'auth_failed'
                ? 'Credentials expired — run the agent CLI to refresh'
                : snap?.status === 'plan_inactive'
                  ? 'Subscription inactive — resumes after renewal'
                  : snap?.status === 'rate_limited'
                    ? 'Rate limited — retrying automatically'
                    : snap?.status === 'network_failed'
                      ? 'Network error — showing last known data'
                      : 'Not bound'
          return (
            <div className="setting-row" key={info.platform}>
              <span>
                <strong style={{ textTransform: 'capitalize' }}>{info.platform}</strong>
                <br />
                <small>
                  {!info.present
                    ? 'No local credentials found'
                    : !info.parseable
                      ? 'Credential file unreadable'
                      : `${info.account_hint ?? 'Local account'} · ${statusText}`}
                </small>
              </span>
              <div className="setting-seg">
                {bound ? (
                  <button
                    className="setting-seg-btn"
                    onClick={() => doUnbind(info.platform)}
                    title="Stop polling this platform"
                  >
                    Unbind
                  </button>
                ) : (
                  <button
                    className="setting-seg-btn"
                    disabled={!info.present || !info.parseable}
                    onClick={() => doBind(info.platform)}
                    title="Read local credentials and start polling"
                  >
                    Bind
                  </button>
                )}
              </div>
            </div>
          )
        })}
        <div className="setting-note">
          Credentials stay local — read from agent CLI files on this machine, never uploaded.
        </div>
      </div>
    </>
  )
}

/* ---------------- About：版本与更新（更新源 = 公开仓 Release） ---------------- */

/** 版本与更新：检查更新（签名校验）/ 自动更新开关 / 版本号按钮跳 Releases。
 * 状态不写 prefs：检查结果属会话态，重开面板重新检查即可。 */
function AboutTab() {
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  useEffect(() => subscribeDesignPrefs(setDesign), [])
  const [version, setVersion] = useState<string | null>(null)
  const [checking, setChecking] = useState(false)
  const [installing, setInstalling] = useState(false)
  const [result, setResult] = useState<UpdateCheck | null>(null)
  const [progress, setProgress] = useState<UpdateProgress | null>(null)
  const [note, setNote] = useState<string | null>(null)

  useEffect(() => {
    updateService.currentVersion().then((v) => v && setVersion(v)).catch(console.error)
  }, [])

  const doCheck = async () => {
    setChecking(true)
    setNote(null)
    setProgress(null)
    const r = await updateService.checkForUpdate()
    setResult(r)
    setChecking(false)
  }

  const doInstall = async (install: (cb: (p: UpdateProgress) => void) => Promise<void>) => {
    setInstalling(true)
    setNote(null)
    try {
      await install(setProgress)
      // Windows 上安装器接管后应用即退出；能走到这里说明安装器已启动。
      setNote('Installer launched — the app closes while the update is applied. Reopen it afterwards.')
    } catch (e) {
      setNote(`Install failed: ${e instanceof Error ? e.message : String(e)}`)
      setInstalling(false)
    }
  }

  const statusText = (): string => {
    if (checking) return 'Checking for updates…'
    if (installing) {
      if (!progress || progress.total === 0) return 'Downloading update…'
      const pct = Math.min(100, Math.round((progress.downloaded / progress.total) * 100))
      return progress.finished ? 'Download complete — starting the installer…' : `Downloading update… ${pct}%`
    }
    if (!result) return 'Updates are served from the public repository releases page.'
    switch (result.status) {
      case 'unavailable':
        return 'Update check is only available in the desktop app.'
      case 'up-to-date':
        return `You're up to date — current v${result.current}.`
      case 'available':
        return result.canInstall
          ? `Version ${result.version} is available — current v${result.current}.`
          : `Version ${result.version} is available — current v${result.current}. Dev build: install is disabled here, download it from the releases page.`
      case 'error':
        return result.message
    }
  }

  // 闭包内不做窄化：先取出可安装结果（available 且非 dev 构建）。
  const installable = result?.status === 'available' && result.canInstall ? result : null

  return (
    <>
      <div className="setting-section">Updates</div>
      <div className="setting-block">
        <div className="setting-row">
          <span title="Current app version">Version</span>
          <button
            className="setting-btn"
            onClick={() => {
              updateService.openReleasesPage().catch(console.error)
            }}
            title="Open the releases page (manual download)"
          >
            {version ? `v${version}` : '—'}
          </button>
        </div>
        <div className="setting-note">{statusText()}</div>
        <div className="setting-actions">
          <button
            className="setting-btn"
            disabled={checking || installing}
            onClick={() => {
              doCheck().catch(console.error)
            }}
            title="Compare with the latest release"
          >
            Check for updates
          </button>
          {installable ? (
            <button
              className="setting-btn is-active"
              disabled={installing}
              onClick={() => {
                doInstall(installable.install).catch(console.error)
              }}
              title="Download, verify the signature and run the installer"
            >
              Download &amp; install
            </button>
          ) : null}
        </div>
        {result?.status === 'available' && result.notes ? <div className="setting-note">{result.notes}</div> : null}
        {note ? <div className="setting-note">{note}</div> : null}

        <ToggleRow
          label="Auto update"
          title="Check and install new versions at startup"
          checked={design.autoUpdate}
          onChange={(v) => setDesignPrefs({ autoUpdate: v })}
        />
      </div>
    </>
  )
}

function BackIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M19 12H5" />
      <path d="m12 19-7-7 7-7" />
    </svg>
  )
}
