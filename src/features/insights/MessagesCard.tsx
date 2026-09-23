// Insights 价格面板第二张卡：分模型消息数估计（的 Insights 全表）。
//
// 每个模型一行：5 小时窗口「约剩 / 满窗」、周窗口「约剩 / 满窗」、当前周窗口里已发条数;
// 卡头给当前周窗口的起止与区间内的周重置次数。
//
// 口径（与悬浮球 hover 同源,见 get_message_budget）：
// - 「消息」= 用户发起的一次对话（request_count 口径）,不是模型调用 / 工具调用;
// - 一条消息多大 = 三级取样的**均值**（最近常用的模型看它自己最近 14 天的消息;最近没怎么用的看它
//   自己近 30 天的;自己的太少才拿全部消息按它的价格估）× 子会话开销 × 该模型的额度系数
//   （不够样本回落平台系数）;
//   周份额 = 5h 份额 × 周窗 / 5h 窗的大小之比;
// - 剩余 % 取**当前快照**,在这里除——与悬浮球表盘同一时刻;窗口已过期的按 100% 算;
// - 周窗口**不一定正好 7 天**（平台会主动提前重置）,起点来自读数里的重置痕迹,不从窗尾倒推。
import { useEffect, useState } from 'react'
import { events, subscriptionService } from '../../services'
import type { SubscriptionPlatform, SubscriptionSnapshot } from '../../services'
import type { MessageBudget, MessageCostRow } from '../../services/subscriptionService'
import type { DayRange } from '../../services/types'
import { abbrevCount, shortModelName, weekCounts } from '../orb/messageBudget'
import { colorFor } from './charts'

const PLATFORM_LABEL: Record<SubscriptionPlatform, string> = { codex: 'Codex', claude: 'Claude' }

/** 时刻 → `Sep 21, 12:34`（en-US,与面板其余英文一致）。 */
const stamp = (t: number): string =>
  new Date(t * 1000).toLocaleString('en-US', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false })
/** 时刻 → 本地日 `YYYY-MM-DD`（与区间的日口径对齐）。 */
const localDay = (t: number): string => new Date(t * 1000).toLocaleDateString('en-CA')

/** 快照里某窗口的剩余 %（窗口已过期 = 已经重置,按 100 算;缺窗口 = null）。 */
function remainOf(snap: SubscriptionSnapshot | undefined, kind: string, nowSec: number): number | null {
  const w = snap?.windows.find((x) => x.kind === kind)
  if (!w) return null
  if (w.resets_at !== null && w.resets_at <= nowSec) return 100
  return Math.max(0, Math.min(100, 100 - w.used_percent))
}

/** 这一行的「一条多大」取自哪批消息（悬停说明）。 */
function basisText(r: MessageCostRow): string {
  const name = shortModelName(r.model_key)
  if (r.basis === 'recent_own') return `Sized on your last ${r.basis_n} ${name} messages (last 14 days)`
  if (r.basis === 'history_own') return `Sized on your ${r.basis_n} ${name} messages from the last 30 days (little recent use)`
  return `Sized on all your ${r.basis_n} recent messages, priced for ${name} (too few of its own)`
}

/** 周一格：全模型周限额与模型专属周限额（Claude Fable 那条）取紧的那条。 */
function weekCell(r: MessageCostRow, windows: SubscriptionSnapshot['windows'] | undefined, nowSec: number): string {
  const c = weekCounts(r, windows, nowSec)
  if (!c) return '—'
  const full = abbrevCount(Math.round(c.full))
  return c.left === null ? `— / ${full}` : `~${abbrevCount(c.left)} / ${full}`
}

/** 周一格的悬停说明：是哪条限额在卡。 */
function weekTitle(r: MessageCostRow, windows: SubscriptionSnapshot['windows'] | undefined, nowSec: number): string {
  const c = weekCounts(r, windows, nowSec)
  if (!c || !r.scoped_kind) return ''
  const name = r.scoped_kind.slice(3).replace(/_/g, ' ').replace(/^\w/, (ch) => ch.toUpperCase())
  if (c.limitedBy === 'own') return `Limited by the separate ${name} weekly limit, which is tighter than the all-models one`
  return r.pct_per_turn_scoped
    ? `The all-models weekly limit is tighter than the separate ${name} one`
    : `${name} also has its own weekly limit; too few ${name} messages this week to size it yet`
}

/** 「~剩 / 满」一格;每条代价未知 → —。 */
function pair(remain: number | null, pct: number | null | undefined): string {
  if (!pct || pct <= 0) return '—'
  const full = abbrevCount(Math.round(100 / pct))
  return remain === null ? `— / ${full}` : `~${abbrevCount(remain / pct)} / ${full}`
}

export default function MessagesCard({ platform, range, refreshTick }: {
  platform: SubscriptionPlatform
  range: DayRange
  refreshTick: number
}) {
  const [budget, setBudget] = useState<MessageBudget | null>(null)
  const [snaps, setSnaps] = useState<SubscriptionSnapshot[]>([])
  const [subTick, setSubTick] = useState(0)

  // 读数一变（subscription:changed）就重查快照与每条代价——落新样本会重拟合系数
  useEffect(() => {
    let timer = 0
    let off: (() => void) | null = null
    void events.onSubscriptionChanged(() => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => setSubTick((t) => t + 1), 300)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      window.clearTimeout(timer)
      off?.()
    }
  }, [])

  useEffect(() => {
    let stale = false
    void Promise.all([subscriptionService.getMessageBudget(platform, true), subscriptionService.getSnapshots()]).then(
      ([b, s]) => {
        if (stale) return
        setBudget(b)
        setSnaps(s ?? [])
      },
    )
    return () => {
      stale = true
    }
  }, [platform, refreshTick, subTick])

  const week = budget?.platform === platform ? budget.week : null
  const snap = snaps.find((s) => s.platform === platform && s.status !== 'idle')
  const nowSec = Date.now() / 1000
  const remain5h = remainOf(snap, '5h', nowSec)
  const remain7d = remainOf(snap, '7d', nowSec)
  const rows = budget?.platform === platform ? budget.rows : []
  // 行 = 有估计的模型 + 本周发过但样本不够估的模型（后者只有「已发」一列）
  const sent = new Map((week?.turns ?? []).map((t) => [t.model_key, t.turns]))
  const extra = (week?.turns ?? []).filter((t) => !rows.some((r) => r.model_key === t.model_key))
  const resetsInRange = (week?.resets ?? []).filter((r) => {
    const d = localDay(r.t)
    return d >= range.startDay && d <= range.endDay
  })
  const early = resetsInRange.filter((r) => r.early).length
  const account = week?.account ? ` (${week.account.charAt(0).toUpperCase()}${week.account.slice(1)})` : ''

  return (
    <section className="insight-card">
      <header className="insight-card-header">
        <span className="insight-card-title">Messages by model · {PLATFORM_LABEL[platform]}</span>
        <span className="insight-card-sub">
          {week?.start
            ? `Weekly window${account}: ${stamp(week.start)} – ${week.resets_at ? stamp(week.resets_at) : 'reset time not reported'}`
            : ''}
        </span>
      </header>
      {budget === null ? (
        <div className="insight-empty">Data unavailable (service not running)</div>
      ) : rows.length === 0 && extra.length === 0 ? (
        <div className="insight-empty">Not enough {PLATFORM_LABEL[platform]} messages in the last 30 days to estimate</div>
      ) : (
        <>
          <div className="price-totals">
            <div className="price-total-item" title="Current 5-hour quota left, from the latest reading">
              <span className="price-total-label">5-hour left</span>
              <span className="price-total-value">{remain5h === null ? '—' : `${Math.round(remain5h)}%`}</span>
            </div>
            <div className="price-total-item" title="Current weekly quota left, from the latest reading">
              <span className="price-total-label">Weekly left</span>
              <span className="price-total-value">{remain7d === null ? '—' : `${Math.round(remain7d)}%`}</span>
            </div>
            {/* 模型专属周限额（Claude Fable 那条）：与全模型周额度同时生效、各算各的*/}
            {(snap?.windows ?? [])
              .filter((w) => w.kind.startsWith('7d_') && w.kind !== '7d_opus' && w.kind !== '7d_sonnet')
              .map((w) => {
                const name = w.kind.slice(3).replace(/_/g, ' ').replace(/^\w/, (ch) => ch.toUpperCase())
                const r = remainOf(snap, w.kind, nowSec)
                return (
                  <div
                    key={w.kind}
                    className="price-total-item"
                    title={`${name} has its own weekly limit on top of the all-models one; whichever runs out first stops ${name}`}
                  >
                    <span className="price-total-label">{name} limit left</span>
                    <span className="price-total-value">{r === null ? '—' : `${Math.round(r)}%`}</span>
                  </div>
                )
              })}
            <div
              className="price-total-item"
              title="Messages you sent since the current weekly window started, on this account"
            >
              <span className="price-total-label">Sent this week</span>
              <span className="price-total-value">{week ? week.total_turns.toLocaleString('en-US') : '—'}</span>
            </div>
            <div
              className="price-total-item"
              title={
                resetsInRange.length
                  ? resetsInRange.map((r) => `${stamp(r.t)}${r.early ? ' · early' : ''}`).join('\n')
                  : 'No weekly reset seen in this range'
              }
            >
              <span className="price-total-label">Weekly resets</span>
              <span className="price-total-value">
                {resetsInRange.length}
                {early > 0 && <span className="price-total-aside"> ({early} early)</span>}
              </span>
            </div>
          </div>
          <table className="price-table">
            <thead>
              <tr>
                <th className="price-col-model">Model</th>
                <th title="Messages left at the current 5-hour quota / messages in a full 5-hour window">5-hour left / full</th>
                <th title="Messages left at the current weekly quota / messages in a full weekly window">Weekly left / full</th>
                <th title="Messages sent since the current weekly window started">Sent this week</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.model_key}>
                  <td
                    className="price-col-model"
                    title={
                      `${r.model_key}\n${basisText(r)}` +
                      `\nAverage message ≈ $${r.usd_per_turn.toFixed(2)} at this model's list price` +
                      `\nA full 5-hour window ≈ $${(100 / r.quota_factor).toFixed(0)} of this model at list price ` +
                      (r.factor_measured ? '(measured on your readings)' : '(platform average — not enough readings on this model yet)')
                    }
                  >
                    <span className="legend-swatch" style={{ background: colorFor(r.model_key) }} />
                    {shortModelName(r.model_key)}
                  </td>
                  <td>{pair(remain5h, r.pct_per_turn)}</td>
                  <td title={weekTitle(r, snap?.windows, nowSec)}>{weekCell(r, snap?.windows, nowSec)}</td>
                  <td>{(sent.get(r.model_key) ?? 0).toLocaleString('en-US')}</td>
                </tr>
              ))}
              {extra.map((t) => (
                <tr key={t.model_key}>
                  <td className="price-col-model" title={`${t.model_key}\nToo few messages in the last 30 days to estimate`}>
                    <span className="legend-swatch" style={{ background: colorFor(t.model_key) }} />
                    {shortModelName(t.model_key)}
                  </td>
                  <td>—</td>
                  <td>—</td>
                  <td>{t.turns.toLocaleString('en-US')}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {/* 说明只留一行：完整口径放 hover,避免卡片底部堆成一段*/}
          <p
            className="price-note"
            title={
              'A message is one prompt you send, averaged with big tasks included and subagent and auto-review overhead spread in.\n' +
              'A model you use a lot is sized on its own recent messages; one you have not used lately on its own last 30 days; ' +
              'one you have barely used on all your recent messages, priced for it.\n' +
              'Each model uses its own measured share of the quota per dollar — platforms do not charge every model at list price.\n' +
              'The weekly window follows the platform\'s own resets and can reset early. Two accounts on the same plan cannot be told apart, and switching between them can look like a reset.'
            }
          >
            Estimates from your own messages and each model's measured quota use; weekly window follows actual resets.
            Hover a model or this line for details.
            {week && week.scale === null && ' Weekly columns need more readings on this plan.'}
          </p>
        </>
      )}
    </section>
  )
}
