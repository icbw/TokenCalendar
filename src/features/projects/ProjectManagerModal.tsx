// 项目管理弹出层:主窗口内 modal,不新开 Tauri 窗口。
// 由 FullWindow 常驻挂载;开合只切 is-open 类（visibility + pointer-events）——
// 透明窗口内浮层禁止条件卸载（否则留脏像素残影）,本层 DOM 恒在。
// 面板内容复用 Settings·Projects 同一组件与设置页面板样式;关闭时 active = false 只停取数。
import { useEffect, useState } from 'react'
import ProjectManager from './ProjectManager'
import { closeProjectManager, isProjectManagerOpen, subscribeProjectManager } from './projectManagerStore'
import { useT } from '../../lib/i18n'
import '../settings/settings.css'
import './projects.css'

export default function ProjectManagerModal() {
  const t = useT('projects')
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
      <section className="pm-modal-dialog" role="dialog" aria-modal="true" aria-label={t('manageProjects')}>
        <header className="pm-modal-header">
          <span className="pm-modal-title">{t('manageProjects')}</span>
          <span className="pm-modal-sub">{t('alsoInSettings')}</span>
          <button className="settings-back pm-modal-close" onClick={closeProjectManager} title={t('closeTitle')} aria-label={t('close')}>
            {t('close')}
          </button>
        </header>
        <div className="pm-modal-body">
          <ProjectManager active={open} />
        </div>
      </section>
    </div>
  )
}
