// materialTheme: 毛玻璃 spike（材质档位应用 + 失焦补偿）。
// 档位单一源在 designPrefs（widgetMaterial/mainMaterial）；本模块把档位变成
// Rust 调用（set_window_material）与伴随形态（material-on 类、DWM ROUND 由
// Rust 侧联动）。Mica 亮暗显式绑定当前生效 scheme（apply_mica 的 dark 参数，
// 不随系统漂移），scheme 变化时整体重应用。
//
// 失焦补偿：挂件 host backdrop 材质随失焦退化
// （变暗/退纯色）是系统设计，无参数可规避——补偿 = 失焦时把卡片底 alpha
// 抬到起点配方（浅色 .65–.8 / 深色 .55–.7，此处取中值），
// 只升不降。spike 后调优，非定论。
import { useEffect, useState } from 'react'
import { getDesignPrefs, setDesignPrefs, subscribeDesignPrefs, type DesignPrefs } from './designPrefs'
import { inTauri } from '../../services/tauri'
import { setWindowMaterial } from '../../services/windowService'

export type MaterialLabel = 'widget' | 'main'

/** 失焦补偿 alpha 起点。 */
export const BLUR_COMPENSATION_LIGHT = 0.72
export const BLUR_COMPENSATION_DARK = 0.62

export function currentSchemeDark(): boolean {
  return typeof window !== 'undefined' && window.matchMedia('(prefers-color-scheme: dark)').matches
}

/** 挂件失焦时的卡片底 alpha（null = 不补偿：材质未开或聚焦）。只升不降。 */
export function blurCompensationAlpha(prefs: DesignPrefs, focused: boolean): number | null {
  if (focused || !prefs.widgetMaterial) return null
  const target = currentSchemeDark() ? BLUR_COMPENSATION_DARK : BLUR_COMPENSATION_LIGHT
  return Math.max(prefs.bgOpacity, target)
}

function materialOf(label: MaterialLabel, prefs: DesignPrefs): 'mica' | 'acrylic' | null {
  return label === 'widget' ? prefs.widgetMaterial ?? null : prefs.mainMaterial ?? null
}

async function applyMaterial(label: MaterialLabel, prefs: DesignPrefs): Promise<void> {
  const effect = materialOf(label, prefs)
  // 材质伴随形态类（主窗口 CSS 据此切 DWM 单源圆角 + 半透明叠色）。
  document.documentElement.classList.toggle('material-on', effect !== null)
  if (!inTauri) return
  const ok = await setWindowMaterial(label, effect, currentSchemeDark())
  if (!ok && effect !== null) {
    // 探测失败静默回退关闭档（Win10 选 Mica / 未来版本漂移），清除键后
    // storage 桥会再次驱动本回调应用 none（终止条件：effect 为 null）。
    console.warn(`[material] ${label} effect "${effect}" unavailable, falling back to off`)
    setDesignPrefs(label === 'widget' ? { widgetMaterial: undefined } : { mainMaterial: undefined })
  }
}

/** 窗口入口挂载一次：初值应用 + 订阅跟随（storage 桥跨窗口同步覆盖）+
 *  系统 scheme 变化重应用（Mica 亮暗显式绑定，须重发 dark 参数）。 */
export function useMaterialSync(label: MaterialLabel): void {
  useEffect(() => {
    const run = (p: DesignPrefs) => {
      void applyMaterial(label, p)
    }
    run(getDesignPrefs())
    const off = subscribeDesignPrefs(run)
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    const onScheme = () => run(getDesignPrefs())
    mq.addEventListener('change', onScheme)
    return () => {
      off()
      mq.removeEventListener('change', onScheme)
    }
  }, [label])
}

/** 窗口焦点跟踪（挂件失焦补偿用）。初值假定失焦（挂件常态），isFocused 校正；
 *  浏览器布局调试（非 Tauri）视为聚焦（补偿不生效，行为与现状一致）。 */
export function useWindowFocus(): boolean {
  const [focused, setFocused] = useState(false)
  useEffect(() => {
    if (!inTauri) {
      setFocused(true)
      return
    }
    let disposed = false
    let unlisten: (() => void) | null = null
    void import('@tauri-apps/api/window').then(({ getCurrentWindow }) => {
      if (disposed) return
      const win = getCurrentWindow()
      void win.isFocused().then((v) => {
        if (!disposed) setFocused(v)
      })
      void win
        .onFocusChanged(({ payload }) => {
          if (!disposed) setFocused(payload)
        })
        .then((fn) => {
          if (disposed) fn()
          else unlisten = fn
        })
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [])
  return focused
}
