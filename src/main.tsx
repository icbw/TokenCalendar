import React from 'react'
import ReactDOM from 'react-dom/client'
import FullWindow from './features/window/FullWindow'
import './styles/tokens.css'
import './styles/globals.css'

// 主窗口入口（index.html，tauri.conf.json label=main）：UI 组合在 FullWindow。
// 挂件入口见 widget.tsx（widget.html，label=widget）。
ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <FullWindow />
  </React.StrictMode>,
)
