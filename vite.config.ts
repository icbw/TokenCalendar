import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Tauri 前端配置：固定端口供 tauri dev 使用；构建目标对齐 WebView2。
// 多入口：main → index.html（主窗口）、widget → widget.html（挂件）、orb → orb.html（悬浮球）、
// timeline → timeline.html（项目推进时间轴），与 tauri.conf.json 的 per-window url 一一对应，
// 装配在构建期定型，无运行时嗅探。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    target: 'es2021',
    rollupOptions: {
      input: {
        main: 'index.html',
        widget: 'widget.html',
        orb: 'orb.html',
        timeline: 'timeline.html',
      },
    },
  },
})
