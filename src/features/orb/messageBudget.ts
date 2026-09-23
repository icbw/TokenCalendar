/** 剩余消息数的展示侧：挑哪些模型、怎么叫、怎么印。
 *
 * 数据源是 get_message_budget 给的「一轮吃掉 5h 窗口几个百分点」;剩余条数在这里用
 * **调用方手里快照的剩余 %** 去除，保证与表盘同一时刻。「消息」= 用户发起的对话轮次。 */
import type { MessageBudget, MessageCostRow } from '../../services/subscriptionService'

/** 模型短名：hover 空间小，价目表的展示名可能是区间名（「Claude Opus 4.5〜5」），
 * 所以从模型键直接取——claude-opus-5-5 → Opus 5.5，gpt-6-astra → GPT-6 Astra，
 * gpt-5.6-sol → GPT-5.6 Sol。认不出形状的键原样返回。 */
export function shortModelName(key: string): string {
  const k = key.trim().toLowerCase()
  const words = (parts: string[]) =>
    parts
      .reduce<string[]>((out, w) => {
        // 相邻两段都是数字 = 版本号被连字符拆开了（5-5 → 5.5）
        const prev = out[out.length - 1]
        if (/^\d+$/.test(w) && prev !== undefined && /^\d+(\.\d+)*$/.test(prev)) out[out.length - 1] = `${prev}.${w}`
        else out.push(w)
        return out
      }, [])
      .map((w) => (/^\d/.test(w) ? w : w.charAt(0).toUpperCase() + w.slice(1)))
  if (k.startsWith('claude-')) {
    // 带日期尾巴的键（claude-haiku-4-5-20251001）去掉日期
    const parts = k.slice('claude-'.length).split('-').filter((w) => !/^\d{8}$/.test(w))
    return words(parts).join(' ') || key
  }
  const gpt = /^gpt-(\d+(?:\.\d+)?)(?:-(.*))?$/.exec(k)
  if (gpt) {
    const [, ver, rest] = gpt
    const tail = rest ? words(rest.split('-')) : []
    // 版本号续段（gpt-5-6-sol）并进主版本
    if (tail.length && /^\d+$/.test(tail[0])) return [`GPT-${ver}.${tail[0]}`, ...tail.slice(1)].join(' ')
    return [`GPT-${ver}`, ...tail].join(' ')
  }
  return key
}

/** 条数缩写：< 1000 印整数，≥ 1000 印 1.2k（hover 只有一行宽）。 */
export function abbrevCount(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '—'
  if (n < 1000) return String(Math.floor(n))
  const k = n / 1000
  return `${k < 10 ? (Math.floor(k * 10) / 10).toFixed(1).replace(/\.0$/, '') : Math.floor(k)}k`
}

/** 要显示的行：`selected` = 设置页选的模型键（undefined = 自动 → 主力模型;[] = 不显示）。
 * 选了但近 30 天样本不够的模型没有行，静默略过——估不出就不印。 */
export function pickBudgetRows(budget: MessageBudget | null, selected: string[] | undefined): MessageCostRow[] {
  if (!budget) return []
  if (selected === undefined) {
    const main = budget.rows.find((r) => r.model_key === budget.main_model)
    return main ? [main] : []
  }
  return selected
    .map((k) => budget.rows.find((r) => r.model_key === k))
    .filter((r): r is MessageCostRow => r !== undefined)
}

/** 一行 hover 文案：`GPT-6 Astra: ~3/30` = 按当前剩余还能发 ≈3 条 / 满窗口 ≈30 条。
 * 「~」就是「这是估计」的全部标记——描述性文字不上 hover（口径说明在设置页）。 */
export function budgetLine(row: MessageCostRow, remainPct: number): string {
  const per = row.pct_per_turn
  const left = per > 0 ? remainPct / per : NaN
  const full = per > 0 ? 100 / per : NaN
  return `${shortModelName(row.model_key)}: ~${abbrevCount(left)}/${abbrevCount(Math.round(full))}`
}
