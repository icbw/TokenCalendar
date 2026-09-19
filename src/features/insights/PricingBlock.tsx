// Insights 价格面板（S3）。
// S1 把价格做成了带生效时间的一等数据、S2 把它与读数接成了五条只读命令,但到
// 之前**用户一眼都没看见过**。本块是这条线的出口,三件事一张卡一件：
//
//   1) 美元当量 + 用量 × 价格 —— 这段时间的 token 若按官方 API 单价计费值多少钱,
//      分模型列出,每个模型按它自己的价目生效段切开;
//   2) 价格梯度 —— 分模型 × 生效期的单价阶梯图（同一模型一条线,降价处是台阶）;
//   3) 官方价目对照表 —— 四项单价 + 出处,它解释了前两块的数字是怎么来的。
//
// **口径红线（-bis,文案里一个字都不能松）**：
// - 美元当量**不是账单**。用户付的是固定订阅月费,这个数衡量的是等价价值 /
//   机会成本,所以一律说「equivalent」「would cost」,绝不说「you spent」。
// - `usd_unknown` 是「这部分价目是估的」的提示,**不是误差棒**。
// - 缓存写按**常用档**估算（部分平台按 TTL 分档,采集层只有一个桶）。
//
// 取数全部经 subscriptionService 的五条只读命令,零网络、不唤醒取数轮。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { events, subscriptionService } from '../../services'
import type { SubscriptionPlatform } from '../../services'
import type { ModelUsageResult, PriceModelRow } from '../../services/subscriptionService'
import { formatCompact, formatFull } from '../matrix/matrixScale'
import { StepChart, colorFor, type StepSeries } from './charts'
import { Seg } from './Seg'
import RangeControl from './RangeControl'
import { formatSpan } from './range'
import { useRangeSelection } from './useRangeSelection'

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

export default function PricingBlock() {
  const [platform, setPlatform] = useState<SubscriptionPlatform>('codex')
  // 默认平台只自动挑一次（挑「本机用得多的那个」）,之后一律听用户的。
  // 用 ref 不用 state：它一翻转就会把下面那条取数 effect 整个重跑一遍。
  const platformPicked = useRef(false)
  const [refreshTick, setRefreshTick] = useState(0)
  const [unit, setUnit] = useState<Unit>('input')
  const [focus, setFocus] = useState('') // 梯度图图例筛选:空串 = 全部
  const [usage, setUsage] = useState<Partial<Record<SubscriptionPlatform, ModelUsageResult | null>>>({})
  const [prices, setPrices] = useState<PriceModelRow[] | null>(null)
  const [now, setNow] = useState<{ at: number; rows: PriceModelRow[] } | null>(null)
  const [scale, setScale] = useState<Partial<Record<SubscriptionPlatform, number>>>({})
  const [loading, setLoading] = useState(false)

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

  // 梯度图:一个 match_key 一条线,点 = 它的各段生效期。所选单价为 0 的模型整条剔掉
  // ——对数轴上 0 无处可放,而「官方没有这一项」与「这一项免费」是两回事,不能画成 0。
  const gradient = useMemo(() => {
    const byKey = new Map<string, PriceModelRow[]>()
    for (const r of prices ?? []) {
      const list = byKey.get(r.match_key)
      if (list) list.push(r)
      else byKey.set(r.match_key, [r])
    }
    const series: StepSeries[] = []
    const missing: string[] = []
    for (const [key, rowsRaw] of byKey) {
      const rows = [...rowsRaw].sort((a, b) => a.effective_from - b.effective_from)
      const label = rows[rows.length - 1].display_name
      if (rows.every((r) => unitPrice(r, unit) <= 0)) {
        missing.push(label)
        continue
      }
      series.push({ key, label, points: rows.map((r) => ({ t: r.effective_from, v: unitPrice(r, unit) })) })
    }
    series.sort((a, b) => b.points[b.points.length - 1].v - a.points[a.points.length - 1].v)
    return { series, missing }
  }, [prices, unit])

  const shown = focus ? gradient.series.filter((s) => s.key === focus) : gradient.series
  const t0 = useMemo(
    () => (gradient.series.length === 0 ? 0 : Math.min(...gradient.series.map((s) => s.points[0].t))),
    [gradient.series],
  )
  const tNow = Math.floor(Date.now() / 1000)
  /** 有第二段生效期的模型 = 被官方降过价的,梯度图上的台阶就是它们。 */
  const stepped = gradient.series.filter((s) => s.points.length > 1).length

  const pickFocus = useCallback((k: string) => setFocus((prev) => (prev === k ? '' : k)), [])

  return (
    <>
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

      {/* ---- 2) 价格梯度 ----*/}
      <section className="insight-card">
        <header className="insight-card-header">
          <span className="insight-card-title">Price gradient · {UNIT_LABEL[unit]}</span>
          <span
            className="insight-card-sub"
            title="Log scale: the published input prices span $0.05 to $30 per Mtok, and a linear axis flattens everything below the top few."
          >
            USD / Mtok, log scale
          </span>
          <Seg
            value={unit}
            options={UNITS.map((u) => ({ v: u, label: UNIT_LABEL[u], hint: `${UNIT_LABEL[u]} price per million tokens` }))}
            onChange={setUnit}
          />
        </header>
        {gradient.series.length === 0 ? (
          <div className="insight-empty">
            No published {UNIT_LABEL[unit].toLowerCase()} price for any {PLATFORM_LABEL[platform]} model
          </div>
        ) : (
          <>
            <div className="series-legend">
              {gradient.series.map((s) => (
                <button
                  key={s.key}
                  className={`legend-item${focus === s.key ? ' is-active' : ''}`}
                  onClick={() => pickFocus(s.key)}
                  title={focus === s.key ? 'Click to show every model' : `Only ${s.label}`}
                >
                  <span className="legend-swatch" style={{ background: colorFor(s.key) }} />
                  {s.label}
                </button>
              ))}
            </div>
            <StepChart
              series={shown}
              t0={t0}
              t1={tNow}
              log
              formatValue={price}
              titleFor={(s) => `${s.label}\n${s.points.map((p) => `${day(p.t)} · ${price(p.v)} / Mtok`).join('\n')}`}
            />
            <p className="price-note">
              {stepped === 0
                ? 'No price change on record yet — each line starts the day its model was first listed and runs at one price. A published price cut shows up here as a step, and usage stays valued at the price of its own day.'
                : `${stepped} ${stepped === 1 ? 'model has' : 'models have'} more than one price period; each step is a published price change, and usage is always valued at the price in effect on its own day.`}
              {gradient.missing.length > 0 && (
                <span title={gradient.missing.join(', ')}>
                  {' '}
                  {gradient.missing.length} {gradient.missing.length === 1 ? 'model publishes' : 'models publish'} no{' '}
                  {UNIT_LABEL[unit].toLowerCase()} price and {gradient.missing.length === 1 ? 'is' : 'are'} left out.
                </span>
              )}
            </p>
          </>
        )}
      </section>

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
                  <th className="price-col-model">Model</th>
                  {UNITS.map((u) => (
                    <th key={u}>{UNIT_LABEL[u]}</th>
                  ))}
                  <th>Since</th>
                </tr>
              </thead>
              <tbody>
                {[...now.rows]
                  .sort((a, b) => b.usd_input - a.usd_input || a.match_key.localeCompare(b.match_key))
                  .map((r) => (
                    <tr key={r.match_key}>
                      <td className="price-col-model" title={`${r.match_key}\n${r.source_note}`}>
                        <span className="legend-swatch" style={{ background: colorFor(r.match_key) }} />
                        {r.display_name}
                      </td>
                      {UNITS.map((u) => (
                        <td key={u} title={unitPrice(r, u) > 0 ? '' : 'Not published for this model'}>
                          {price(unitPrice(r, u))}
                        </td>
                      ))}
                      <td title={`Effective from ${new Date(r.effective_from * 1000).toLocaleString()}`}>
                        {day(r.effective_from)}
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
            <p className="price-note">
              Official API list prices, shipped with the app and updated with it — hover a model for the source entry.
              Cache-write prices are the common tier: some platforms price it by retention, and local collection keeps a
              single cache-write bucket.
            </p>
          </>
        )}
      </section>
    </>
  )
}
