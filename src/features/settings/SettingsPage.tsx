// SettingsPage：设置页（自右缘窄面板改为内容区全幅独立视图）。
// 三子 tab：General（行为）/ Appearance（美化）/ Data（导出）。
// 外观 tab 的挂件美化组：预定义色板 + react-colorful 取色弹层 + hex 输入 +
// 恢复默认；色与透明度分离（调色盘只改色相，alpha 仍走 bgOpacity 滑条）。
// 浮层铁律：取色弹层 DOM 常驻不卸载——隐藏=移出视口+visibility，
// 禁止条件渲染（透明 WebView2 条件卸载留脏像素残影）。
import { useEffect, useRef, useState } from 'react'
// react-colorful 自注入样式（运行时 <style> 注入，无独立 CSS 文件可 import）。
import { HexColorPicker, HexColorInput } from 'react-colorful'
import { collectorService, dataService, events, exportService, subscriptionService, windowService, type CredentialInfo, type ExportResult, type DataInfo, type SubscriptionSnapshot } from '../../services'
import { currentMonth } from '../../lib/time'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs, SIZE_PRESETS, RADIUS_PRESETS, type DesignPrefs, type SizePreset, type WeekStart } from './designPrefs'
import { deriveWidgetTheme } from './widgetTheme'
import './settings.css'

type SettingsTab = 'general' | 'appearance' | 'data' | 'subscriptions'

const TABS: { id: SettingsTab; label: string }[] = [
  { id: 'general', label: 'General' },
  { id: 'appearance', label: 'Appearance' },
  { id: 'data', label: 'Data' },
  { id: 'subscriptions', label: 'Subscriptions' },
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
 *  首项=跟随 scheme 默认（A 圆点）。 */
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
      </div>
    </div>
  )
}

/* ---------------- General：行为类设置（自右缘面板平移） ---------------- */

function GeneralTab() {
  const [paused, setPaused] = useState(false)
  const [snapEnabled, setSnapEnabled] = useState(false)
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)

  // Pull the real backend state every time the tab mounts — the tray
  // checkboxes can change while the page was hidden.
  useEffect(() => {
    collectorService.getPaused().then((p) => p !== null && setPaused(p)).catch(console.error)
    collectorService.getSnapEnabled().then((v) => v !== null && setSnapEnabled(v)).catch(console.error)
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

  const togglePause = () => {
    const next = !paused
    setPaused(next)
    collectorService.setPaused(next).catch(console.error)
  }

  const toggleSnap = () => {
    const next = !snapEnabled
    setSnapEnabled(next)
    collectorService.setSnapEnabled(next).catch(console.error)
  }

  return (
    <>
      <div className="setting-section">Collection</div>
      <label className="setting-row">
        <span>Pause collection</span>
        <input type="checkbox" checked={paused} onChange={togglePause} />
      </label>
      <div className="setting-note">Paused data is kept; collection resumes incrementally.</div>

      <div className="setting-section">Matrix</div>
      <div className="setting-row">
        <span>Max rows</span>
        <div className="setting-seg">
          {[8, 12, 15, 20, 0].map((n) => (
            <button
              key={n}
              className={`setting-seg-btn${design.matrixMaxRows === n ? ' is-active' : ''}`}
              onClick={() => setDesignPrefs({ matrixMaxRows: n })}
            >
              {n === 0 ? 'All' : n}
            </button>
          ))}
        </div>
      </div>
      <div className="setting-note">
        Cap how many agent/model rows the main heatmap shows (highest totals kept;
        the rest collapse into a summary line). Lower it if the chart panel below gets
        squeezed on small windows.
      </div>

      <div className="setting-section">Widget</div>
      <div className="setting-row">
        <span>Widget size</span>
        <div className="setting-seg">
          {(['large', 'medium', 'small'] as SizePreset[]).map((p) => (
            <button
              key={p}
              className={`setting-seg-btn${design.sizePreset === p ? ' is-active' : ''}`}
              onClick={() => setSizePreset(p)}
            >
              {p[0].toUpperCase() + p.slice(1)}
            </button>
          ))}
        </div>
      </div>
      <div className="setting-row">
        <span>Week starts on</span>
        <div className="setting-seg">
          {(['sunday', 'monday'] as WeekStart[]).map((d) => (
            <button
              key={d}
              className={`setting-seg-btn${(design.weekStart ?? 'sunday') === d ? ' is-active' : ''}`}
              onClick={() => setDesignPrefs({ weekStart: d })}
            >
              {d === 'sunday' ? 'Sunday' : 'Monday'}
            </button>
          ))}
        </div>
      </div>
      <div className="setting-note">
        Rows run from the start day to the day before it; columns are always full weeks —
        the rightmost column is the current week (upcoming days shown as empty gray),
        the leftmost is the closest full week to one year ago.
      </div>
      <label className="setting-row">
        <span>Lock widget</span>
        <input
          type="checkbox"
          checked={design.locked}
          onChange={(e) => setDesignPrefs({ locked: e.target.checked })}
        />
      </label>
      <label className="setting-row">
        <span>Lock aspect ratio</span>
        <input
          type="checkbox"
          checked={design.lockAspectRatio}
          onChange={(e) => setDesignPrefs({ lockAspectRatio: e.target.checked })}
        />
      </label>
      <label className="setting-row">
        <span>Snap to grid</span>
        <input type="checkbox" checked={snapEnabled} onChange={toggleSnap} />
      </label>
      <div className="setting-note">
        When enabled, releasing the widget snaps its top-right corner to the nearest
        desktop grid vertex (10px); size presets keep that vertex anchored.
      </div>
      <div className="setting-actions">
        <button className="setting-btn" onClick={resetWidgetSize}>
          Reset widget size
        </button>
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
   *  锚定本行正下方（四个槽位互不串位）。浮层 DOM 常驻不卸载（铁律），
   *  closed = visibility + pointer-events，open 才恢复交互。 */
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
            <button className="setting-btn" onClick={() => s.set(undefined)}>
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
          <span>Radius scheme</span>
          <div className="setting-seg">
            {(['small', 'medium', 'large'] as const).map((r) => (
              <button
                key={r}
                className={`setting-seg-btn${(design.radiusScheme ?? 'large') === r ? ' is-active' : ''}`}
                onClick={() => setDesignPrefs({ radiusScheme: r })}
              >
                {r === 'small' ? `Small (${RADIUS_PRESETS.small.panel}px)` : r === 'medium' ? `Medium (${RADIUS_PRESETS.medium.panel}px)` : `Large (${RADIUS_PRESETS.large.panel}px)`}
              </button>
            ))}
          </div>
        </div>
        <div className="setting-note">
          One scheme, two aligned surfaces: the main panel and the widget always share the
          same radius. Glass-material windows are clipped by the system (~8px) regardless
          of this scheme — pick Small to match that look everywhere.
        </div>
      </div>

      <div className="setting-section">Widget appearance</div>
      <div className="setting-block">
        <div className="setting-row">
          <span>Card color</span>
          <span className="setting-value">{custom ? custom.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('card', WIDGET_SWATCHES)}

        <div className="setting-row">
          <span>Background opacity</span>
          <input
            type="range"
            min={0.2}
            max={1}
            step={0.05}
            value={design.bgOpacity}
            onChange={(e) => setDesignPrefs({ bgOpacity: Number(e.target.value) })}
          />
          <span className="setting-value">{Math.round(design.bgOpacity * 100)}%</span>
        </div>
        <div className="setting-row">
          <span>Heatmap opacity</span>
          <input
            type="range"
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
        />
        <div className="setting-note">
          SPIKE (experimental): desktop glass behind the card. Mica needs Windows 11 (recommended);
          Acrylic is real-time blur and may lag while dragging. Losing focus dims the material —
          that is system behavior; the card compensates by raising its own background opacity.
          Lower "Background opacity" to see the glass through the card.
        </div>

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
          <button className="setting-btn" onClick={() => setDesignPrefs({ widgetCardBg: undefined })} disabled={!custom}>
            Restore default colors
          </button>
        </div>
        <div className="setting-note">
          Custom color applies to both light &amp; dark system themes (treated as explicit intent);
          text/border/zero-cell shades adapt automatically. Opacity sliders stay independent of color.
          Changes sync to the widget instantly.
        </div>
      </div>

      <div className="setting-section">Main interface</div>
      <div className="setting-block">
        <div className="setting-row">
          <span>Titlebar color</span>
          <span className="setting-value">{design.titlebarBg ? design.titlebarBg.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('titlebar', MAIN_SWATCHES)}
        <div className="setting-row">
          <span>Titlebar opacity</span>
          <input
            type="range"
            min={0.2}
            max={1}
            step={0.05}
            value={design.titlebarAlpha ?? 0.8}
            onChange={(e) => setDesignPrefs({ titlebarAlpha: Number(e.target.value) })}
          />
          <span className="setting-value">{Math.round((design.titlebarAlpha ?? 0.8) * 100)}%</span>
        </div>

        <div className="setting-row">
          <span>Main panel color</span>
          <span className="setting-value">{design.panelBg ? design.panelBg.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('panel', MAIN_SWATCHES)}

        <div className="setting-row">
          <span>Border color</span>
          <span className="setting-value">{design.borderColor ? design.borderColor.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('border', MAIN_SWATCHES)}

        <MaterialRow
          value={design.mainMaterial}
          onChange={(m) => setDesignPrefs({ mainMaterial: m })}
        />
        <div className="setting-note">
          SPIKE (experimental): system glass behind the main window. While active the window
          switches to system (DWM) round corners and the panel lets the glass through; turning it
          off restores the floating-card look. Losing focus dims the material — system behavior.
          Unsupported systems fall back to Off automatically.
        </div>

        <div className="setting-actions">
          <button
            className="setting-btn"
            disabled={!design.titlebarBg && design.titlebarAlpha === undefined && !design.panelBg && !design.borderColor}
            onClick={() => setDesignPrefs({ titlebarBg: undefined, titlebarAlpha: undefined, panelBg: undefined, borderColor: undefined })}
          >
            Restore default colors
          </button>
        </div>
        <div className="setting-note">
          Applies to the main window only — the widget stays untouched (they share no theme
          variables). Maximized state keeps solid background rules; opacity slider affects the
          floating titlebar only.
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

const MATERIAL_OPTIONS: { label: string; value: 'mica' | 'acrylic' | null }[] = [
  { label: 'Off', value: null },
  { label: 'Mica', value: 'mica' },
  { label: 'Acrylic', value: 'acrylic' },
]

function MaterialRow({
  value,
  onChange,
}: {
  value: 'mica' | 'acrylic' | undefined
  onChange(m: 'mica' | 'acrylic' | undefined): void
}) {
  return (
    <div className="setting-row">
      <span>Glass material</span>
      <div className="setting-seg">
        {MATERIAL_OPTIONS.map((o) => (
          <button
            key={o.label}
            className={`setting-seg-btn${(value ?? null) === o.value ? ' is-active' : ''}`}
            onClick={() => onChange(o.value ?? undefined)}
          >
            {o.label}
          </button>
        ))}
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
   *  即时入库（毫秒级,无 30s 轮询等待）。成功后刷新 storage 概要。 */
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
        <button className="setting-btn" onClick={() => void dataService.openDataDir()}>Open data folder</button>
      </div>

      <div className="setting-section">CodeBuddy credit import</div>
      <div className="setting-note">
        Import an official CodeBuddy credit export (.xlsx) to map request IDs to models —
        rows otherwise show as "unknown". The file is validated, copied into the imports
        folder and applied immediately; repeat imports are idempotent.
      </div>
      <div className="setting-actions">
        <button className="setting-btn" disabled={importing} onClick={doImport}>
          {importing ? 'Importing…' : 'Import export file…'}
        </button>
      </div>
      {importNote ? <div className="setting-note">{importNote}</div> : null}
      <label className="setting-row">
        <span>Show credit card in Insights</span>
        <input
          type="checkbox"
          checked={design.insightsCredit}
          onChange={(e) => setDesignPrefs({ insightsCredit: e.target.checked })}
        />
      </label>
      <div className="setting-note">
        Show the tokens × credit card at the bottom of the Chart view. Off by default —
        enable it once an export has been imported.
      </div>

      <div className="setting-section">Migrate data root</div>
      <div className="setting-note">Move cache / prefs / imports / exports to a new directory (writes a pointer, restart to apply; the old directory is left for you to handle). Pause collection first (General → Pause).</div>
      <div className="setting-actions">
        <input className="setting-input" placeholder="D:\Data\TokenCalendar" value={migTarget} onChange={(e) => setMigTarget(e.target.value)} />
        <button className="setting-btn" onClick={() => void pickInto(setMigTarget, dirPick.migrate)}>Browse</button>
        <button className="setting-btn" disabled={busy !== null || !migTarget.trim()} onClick={() => void run('migrate')}>
          {busy === 'migrate' ? 'Migrating…' : 'Migrate'}
        </button>
      </div>

      <div className="setting-section">Backup</div>
      <div className="setting-note">Consistent snapshot (SQLite VACUUM INTO) + prefs + import files → target directory.</div>
      <div className="setting-actions">
        <input className="setting-input" placeholder="E:\Backup\TokenCalendar" value={bakTarget} onChange={(e) => setBakTarget(e.target.value)} />
        <button className="setting-btn" onClick={() => void pickInto(setBakTarget, dirPick.backup)}>Browse</button>
        <button className="setting-btn" disabled={busy !== null || !bakTarget.trim()} onClick={() => void run('backup')}>
          {busy === 'backup' ? 'Backing up…' : 'Backup now'}
        </button>
      </div>

      <div className="setting-section">Restore</div>
      <div className="setting-note">Restore the newest snapshot + prefs from a backup directory (overwrites current data; pause collection first).</div>
      <div className="setting-actions">
        <input className="setting-input" placeholder="E:\Backup\TokenCalendar" value={resTarget} onChange={(e) => setResTarget(e.target.value)} />
        <button className="setting-btn" onClick={() => void pickInto(setResTarget, dirPick.restore)}>Browse</button>
        <button className="setting-btn" disabled={busy !== null || !resTarget.trim()} onClick={() => void run('restore')}>
          {busy === 'restore' ? 'Restoring…' : 'Restore'}
        </button>
      </div>
      {notice ? <div className="setting-note">{notice}</div> : null}

      <div className="setting-section">Export month ({month})</div>
      <div className="setting-actions">
        <button className="setting-btn" disabled={exporting} onClick={() => doExport('csv')}>
          {exporting ? 'Exporting…' : 'Export CSV'}
        </button>
        <button className="setting-btn" disabled={exporting} onClick={() => doExport('json')}>
          Export JSON
        </button>
      </div>
      {exportResult ? <div className="setting-note">{exportResult}</div> : null}
      <div className="setting-note">Exports exclude session/project paths; day × agent × model only.</div>
    </>
  )
}

/* ---------------- Subscriptions：悬浮球总开关 + 平台绑定卡 ---------------- */

const POLL_OPTIONS = [300, 600, 900, 1800] // 秒 → 5/10/15/30 分钟

function SubscriptionsTab() {
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  const [scan, setScan] = useState<CredentialInfo[]>([])
  const [snapshots, setSnapshots] = useState<SubscriptionSnapshot[]>([])
  const [notice, setNotice] = useState<string | null>(null)

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

  const orbEnabled = design.orbEnabled ?? false
  const toggleOrbEnabled = () => {
    const next = !orbEnabled
    setDesignPrefs({ orbEnabled: next })
    // 开 = 显示 orb 窗口;关 = 隐藏（停轮询与绑定数据不在此处——绑定管理独立于呈现,
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

  const snapOf = (platform: string) => snapshots.find((s) => s.platform === platform)
  const boundOf = (platform: string) => {
    const s = snapOf(platform)
    return !!s && s.status !== 'idle'
  }

  return (
    <>
      <div className="setting-section">Floating orb</div>
      <label className="setting-row">
        <span>Show floating orb</span>
        <input type="checkbox" checked={orbEnabled} onChange={toggleOrbEnabled} />
      </label>
      <div className="setting-note">
        Desktop orb shows your AI subscription quota at a glance. The orb window can
        also be toggled from the titlebar or tray.
      </div>
      <div className="setting-row">
        <span>Refresh interval</span>
        <div className="setting-seg">
          {POLL_OPTIONS.map((s) => (
            <button
              key={s}
              className={`setting-seg-btn${(design.subscriptionPollSecs ?? 300) === s ? ' is-active' : ''}`}
              onClick={() => setPoll(s)}
            >
              {s / 60}m
            </button>
          ))}
        </div>
      </div>
      <div className="setting-actions">
        <button className="setting-btn" onClick={doRefresh}>Refresh now</button>
      </div>
      {notice ? <div className="setting-note">{notice}</div> : null}

      <div className="setting-section">Platforms</div>
      {scan.map((info) => {
        const snap = snapOf(info.platform)
        const bound = boundOf(info.platform)
        const statusText =
          snap?.status === 'ok'
            ? `Active — ${snap.windows.map((w) => `${w.kind} ${(100 - w.used_percent).toFixed(0)}% left`).join(', ')}`
            : snap?.status === 'auth_failed'
              ? 'Credentials expired — sign in to the agent CLI again'
              : snap?.status === 'plan_inactive'
                ? 'Subscription inactive — resumes after renewal'
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
                <button className="setting-seg-btn" onClick={() => doUnbind(info.platform)}>Unbind</button>
              ) : (
                <button
                  className="setting-seg-btn"
                  disabled={!info.present || !info.parseable}
                  onClick={() => doBind(info.platform)}
                >
                  Bind
                </button>
              )}
            </div>
          </div>
        )
      })}
      <div className="setting-note">
        Credentials are read locally from the agent CLI files on this machine only —
        never uploaded, and token refresh never writes back to those files.
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
