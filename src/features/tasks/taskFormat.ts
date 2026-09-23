// Tasks 视图共用的显示格式:月日按当前语言（fmt.date）,时分固定 24 小时 HH:MM。
import { fmt } from '../../lib/i18n'

const pad2 = (n: number) => String(n).padStart(2, '0')
const hm = (d: Date) => `${pad2(d.getHours())}:${pad2(d.getMinutes())}`

/** 任务开始时间:Sep 3 14:05 / 9月3日 14:05;非今年加年份。 */
export function startedLabel(ms: number): string {
  const d = new Date(ms)
  const withYear = d.getFullYear() !== new Date().getFullYear()
  const day = fmt.date(d, withYear ? { year: 'numeric', month: 'short', day: 'numeric' } : { month: 'short', day: 'numeric' })
  return `${day} ${hm(d)}`
}

/** 逐轮时间:不带年份。 */
export function clockLabel(ms: number): string {
  const d = new Date(ms)
  return `${fmt.date(d, { month: 'short', day: 'numeric' })} ${hm(d)}`
}
