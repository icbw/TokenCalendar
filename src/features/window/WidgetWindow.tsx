// WidgetWindow：挂件窗口薄壳（双窗口拆分，自 AppShell 的 widget 分支独立）。
// 仅渲染年度热力图卡片；透明由 html.mode-widget 保证（本窗口恒为挂件形态，
// 不再有模式往返）。与主窗口独立运行：设计偏好经 localStorage 跨窗口同步
// （自定义卡片色即走此桥，挂件窗口订阅 designPrefs 即时生效），
// 显隐状态经 Rust 广播（widget-visibility-changed）。
import { useLayoutEffect } from 'react'
import YearMatrix from '../matrix/YearMatrix'
import { useShowOnLoad } from './useShowOnLoad'
import { useWidgetThemeSync } from '../settings/widgetTheme'
import { useMaterialSync } from '../settings/materialTheme'
import { useRadiusSchemeSync } from '../settings/radiusTheme'
import './shell.css'

export default function WidgetWindow() {
  useShowOnLoad()
  useWidgetThemeSync()
  // 毛玻璃材质档（挂件自己的 widgetMaterial 键，设置页改键经
  // storage 桥广播到这里应用；DWM 边缘原子组合不动，见 effects.rs 注释）。
  useMaterialSync('widget')
  // 圆角方案档（widget 值 → --widget-card-radius；与主面板同档对齐）。
  useRadiusSchemeSync('widget')

  // 挂件窗口全程透明（透明检查清单）：宿主层不得有不透明默认背景
  useLayoutEffect(() => {
    document.documentElement.classList.add('mode-widget')
  }, [])

  return (
    <div className="shell is-widget">
      <YearMatrix />
    </div>
  )
}
