// OrbWindow：悬浮球窗口（完整两态）。
// 独立根类 .orb-shell（设计红线：不进 .shell 体系——主窗口样式只属于
// .is-expanded、挂件样式只属于 .is-widget，orb 颜色变量按窗口拆分 --orb-*）。
//
// 两态：
// - 收起态 = 竖向外轮廓贴片条：**外轮廓色 = 周额度**（额度消耗沿轮廓向下褪色,
//   包含语义「外包内」）,**内条 = 5 小时额度**;双击展开;
// - 展开态 = 圆角卡片：app 图标 + 双环（周额度主环/5h 副环）+ 右上角状态点 +
//   底部刷新时间/重置倒计时行 + 收起按钮;按钮收起。尺寸切换走 set_orb_size
//。
//
// 数据：get_subscription_snapshots 初查 + subscription:changed 事件重查（Rust
// 轮询 daemon 发）+ 30s 兜底节流轮询。状态四态互斥文案（预案）：
// ok / auth_failed（重新登录 agent CLI）/ plan_inactive（续费后自动恢复）/
// network_failed（静默保留旧数据）。
// 主题：复用挂件族 sync hook（卡片色/WCAG 派生）映射到 --orb-* 变量；材质
// hook 不装配（orb 首期无毛玻璃材质档）。
import { useCallback, useEffect, useRef, useState } from 'react'
import { useLayoutEffect } from 'react'
import appLogo from '../../assets/app-logo.png'
import { events, subscriptionService, windowService, type OrbDockState, type SubscriptionSnapshot } from '../../services'
import { getDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'
import { deriveWidgetTheme } from '../settings/widgetTheme'
import { useShowOnLoad } from '../window/useShowOnLoad'
import { useRadiusSchemeSync } from '../settings/radiusTheme'
import './orb.css'

/** 挂件派生主题 → --orb-* 变量镜像（orb.css 消费 --orb-* 键,GUIDE 红线：
 *  颜色变量按窗口拆分,不复用 --widget-card-*。映射在此处单点完成,widgetTheme
 *  单一源不动）。scheme 内置色（未自定义卡片色）时变量缺省,orb.css 兜底值
 *  已是蓝白透明族。 */
function applyOrbThemeVars(): void {
  const root = document.documentElement
  const theme = deriveWidgetTheme(getDesignPrefs())
  const pairs: [string, string | null][] = theme
    ? [
        ['--orb-card-bg', `rgba(${theme.cardBg},0.92)`],
        ['--orb-card-border', `rgba(${theme.cardBorder},0.25)`],
        ['--orb-card-text', `rgb(${theme.text})`],
        ['--orb-card-text-muted', `rgb(${theme.textMuted})`],
      ]
    : []
  for (const [k, v] of pairs) {
    if (v === null) root.style.removeProperty(k)
    else root.style.setProperty(k, v)
  }
  if (!theme) {
    for (const k of ['--orb-card-bg', '--orb-card-border', '--orb-card-text', '--orb-card-text-muted']) {
      root.style.removeProperty(k)
    }
  }
}

function useOrbThemeSync(): void {
  applyOrbThemeVars()
  useEffect(() => subscribeDesignPrefs(applyOrbThemeVars), [])
}

/** 两态窗口尺寸（逻辑像素;tauri.conf.json 初始值 = COLLAPSED）。
 *  收起态窗口 ≈ 竖条本体（24×84 轨道 + 4px 呼吸位:
 *  容器收到与竖条等大,不留大件不可见拖动盲区）;展开态卡片 240 宽——
 *  密度高度 320→232。 */
const COLLAPSED_SIZE = { w: 32, h: 96 }
const EXPANDED_SIZE = { w: 240, h: 232 }

/** 快照兜底节流（Rust 事件即时推送之外的低频兜底,S4：30s）。 */
const RESNAP_INTERVAL_MS = 30_000

/** 单平台窗口语义序：5h 主显（收起态内条）,7d 为周额度（收起态外轮廓）。 */
function windowOf(snap: SubscriptionSnapshot | undefined, kind: string) {
  return snap?.windows.find((w) => w.kind === kind)
}

/** 四态互斥文案（预案;auth_failed 与 plan_inactive 语义勿混）：
 *  auth_failed = 凭据失效,需要用户重新登录 agent CLI;
 *  plan_inactive = 凭据仍有效但订阅过期/降级,续费后自动恢复,用户零操作。 */
function statusHint(status: string): { text: string; level: 'ok' | 'warn' | 'error' | 'muted' } {
  switch (status) {
    case 'ok':
      return { text: 'Active', level: 'ok' }
    case 'plan_inactive':
      return { text: 'Subscription inactive — resumes after renewal', level: 'warn' }
    case 'auth_failed':
      return { text: 'Credentials expired — sign in to the agent CLI again', level: 'error' }
    case 'network_failed':
      return { text: 'Network error — showing last known data', level: 'warn' }
    case 'parse_failed':
      return { text: 'Upstream response unrecognized', level: 'warn' }
    default:
      return { text: 'Not bound — manage in Settings', level: 'muted' }
  }
}

/** 重置倒计时（unix 秒 → 「1h 23m」短格式;None/已过 → null）。 */
function resetCountdown(resetsAt: number | null | undefined): string | null {
  if (!resetsAt) return null
  const diff = resetsAt * 1000 - Date.now()
  if (diff <= 0) return null
  const mins = Math.ceil(diff / 60_000)
  if (mins < 60) return `${mins}m`
  const h = Math.floor(mins / 60)
  const m = mins % 60
  if (h < 24) return m > 0 ? `${h}h ${m}m` : `${h}h`
  const d = Math.floor(h / 24)
  return `${d}d ${h % 24}h`
}

function formatFetchedAt(fetchedAt: number | null | undefined): string {
  if (!fetchedAt) return '—'
  const d = new Date(fetchedAt * 1000)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

export default function OrbWindow() {
  useShowOnLoad()
  useOrbThemeSync()
  // 圆角方案档（与挂件/主面板同档对齐 → --widget-card-radius，orb 卡片复用该档位值）。
  useRadiusSchemeSync('widget')

  // 悬浮球窗口全程透明（透明检查清单同款）：宿主层不得有不透明默认背景
  useLayoutEffect(() => {
    document.documentElement.classList.add('mode-widget')
  }, [])

  // 两态（收起 = 默认;状态仅前端内存——重启回收起态,几何/可见性由 window-state 承担）
  const [expanded, setExpanded] = useState(false)
  // 贴边停靠：docked = Rust 侧判定结果（拖动松手
  // 贴缘 dock / 离缘 undock 广播,重启由 get_orb_dock 恢复）;边缘自位置承担,
  // 前端只跟形态。
  const [dock, setDock] = useState<OrbDockState | null>(null)
  // 挂载恢复：有 dock 态 → 回收起态（几何 Rust restore 已按 anchor 归位）
  useLayoutEffect(() => {
    windowService.getOrbDock().then((d) => {
      if (d) setDock(d)
    }).catch(() => {})
  }, [])
  // 拖动松手的 dock/undock 广播（Rust orb_dock 子类化线程发）
  useEffect(() => {
    let off: (() => void) | null = null
    void events.onOrbDockChanged((p) => {
      if (p.docked && p.edge) {
        // dock：形态收到竖条（几何 Rust place_docked 已归位,尺寸按当前态校准）
        setDock({ edge: p.edge, anchor_y_ratio: 0.5, work: [0, 0, 0, 0] })
        setExpanded(false)
        windowService.setOrbSize(COLLAPSED_SIZE.w, COLLAPSED_SIZE.h).catch(console.error)
      } else {
        // undock（拖离边缘）：形态回展开卡片。钳制：竖条停靠时窗口贴死
        // 屏缘,小拖距松手后窗口缘仍在缘附近,直接向屏外 set_size（240) 会把
        // 卡片推出屏——先按原 dock 侧把窗口位置钳回屏内（EXPANDED_W+gap）,
        // 再展开。位置/工作区物理像素在 Rust,经 orb_undock 的 expand-ready
        // 归位（传 origin=undock 复用同一钳制路径）。
        setDock(null)
        setExpanded(true)
        windowService.orbUndock(p.edge).catch(console.error)
        windowService.setOrbSize(EXPANDED_SIZE.w, EXPANDED_SIZE.h).catch(console.error)
      }
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])

  // S4 ：挂载即按当前态校准窗口尺寸——窗口恢复流程只归位不恢复 orb 尺寸
  // （单一执行者原则:两态尺寸由前端统一定义,重启后 React 回收起态,窗口若残留
  // 上次展开态的落盘尺寸会出「内容收起态+窗口展开尺寸」的粗柱子错位）
  useLayoutEffect(() => {
    windowService.setOrbSize(COLLAPSED_SIZE.w, COLLAPSED_SIZE.h).catch(console.error)
  }, [])
  const [snapshots, setSnapshots] = useState<SubscriptionSnapshot[]>([])
  // 本窗口 = orb；绑定多平台时首期显示第一个有数据的平台（单平台起步）
  const [activePlatform, setActivePlatform] = useState(0)

  // 数据消费：初查 + subscription:changed 即时重查 + 30s 兜底节流
  const refresh = useCallback(() => {
    subscriptionService.getSnapshots().then((s) => {
      if (s) setSnapshots(s)
    }).catch(() => {})
  }, [])

  useEffect(() => {
    refresh()
    let off: (() => void) | null = null
    void events.onSubscriptionChanged(() => {
      // 节流：Rust 每轮轮询后都会 emit,前端 30s 内不重复 invoke
      refresh()
    }).then((unlisten) => {
      off = unlisten
    })
    const timer = window.setInterval(refresh, RESNAP_INTERVAL_MS)
    return () => {
      off?.()
      window.clearInterval(timer)
    }
  }, [refresh])

  // 有快照的平台序列（未绑定平台 idle 占位跳过——orb 只显示已绑定平台）
  const boundSnaps = snapshots.filter((s) => s.status !== 'idle')
  useEffect(() => {
    if (activePlatform >= boundSnaps.length) setActivePlatform(0)
  }, [activePlatform, boundSnaps.length])

  const snap = boundSnaps[activePlatform]
  const w5h = windowOf(snap, '5h')
  const w7d = windowOf(snap, '7d') ?? windowOf(snap, '7d_opus')
  // 「剩余 = 100 − 已用」换算（口径单侧）。
  const remain7d = w7d ? Math.max(0, Math.min(100, 100 - w7d.used_percent)) : null
  const remain5h = w5h ? Math.max(0, Math.min(100, 100 - w5h.used_percent)) : null
  const hint = statusHint(snap?.status ?? 'idle')

  // 两态切换：程序化 set_orb_size;
  // dock 态展开 = undock（离开贴边语义,清 Rust 状态,屏内生长归位在 Rust）。
  const applySize = useCallback((target: 'expanded' | 'collapsed') => {
    const size = target === 'expanded' ? EXPANDED_SIZE : COLLAPSED_SIZE
    windowService.setOrbSize(size.w, size.h).catch(console.error)
  }, [])

  const expand = useCallback(() => {
    setExpanded(true)
    // 双击展开（无拖动）：dock 态下竖条贴在缘上,直接 set_size（240) 会把卡片
    // 推出屏外——orb_undock 顺带把窗口位置收进屏内（expand-ready 归位,工作区
    // 物理像素只在 Rust 可得）;前端只管清状态+展开尺寸。
    if (dock) {
      const edge = dock.edge
      setDock(null)
      windowService.orbUndock(edge).catch(console.error)
    }
    applySize('expanded')
  }, [applySize, dock])

  const collapse = useCallback(() => {
    setExpanded(false)
    applySize('collapsed')
  }, [applySize])

  // 右键菜单（减法：展开/刷新/打开设置都有既有交互入口
  // （双击、刷新按钮、Manage 按钮）——菜单只留「隐藏悬浮球」一项;隐藏后从
  // 托盘/设置页/顶栏 Orbit 钮可再开）
  // 收起态忽略——32×96
  // 窗口放不下菜单（裁切的根源）,收起态隐藏走展开态或托盘;弹出位置按菜单
  // 实际尺寸 clamp 到窗口内。
  const onContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault()
    if (!expanded) return
    setMenu({ x: e.clientX, y: e.clientY })
  }, [expanded])
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)
  // 菜单尺寸（首次弹出时测量正;缺省按典型值钳制避免首帧越界）
  const menuRef = useRef<HTMLDivElement | null>(null)
  const [menuSize, setMenuSize] = useState({ w: 120, h: 30 })
  useLayoutEffect(() => {
    if (menu && menuRef.current) {
      const r = menuRef.current.getBoundingClientRect()
      setMenuSize((s) => (s.w === r.width && s.h === r.height ? s : { w: r.width, h: r.height }))
    }
  }, [menu])
  const menuStyle = menu
    ? {
        left: Math.max(0, Math.min(menu.x, window.innerWidth - menuSize.w - 2)),
        top: Math.max(0, Math.min(menu.y, window.innerHeight - menuSize.h - 2)),
      }
    : undefined
  // 浮层铁律：菜单 DOM 常驻不卸载——隐藏=移出视口+visibility,禁止条件渲染
  const closeMenu = useCallback(() => setMenu(null), [])
  useEffect(() => {
    if (!menu) return
    // mousedown 在菜单内部时**不关**——否则按钮 click 落空（pointer-events 已 none）
    const onDown = (e: MouseEvent) => {
      if (!(e.target instanceof Element) || !e.target.closest('.orb-menu')) closeMenu()
    }
    const onBlur = () => closeMenu()
    window.addEventListener('mousedown', onDown)
    window.addEventListener('blur', onBlur)
    return () => {
      window.removeEventListener('mousedown', onDown)
      window.removeEventListener('blur', onBlur)
    }
  }, [menu, closeMenu])

  const menuHide = useCallback(() => {
    closeMenu()
    windowService.hideOrb().catch(console.error)
  }, [closeMenu])
  // 展开态刷新按钮;
  // 点击后旋转 ~800ms 作操作反馈（快照到达时数据自会更新）
  const [refreshing, setRefreshing] = useState(false)
  const refreshTimer = useRef<number>(0)
  const refreshNow = useCallback(() => {
    subscriptionService.refreshNow().catch(console.error)
    setRefreshing(true)
    window.clearTimeout(refreshTimer.current)
    refreshTimer.current = window.setTimeout(() => setRefreshing(false), 800)
  }, [])
  useEffect(() => () => window.clearTimeout(refreshTimer.current), [])

  // 周期切换（多绑定时点击竖条底部标记循环;单平台无感）
  const cyclePlatform = useCallback(() => {
    setActivePlatform((i) => (boundSnaps.length ? (i + 1) % boundSnaps.length : 0))
  }, [boundSnaps.length])

  const planLabel = snap?.plan_type && snap.plan_type !== 'unknown' ? snap.plan_type : boundSnaps.length ? 'Subscription' : 'Orb'

  return (
    <div
      className={`orb-shell${expanded ? ' is-expanded' : ''}`}
      onDoubleClick={expanded ? undefined : expand}
      onContextMenu={onContextMenu}
    >
      {/* ---- 收起态：竖向外轮廓贴片条（外轮廓=周额度褪色,内条=5h） ----*/}
      {/* drag-region 判定器只认 HTMLElement——
          SVG 子树整体跳过,S4 往 svg/rect/stop 上挂的属性全部无效,点在
          描边环（竖条唯一显眼视觉）上永远拖不动。根 = 容器挂 "deep"
          （tauri ≥2.11：子树内任意点触发拖动,交互元素 button 天然豁免,
          值=false 可再挖洞）,子元素属性全量撤除。*/}
      <div className={`orb-pill${expanded ? ' is-hidden' : ''}`} data-tauri-drag-region="deep">
        <div className="orb-pill-track">
          {/* 外轮廓：SVG rect 描边,周额度剩余比例决定描边向下褪色终点*/}
          <svg className="orb-pill-outline" viewBox="0 0 24 84" aria-hidden="true">
            <defs>
              <linearGradient id="orbOutlineFade" x1="0" y1="0" x2="0" y2="1">
                <stop className="orb-outline-stop-hi" offset="0" />
                <stop className="orb-outline-stop-lo" offset="1" />
              </linearGradient>
            </defs>
            <rect x="1.5" y="1.5" width="21" height="81" rx="9" fill="none" strokeWidth="3"
              className="orb-outline-base" />
            <rect x="1.5" y="1.5" width="21" height="81" rx="9" fill="none" strokeWidth="3"
              stroke="url(#orbOutlineFade)"
              className="orb-outline-fade"
              strokeDasharray="169"
              strokeDashoffset={169 * (1 - (remain7d ?? 0) / 100)}
              strokeLinecap="round"
              transform="rotate(90 12 42)" />
          </svg>
          {/* 内条：5h 额度（自下而上填充,消耗越多条越短）*/}
          <div className="orb-pill-inner">
            <div className="orb-pill-inner-fill" style={{ height: `${remain5h ?? 0}%` }} />
          </div>
        </div>
        {/* 平台切换标记撤除——回归
            「收起态视觉只留竖条本体」;多平台切换收敛到展开卡片头部 chip
            （与状态点/刷新钮同区,语义有上下文可解释）。*/}
      </div>

      {/* ---- 展开态：卡片（app 图标+平台 chip+双环+信息行） ----*/}
      {/* 同款：容器 deep 子树拖动（按钮豁免,S4 属性刷屏撤除）。
          密度:环缩一档/间距收紧/
          meta 合一行,卡高 320→232。*/}
      <div className={`orb-card${expanded ? '' : ' is-hidden'}`} data-tauri-drag-region="deep">
        <div className="orb-card-head">
          <img className="orb-card-logo" src={appLogo} alt="" aria-hidden="true" />
          <span className="orb-card-plan">{planLabel}</span>
          {/* 平台切换 chip（落点;仅多绑定时渲染,点击循环）*/}
          {boundSnaps.length > 1 && (
            <button
              className="orb-card-cycle"
              onClick={cyclePlatform}
              title="Switch platform"
            >
              {boundSnaps[activePlatform]?.platform ?? ''} {activePlatform + 1}/{boundSnaps.length}
            </button>
          )}
          <span className={`orb-card-dot is-${hint.level}`} title={hint.text} />
          {/* 刷新按钮（右键菜单 Refresh 项的替代落点）*/}
          <button className="orb-card-iconbtn" onClick={refreshNow} title="Refresh now" aria-label="Refresh now">
            <RefreshIcon spinning={refreshing} />
          </button>
          <button className="orb-card-collapse" onClick={collapse} title="Collapse to strip" aria-label="Collapse">
            <CollapseIcon />
          </button>
        </div>

        <div className="orb-rings">
          <Ring pct={remain7d} label="7d" size={80} />
          <Ring pct={remain5h} label="5h" size={58} />
        </div>

        <div className={`orb-card-hint is-${hint.level}`}>{hint.text}</div>

        {/* 未绑定空态（S5 提前）:订阅额度区替换为绑定引导*/}
        {boundSnaps.length === 0 && (
          <div className="orb-card-empty">
            <div className="orb-card-empty-title">No subscription bound</div>
            <div className="orb-card-empty-sub">Bind your Codex or Claude credentials in Settings to see quota here.</div>
          </div>
        )}

        {/* 底部信息行（合一行:刷新时刻 · 5h 重置 · 7d 重置同排,分隔点）*/}
        <div className="orb-card-meta">
          <span>Updated {formatFetchedAt(snap?.fetched_at)}</span>
          {w5h?.resets_at && <span><i>·</i>5h {resetCountdown(w5h.resets_at)}</span>}
          {w7d?.resets_at && <span><i>·</i>7d {resetCountdown(w7d.resets_at)}</span>}
        </div>

        <button className="orb-card-manage" onClick={() => windowService.openMainAtView('settings', 'subscriptions').catch(console.error)}>
          Manage subscriptions
        </button>
      </div>

      {/* 右键菜单（只留隐藏项;DOM 常驻浮层,铁律;
           位置钳回窗口内+收起态忽略）*/}
      <div
        ref={menuRef}
        className={`orb-menu${menu ? '' : ' is-hidden'}`}
        style={menuStyle}
      >
        <button onClick={menuHide}>Hide orb</button>
      </div>
    </div>
  )
}

/** 额度环（展开态）：剩余百分比 + 环形进度（紫蓝渐变,同族）。
 *  容器 deep drag-region 子树内——环心数字等 div 不再逐个挂属性。 */
function Ring({ pct, label, size }: { pct: number | null; label: string; size: number }) {
  const r = 44 // viewBox 100 固定半径,外层 CSS 缩放
  const circ = 2 * Math.PI * r
  const shown = pct ?? 0
  const valid = pct !== null
  return (
    <div className={`orb-ring${valid ? '' : ' is-empty'}`} style={{ width: size, height: size }}>
      <svg viewBox="0 0 100 100" aria-hidden="true">
        <circle cx="50" cy="50" r={r} className="orb-ring-track" strokeWidth="7" fill="none" />
        <circle
          cx="50" cy="50" r={r}
          className="orb-ring-fill"
          strokeWidth="7" fill="none"
          strokeDasharray={circ}
          strokeDashoffset={circ * (1 - shown / 100)}
          transform="rotate(-90 50 50)"
        />
      </svg>
      <div className="orb-ring-center">
        <span className="orb-ring-num">{valid ? Math.round(shown) : '—'}</span>
        <span className="orb-ring-label">{label}</span>
      </div>
    </div>
  )
}

function CollapseIcon() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
      <path d="M2 4 L5 1 L8 4 M2 6 L5 9 L8 6" fill="none" stroke="currentColor" strokeWidth="1.2" />
    </svg>
  )
}

function RefreshIcon({ spinning }: { spinning: boolean }) {
  return (
    <svg
      className={spinning ? 'is-spinning' : ''}
      width="11" height="11" viewBox="0 0 12 12" aria-hidden="true"
    >
      <path
        d="M10.5 6 A4.5 4.5 0 1 1 8.6 2.45 M8.4 0.9 L8.7 2.6 L7 2.9"
        fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"
      />
    </svg>
  )
}
