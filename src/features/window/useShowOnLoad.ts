// 窗口生命周期：首帧提交后再请后端显示（白闪对策的形态）。
// 窗口带 visible 创建会让 WebView2 画出白色首帧（tauri#4881 家族），所以两个
// 窗口都 visible:false 创建；双 rAF 等首个透明帧提交后调 window_ready，由 Rust
// 按可见性单一源裁决是否 show（widget 默认显示、main 默认隐藏，恢复自落盘状态）。
import { useEffect } from 'react'
import { inTauri, windowService } from '../../services'

export function useShowOnLoad() {
  useEffect(() => {
    if (!inTauri) return
    let innerRaf = 0
    const outerRaf = requestAnimationFrame(() => {
      innerRaf = requestAnimationFrame(() => {
        void windowService.windowReady()
      })
    })
    return () => {
      cancelAnimationFrame(outerRaf)
      cancelAnimationFrame(innerRaf)
    }
  }, [])
}
