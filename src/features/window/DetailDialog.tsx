import { useEffect, useRef, type ReactNode } from 'react'
import { useT } from '../../lib/i18n'
import './details.css'

/** 浮层 DOM 常驻；浏览器负责焦点圈定、背景 inert 与关闭后的焦点恢复。 */
export default function DetailDialog({ open, title, onClose, children, size = 'full', closeLabel, closeDisabled = false }: {
  open: boolean
  title: string
  onClose: () => void
  children: ReactNode
  size?: 'full' | 'settings'
  closeLabel?: string
  closeDisabled?: boolean
}) {
  const t = useT('window')
  const ref = useRef<HTMLDialogElement>(null)
  useEffect(() => {
    const dialog = ref.current
    if (!dialog) return
    if (open && !dialog.open) dialog.showModal()
    else if (!open && dialog.open) dialog.close()
  }, [open])
  return (
    <dialog
      ref={ref}
      className={`detail-dialog${size === 'settings' ? ' is-settings' : ''}`}
      aria-label={title}
      onCancel={(event) => { event.preventDefault(); if (!closeDisabled) onClose() }}
      onClick={(event) => { if (!closeDisabled && event.target === event.currentTarget) onClose() }}
    >
      <section className="detail-panel">
        <header className="detail-header">
          <h2>{title}</h2>
          <button className="seg" onClick={onClose} disabled={closeDisabled} autoFocus>{closeLabel ?? t('closeDetails')}</button>
        </header>
        <div className="detail-body">{children}</div>
      </section>
    </dialog>
  )
}
