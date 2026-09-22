// mainTheme: 主界面外观（顶栏色/顶栏 alpha/主体色/边框色）。
// 与 widgetTheme 同机制：hex → CSS 变量覆写，undefined = 清除覆写回 scheme。
// **仅主窗口入口（FullWindow）挂载**——--shell-bg/--panel/--border 是主窗口
// 体系变量；挂件窗口不读这三个变量（.shell.is-widget 全透明全出血），
// 因此改主界面取色时挂件外围零变化。
//
// 顶栏 alpha 独立于挂件 bgOpacity：shell.css 读 --shell-bg-alpha（默认 0.8）；
// 最大化态转实底（alpha 不参与，见 shell.css）。
import { getDesignPrefs, subscribeDesignPrefs, type DesignPrefs } from './designPrefs'
import { hexToRgbTriple } from './widgetTheme'

/** 把主界面 theme 覆写到 documentElement（缺省字段逐一清除）。 */
export function applyMainTheme(prefs: DesignPrefs): void {
  const root = document.documentElement
  const titlebar = prefs.titlebarBg ? hexToRgbTriple(prefs.titlebarBg) : null
  if (titlebar) root.style.setProperty('--shell-bg', titlebar)
  else root.style.removeProperty('--shell-bg')

  if (prefs.titlebarAlpha !== undefined) {
    root.style.setProperty('--shell-bg-alpha', String(prefs.titlebarAlpha))
  } else {
    root.style.removeProperty('--shell-bg-alpha')
  }

  if (prefs.panelBg) root.style.setProperty('--panel', prefs.panelBg)
  else root.style.removeProperty('--panel')

  if (prefs.borderColor) root.style.setProperty('--border', prefs.borderColor)
  else root.style.removeProperty('--border')
}

/** 主窗口入口挂载一次：初值 + 订阅跟随（storage 桥跨窗口同步天然覆盖）。 */
export function useMainThemeSync(): void {
  applyMainTheme(getDesignPrefs())
  subscribeDesignPrefs(applyMainTheme)
}
