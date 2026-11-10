import { useState, type ReactNode } from 'react'
import { useT } from '../../lib/i18n'
import DetailDialog from '../window/DetailDialog'

/** 配置入口保留摘要；弹层 DOM 常驻，内容首次打开后保留草稿与控件状态。 */
export default function SettingsDetail({ title, summary, children, busy = false, onClose }: {
  title: string
  summary: string
  children: ReactNode
  busy?: boolean
  onClose?: () => void
}) {
  const t = useT('settings')
  const [open, setOpen] = useState(false)
  const [visited, setVisited] = useState(false)
  const close = () => {
    if (busy) return
    onClose?.()
    setOpen(false)
  }
  return (
    <div className="settings-detail-entry">
      <button className="settings-detail-link" aria-haspopup="dialog" aria-expanded={open}
        onClick={() => { setVisited(true); setOpen(true) }}>
        <span className="settings-detail-label"><strong>{title}</strong><span>{summary}</span></span>
        <span className="settings-detail-arrow" aria-hidden="true">›</span>
      </button>
      <DetailDialog open={open} title={title} onClose={close} size="settings" closeLabel={t('closeSettingDetails')} closeDisabled={busy}>
        <div className="settings-detail-content">{visited ? children : null}</div>
      </DetailDialog>
    </div>
  )
}
