// MatrixTooltip：主窗口月矩阵与挂件年度热力图共用的悬浮提示（统一）。
// 格式单一源：标题 `${label} · ${date}` + 行 `Tokens: X` / `No usage`（+
// estimated 语义行）。fixed 定位 + 窗口内钳制（挂件窗口矮，上下边缘翻转）。
import { useEffect, useRef, useState } from 'react'
import './matrixTooltip.css'

export interface TooltipContent {
  title: string
  lines: string[]
}

interface MatrixTooltipProps {
  content: TooltipContent | null
  /** 锚点元素（格子）：tooltip 显示在其上方，越界时翻转到下方并水平钳制。 */
  anchor: HTMLElement | null
  /** 顶部保留区高度（px，默认 0）：挂件 hover 时顶部有 chrome 浮层，tooltip
   * 即使数学上放得下也会与其重叠——锚点上方空间小于保留区时直接翻到下方。
   * 主窗口无浮层，不传即保持原行为。 */
  topReserve?: number
}

/** 计算窗口内钳制后的坐标：默认锚点上方居中；左/右越界夹回；顶部空间不足
 * （含 topReserve 保留区）翻到锚点下方。 */
function placeFor(
  anchor: HTMLElement,
  el: HTMLElement,
  topReserve: number,
): { left: number; top: number; below: boolean } {
  const a = anchor.getBoundingClientRect()
  const t = el.getBoundingClientRect()
  const margin = 4
  let left = a.left + a.width / 2
  left = Math.max(margin, Math.min(left, window.innerWidth - margin - t.width))
  const above = a.top - t.height - margin
  const below = a.bottom + margin
  const flip = above < margin + topReserve
  return { left, top: flip ? below : above, below: flip }
}

export default function MatrixTooltip({ content, anchor, topReserve = 0 }: MatrixTooltipProps) {
  const ref = useRef<HTMLDivElement>(null)
  const [placed, setPlaced] = useState<{ left: number; top: number; below: boolean } | null>(null)

  // content 变化（换格子）时按新锚点重算位置；首次渲染后测量 tooltip 尺寸。
  useEffect(() => {
    if (!content || !anchor || !ref.current) {
      setPlaced(null)
      return
    }
    setPlaced(placeFor(anchor, ref.current, topReserve))
  }, [content, anchor, topReserve])

  // DOM 常驻（教训：条件卸载会在透明 WebView2 上留脏像素——右下角
  // 白块残影）。隐藏时仅移出窗口 + visibility，合成层不销毁。
  const visible = content !== null
  const { left, top, below } = placed ?? { left: -9999, top: -9999, below: false }
  return (
    <div
      ref={ref}
      className={`matrix-tooltip${below ? ' is-below' : ''}${visible ? '' : ' is-hidden'}`}
      style={{ left, top }}
      role="tooltip"
      aria-hidden={!visible}
    >
      <div className="matrix-tooltip-title">{content?.title ?? ''}</div>
      {(content?.lines ?? []).map((l, i) => (
        <div key={i} className="matrix-tooltip-line">{l}</div>
      ))}
    </div>
  )
}
