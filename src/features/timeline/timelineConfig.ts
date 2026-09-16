// 项目推进时间轴常量。过去 / 未来天数首期是常量不进设置（/）。

/** 时间窗默认值（进设置：prefs timelinePastDays / timelineFutureDays,0〜30）。
 * 31 天太多——每格只能放一个会话;默认收到 7 + 今天 + 7。 */
export const TIMELINE_PAST_DAYS = 7
export const TIMELINE_FUTURE_DAYS = 7
/** 最多显示项目数默认值（prefs timelineMaxProjects;0 = 不限,只按容量）。 */
export const TIMELINE_MAX_PROJECTS = 8
/** ：过去每天显示的会话数 / 今天显示的会话数默认值（prefs timelinePastSessions / timelineTodaySessions）。 */
export const TIMELINE_PAST_SESSIONS = 1
export const TIMELINE_TODAY_SESSIONS = 5

/** 「⚠ Nd」未动徽章的最小天数（小于此值不显示,避免噪音）。 */
export const INACTIVE_BADGE_DAYS = 3

/** 格子尺寸范围：
 * 横向 = 项目行高 / 日列宽;纵向 = 项目列宽 / 会话格高。显示项目数 = min（设置上限, 容量),
 * 容量按容器尺寸除以最小尺寸;超出最小尺寸的窗口在 [min, max] 之间等分,再多留白。 */
export const ROW_MIN_PX = 52
export const ROW_MAX_PX = 96
export const COL_MIN_PX = 140
export const COL_MAX_PX = 320
export const DAY_COL_MIN_PX = 44
/** 纵向会话格（一格一会话,向下堆叠）最小高;空日行最小高。 */
export const ITEM_MIN_PX = 36
export const EMPTY_DAY_MIN_PX = 26
/** 横向:日轴表头高;纵向:日标签列宽 + 右缘图标列占位。 */
export const DAY_HEADER_PX = 22
export const DAY_LABEL_COL_PX = 46
export const SIDE_ICONS_PX = 28
/** 紧凑档阈值：横向日列宽低于此只显示轮数（纵向已有最小列宽,不再有紧凑档）。 */
export const CELL_NARROW_PX = 72
/** 横向项目标签列宽（纵向时项目表头高 = DAY_HEADER_PX × 2）。 */
export const PROJECT_LABEL_COL_PX = 150

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
