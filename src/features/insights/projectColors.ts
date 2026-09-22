// 项目固定配色:每个项目一个自己的颜色,跨会话 / 跨时间范围 / 跨视图不变。
// 与 Agent 同一套精选色环（charts.FAMILY_HUES,每个系列一个独立色相）,两轮明度共 24 色;
// 不走 colorFor 的「家族」逻辑——项目键是路径,首段全是盘符,会全部挤进同一色相。
// 槽位在项目第一次出图时分配（取当前用得最少、编号最小的槽位;图表按用量降序出系列,
// 大项目先拿到前面的槽位）,写进 prefs.json 的 projectColors,此后固定。
// 内置伪项目（Scratch / Hidden / Unknown）用中性灰,不占色板槽位。
import { FAMILY_HUES, hsl } from './charts'
import { HIDDEN_PROJECTS_KEY, SCRATCH_PROJECT_KEY, UNKNOWN_PROJECT_KEY } from './analytics'
import { designPrefsReady, getDesignPrefs, setDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'

export const PROJECT_PALETTE: string[] = [
  ...FAMILY_HUES.map((h) => hsl(h, 0.66, 0.6)),
  ...FAMILY_HUES.map((h) => hsl(h, 0.58, 0.42)),
]

const SPECIAL: Record<string, string> = {
  [SCRATCH_PROJECT_KEY]: hsl(220, 0.08, 0.62),
  [HIDDEN_PROJECTS_KEY]: hsl(220, 0.08, 0.78),
  [UNKNOWN_PROJECT_KEY]: hsl(220, 0.06, 0.48),
}

/** 已用槽位 → 下一个槽位:用得最少者优先,同数取编号最小。 */
export function pickSlot(used: number[], size: number): number {
  const counts = new Array<number>(size).fill(0)
  for (const u of used) if (u >= 0 && u < size) counts[u]++
  let best = 0
  for (let i = 1; i < size; i++) if (counts[i] < counts[best]) best = i
  return best
}

// 本会话新分配、尚未写入 prefs 的槽位（prefs.json 载入前只记在内存,载入后合并写盘）
const pending = new Map<string, number>()
let flushQueued = false

function saved(): Record<string, number> {
  return getDesignPrefs().projectColors ?? {}
}

function flush(): void {
  flushQueued = false
  if (!designPrefsReady() || pending.size === 0) return
  const base = saved()
  const add: Record<string, number> = {}
  for (const [k, slot] of pending) if (!(k in base)) add[k] = slot
  pending.clear()
  if (Object.keys(add).length > 0) setDesignPrefs({ projectColors: { ...base, ...add } })
}

function queueFlush(): void {
  if (flushQueued) return
  flushQueued = true
  // 分配发生在渲染期间,写 prefs（会通知订阅者）推迟到渲染之后
  window.setTimeout(flush, 0)
}

subscribeDesignPrefs(() => {
  if (pending.size > 0 && designPrefsReady()) queueFlush()
})

/** 项目键（解析层有效键:合并目标 / __scratch / 原键）→ 固定颜色。 */
export function projectColor(key: string): string {
  const special = SPECIAL[key]
  if (special) return special
  let slot = saved()[key] ?? pending.get(key)
  if (slot === undefined) {
    slot = pickSlot([...Object.values(saved()), ...pending.values()], PROJECT_PALETTE.length)
    pending.set(key, slot)
    queueFlush()
  }
  return PROJECT_PALETTE[slot % PROJECT_PALETTE.length]
}
