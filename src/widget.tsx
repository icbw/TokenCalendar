import React from 'react'
import ReactDOM from 'react-dom/client'
import WidgetWindow from './features/window/WidgetWindow'
import './styles/tokens.css'
import './styles/globals.css'

// 挂件窗口入口（widget.html，tauri.conf.json label=widget）。
// 浏览器布局调试：vite dev server 直接开 /widget.html。
ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <WidgetWindow />
  </React.StrictMode>,
)
