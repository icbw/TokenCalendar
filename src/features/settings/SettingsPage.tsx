// SettingsPage：设置页（内容区全幅独立视图）。
// 子 tab：General（行为）/ Appearance（美化）/ Projects（项目管理）/ Data（导出）/
// Subscriptions（订阅额度）/ About（版本与更新）。
// 外观 tab 的挂件美化组：预定义色板 + react-colorful 取色弹层 + hex 输入 +
// 恢复默认；色与透明度分离（调色盘只改色相，alpha 仍走 bgOpacity 滑条）。
// 浮层约束：取色弹层 DOM 常驻不卸载——隐藏=移出视口+visibility，
// 禁止条件渲染（透明 WebView2 条件卸载留脏像素残影）。
import { useEffect, useRef, useState } from 'react'
// react-colorful 自注入样式（运行时 <style> 注入，无独立 CSS 文件可 import）。
import { HexColorPicker, HexColorInput } from 'react-colorful'
import { autostartService, collectorService, dataService, events, exportService, subscriptionService, updateService, windowService, type AutostartInfo, type CredentialInfo, type EstimatorState, type ExportResult, type DataInfo, type SubscriptionSnapshot, type ReadyUpdate, type UpdateCheck, type UpdateProgress } from '../../services'
import { currentMonth } from '../../lib/time'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs, SIZE_PRESETS, RADIUS_PRESETS, SUBSCRIPTION_FETCH_PCT, MONTHLY_USD_MAX, ORB_MESSAGE_MODELS_MAX, applySubscriptionFetchPolicy, subscriptionFetchPct, subscriptionTightenLow, type DesignPrefs, type SizePreset, type WeekStart } from './designPrefs'
import { deriveWidgetTheme } from './widgetTheme'
import { TIMELINE_BAR_ALPHA, TIMELINE_BG_ALPHA, TIMELINE_CELL_ALPHA } from '../timeline/timelineConfig'
import ProjectManager from '../projects/ProjectManager'
import type { MessageBudget } from '../../services/subscriptionService'
import { shortModelName } from '../orb/messageBudget'
import './settings.css'

export type SettingsTab = 'general' | 'appearance' | 'projects' | 'data' | 'subscriptions' | 'about'

/** 页签（hint = hover 提示：一句话说明本页管什么）。 */
const TABS: { id: SettingsTab; label: string; hint: string }[] = [
  { id: 'general', label: 'General', hint: 'Collection, matrix and widget behavior' },
  { id: 'appearance', label: 'Appearance', hint: 'Colors, glass material and corner radius' },
  { id: 'projects', label: 'Projects', hint: 'Rename, hide and merge project folders; Scratch rule' },
  { id: 'data', label: 'Data', hint: 'Storage, backup, restore and export' },
  { id: 'subscriptions', label: 'Subscriptions', hint: 'Quota readings, standby and binding' },
  { id: 'about', label: 'About', hint: 'Version and updates' },
]

/** 策展预定义色板：8 色，点击即用；首项=跟随 scheme 默认。 */
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

/** 时间轴主题色色板：强调色而非底色——中等饱和度,亮 / 暗两主题下与卡片底混色都可读。
 * 首项 = 跟随全局 accent。 */
const TIMELINE_ACCENT_SWATCHES: { label: string; hex: string | null }[] = [
  { label: 'Default', hex: null },
  { label: 'Indigo', hex: '#6366f1' },
  { label: 'Violet', hex: '#8b5cf6' },
  { label: 'Rose', hex: '#e11d48' },
  { label: 'Amber', hex: '#d97706' },
  { label: 'Emerald', hex: '#059669' },
  { label: 'Teal', hex: '#0d9488' },
  { label: 'Sky', hex: '#0284c7' },
  { label: 'Slate', hex: '#64748b' },
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
      <div className={`settings-content${tab === 'projects' ? ' is-wide' : ''}`} role="tabpanel">
        {tab === 'general' && <GeneralTab />}
        {tab === 'appearance' && <AppearanceTab />}
        {tab === 'projects' && <ProjectManager active />}
        {tab === 'data' && <DataTab />}
        {tab === 'subscriptions' && <SubscriptionsTab />}
        {tab === 'about' && <AboutTab />}
      </div>
    </div>
  )
}

/* ---------------- General：行为类设置 ---------------- */

/** 采集频率五档（与 Rust collector:POLL_INTERVAL_CHOICES_SECS 同域）。 */
const COLLECT_INTERVAL_CHOICES = [
  { secs: 30, label: '30s' },
  { secs: 60, label: '1m' },
  { secs: 120, label: '2m' },
  { secs: 180, label: '3m' },
  { secs: 300, label: '5m' },
] as const

function GeneralTab() {
  const [paused, setPaused] = useState(false)
  // 采集频率:运行时值在 Rust,进 tab 现查;未就绪时按偏好 / 默认 30s 显示。
  const [collectSecs, setCollectSecs] = useState<number>(() => getDesignPrefs().collectIntervalSecs ?? 30)
  const [snapEnabled, setSnapEnabled] = useState(false)
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  // 开机自启：状态单一源 = 系统启动项（Rust 侧插件读写），前端不存镜像——
  // 与 paused/snap 同款：每次进 tab 现查，勾选直接写系统。
  const [autostart, setAutostart] = useState<AutostartInfo | null>(null)
  const [autostartNote, setAutostartNote] = useState<string | null>(null)
  // 时间轴总开关：与 orb 开关同款——状态直接取可见性单一源
  // （Rust visibility.rs 的 timeline_visible），get_visibility 初查 +
  // timeline-visibility-changed 广播跟随;托盘勾选、窗口关闭钮都汇入那里，
  // 前端不存第二份镜像（可见性持久化在 window-state.json）。
  const [timelineVisible, setTimelineVisible] = useState(false)

  // Pull the real backend state every time the tab mounts — the tray
  // checkboxes can change while the page was hidden.
  useEffect(() => {
    collectorService.getPaused().then((p) => p !== null && setPaused(p)).catch(console.error)
    collectorService.getCollectInterval().then((r) => r && setCollectSecs(r.secs)).catch(console.error)
    collectorService.getSnapEnabled().then((v) => v !== null && setSnapEnabled(v)).catch(console.error)
    autostartService.getAutostart().then((v) => v !== null && setAutostart(v)).catch(console.error)
    windowService.getVisibility().then((v) => v && setTimelineVisible(Boolean(v.timeline))).catch(console.error)
  }, [])
  useEffect(() => {
    let off: (() => void) | null = null
    void events.onTimelineVisibilityChanged(setTimelineVisible).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])
  const toggleTimeline = (next: boolean) => {
    windowService[next ? 'showTimeline' : 'hideTimeline']().catch(console.error)
  }
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

  /** 采集频率:乐观切档 → Rust 写 prefs 并即时下发 → 同步偏好快照;失败回读真实值。 */
  const changeCollectInterval = (secs: number) => {
    const prev = collectSecs
    setCollectSecs(secs)
    collectorService
      .setCollectInterval(secs)
      .then((r) => {
        if (r) {
          setCollectSecs(r.secs)
          setDesignPrefs({ collectIntervalSecs: r.secs })
        } else {
          setCollectSecs(prev)
        }
      })
      .catch((e: unknown) => {
        console.error('[collect interval]', e)
        setCollectSecs(prev)
      })
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

  // 分组用 setting-block 卡片化（与 Appearance 一致）；布尔项用 ToggleRow（Off/On 分段开关）；
  // 解释性长文放在行 label / 按钮 title hover，仅动态结果与危险警示保留常驻 note。
  // Week starts on 放在 Matrix 组（它决定矩阵行列排布口径）。
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
        <div className="setting-row">
          <span title="How often local agent data is scanned; a change applies immediately">
            Collect every
          </span>
          <div className="setting-seg">
            {COLLECT_INTERVAL_CHOICES.map(({ secs, label }) => (
              <button
                key={secs}
                className={`setting-seg-btn${collectSecs === secs ? ' is-active' : ''}`}
                title={secs === 30 ? `Scan every ${label} (default)` : `Scan every ${label}`}
                onClick={() => changeCollectInterval(secs)}
              >
                {label}
              </button>
            ))}
          </div>
        </div>
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

      {/* 时间轴窗口设置（显隐 / 前后天数;项目集在 Projects tab 的 Timeline projects 组）,prefs 经 storage 桥即时同步到 timeline 窗口*/}
      <div className="setting-section">Timeline</div>
      <div className="setting-block">
        <ToggleRow
          label="Show timeline"
          title="Cross-project board window; togglable from tray"
          checked={timelineVisible}
          onChange={toggleTimeline}
        />
        <div className="setting-row">
          <span title="Days before today on the board">Past days</span>
          <div className="setting-seg">
            {[3, 7, 15, 30].map((n) => (
              <button
                key={n}
                className={`setting-seg-btn${(design.timelinePastDays ?? 7) === n ? ' is-active' : ''}`}
                title={`Show ${n} days before today`}
                onClick={() => setDesignPrefs({ timelinePastDays: n })}
              >
                {n}
              </button>
            ))}
          </div>
        </div>
        <div className="setting-row">
          <span title="Sessions shown per past day; the rest are counted as +N (click the cell on the timeline to expand and scroll)">Past sessions</span>
          <div className="setting-seg">
            {[1, 2, 3, 5].map((n) => (
              <button
                key={n}
                className={`setting-seg-btn${(design.timelinePastSessions ?? 1) === n ? ' is-active' : ''}`}
                title={`Show ${n} ${n === 1 ? 'session' : 'sessions'} per past day`}
                onClick={() => setDesignPrefs({ timelinePastSessions: n })}
              >
                {n}
              </button>
            ))}
          </div>
        </div>
        <div className="setting-row">
          <span title="Sessions visible for today; scroll inside the cell to see the rest">Today sessions</span>
          <div className="setting-seg">
            {[3, 5, 8, 12].map((n) => (
              <button
                key={n}
                className={`setting-seg-btn${(design.timelineTodaySessions ?? 5) === n ? ' is-active' : ''}`}
                title={`Show up to ${n} sessions for today`}
                onClick={() => setDesignPrefs({ timelineTodaySessions: n })}
              >
                {n}
              </button>
            ))}
          </div>
        </div>
        <div className="setting-row">
          <span title="Which sessions represent a day when more happened than shown">Pick by</span>
          <div className="setting-seg">
            {(
              [
                { v: 'latest', label: 'Latest', hint: 'The newest sessions represent the day' },
                { v: 'earliest', label: 'Earliest', hint: 'The first sessions represent the day' },
                { v: 'longest', label: 'Longest', hint: 'The sessions with the most tokens represent the day' },
              ] as const
            ).map((o) => (
              <button
                key={o.v}
                className={`setting-seg-btn${(design.timelinePick ?? 'latest') === o.v ? ' is-active' : ''}`}
                title={o.hint}
                onClick={() => setDesignPrefs({ timelinePick: o.v })}
              >
                {o.label}
              </button>
            ))}
          </div>
        </div>
        <ToggleRow
          label="Newest first"
          title="Reverse the order: newest days and sessions at the top (default is oldest at the top)"
          checked={design.timelineReverse ?? false}
          onChange={(v) => setDesignPrefs({ timelineReverse: v })}
        />
        <div className="setting-row">
          <span title="Fold the board into a strip at the top of the screen after it loses focus">Auto fold</span>
          <div className="setting-seg">
            {[0, 5, 30, 60, 300].map((n) => (
              <button
                key={n}
                className={`setting-seg-btn${(design.timelineAutoStripSecs ?? 0) === n ? ' is-active' : ''}`}
                title={n === 0 ? 'Never fold automatically' : `Fold ${n < 60 ? `${n} seconds` : `${n / 60} ${n === 60 ? 'minute' : 'minutes'}`} after the board loses focus`}
                onClick={() => setDesignPrefs({ timelineAutoStripSecs: n })}
              >
                {n === 0 ? 'Off' : n < 60 ? `${n}s` : `${n / 60}m`}
              </button>
            ))}
          </div>
        </div>
        <div className="setting-row">
          <span title="Days after today on the board (axis only, no data yet)">Future days</span>
          <div className="setting-seg">
            {[0, 3, 7, 15].map((n) => (
              <button
                key={n}
                className={`setting-seg-btn${(design.timelineFutureDays ?? 7) === n ? ' is-active' : ''}`}
                title={n === 0 ? 'No future days' : `Show ${n} days after today`}
                onClick={() => setDesignPrefs({ timelineFutureDays: n })}
              >
                {n}
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

/** 取色槽位：挂件卡片 / 主界面顶栏 / 主界面主体 / 主界面边框 / 时间轴主题色。 */
type ColorSlot = 'card' | 'titlebar' | 'panel' | 'border' | 'timeline'

function AppearanceTab() {
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  useEffect(() => subscribeDesignPrefs(setDesign), [])

  // 取色弹层开合（单一弹层服务所有槽位）。浮层 DOM 常驻：open 只驱动
  // 类名/aria，绝不条件渲染卸载。
  const [pickerFor, setPickerFor] = useState<ColorSlot | null>(null)
  const colorRowRefs = {
    card: useRef<HTMLDivElement>(null),
    titlebar: useRef<HTMLDivElement>(null),
    panel: useRef<HTMLDivElement>(null),
    border: useRef<HTMLDivElement>(null),
    timeline: useRef<HTMLDivElement>(null),
  }
  const popoverRefs = {
    card: useRef<HTMLDivElement>(null),
    titlebar: useRef<HTMLDivElement>(null),
    panel: useRef<HTMLDivElement>(null),
    border: useRef<HTMLDivElement>(null),
    timeline: useRef<HTMLDivElement>(null),
  }

  // 点击弹层外关闭。内点 = 色板行 + 取色弹层两者：二者是平级节点，只认色板行
  // 的话，弹层内拖取色盘/点输入框会被误判外点而闪关（react-colorful 不拦
  // mousedown 冒泡）。
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
    timeline: { value: design.timelineAccent, set: (h) => setDesignPrefs({ timelineAccent: h }), rowRef: colorRowRefs.timeline, popoverRef: popoverRefs.timeline, fallback: '#2563eb' },
  }

  /** 色板行（PRESETS + 自定义钮）+ 取色弹层，包在槽位容器内：弹层 absolute
   * 锚定本行正下方（各槽位互不串位）。浮层 DOM 常驻不卸载，
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

      {/* 时间轴外观：窗口风格 + 三档背景 alpha,经 storage 桥即时同步到 timeline 窗口;与挂件 / 主界面零关联*/}
      <div className="setting-section">Timeline appearance</div>
      <div className="setting-block">
        <div className="setting-row">
          <span title="Accent for session cells, today and highlights; the top bar and panel colors are not affected">Theme color</span>
          <span className="setting-value">{design.timelineAccent ? design.timelineAccent.toUpperCase() : 'Auto (follows system)'}</span>
        </div>
        {renderColorField('timeline', TIMELINE_ACCENT_SWATCHES)}
        <div className="setting-row">
          <span title="Shadow: floating cards with a system shadow, like the main window. Flat: no shadow, edge to edge">Window style</span>
          <div className="setting-seg">
            {(['shadow', 'flat'] as const).map((v) => (
              <button
                key={v}
                className={`setting-seg-btn${(design.timelineWindowStyle ?? 'shadow') === v ? ' is-active' : ''}`}
                title={v === 'shadow' ? 'System shadow and a transparent margin, like the main window' : 'No shadow; the cards fill the window'}
                onClick={() => setDesignPrefs({ timelineWindowStyle: v })}
              >
                {v === 'shadow' ? 'Shadow' : 'Flat'}
              </button>
            ))}
          </div>
        </div>
        <ToggleRow
          label="Color cells by usage"
          title="Darker session cells for more tokens; off gives every session cell the same light tint"
          checked={design.timelineHeat ?? true}
          onChange={(v) => setDesignPrefs({ timelineHeat: v })}
        />
        {TIMELINE_ALPHA_ROWS.map((r) => (
          <div key={r.key} className="setting-row">
            <span title={r.hint}>{r.label}</span>
            <input
              type="range"
              title="Drag to adjust"
              min={r.min}
              max={1}
              step={0.05}
              value={design[r.key] ?? r.def}
              onChange={(e) => setDesignPrefs({ [r.key]: Number(e.target.value) })}
            />
            <span className="setting-value">{Math.round((design[r.key] ?? r.def) * 100)}%</span>
          </div>
        ))}
        <div className="setting-actions">
          <button
            className="setting-btn"
            disabled={design.timelineAccent === undefined && design.timelineWindowStyle === undefined && design.timelineHeat === undefined && TIMELINE_ALPHA_ROWS.every((r) => design[r.key] === undefined)}
            onClick={() => setDesignPrefs({ timelineAccent: undefined, timelineWindowStyle: undefined, timelineHeat: undefined, timelineBgAlpha: undefined, timelineBarAlpha: undefined, timelineCellAlpha: undefined })}
            title="Reset the timeline theme color, window style, usage colors and opacity settings"
          >
            Restore defaults
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

/* ---------------- 时间轴外观三档 alpha（默认值与 timelineConfig 同源） ---------------- */

const TIMELINE_ALPHA_ROWS: { key: 'timelineBgAlpha' | 'timelineBarAlpha' | 'timelineCellAlpha'; label: string; hint: string; min: number; def: number }[] = [
  { key: 'timelineBgAlpha', label: 'Panel background opacity', hint: 'The board panel below the top bar; the date labels float on it', min: 0, def: TIMELINE_BG_ALPHA },
  { key: 'timelineBarAlpha', label: 'Top bar opacity', hint: 'The header bar with project names and the fold button', min: 0.2, def: TIMELINE_BAR_ALPHA },
  { key: 'timelineCellAlpha', label: 'Session cell opacity', hint: 'Session cell backgrounds; text stays fully opaque', min: 0.2, def: TIMELINE_CELL_ALPHA },
]

/* ---------------- 材质三档（两窗口共用行控件） ---------------- */

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
 * 分段控件，与档位切换同语言。说明文案走 label 的 title hover；
 * disabled 段保持可 hover（title 仍可读）。 ---------------- */

function ToggleRow({
  label,
  title,
  checked,
  disabled = false,
  onChange,
}: {
  label: string
  /** 行说明（hover 提示）。 */
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

/* ---------------- Data：月度导出 ---------------- */

function DataTab() {
  const [exporting, setExporting] = useState(false)
  const [exportResult, setExportResult] = useState<string | null>(null)
  const month = currentMonth()
  // 数据管理：信息 + 迁移 + 备份/恢复 + 打开目录。
  // 目录输入用手输路径（写路径场景）;目录/文件选择走 dialog 插件。
  const [info, setInfo] = useState<DataInfo | null>(null)
  const [migTarget, setMigTarget] = useState('')
  const [bakTarget, setBakTarget] = useState('')
  const [resTarget, setResTarget] = useState('')
  const [busy, setBusy] = useState<'migrate' | 'backup' | 'restore' | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  // credit 卡开关（insightsCredit）归在本 tab。
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

  return (
    <>
      <div className="setting-section">Storage</div>
      <div className="setting-block">
        {info ? (
          <div className="setting-note">
            Root: {info.root}
            {info.fell_back ? ' (default root not writable, fell back to AppData)' : ''}
            {info.custom_root ? <><br />Custom: {info.custom_root}</> : null}
            <br />DB {fmtBytes(info.db_bytes)} · exports {info.exports_count} files
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

      <div className="setting-section">Credit card</div>
      <div className="setting-block">
        <div className="setting-note">
          Credits and models are read from local CodeBuddy / WorkBuddy session data; no export import needed.
        </div>
        <ToggleRow
          label="Show credit card in Insights"
          title="Adds a Credit module (tokens × credit) to Insights, with its own button in the module switcher"
          checked={design.insightsCredit}
          onChange={(v) => setDesignPrefs({ insightsCredit: v })}
        />
      </div>

      <div className="setting-section">Migrate data root</div>
      <div className="setting-block">
        <div className="setting-note">
          Moves cache, prefs and exports to a new folder (restart to apply).
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

const POLL_OPTIONS = [300, 600, 900, 1800] // 秒 → 5/10/15/30 分钟（兜底取数间隔）
const POLL_DEFAULT_SECS = 1800 // 默认 30 分钟：兜底无动态检测能力，取封顶档；取数时机由本地 token 驱动

function SubscriptionsTab() {
  const [design, setDesign] = useState<DesignPrefs>(getDesignPrefs)
  const [scan, setScan] = useState<CredentialInfo[]>([])
  const [snapshots, setSnapshots] = useState<SubscriptionSnapshot[]>([])
  const [estimator, setEstimator] = useState<EstimatorState[]>([])
  const [notice, setNotice] = useState<string | null>(null)

  useEffect(() => subscribeDesignPrefs(setDesign), [])

  // 初查 + subscription:changed 跟随（bind/unbind/轮询完成都会汇入同一事件）。
  // 估算器是诊断只读接口,同样只在这两处查——不轮询。
  useEffect(() => {
    const loadEstimator = () =>
      subscriptionService
        .getEstimator()
        // null = 非 Tauri 环境或 Rust 缺这条命令 → 保持空,校准状态行整行不显示
        .then((e) => e && setEstimator(e))
        .catch(() => {})
    subscriptionService.scanCredentials().then((s) => s && setScan(s)).catch(console.error)
    subscriptionService.getSnapshots().then((s) => s && setSnapshots(s)).catch(console.error)
    void loadEstimator()
    let off: (() => void) | null = null
    void events.onSubscriptionChanged(() => {
      subscriptionService.getSnapshots().then((s) => s && setSnapshots(s)).catch(console.error)
      void loadEstimator()
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])

  // 兜底取数间隔运行时值恢复（prefs 装载完成后一次性下发）
  useEffect(() => {
    const secs = design.subscriptionPollSecs
    if (secs) subscriptionService.applyPollSecs(secs)
  }, [design.subscriptionPollSecs])

  // 取数策略（阈值 + 低余量收紧）运行时值恢复：两键合成一次下发,值变化时重发。
  const fetchPct = subscriptionFetchPct(design)
  const tightenLow = subscriptionTightenLow(design)
  useEffect(() => {
    void applySubscriptionFetchPolicy(getDesignPrefs())
  }, [fetchPct, tightenLow])

  // 阈值输入 draft + onBlur 提交（与 Projects 的 Scratch 规则同款）：非法值回退显示当前值。
  const [pctDraft, setPctDraft] = useState(() => String(fetchPct))
  useEffect(() => setPctDraft(String(fetchPct)), [fetchPct])
  const commitFetchPct = (draft: string) => {
    const n = Number(draft)
    const { min, max, step } = SUBSCRIPTION_FETCH_PCT
    if (!Number.isFinite(n) || n < min || n > max || !Number.isInteger(n / step)) {
      setNotice(`Fetch threshold must be ${min} to ${max}% in steps of ${step}`)
      setPctDraft(String(fetchPct))
      return
    }
    setNotice(null)
    if (n === fetchPct) return
    setDesignPrefs({ subscriptionFetchPct: n })
    subscriptionService.setFetchPolicy(n, tightenLow).catch(console.error)
  }

  // 悬浮球总开关：状态**直接取可见性单一源**——Rust visibility.rs 的 orb_visible,
  // get_visibility 初查 + orb-visibility-changed 广播跟随;顶栏 Orbit 钮、托盘勾选、
  // orb 自身右键「Hide orb」三条路径都汇入那里（与顶栏按钮完全同款消费方式）,
  // 因此任一路径改动这里都能跟随。可见性的持久化在 window-state.json 的 orb_visible
  // （Rust 唯一源）,前端不存第二份镜像。
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
    // 开 = showOrb;关 = hideOrb。总开关只控呈现:不停取数、不动绑定数据
    // （凭据绑定在平台卡卸载,兜底取数始终低频）。
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
      <div className="setting-block">
        <ToggleRow
          label="Show floating orb"
          title="Desktop quota orb; togglable from titlebar or tray"
          checked={orbVisible}
          onChange={toggleOrb}
        />
        {/* 按预计消耗取数（取数主路径）：本地 token 按模型加权折算出代价,
            乘以自动校准的系数 → 「距上次读数大约消耗了百分之几」,达到阈值就取一次读数。*/}
        <div className="setting-row">
          <span title="Pull a reading when the estimated use since the last one reaches this. Estimated from local tokens, weighted per model.">
            Fetch threshold
          </span>
          <span>
            <input
              className="setting-num"
              type="number"
              aria-label="Fetch threshold"
              value={pctDraft}
              min={SUBSCRIPTION_FETCH_PCT.min}
              max={SUBSCRIPTION_FETCH_PCT.max}
              step={SUBSCRIPTION_FETCH_PCT.step}
              onChange={(e) => setPctDraft(e.target.value)}
              onBlur={() => commitFetchPct(pctDraft)}
              onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
            />
            <span className="setting-unit">%</span>
          </span>
        </div>
        {/* 低余量收紧：5h 剩余 ≤ 20% 时阈值减半（默认 5% → 2.5%）,额度见底那段读数更密。*/}
        <ToggleRow
          label="Tighten when low"
          title="Halve the threshold when 5h remaining is at or below 20%"
          checked={tightenLow}
          onChange={(v) => {
            setDesignPrefs({ subscriptionTightenLow: v })
            subscriptionService.setFetchPolicy(fetchPct, v).catch(console.error)
          }}
        />
        {/* 校准状态（只读诊断,每平台一行）：估算器是否已被读数校准 + 样本数;
            距上次读数的预计消耗放 hover,不占版面。命令不可用时整块不渲染。*/}
        {estimator.length > 0 ? (
          <div className="setting-note">
            {estimator.map((e) => (
              <div
                key={e.platform}
                title={`Estimated ${e.est_pct_since_fetch.toFixed(1)}% used since the last reading`}
              >
                {e.platform === 'claude' ? 'Claude' : 'Codex'} ·{' '}
                {e.calibrated ? 'calibrated' : 'calibrating — factory weights'} ({e.pairs}{' '}
                {e.pairs === 1 ? 'sample' : 'samples'})
                {/* 出厂预设是在某一档上、再按官方限额比折到用户这一档的 → 没校准前可能偏一点
                    （-10）。已校准后不再相关,不显示。*/}
                {!e.calibrated ? (
                  <span>
                    {` · preset measured on ${e.platform === 'claude' ? 'Max 5x' : 'Plus'} and scaled to your plan, so it may run a little off until your own readings calibrate it`}
                  </span>
                ) : null}
                {/* 收割留存的本地读数条数（Claude = 桌面端采样;Codex = 会话 rollout 里的
                    rate_limits;0 / 缺字段时整段不显示）——两个源自己都会滚掉旧数据,
                    这个数越过源的保留线继续涨就是密度在累积。*/}
                {e.desktop_samples ? ` · ${e.desktop_samples} local readings kept` : ''}
                {/* 本机 agent 解释不了的消耗：只在真的检出时显示,没有就整段不出现
                    ——不给用户增加一条恒为 0 的噪声读数。**不写成「别的设备」**：
                    本机桌面端自己的对话同样不走 collector 源却吃同一份配额。*/}
                {e.foreign && e.foreign.unexplained > 0 ? (
                  <span
                    className="setting-note-warn"
                    title={`Usage grew in ${e.foreign.unexplained} of ${e.foreign.windows} sampled windows with no local agent activity (+${e.foreign.unexplained_pct.toFixed(1)}% of the 5h window). Something else draws on the same quota — the Claude desktop app's own chats, the web app, a phone, or another computer. Readings stay correct; only the local consumption estimate is affected.`}
                  >
                    {` · ${e.foreign.unexplained}/${e.foreign.windows} windows unexplained in ${e.foreign.window_hours}h (+${e.foreign.unexplained_pct.toFixed(1)}%)`}
                  </span>
                ) : null}
              </div>
            ))}
          </div>
        ) : null}
        {/* 兜底取数：主路径是上面的预计消耗阈值。本项只管「本地留不下
            痕迹」的用量（网页 / 在线会话）的兜底节奏,默认 30 分钟。*/}
        <div className="setting-row">
          <span title="Readings follow the estimated use above; this is the fallback for usage with no local trace (web/online sessions)">Fallback poll</span>
          <div className="setting-seg">
            {POLL_OPTIONS.map((s) => (
              <button
                key={s}
                className={`setting-seg-btn${(design.subscriptionPollSecs ?? POLL_DEFAULT_SECS) === s ? ' is-active' : ''}`}
                title={`Fallback fetch every ${s / 60} minutes`}
                onClick={() => setPoll(s)}
              >
                {s / 60}m
              </button>
            ))}
          </div>
        </div>
        {/* Standby monitoring：待机看的是**本地 agent 十分钟没有新 token**
            → 悬浮球减淡;新 token / 手动刷新·展开·切换平台立即退出。
            取数频次不受待机影响（兜底本身已是封顶档）。默认开。*/}
        <ToggleRow
          label="Standby monitoring"
          title="Dims the orb after 10 minutes with no new tokens from local agents"
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

      {(['codex', 'claude'] as const).some(boundOf) ? (
        <>
          <div className="setting-section">Orb messages</div>
          <div className="setting-block">
            {(['codex', 'claude'] as const).filter(boundOf).map((p) => (
              <MessageModelsRow key={p} platform={p} value={design.orbMessageModels?.[p]} />
            ))}
            <div className="setting-note">
              Shown when hovering the orb's 5-hour dial as messages left / messages in a full window, e.g. “GPT-6 Astra:
              ~3/30”. A message is one prompt you send. Estimated from your own median
              cost per message over the last 30 days, so it moves with how you work; models need at least 5 messages
              in that time to appear. Auto shows only the model you used most in the last 7 days.
            </div>
          </div>
        </>
      ) : null}

      <div className="setting-section">Plan fees</div>
      <div className="setting-block">
        {(['codex', 'claude'] as const).map((p) => (
          <MonthlyFeeRow key={p} platform={p} value={design.subscriptionMonthlyUsd?.[p]} onNotice={setNotice} />
        ))}
        <div className="setting-note">
          Optional. What you pay per month for your own plan, in US dollars. Only used by Insights › Pricing to show the
          equivalent API value as a multiple of the fee for the same days — a value multiple, not money saved. Leave empty
          to hide it.
        </div>
      </div>
    </>
  )
}

/** 一个平台在悬浮球 hover 里显示哪些模型的剩余消息数：Auto（缺键 = 主力模型）/
 * Off（空数组）/ 逐个模型多选（按点选顺序印,最多 ORB_MESSAGE_MODELS_MAX 个）。
 * 候选 = 近 30 天够样本的模型;已选但样本掉出窗口的也列出来,好让用户能取消。 */
function MessageModelsRow({ platform, value }: {
  platform: 'codex' | 'claude'
  value: string[] | undefined
}) {
  const [budget, setBudget] = useState<MessageBudget | null>(null)
  useEffect(() => {
    let stale = false
    subscriptionService.getMessageBudget(platform).then((b) => !stale && setBudget(b)).catch(() => {})
    return () => {
      stale = true
    }
  }, [platform])
  const set = (next: string[] | undefined) => {
    const all = { ...(getDesignPrefs().orbMessageModels ?? {}) }
    if (next === undefined) delete all[platform]
    else all[platform] = next
    setDesignPrefs({ orbMessageModels: Object.keys(all).length > 0 ? all : undefined })
  }
  const candidates = [
    ...(budget?.rows.map((r) => r.model_key) ?? []),
    ...(value ?? []).filter((k) => !budget?.rows.some((r) => r.model_key === k)),
  ]
  const toggle = (key: string) => {
    const cur = value ?? []
    if (cur.includes(key)) set(cur.filter((k) => k !== key))
    else if (cur.length < ORB_MESSAGE_MODELS_MAX) set([...cur, key])
  }
  const label = platform === 'claude' ? 'Claude' : 'Codex'
  const perMsg = (key: string) => {
    const r = budget?.rows.find((x) => x.model_key === key)
    return r
      ? `${r.display_name} · about ${Math.round(100 / r.pct_per_turn)} messages per full 5-hour window (median of ${r.turns} messages)`
      : 'Not enough messages in the last 30 days to estimate'
  }
  return (
    <div className="setting-row">
      <span title={`Which ${label} models show messages left in the orb hover`}>{label}</span>
      <div className="setting-seg is-wrap">
        <button
          className={`setting-seg-btn${value === undefined ? ' is-active' : ''}`}
          title={
            budget?.main_model
              ? `Only your most-used model lately (${shortModelName(budget.main_model)})`
              : 'Only your most-used model lately'
          }
          onClick={() => set(undefined)}
        >
          Auto
        </button>
        <button
          className={`setting-seg-btn${value !== undefined && value.length === 0 ? ' is-active' : ''}`}
          title="Don't show messages left"
          onClick={() => set([])}
        >
          Off
        </button>
        {candidates.map((k) => {
          const on = value?.includes(k) ?? false
          return (
            <button
              key={k}
              className={`setting-seg-btn${on ? ' is-active' : ''}`}
              title={perMsg(k)}
              disabled={!on && (value?.length ?? 0) >= ORB_MESSAGE_MODELS_MAX}
              onClick={() => toggle(k)}
            >
              {shortModelName(k)}
            </button>
          )
        })}
      </div>
    </div>
  )
}

/** 一个平台的订阅月费输入（draft + onBlur 提交,与取数阈值同款）:空 = 清掉该平台的键。 */
function MonthlyFeeRow({ platform, value, onNotice }: {
  platform: 'codex' | 'claude'
  value: number | undefined
  onNotice: (text: string | null) => void
}) {
  const [draft, setDraft] = useState(value === undefined ? '' : String(value))
  useEffect(() => setDraft(value === undefined ? '' : String(value)), [value])
  const commit = () => {
    const text = draft.trim()
    const all = { ...(getDesignPrefs().subscriptionMonthlyUsd ?? {}) }
    if (text === '') {
      onNotice(null)
      if (value === undefined) return
      delete all[platform]
      setDesignPrefs({ subscriptionMonthlyUsd: all })
      return
    }
    const n = Number(text)
    if (!Number.isFinite(n) || n <= 0 || n > MONTHLY_USD_MAX) {
      onNotice(`Monthly fee must be a positive amount up to $${MONTHLY_USD_MAX.toLocaleString('en-US')}`)
      setDraft(value === undefined ? '' : String(value))
      return
    }
    onNotice(null)
    if (n === value) return
    setDesignPrefs({ subscriptionMonthlyUsd: { ...all, [platform]: n } })
  }
  const label = platform === 'claude' ? 'Claude' : 'Codex'
  return (
    <div className="setting-row">
      <span title={`What you pay per month for your ${label} plan (USD). Empty = not shown in Insights.`}>{label} monthly fee</span>
      <span>
        <input
          className="setting-num"
          type="number"
          aria-label={`${label} monthly fee`}
          placeholder="—"
          value={draft}
          min={0}
          step={1}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
        />
        <span className="setting-unit">USD / month</span>
      </span>
    </div>
  )
}

/* ---------------- About：版本与更新（更新源 = 公开仓 Release） ---------------- */

/** 版本与更新：检查更新（签名校验）/ 启动自动检查开关 / 版本号按钮跳 Releases。
 * 启动时自动检查只预下载安装包,安装永远由这里的按钮触发。
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
  // 启动时自动检查预下载好的新版（进程内共享,不用重新检查就能直接装）
  const [ready, setReady] = useState<ReadyUpdate | null>(updateService.getReadyUpdate)
  useEffect(() => updateService.subscribeReadyUpdate(setReady), [])

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
    if (installing) return
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
    // 已预下载的版本不比检查结果旧时,以「可直接安装」为准
    if (ready && (result?.status !== 'available' || result.version === ready.version)) {
      return `Version ${ready.version} has been downloaded and is ready to install${version ? ` — current v${version}` : ''}.`
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
  // 安装入口：检查出的新版优先（它可能比预下载的更新）;否则装预下载好的那份
  const installAction: ((cb: (p: UpdateProgress) => void) => Promise<void>) | null = installable
    ? installable.install
    : ready
      ? () => updateService.installReadyUpdate()
      : null
  const installDownloaded = ready !== null && (installable === null || installable.version === ready.version)

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
          {installAction ? (
            <button
              className="setting-btn is-active"
              disabled={installing}
              onClick={() => {
                doInstall(installAction).catch(console.error)
              }}
              title={
                installDownloaded
                  ? 'Run the installer for the downloaded, signature-verified update'
                  : 'Download, verify the signature and run the installer'
              }
            >
              {installDownloaded ? 'Install' : <>Download &amp; install</>}
            </button>
          ) : null}
        </div>
        {result?.status === 'available' && result.notes ? (
          <div className="setting-note">{result.notes}</div>
        ) : ready?.notes && result?.status !== 'available' ? (
          <div className="setting-note">{ready.notes}</div>
        ) : null}
        {note ? <div className="setting-note">{note}</div> : null}

        <ToggleRow
          label="Check for updates at startup"
          title="At every launch, check for a new version and download it in the background, then show a notification. Nothing is installed until you click Install."
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
