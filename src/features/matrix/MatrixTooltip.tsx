// MatrixTooltip：主窗口月矩阵与挂件年度热力图共用的悬浮提示。
// 格式单一源：标题 `${label} · ${date}` + 行 `Tokens: X` / `No usage`（+
// estimated 语义行）。fixed 定位 + 窗口内钳制（挂件窗口矮，上下边缘翻转）。
import { useLayoutEffect, useRef, useState } from 'react'
import './matrixTooltip.css'

export interface TooltipContent {
  title: string
  lines: string[]
}

interface MatrixTooltipProps {
  content: TooltipContent | null
  /** 锚点元素（格子）：tooltip 显示在其上方，越界时翻转到下方并钳制在窗口内。 */
  anchor: HTMLElement | null
  /** 顶部保留区高度（px，默认 0）：挂件 hover 时顶部有 chrome 浮层，tooltip
   * 即使数学上放得下也会与其重叠——锚点上方空间小于保留区时直接翻到下方。
   * 主窗口无浮层，不传即可。 */
  topReserve?: number
  /** 紧凑单行变体（挂件用）：两行浮层高 ≈52px，在挂件矮窗里等于压住 2〜3 行格子，
   * 滑到相邻行时看不到自己 hover 的行。紧凑变体 = 单行「日期 · 读数」，高 ≈23px
   * （一行格子），只压相邻一行。主窗口空间充裕，保持完整两行格式。 */
  compact?: boolean
}

/** 计算窗口内钳制后的坐标，保证 tooltip 完整可见（不被窗口边裁切）。
 *
 * 1. **水平**：CSS 是 translate（-50%)，left 是**中心点**——必须按半宽钳制，
 * 否则最左几列的 tooltip 左半截被窗口边切掉。
 * 2. **垂直**：必须检查目标落位的下边是否出窗——挂件矮窗（92/134/155 三档）里
 * 中间几行翻到下方后会被窗口底边裁掉。定序：上方优先（避开顶部保留区）→
 * 下方（须完整放得下）→ 两侧都放不下时取空间更大的一侧，再钳进窗口。
 * 不靠放大窗口解决：窗口 = 卡片是贴边方案与材质态的硬约束，出界是定位数学问题。 */
function placeFor(
  anchor: HTMLElement,
  el: HTMLElement,
  topReserve: number,
): { left: number; top: number; below: boolean } {
  const a = anchor.getBoundingClientRect()
  const t = el.getBoundingClientRect()
  const margin = 4
  const vw = window.innerWidth
  const vh = window.innerHeight

  // 水平：left 是中心点，按半宽夹回；窗口比 tooltip 还窄时退化为左对齐。
  const half = t.width / 2
  const minCx = margin + half
  const maxCx = vw - margin - half
  const cx = a.left + a.width / 2
  const left = maxCx >= minCx ? Math.min(Math.max(cx, minCx), maxCx) : minCx

  // 垂直：两个候选落位都换算成「渲染后的上边」再比较（below 标志决定 CSS
  // 位移基线：below = 顶边即 top；above = 底边即 top，CSS translateY（-100%)）。
  const boxAbove = a.top - margin - t.height
  const boxBelow = a.bottom + margin
  const minTop = margin + topReserve
  const maxTop = vh - margin - t.height
  let boxTop: number
  let below: boolean
  if (boxAbove >= minTop) {
    boxTop = boxAbove
    below = false
  } else if (boxBelow <= maxTop) {
    boxTop = boxBelow
    below = true
  } else {
    // 两侧都放不下（挂件矮窗）：取空间更大的一侧，再钳进窗口保证完整可。
    below = vh - margin - a.bottom >= a.top - margin - topReserve
    boxTop = Math.max(margin, Math.min(below ? boxBelow : boxAbove, Math.max(margin, maxTop)))
  }
  return { left, top: below ? boxTop : boxTop + t.height, below }
}

export default function MatrixTooltip({ content, anchor, topReserve = 0, compact = false }: MatrixTooltipProps) {
  const ref = useRef<HTMLDivElement>(null)
  const [placed, setPlaced] = useState<{ left: number; top: number; below: boolean } | null>(null)

  // content/anchor 变化（换格子）时按新锚点重算位置；用 useLayoutEffect 而非
  // useEffect：绘制前完成定位，滑动换格不会先把新内容画在旧位置再跳一帧。
  useLayoutEffect(() => {
    if (!content || !anchor || !ref.current) {
      setPlaced(null)
      return
    }
    setPlaced(placeFor(anchor, ref.current, topReserve))
  }, [content, anchor, topReserve])

  // DOM 常驻：条件卸载会在透明 WebView2 上留脏像素（右下角白块残影）。
  // 隐藏时仅移出窗口 + visibility，合成层不销毁。
  const visible = content !== null
  const { left, top, below } = placed ?? { left: -9999, top: -9999, below: false }
  return (
    <div
      ref={ref}
      className={`matrix-tooltip${compact ? ' is-compact' : ''}${below ? ' is-below' : ''}${visible ? '' : ' is-hidden'}`}
      style={{ left, top }}
      role="tooltip"
      aria-hidden={!visible}
    >
      {compact ? (
        <div className="matrix-tooltip-line">
          {content ? [content.title, ...content.lines].join(' · ') : ''}
        </div>
      ) : (
        <>
          <div className="matrix-tooltip-title">{content?.title ?? ''}</div>
          {(content?.lines ?? []).map((l, i) => (
            <div key={i} className="matrix-tooltip-line">{l}</div>
          ))}
        </>
      )}
    </div>
  )
}
