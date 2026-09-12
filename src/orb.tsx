import React from 'react'
import ReactDOM from 'react-dom/client'
import OrbWindow from './features/orb/OrbWindow'
import './styles/tokens.css'
import './styles/globals.css'

// 悬浮球窗口入口（orb.html，tauri.conf.json label=orb，空壳）。
// 浏览器布局调试：vite dev server 直接开 /orb.html。
ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <OrbWindow />
  </React.StrictMode>,
)
