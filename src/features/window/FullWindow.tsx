// FullWindow：主窗口薄壳（现代化：frameless + 自绘标题栏 + 四层栅格）。
// 层次：TitleBar（品牌 + 窗口控制）/ Toolbar（矩阵视角与筛选，UsageMatrixView 内）
// / 内容 / StatusFooter（采集状态）。：设置自右缘窄面板改为内容区独立
// 设置页（SettingsPage，三子 tab），标题栏齿轮 = 矩阵 ⇄ 设置顶层视图切换。
// 窗口操作 Rust 命令（main_minimize/toggle_maximize/close）；拖动区用
// data-tauri-drag-region；最大化状态经 main-maximized-changed 广播切换圆角与按钮态。
import { useCallback, useEffect, useLayoutEffect, useState } from 'react'
import appLogo from '../../assets/app-logo.png'
import UsageMatrixView from '../matrix/UsageMatrixView'
import MatrixPanel from '../matrix/MatrixPanel'
import CollectorHealth from '../collectors/CollectorHealth'
import SettingsPage, { type SettingsTab } from '../settings/SettingsPage'
import ProjectManagerModal from '../projects/ProjectManagerModal'
import InsightsView from '../insights/InsightsView'
import TasksView from '../tasks/TasksView'
import type { GroupBy as MatrixGroupBy } from '../matrix/UsageMatrixView'
import { events, updateService, windowService } from '../../services'
import { getDesignPrefs } from '../settings/designPrefs'
import { useShowOnLoad } from './useShowOnLoad'
import { useMainThemeSync } from '../settings/mainTheme'
import { useMaterialSync } from '../settings/materialTheme'
import { useRadiusSchemeSync } from '../settings/radiusTheme'
import './shell.css'

// 矩阵与图表拆成两个独立视图按钮——'matrix'（热力图）
// 与 'insights'（洞察图表）本来就是两个视图,共用主内容区三态互斥;齿轮（settings)
// 逻辑不变。'matrix' 视图 = 矩阵 + 下方全系列联动曲线（跟随矩阵 groupBy）。
// 'tasks'（任务列表）为第三个视图按钮,与 matrix / insights 三态互斥。
type MainView = 'matrix' | 'insights' | 'tasks' | 'settings'

export default function FullWindow() {
  useShowOnLoad()
  // 主界面主题（顶栏/主体/边框取色 + 顶栏 alpha）仅主窗口消费；
  // 挂件窗口不读这些变量，独立性验收=改主界面取色挂件外围零变化。
  useMainThemeSync()
  // 毛玻璃材质档（本窗口自己的 mainMaterial 键；Rust 联动
  // DWM ROUND，CSS 侧 material-on 类切半透明叠色）。
  useMaterialSync('main')
  // 圆角方案档（panel 值 → --radius-card；与挂件同档对齐）。
  useRadiusSchemeSync('main')

  // 主窗口也走透明宿主约定（globals.css .mode-expanded 全透明）
  useLayoutEffect(() => {
    document.documentElement.classList.add('mode-expanded')
  }, [])

  const [view, setView] = useState<MainView>('matrix')
  const [settingsTab, setSettingsTab] = useState<SettingsTab>('general')
  const [widgetVisible, setWidgetVisible] = useState(false)
  // 悬浮球可见性（顶栏 Orbit 按钮；与托盘/设置页同源单一广播）
  const [orbVisible, setOrbVisible] = useState(false)
  const [maximized, setMaximized] = useState(false)

  // orb「订阅设置」导航中转消费（openMainAtView 写 localStorage——
  // orb 与 main 是两个 WebView 窗口,sessionStorage 按窗口隔离（orb 写入 main
  // 永远读不到）,必须走跨窗口共享的 localStorage,与 designPrefs 桥同款机制）。
  // 两条消费路径：挂载时读一次（主窗口首次创建——storage 事件不会补发给
  // 尚未存在的窗口）;常驻 storage 事件（主窗口已启动、只是隐藏时,它自己
  // 不会重新挂载,只有事件路径能把导航送到）。消费即清除,防下次加载重跳。
  const applyMainNav = useCallback((raw: string | null) => {
    if (!raw) return
    try {
      const nav = JSON.parse(raw) as { view?: string; tab?: string }
      if (nav.view === 'settings') {
        const t = nav.tab
        setSettingsTab(
          t === 'general' || t === 'appearance' || t === 'projects' || t === 'data' || t === 'subscriptions' || t === 'about'
            ? t
            : 'general',
        )
        setView('settings')
      }
    } catch {
      /* 损坏载荷忽略 */
    }
  }, [])
  useLayoutEffect(() => {
    try {
      const raw = localStorage.getItem(windowService.MAIN_NAV_KEY)
      if (raw) {
        localStorage.removeItem(windowService.MAIN_NAV_KEY)
        applyMainNav(raw)
      }
    } catch {
      /* private mode */
    }
  }, [applyMainNav])
  useEffect(() => {
    // storage 事件只在**其它**窗口写入时触发（本窗口 remove 不触发自己）——
    // newValue=null（清除广播）直接忽略。
    const onStorage = (e: StorageEvent) => {
      if (e.key !== windowService.MAIN_NAV_KEY || !e.newValue) return
      try {
        localStorage.removeItem(windowService.MAIN_NAV_KEY)
      } catch {
        /* private mode */
      }
      applyMainNav(e.newValue)
    }
    window.addEventListener('storage', onStorage)
    return () => window.removeEventListener('storage', onStorage)
  }, [applyMainNav])

  // 9.1 联动：'model' 视图的曲线跟随矩阵 groupBy（Model 视角 → 全模型曲线;
  // Agent 视角 → 全 Agent 曲线）。groupBy 状态提升到 FullWindow,UsageMatrixView
  // 受控消费;切到 chart 视图再回来时保持上次视角。
  const [matrixGroupBy, setMatrixGroupByState] = useState<MatrixGroupBy>('agent')
  // v3.1 图表面板状态（会话记忆,切视图/重启窗口不丢——保持在 FullWindow 不随
  // 面板卸载重置）:选中行（null = 预设全系列）、折叠、合计模式。
  const [panelRow, setPanelRow] = useState<string | null>(null)
  const [panelCollapsed, setPanelCollapsed] = useState(false)
  const [panelTotalOnly, setPanelTotalOnly] = useState(false)
  // 进出 project 维时清掉面板选中行——项目键与 agent / model 键不同族,
  // 带过去只会钻取出空曲线（agent ⇄ model 之间的既有行为不动）。
  const setMatrixGroupBy = useCallback(
    (g: MatrixGroupBy) => {
      if (g !== matrixGroupBy && (matrixGroupBy === 'project' || g === 'project')) setPanelRow(null)
      setMatrixGroupByState(g)
    },
    [matrixGroupBy],
  )
  const handleRowSelect = useCallback((rowKey: string | null) => {
    // 与 chart 页行联动同逻辑:点行名 → 显示该行;再点同一行 → 恢复预设
    setPanelRow((prev) => (prev === rowKey ? null : rowKey))
  }, [])

  // 挂件显隐状态：挂载时拉初值 + 事件跟随（拖动/托盘/挂件侧开合都汇入同一广播）
  useEffect(() => {
    void windowService
      .getVisibility()
      .then((v) => {
        if (v) setWidgetVisible(v.widget)
      })
      .catch(() => {})
    let off: (() => void) | null = null
    void events.onWidgetVisibilityChanged(setWidgetVisible).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])

  // 悬浮球显隐状态（初值与事件同 widget 族；托盘/设置页路径同源汇入）
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

  // 最大化状态（Rust Resized 去重广播）：标题栏按钮态 + 内容圆角归零
  useEffect(() => {
    let off: (() => void) | null = null
    void events.onMainMaximizedChanged(setMaximized).then((unlisten) => {
      off = unlisten
    })
    return () => {
      off?.()
    }
  }, [])

  // 应用更新（设置·About 的自动更新开关，默认开）：安装版启动后延迟检查，
  // 有新版直接下载并安装（签名校验在 updater 插件内完成；Windows 上应用会随
  // 安装器退出）。dev 构建整段跳过——dev 的 identifier 与安装版不同，
  // 在 dev 里执行安装会把正式版装进系统。
  useEffect(() => {
    if (import.meta.env.DEV) return
    const timer = window.setTimeout(() => {
      void (async () => {
        if (!getDesignPrefs().autoUpdate) return
        const r = await updateService.checkForUpdate()
        if (r.status !== 'available' || !r.canInstall) return
        await r.install(() => {})
      })().catch((e) => console.error('[update] auto update failed:', e))
    }, 5000)
    return () => window.clearTimeout(timer)
  }, [])

  const toggleWidget = useCallback(() => {
    windowService.toggleWidget().catch(console.error)
  }, [])
  // 顶栏 Orbit 按钮（widget 左侧，独立功能层级——桌面悬浮球形态开关）
  const toggleOrb = useCallback(() => {
    windowService.toggleOrb().catch(console.error)
  }, [])
  const minimize = useCallback(() => windowService.mainMinimize().catch(console.error), [])
  const toggleMaximize = useCallback(() => windowService.mainToggleMaximize().catch(console.error), [])
  const close = useCallback(() => windowService.mainClose().catch(console.error), [])

  return (
    <div className={`shell is-expanded${maximized ? ' is-maximized' : ''}`}>
      {/* 自绘标题栏：拖动区 + 双击最大化；控制按钮不拖动*/}
      <header
        className="titlebar"
        data-tauri-drag-region
        onDoubleClick={toggleMaximize}
      >
        <div className="titlebar-brand" data-tauri-drag-region>
          {/* 品牌位用 app 图标（原 CSS 渐变紫贴图弃用）,
              资产与 bundle 图标同源（src-tauri/icons/128x128.png 拷贝）*/}
          <img className="titlebar-logo" src={appLogo} alt="" aria-hidden="true" />
          <span className="titlebar-title">TokenCalendar</span>
        </div>
        <div className="titlebar-actions">
          {/* Orbit 入口（widget 左侧,与其他按钮并列同级——桌面悬浮球
              独立功能形态;按钮态经 orb-visibility-changed 广播跟随,与托盘/设置页
              同一单一源）*/}
          <button
            className={`seg titlebar-view titlebar-orb${orbVisible ? ' is-active' : ''}`}
            onClick={toggleOrb}
            title={orbVisible ? 'Hide orbit orb' : 'Show orbit orb'}
          >
            Orbit
          </button>
          <button
            className={`seg titlebar-widget${widgetVisible ? ' is-active' : ''}`}
            onClick={toggleWidget}
            title={widgetVisible ? 'Hide widget window' : 'Show widget window'}
          >
            Widget
          </button>
          {/* 按钮名回归原版——Matrix / Insights（文字 seg,
              与 Widget 同款组件形式）;三态互斥,再点活动按钮回 matrix。*/}
          <button
            className={`seg titlebar-view${view === 'matrix' ? ' is-active' : ''}`}
            onClick={() => setView('matrix')}
            title="Matrix view"
          >
            Matrix
          </button>
          <button
            className={`seg titlebar-view${view === 'insights' ? ' is-active' : ''}`}
            onClick={() => setView('insights')}
            title="Insights charts"
          >
            Insights
          </button>
          <button
            className={`seg titlebar-view${view === 'tasks' ? ' is-active' : ''}`}
            onClick={() => setView('tasks')}
            title="Tasks: per-session turns, steps and time"
          >
            Tasks
          </button>
          <button
            className={`seg titlebar-view${view === 'settings' ? ' is-active' : ''}`}
            onClick={() => setView((v) => (v === 'settings' ? 'matrix' : 'settings'))}
            title="Settings"
          >
            Settings
          </button>
          <div className="titlebar-sep" />
          <button className="titlebar-btn" onClick={minimize} title="Minimize" aria-label="Minimize">
            <MinimizeIcon />
          </button>
          <button
            className="titlebar-btn"
            onClick={toggleMaximize}
            title={maximized ? 'Restore' : 'Maximize'}
            aria-label={maximized ? 'Restore' : 'Maximize'}
          >
            {maximized ? <RestoreIcon /> : <MaximizeIcon />}
          </button>
          <button className="titlebar-btn is-close" onClick={close} title="Close" aria-label="Close">
            <CloseIcon />
          </button>
        </div>
      </header>

      <div className="shell-body">
        <main className="shell-main">
          {view === 'settings' ? (
            <SettingsPage onBack={() => setView('matrix')} initialTab={settingsTab} />
          ) : view === 'insights' ? (
            <InsightsView />
          ) : view === 'tasks' ? (
            <TasksView />
          ) : (
            <>
              {/* 热力图主体化——矩阵区 flex:1 全量显示（去滚动）,图表面板
                  固定高度可折叠;点行名联动替换预设曲线,再点恢复（逻辑与 chart 页
                  行联动一致）。矩阵内部行名点击经 onRowSelect 上抛。
                  matrix-stage 包裹矩阵+面板——面板消费的 --cells-w 等
                  共享变量在 UsageMatrixView 内部生成,必须是其后代才能继承
                  （此前面板是兄弟节点,变量断链 → 图表错位/按钮飞出）。*/}
              <div className="matrix-stage">
                <UsageMatrixView
                  groupBy={matrixGroupBy}
                  onGroupByChange={setMatrixGroupBy}
                  selectedRow={panelRow}
                  onRowSelect={handleRowSelect}
                />
                <MatrixPanel
                  groupBy={matrixGroupBy}
                  selectedRow={panelRow}
                  collapsed={panelCollapsed}
                  totalOnly={panelTotalOnly}
                  onToggleCollapsed={() => setPanelCollapsed((c) => !c)}
                  onToggleTotalOnly={() => setPanelTotalOnly((t) => !t)}
                  onClearRow={() => setPanelRow(null)}
                />
              </div>
              <CollectorHealth />
            </>
          )}
        </main>
      </div>
      {/* 项目管理弹出层（Tasks / Insights 的「Manage projects…」入口;DOM 常驻,只切类名显隐）*/}
      <ProjectManagerModal />
    </div>
  )
}

function MinimizeIcon() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
      <line x1="0" y1="5" x2="10" y2="5" stroke="currentColor" strokeWidth="1" />
    </svg>
  )
}

function MaximizeIcon() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
      <rect x="0.5" y="0.5" width="9" height="9" rx="1" fill="none" stroke="currentColor" />
    </svg>
  )
}

function RestoreIcon() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
      <rect x="0.5" y="2.5" width="7" height="7" rx="1" fill="none" stroke="currentColor" />
      <path d="M2.5 2.5 V0.5 H9.5 V7.5 H7.5" fill="none" stroke="currentColor" />
    </svg>
  )
}

function CloseIcon() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
      <line x1="0" y1="0" x2="10" y2="10" stroke="currentColor" strokeWidth="1" />
      <line x1="10" y1="0" x2="0" y2="10" stroke="currentColor" strokeWidth="1" />
    </svg>
  )
}

