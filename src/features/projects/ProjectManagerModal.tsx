// 项目管理弹出层:主窗口内 modal,不新开 Tauri 窗口。
// 由 FullWindow 常驻挂载;开合只切 is-open 类（visibility + pointer-events）——
// 透明窗口内浮层禁止条件卸载（否则留脏像素残影）,本层 DOM 恒在。
// 面板内容复用 Settings·Projects 同一组件与设置页面板样式;关闭时 active = false 只停取数。
import { useEffect, useState } from 'react'
import ProjectManager from './ProjectManager'
import { closeProjectManager, isProjectManagerOpen, subscribeProjectManager } from './projectManagerStore'
import '../settings/settings.css'
import './projects.css'

export default function ProjectManagerModal() {
  const [open, setOpen] = useState(isProjectManagerOpen)
  useEffect(() => subscribeProjectManager(setOpen), [])

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeProjectManager()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open])

  return (
    <div className={`pm-modal${open ? ' is-open' : ''}`} aria-hidden={!open}>
      <div className="pm-modal-backdrop" onClick={closeProjectManager} />
      <section className="pm-modal-dialog" role="dialog" aria-modal="true" aria-label="Manage projects">
        <header className="pm-modal-header">
          <span className="pm-modal-title">Manage projects</span>
          <span className="pm-modal-sub">Also in Settings › Projects</span>
          <button className="settings-back pm-modal-close" onClick={closeProjectManager} title="Close (Esc)" aria-label="Close">
            Close
          </button>
        </header>
        <div className="pm-modal-body">
          <ProjectManager active={open} />
        </div>
      </section>
    </div>
  )
}
