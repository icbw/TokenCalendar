import { useState, type ReactNode } from 'react'
import './details.css'

/** 普通内容折叠，不是浮层；首次打开才装载，之后保留内部筛选状态。 */
export default function Disclosure({ title, children, initialOpen = false }: {
  title: string; children: ReactNode | ((open: boolean) => ReactNode); initialOpen?: boolean
}) {
  const [open, setOpen] = useState(initialOpen)
  const [visited, setVisited] = useState(initialOpen)
  return (
    <details className="detail-disclosure" open={open} onToggle={(event) => {
      setOpen(event.currentTarget.open)
      if (event.currentTarget.open) setVisited(true)
    }}>
      <summary>{title}</summary>
      <div className="detail-disclosure-body">{visited ? (typeof children === 'function' ? children(open) : children) : null}</div>
    </details>
  )
}
