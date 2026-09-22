import React from 'react'
import ReactDOM from 'react-dom/client'
import TimelineWindow from './features/timeline/TimelineWindow'
import './styles/tokens.css'
import './styles/globals.css'

// 项目推进时间轴窗口入口（timeline.html，tauri.conf.json label=timeline）。
// 浏览器布局调试：vite dev server 直接开 /timeline.html。
ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <TimelineWindow />
  </React.StrictMode>,
)
