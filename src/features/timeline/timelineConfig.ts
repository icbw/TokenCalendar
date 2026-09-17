// 项目推进时间轴常量。过去 / 未来天数首期是常量不进设置（/）。

/** 时间窗默认值（进设置：prefs timelinePastDays / timelineFutureDays,0〜30）。
 * 31 天太多——每格只能放一个会话;默认收到 7 + 今天 + 7。 */
export const TIMELINE_PAST_DAYS = 7
export const TIMELINE_FUTURE_DAYS = 7
/** ：过去每天显示的会话数 / 今天显示的会话数默认值（prefs timelinePastSessions / timelineTodaySessions）。 */
export const TIMELINE_PAST_SESSIONS = 1
export const TIMELINE_TODAY_SESSIONS = 5
/** 格内滚动：点击格子临时展开到的可见会话数（设置值更大时取设置值）;
 * 格子失焦自动折回设置状态。首期是常量不进设置。 */
export const TIMELINE_EXPANDED_SESSIONS = 5

/** 「⚠ Nd」未动徽章的最小天数（小于此值不显示,避免噪音）。 */
export const INACTIVE_BADGE_DAYS = 3

/** 格子尺寸范围：
 * 项目列宽 / 会话格高。显示项目数 = min（设置上限, 容量),容量按面板宽除以最小列宽;
 * 超出最小尺寸的窗口在 [min, max] 之间等分,再多留白。横向日程视图删除,其常量随之移除。 */
export const COL_MIN_PX = 140
export const COL_MAX_PX = 320
/** 会话格（一格一会话,向下堆叠）最小高;空日行最小高。 */
export const ITEM_MIN_PX = 36
export const EMPTY_DAY_MIN_PX = 26
/** 顶栏项目表头行高;
 * 左侧日标签列宽。 */
export const BAR_HEAD_PX = 32
export const DAY_LABEL_COL_PX = 46
/** 顶栏右侧按钮区宽:面板右侧留同宽,顶栏表头网格与面板网格列对齐。 */
export const BAR_ACTIONS_PX = 36
/** 外观 alpha 默认值（prefs timelineBgAlpha / timelineBarAlpha / timelineCellAlpha）。 */
export const TIMELINE_BG_ALPHA = 0.55
export const TIMELINE_BAR_ALPHA = 0.85
export const TIMELINE_CELL_ALPHA = 0.8
/** 用量着色关闭（prefs timelineHeat = false）时所有会话格的统一强调色占比（开启时按 tokens 在 0.14〜0.5 间取值）。 */
export const TIMELINE_FLAT_HEAT = 0.22

/** 条态 / 窥视把手四周（左右下）留给窄阴影的透明边（CSS 像素;与 timeline.css --tl-edge-pad、
 * timeline_form.rs STRIP_H_LOGICAL / PEEK_*_LOGICAL 同源,改一处三处同改）。 */
export const STRIP_SHADOW_PAD_PX = 5

/** 条态无操作（指针不在窗口、无亮起项目）多久后收成窥视态细边。 */
export const PEEK_DELAY_MS = 5000

/** hover 状态卡显示延迟 / 收尾宽限（对齐 orb ㉝ 的手感档）。 */
export const HOVER_DELAY_MS = 350
export const HOVER_GRACE_MS = 140

export const MONTH_ABBR = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

/** 本地日历日 YYYY-MM-DD（与 Rust `Local:now.date_naive` 同口径,不拼时区）。 */
export function localDay(d: Date = new Date()): string {
  const y = d.getFullYear()
  const m = String(d.getMonth() + 1).padStart(2, '0')
  const day = String(d.getDate()).padStart(2, '0')
  return `${y}-${m}-${day}`
}

/** 本地日 ± n 天（按本地日历,跨月 / 跨年由 Date 处理）。 */
export function addDays(day: string, n: number): string {
  const [y, m, d] = day.split('-').map(Number)
  return localDay(new Date(y, m - 1, d + n))
}

/** 本地日 a 到 b 相隔天数（b 晚为正;按日历日,不受夏令时影响）。 */
export function daysBetween(a: string, b: string): number {
  const [ay, am, ad] = a.split('-').map(Number)
  const [by, bm, bd] = b.split('-').map(Number)
  return Math.round((Date.UTC(by, bm - 1, bd) - Date.UTC(ay, am - 1, ad)) / 86_400_000)
}

export function dayParts(day: string): { y: number; m: number; d: number } {
  const [y, m, d] = day.split('-').map(Number)
  return { y, m, d }
}

/** 短日期:`Sep 5`（跨年补年份）。 */
export function shortDay(day: string | null): string {
  if (!day) return '—'
  const { y, m, d } = dayParts(day)
  const year = y !== new Date().getFullYear() ? `, ${y}` : ''
  return `${MONTH_ABBR[m - 1]} ${d}${year}`
}

/** 时刻 HH:mm（标题为空时的回退标签,与 Tasks 视图同口径）。 */
export function clockLabel(ms: number): string {
  const d = new Date(ms)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}
