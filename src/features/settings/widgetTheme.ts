// widgetTheme: 挂件美化（卡片背景色可配置 + 对比度自动派生）。
// 单一职责：读 DesignPrefs.widgetCardBg（hex 或 undefined=跟随内置 scheme），
// 换算成 RGB 三元组覆写 CSS 变量，并按 WCAG 相对亮度派生边框/零值格/文字色。
// 两个窗口（main 的预览/设置页、widget 的 YearMatrix）都调用 applyWidgetTheme，
// 保证任意一侧取值口径一致；跨窗口同步走 designPrefs 的 storage 桥。
//
// 对比度守卫：卡片可被调成深色，卡片内文字/边框/零值格不能
// 仍取浅色内置值——全部从卡片底色亮度派生，深浅底两态自动切换。
import { getDesignPrefs, subscribeDesignPrefs, type DesignPrefs } from './designPrefs'

/** hex （#rgb/#rrggbb) → "r g b" 三元组；非法输入返回 null（回到 scheme 内置值）。 */
export function hexToRgbTriple(hex: string): string | null {
  const m = /^#?([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(hex.trim())
  if (!m) return null
  let h = m[1]
  if (h.length === 3) h = h[0] + h[0] + h[1] + h[1] + h[2] + h[2]
  const n = parseInt(h, 16)
  return `${(n >> 16) & 255} ${(n >> 8) & 255} ${n & 255}`
}

/** WCAG 相对亮度（0=黑，1=白）。 */
export function luminance(triple: string): number {
  const [r, g, b] = triple.split(/\s+/).map((v) => {
    const c = Number(v) / 255
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4
  })
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

/** 在「与卡片底保持 delta ~20」的约束下派生零值格底色（GitHub 白底同级的
 * 既有规则）：浅底取更暗一档，深底取更亮一档。 */
function deriveZeroBg(triple: string, light: boolean): string {
  const [r, g, b] = triple.split(/\s+/).map(Number)
  // delta ~20：浅底压暗 20，深底提亮 20（不越过端点）
  const adj = (v: number) => Math.max(0, Math.min(255, light ? v - 20 : v + 20))
  return `rgb(${adj(r)} ${adj(g)} ${adj(b)})`
}

/** 卡片内文字色：浅底取深字、深底取浅字（对比度 > 7:1 的安全档）。 */
function deriveText(triple: string, light: boolean): string {
  void triple
  return light ? '#1e293b' : '#e2e8f0'
}

/** 次级文字（月标签）：同向切换、降一档对比。 */
function deriveTextMuted(triple: string, light: boolean): string {
  void triple
  return light ? '#64748b' : '#94a3b8'
}

/** 卡片描边：color-mix 思路的手工版——浅底压暗 8%、深底提亮 10%。 */
function deriveBorder(triple: string, light: boolean): string {
  const [r, g, b] = triple.split(/\s+/).map(Number)
  const adj = (v: number) => Math.max(0, Math.min(255, light ? Math.round(v * 0.92) : Math.round(v + (255 - v) * 0.1)))
  return `rgb(${adj(r)} ${adj(g)} ${adj(b)})`
}

export interface WidgetThemeVars {
  cardBg: string
  cardBorder: string
  zeroBg: string
  text: string
  textMuted: string
}

/** 从当前 prefs 派生整套卡片主题变量。widgetCardBg=undefined → null（表示
 * 跟随 scheme 内置值，调用方清除覆写让 tokens.css 回归）。 */
export function deriveWidgetTheme(prefs: DesignPrefs): WidgetThemeVars | null {
  const triple = prefs.widgetCardBg ? hexToRgbTriple(prefs.widgetCardBg) : null
  if (!triple) return null
  const light = luminance(triple) > 0.5
  return {
    cardBg: triple,
    cardBorder: deriveBorder(triple, light),
    zeroBg: deriveZeroBg(triple, light),
    text: deriveText(triple, light),
    textMuted: deriveTextMuted(triple, light),
  }
}

/** 把派生结果覆写到 documentElement；undefined 色时清除全部覆写（恢复默认）。 */
export function applyWidgetTheme(prefs: DesignPrefs): void {
  const root = document.documentElement
  const theme = deriveWidgetTheme(prefs)
  if (!theme) {
    root.style.removeProperty('--widget-card-bg')
    root.style.removeProperty('--widget-card-border')
    root.style.removeProperty('--widget-zero-bg')
    root.style.removeProperty('--widget-card-text')
    root.style.removeProperty('--widget-card-text-muted')
    return
  }
  root.style.setProperty('--widget-card-bg', theme.cardBg)
  root.style.setProperty('--widget-card-border', theme.cardBorder)
  root.style.setProperty('--widget-zero-bg', theme.zeroBg)
  root.style.setProperty('--widget-card-text', theme.text)
  root.style.setProperty('--widget-card-text-muted', theme.textMuted)
}

/** 订阅入口：窗口根组件 effect 里调用一次即可（两个窗口各一份）。 */
export function useWidgetThemeSync(): void {
  applyWidgetTheme(getDesignPrefs())
  subscribeDesignPrefs(applyWidgetTheme)
}
