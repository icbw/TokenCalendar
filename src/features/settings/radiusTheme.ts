// radiusTheme: 圆角方案主题化（全局圆角统一为可设置参数）。
// 三档预设（RADIUS_PRESETS 单一源在 designPrefs），每档拆分主面板 panel /
// 挂件 widget 两个值（当前同档对齐相等，结构拆分留独立演化空间）。
// 两窗口参数按红线拆分（GUIDE）：主窗口覆写 --radius-card（.shell-main
// 及主窗口内部圆角族消费），挂件覆写 --widget-card-radius（.year-card 消费，
// 原硬编码 16px 改量化）；各自入口只挂本窗口的 label。
// 材质态窗口圆角由 DWM 裁切（系统 ~8px），CSS 方案不作用——选 small 档
// （8px）即可与材质态一致。
import { useEffect } from 'react'
import { getDesignPrefs, subscribeDesignPrefs, RADIUS_PRESETS } from './designPrefs'

function applyRadiusScheme(label: 'widget' | 'main'): void {
  const { radiusScheme } = getDesignPrefs()
  const preset = RADIUS_PRESETS[radiusScheme ?? 'large']
  const root = document.documentElement
  if (label === 'main') {
    root.style.setProperty('--radius-card', `${preset.panel}px`)
    root.style.removeProperty('--widget-card-radius')
  } else {
    root.style.setProperty('--widget-card-radius', `${preset.widget}px`)
    root.style.removeProperty('--radius-card')
  }
}

/** 窗口入口挂载一次：初值 + storage 桥跟随（跨窗口同步与主题字段同款）。 */
export function useRadiusSchemeSync(label: 'widget' | 'main'): void {
  useEffect(() => {
    applyRadiusScheme(label)
    return subscribeDesignPrefs(() => applyRadiusScheme(label))
  }, [label])
}
