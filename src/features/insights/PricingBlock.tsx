// Insights 价格面板,三件事一张卡一件：
//
//   1) 美元当量 + 用量 × 价格 —— 这段时间的 token 若按官方 API 单价计费值多少钱,
//      分模型列出,每个模型按它自己的价目生效段切开;
//   2) 分模型消息数估计 —— 5h / 周窗口约剩几条、满窗几条、本周已发、周重置次数（MessagesCard）;
//   3) 官方价目对照表 —— 四项单价 + 出处,它解释了第一块的数字是怎么来的。
//      可按列排序（默认 Since 降序,新上架 / 刚改价的在上）;某项单价相对该模型
//      上一段生效期有变动时格内标箭头（涨 = 红↑,降 = 绿↓）,点箭头展开变动量。
//      原先的「价格梯度」阶梯图已删:官方极少改价,整张图几乎全是平线,参考价值低。
//
// **口径红线（文案里一个字都不能松）**：
// - 美元当量**不是账单**。用户付的是固定订阅月费,这个数衡量的是等价价值 /
//   机会成本,所以一律说「equivalent」「would cost」,绝不说「you spent」。
// - `usd_unknown` 是「这部分价目是估的」的提示,**不是误差棒**。
// - 缓存写按**常用档**估算（部分平台按 TTL 分档,采集层只有一个桶）。
// - 回本倍数（Value vs. fee）= 区间美元当量 ÷（用户自填月费 × 区间天数 / 月均天数）,
//   讲的是**等价价值的倍数**,不是「省了多少钱」。月费只来自设置里用户自己填的数
//   （designPrefs.subscriptionMonthlyUsd）,不维护官方月费表;未填 → 整项不显示。
//
// 取数全部经 subscriptionService 的五条只读命令,零网络、不唤醒取数轮。
import { useEffect, useMemo, useRef, useState } from 'react'
import { events, subscriptionService } from '../../services'
import type { SubscriptionPlatform } from '../../services'
import type { ModelUsageResult, PriceModelRow } from '../../services/subscriptionService'
import { formatCompact, formatFull } from '../matrix/matrixScale'
import { colorFor } from './charts'
import { Seg } from './Seg'
import RangeControl from './RangeControl'
import MessagesCard from './MessagesCard'
import { formatSpan, spanDays } from './range'
import { useRangeSelection } from './useRangeSelection'
import { getDesignPrefs, subscribeDesignPrefs } from '../settings/designPrefs'

const PLATFORMS: SubscriptionPlatform[] = ['codex', 'claude']
const PLATFORM_LABEL: Record<SubscriptionPlatform, string> = { codex: 'Codex', claude: 'Claude' }

type Unit = 'input' | 'output' | 'cache_write' | 'cache_read'
/** 四项的展示顺序 = 官方目录的顺序（输入 / 输出 / 缓存写 / 缓存命中）。 */
const UNITS: Unit[] = ['input', 'output', 'cache_write', 'cache_read']
const UNIT_LABEL: Record<Unit, string> = {
  input: 'Input',
  output: 'Output',
  cache_write: 'Cache write',
  cache_read: 'Cache read',
}
const unitPrice = (r: PriceModelRow, u: Unit): number =>
  u === 'input' ? r.usd_input : u === 'output' ? r.usd_output : u === 'cache_write' ? r.usd_cache_write : r.usd_cache_read

/** 金额（合计口径）：两位小数 + 千分位。 */
const usd = (v: number): string => `$${v.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
/** 单价口径：跨 $0.005〜$180,固定位数会把小的印成 $0.00 —— 走有效数字。 */
const price = (v: number): string => (v <= 0 ? '—' : `$${Number(v.toPrecision(3)).toLocaleString('en-US')}`)
const day = (t: number): string => new Date(t * 1000).toLocaleDateString('en-CA')
/** 单价变动量：带符号 + 百分比,如 `+$1.25 （+25%)`。 */
const deltaText = (prev: number, cur: number): string => {
  const d = cur - prev
  const sign = d > 0 ? '+' : '−'
  const abs = Number(Math.abs(d).toPrecision(3)).toLocaleString('en-US')
  const pct = Number(((Math.abs(d) / prev) * 100).toPrecision(3))
  return `${sign}$${abs} (${sign}${pct}%)`
}

type SortKey = 'model' | Unit | 'since'
interface Sort {
  key: SortKey
  desc: boolean
}
/** 月均天数（365.25 / 12）:月费按区间天数折算用。 */
const DAYS_PER_MONTH = 365.25 / 12
/** 倍数:10× 以下一位小数,以上取整。 */
const multiple = (v: number): string => (v < 10 ? v.toFixed(1) : Math.round(v).toLocaleString('en-US')) + '×'

/** 默认 Since 降序：最新上架 / 最近改价的模型排在最上。 */
const DEFAULT_SORT: Sort = { key: 'since', desc: true }

export default function PricingBlock() {
  const [platform, setPlatform] = useState<SubscriptionPlatform>('codex')
  // 默认平台只自动挑一次（挑「本机用得多的那个」）,之后一律听用户的。
  // 用 ref 不用 state：它一翻转就会把下面那条取数 effect 整个重跑一遍。
  const platformPicked = useRef(false)
  const [refreshTick, setRefreshTick] = useState(0)
  const [sort, setSort] = useState<Sort>(DEFAULT_SORT)
  /** 已展开变动量的格子：`${match_key}:${unit}`。 */
  const [revealed, setRevealed] = useState<ReadonlySet<string>>(new Set())
  const [usage, setUsage] = useState<Partial<Record<SubscriptionPlatform, ModelUsageResult | null>>>({})
  const [prices, setPrices] = useState<PriceModelRow[] | null>(null)
  const [now, setNow] = useState<{ at: number; rows: PriceModelRow[] } | null>(null)
  const [scale, setScale] = useState<Partial<Record<SubscriptionPlatform, number>>>({})
  const [loading, setLoading] = useState(false)
  const [fees, setFees] = useState(() => getDesignPrefs().subscriptionMonthlyUsd ?? {})
  useEffect(() => subscribeDesignPrefs((p) => setFees(p.subscriptionMonthlyUsd ?? {})), [])

  const selection = useRangeSelection('', refreshTick)
  const { startDay, endDay } = selection.range

  useEffect(() => {
    let timer = 0
    let off: (() => void) | null = null
    void events.onUsageChanged(() => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => setRefreshTick((t) => t + 1), 300)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      window.clearTimeout(timer)
      off?.()
    }
  }, [])

  // 分模型用量:两个平台各拉一份——第一次装载要据此挑默认平台（本机用得多的那个）,
  // 而且平台切换后立刻有数可画,不必等一轮往返。两条都是本地只读查询。
  useEffect(() => {
    let cancelled = false
    setLoading(true)
    void Promise.all(PLATFORMS.map((p) => subscriptionService.getModelUsage(p, startDay, endDay))).then((res) => {
      if (cancelled) return
      const next: Partial<Record<SubscriptionPlatform, ModelUsageResult | null>> = {}
      PLATFORMS.forEach((p, i) => {
        next[p] = res[i]
      })
      setUsage(next)
      setLoading(false)
      if (!platformPicked.current) {
        platformPicked.current = true
        const best = PLATFORMS.reduce((a, b) => ((next[b]?.usd_total ?? 0) > (next[a]?.usd_total ?? 0) ? b : a))
        setPlatform(best)
      }
    })
    return () => {
      cancelled = true
    }
  }, [startDay, endDay, refreshTick])

  // 价目表随版本走,运行时不变 —— 只在换平台时重取
  useEffect(() => {
    let cancelled = false
    void Promise.all([subscriptionService.getPriceModels(platform), subscriptionService.getPriceAt(platform)]).then(
      ([all, at]) => {
        if (cancelled) return
        setPrices(all)
        setNow(at === null ? null : { at: at.at, rows: at.rows })
      },
    )
    return () => {
      cancelled = true
    }
  }, [platform])

  // 归一化系数:把「相当于多少钱」与「还剩多少额度」接上（1% 配额 ≈ 多少美元当量）。
  // 落一条新样本就重拟合一次,所以跟着 subscription:changed 重查（本地只读,零请求）。
  useEffect(() => {
    let timer = 0
    let off: (() => void) | null = null
    const load = () =>
      void subscriptionService.getEstimator().then((rows) => {
        if (!rows) return
        const next: Partial<Record<SubscriptionPlatform, number>> = {}
        for (const r of rows) if (typeof r.scale === 'number' && r.scale > 0) next[r.platform] = r.scale
        setScale(next)
      })
    load()
    void events.onSubscriptionChanged(() => {
      window.clearTimeout(timer)
      timer = window.setTimeout(load, 300)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      window.clearTimeout(timer)
      off?.()
    }
  }, [])

  const cur = usage[platform] ?? null
  const perDollar = scale[platform]
  const totalTurns = cur === null ? 0 : cur.rows.reduce((s, r) => s + r.requests, 0)
  const totalTokens = cur === null ? 0 : cur.rows.reduce((s, r) => s + r.total_tokens, 0)
  const fee = fees[platform]
  const rangeDays = spanDays(selection.range)
  const feeForRange = fee !== undefined && rangeDays > 0 ? (fee * rangeDays) / DAYS_PER_MONTH : null

  // 每个现行价目行的「上一段」：同一 match_key 里生效期早于它的最近一行。
  // 两段都公布了该项单价（> 0）且不相等才算变动——「从没公布到公布」不是涨价。
  const prevOf = useMemo(() => {
    const out = new Map<string, PriceModelRow>()
    if (now === null) return out
    for (const r of now.rows) {
      let best: PriceModelRow | undefined
      for (const p of prices ?? []) {
        if (p.match_key !== r.match_key || p.effective_from >= r.effective_from) continue
        if (!best || p.effective_from > best.effective_from) best = p
      }
      if (best) out.set(r.match_key, best)
    }
    return out
  }, [now, prices])

  const listRows = useMemo(() => {
    if (now === null) return []
    const dir = sort.desc ? -1 : 1
    const { key } = sort
    return [...now.rows].sort((a, b) => {
      let c = 0
      if (key === 'model') c = a.display_name.localeCompare(b.display_name, 'en', { numeric: true }) * dir
      else if (key === 'since') c = (a.effective_from - b.effective_from) * dir
      else {
        const va = unitPrice(a, key)
        const vb = unitPrice(b, key)
        // 未公布（—）的一律沉底,不随升降序翻到顶上
        if (va <= 0 || vb <= 0) c = va <= 0 && vb <= 0 ? 0 : va <= 0 ? 1 : -1
        else c = (va - vb) * dir
      }
      return c || b.usd_input - a.usd_input || a.match_key.localeCompare(b.match_key)
    })
  }, [now, sort])

  // 同列再点翻转方向;换列时价格 / 日期先降序（贵的、新的在上）,名称先升序
  const pickSort = (key: SortKey) =>
    setSort((prev) => (prev.key === key ? { key, desc: !prev.desc } : { key, desc: key !== 'model' }))
  const toggleReveal = (id: string) =>
    setRevealed((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  const sortHeader = (key: SortKey, label: string, className?: string) => {
    const active = sort.key === key
    return (
      <th key={key} className={className} aria-sort={active ? (sort.desc ? 'descending' : 'ascending') : undefined}>
        <button className={`price-th${active ? ' is-active' : ''}`} title="Click to sort" onClick={() => pickSort(key)}>
          {label}
          <span className="price-th-arrow">{active ? (sort.desc ? '▼' : '▲') : ''}</span>
        </button>
      </th>
    )
  }

  return (
    <>
      <div className="insight-module-bar">
      <header className="insight-toolbar">
        <span className="insight-card-title">Pricing</span>
        <Seg
          value={platform}
          options={PLATFORMS.map((p) => ({
            v: p,
            label: PLATFORM_LABEL[p],
            hint: `Official API list prices and equivalent value for ${PLATFORM_LABEL[p]}`,
          }))}
          onChange={setPlatform}
        />
        {loading && <span className="matrix-loading">Loading…</span>}
      </header>
      <div className="insight-rangebar">
        <RangeControl selection={selection} noun="Usage" />
      </div>
      </div>

      {/* ---- 1) 美元当量 + 用量 × 价格 ----*/}
      <section className="insight-card">
        <header className="insight-card-header">
          <span className="insight-card-title">Equivalent API value · {PLATFORM_LABEL[platform]}</span>
          <span className="insight-card-sub">{formatSpan(selection.range)}</span>
        </header>
        {cur === null ? (
          <div className="insight-empty">Data unavailable (service not running)</div>
        ) : cur.rows.length === 0 ? (
          <div className="insight-empty">No {PLATFORM_LABEL[platform]} usage in this range</div>
        ) : (
          <>
            <div className="price-totals">
              <div
                className="price-total-item"
                title="What these tokens would cost at the official API list prices. You pay a flat subscription, so this is equivalent value, not a bill."
              >
                <span className="price-total-label">Equivalent API value</span>
                <span className="price-total-value">≈ {usd(cur.usd_total)}</span>
              </div>
              {feeForRange !== null && cur.usd_total > 0 && (
                <div
                  className="price-total-item"
                  title={`Equivalent API value ÷ your ${PLATFORM_LABEL[platform]} fee for these ${rangeDays} days (${usd(fee ?? 0)}/month → ${usd(feeForRange)}). How much list-price usage the subscription bought, as a multiple — not money saved. Set the fee in Settings › Subscriptions.`}
                >
                  <span className="price-total-label">Value vs. fee</span>
                  <span className="price-total-value">≈ {multiple(cur.usd_total / feeForRange)}</span>
                </div>
              )}
              <div className="price-total-item" title="User-initiated turns, summed over the range">
                <span className="price-total-label">Turns</span>
                <span className="price-total-value">{formatFull(totalTurns)}</span>
              </div>
              <div className="price-total-item" title={`${formatFull(totalTokens)} tokens: input + output + cache`}>
                <span className="price-total-label">Tokens</span>
                <span className="price-total-value">{formatCompact(totalTokens)}</span>
              </div>
              {perDollar !== undefined && (
                <div
                  className="price-total-item"
                  title={`Measured on your own readings: $1 of equivalent API usage draws ${perDollar.toFixed(2)}% of the quota, so a full window is worth roughly ${usd(100 / perDollar)}. It follows your plan and shifts as more samples land.`}
                >
                  <span className="price-total-label">1% of quota</span>
                  <span className="price-total-value">≈ {usd(1 / perDollar)}</span>
                </div>
              )}
            </div>
            <p className="price-note">
              What these tokens would cost at official API list prices — you pay a flat subscription, so this is
              equivalent value, not a bill.
              {cur.usd_unknown > 0 && (
                <span
                  className="price-note-warn"
                  title="Fallback pricing: models with no published price of their own (and the codex-auto-review routing label) are priced by proxy. It marks the part of the number that is estimated — it is not an error bar."
                >
                  {' '}
                  {usd(cur.usd_unknown)} ({((cur.usd_unknown / Math.max(cur.usd_total, 1e-9)) * 100).toFixed(1)}%) of it
                  is priced by fallback.
                </span>
              )}
            </p>
            <table className="price-table">
              <thead>
                <tr>
                  <th className="price-col-model">Model</th>
                  <th>Turns</th>
                  <th>Input</th>
                  <th>Output</th>
                  <th>Cache write</th>
                  <th>Cache read</th>
                  <th className="price-col-usd">Equivalent</th>
                </tr>
              </thead>
              <tbody>
                {cur.rows.map((r) => {
                  const last = r.segments[r.segments.length - 1]
                  const share = cur.usd_total > 0 ? (r.usd / cur.usd_total) * 100 : 0
                  const rows = [
                    <tr key={r.model_key}>
                      <td
                        className="price-col-model"
                        title={
                          `${r.model_key}` +
                          (last
                            ? `\n${price(last.usd_input)} in / ${price(last.usd_output)} out / ` +
                              `${price(last.usd_cache_write)} cache write / ${price(last.usd_cache_read)} cache read per Mtok`
                            : '') +
                          (r.known
                            ? ''
                            : `\nFallback pricing, valued as ${r.display_name} — this key has no published price of its own.`)
                        }
                      >
                        <span className="legend-swatch" style={{ background: colorFor(r.model_key) }} />
                        {/* 价目不可信时印 collector 的原始模型键,**不印折价目标的名字**：
                            codex-auto-review 折的是 gpt-5.6-luna,印成「GPT-5.6 Luna」就会与
                            真正的 Luna 并排出现两行同名,谁也分不出哪行是哪行。*/}
                        {r.known ? r.display_name : r.model_key}
                        {!r.known && <span className="price-unknown-mark"> ≈</span>}
                      </td>
                      <td title="User-initiated turns. Daily-grained, so they are never split across price periods; a routing label such as codex-auto-review has none of its own.">
                        {formatFull(r.requests)}
                      </td>
                      <td title={formatFull(r.input_tokens)}>{formatCompact(r.input_tokens)}</td>
                      <td title={formatFull(r.output_tokens)}>{formatCompact(r.output_tokens)}</td>
                      <td title={formatFull(r.cache_write_tokens)}>{formatCompact(r.cache_write_tokens)}</td>
                      <td title={formatFull(r.cache_read_tokens)}>{formatCompact(r.cache_read_tokens)}</td>
                      <td className="price-col-usd">
                        <span className="price-share-track">
                          <span
                            className="price-share-bar"
                            style={{ width: `${Math.min(100, share)}%`, background: colorFor(r.model_key) }}
                          />
                        </span>
                        <span className="price-usd-value" title={`${share.toFixed(1)}% of the range`}>
                          {usd(r.usd)}
                        </span>
                      </td>
                    </tr>,
                  ]
                  // 一个模型被官方降过价才会有第二段:按段列出,每段配它当期的单价
                  if (r.segments.length > 1) {
                    for (const s of r.segments) {
                      rows.push(
                        <tr key={`${r.model_key}@${s.effective_from ?? 0}`} className="price-seg-row">
                          <td className="price-col-model">
                            since {s.effective_from === null ? 'unpriced' : day(s.effective_from)} · {price(s.usd_input)}{' '}
                            in / {price(s.usd_output)} out
                          </td>
                          <td title="Turns are daily-grained and never split across price periods">—</td>
                          <td title={formatFull(s.input_tokens)}>{formatCompact(s.input_tokens)}</td>
                          <td title={formatFull(s.output_tokens)}>{formatCompact(s.output_tokens)}</td>
                          <td title={formatFull(s.cache_write_tokens)}>{formatCompact(s.cache_write_tokens)}</td>
                          <td title={formatFull(s.cache_read_tokens)}>{formatCompact(s.cache_read_tokens)}</td>
                          <td className="price-col-usd">
                            <span className="price-usd-value">{usd(s.usd)}</span>
                          </td>
                        </tr>,
                      )
                    }
                  }
                  return rows
                })}
              </tbody>
            </table>
          </>
        )}
      </section>

      {/* ---- 2) 分模型消息数估计（5h / 周剩余、本周已发、周重置） ----*/}
      <MessagesCard platform={platform} range={selection.range} refreshTick={refreshTick} />

      {/* ---- 3) 官方价目对照表 ----*/}
      <section className="insight-card">
        <header className="insight-card-header">
          <span className="insight-card-title">Official price list · {PLATFORM_LABEL[platform]}</span>
          <span className="insight-card-sub">{now === null ? '' : `USD / Mtok, in effect ${day(now.at)}`}</span>
        </header>
        {now === null ? (
          <div className="insight-empty">Data unavailable (service not running)</div>
        ) : (
          <>
            <table className="price-table">
              <thead>
                <tr>
                  {sortHeader('model', 'Model', 'price-col-model')}
                  {UNITS.map((u) => sortHeader(u, UNIT_LABEL[u]))}
                  {sortHeader('since', 'Since')}
                </tr>
              </thead>
              <tbody>
                {listRows.map((r) => {
                  const prev = prevOf.get(r.match_key)
                  return (
                    <tr key={r.match_key}>
                      <td className="price-col-model" title={`${r.match_key}\n${r.source_note}`}>
                        <span className="legend-swatch" style={{ background: colorFor(r.match_key) }} />
                        {r.display_name}
                      </td>
                      {UNITS.map((u) => {
                        const v = unitPrice(r, u)
                        const pv = prev === undefined ? 0 : unitPrice(prev, u)
                        const changed = v > 0 && pv > 0 && Math.abs(v - pv) > 1e-9
                        const dir = v > pv ? 'is-up' : 'is-down'
                        const id = `${r.match_key}:${u}`
                        const open = revealed.has(id)
                        return (
                          <td key={u} title={v > 0 ? '' : 'Not published for this model'}>
                            {changed && open && <span className={`price-delta ${dir}`}>{deltaText(pv, v)}</span>}
                            {price(v)}
                            {changed ? (
                              <button
                                className={`price-change ${dir}`}
                                title={`${v > pv ? 'Raised' : 'Cut'} on ${day(r.effective_from)}: ${price(pv)} → ${price(v)} · click to ${open ? 'hide' : 'show'} the change`}
                                onClick={() => toggleReveal(id)}
                              >
                                {v > pv ? '↑' : '↓'}
                              </button>
                            ) : (
                              <span className="price-change" aria-hidden />
                            )}
                          </td>
                        )
                      })}
                      <td title={`Effective from ${new Date(r.effective_from * 1000).toLocaleString()}`}>
                        {day(r.effective_from)}
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
            {/* 缓存写按常用档是文案红线,常驻可;其余说明放 hover*/}
            <p
              className="price-note"
              title={
                'Shipped with the app and updated with it — hover a model for the source entry.\n' +
                'An arrow marks a price changed from the model\'s previous period (red up, green down); click it for the amount.\n' +
                'Cache write uses the common tier: some platforms price it by retention, and local collection keeps a single cache-write bucket.'
              }
            >
              Official API list prices; arrows mark a price change (click for the amount). Cache write uses the common tier.
            </p>
          </>
        )}
      </section>
    </>
  )
}
