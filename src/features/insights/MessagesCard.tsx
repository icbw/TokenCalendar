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
import { fmt, getT, useT } from '../../lib/i18n'

const PLATFORM_LABEL: Record<SubscriptionPlatform, string> = { codex: 'Codex', claude: 'Claude' }

/** 时刻 → en `Sep 21, 12:34` / zh `9月21日 12:34`（显示用,按当前语言）。 */
const stamp = (t: number): string =>
  fmt.dateTime(t * 1000, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false })
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
  const t = getT('insights')
  const name = shortModelName(r.model_key)
  if (r.basis === 'recent_own') return t('basisRecent', { n: r.basis_n, name })
  if (r.basis === 'history_own') return t('basisHistory', { n: r.basis_n, name })
  return t('basisAll', { n: r.basis_n, name })
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
  const t = getT('insights')
  if (c.limitedBy === 'own') return t('limitedOwn', { name })
  return r.pct_per_turn_scoped ? t('limitedAll', { name }) : t('limitedUnsized', { name })
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
  const t = useT('insights')
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
  const account = week?.account ? t('msgAccount', { a: `${week.account.charAt(0).toUpperCase()}${week.account.slice(1)}` }) : ''

  return (
    <section className="insight-card">
      <header className="insight-card-header">
        <span className="insight-card-title">{t('msgTitle', { p: PLATFORM_LABEL[platform] })}</span>
        <span className="insight-card-sub">
          {week?.start
            ? t('msgWeekWindow', {
                account,
                start: stamp(week.start),
                end: week.resets_at ? stamp(week.resets_at) : t('msgResetUnknown'),
              })
            : ''}
        </span>
      </header>
      {budget === null ? (
        <div className="insight-empty">{t('dataUnavailable')}</div>
      ) : rows.length === 0 && extra.length === 0 ? (
        <div className="insight-empty">{t('msgNotEnough', { p: PLATFORM_LABEL[platform] })}</div>
      ) : (
        <>
          <div className="price-totals">
            <div className="price-total-item" title={t('left5hHint')}>
              <span className="price-total-label">{t('left5h')}</span>
              <span className="price-total-value">{remain5h === null ? '—' : `${Math.round(remain5h)}%`}</span>
            </div>
            <div className="price-total-item" title={t('leftWeekHint')}>
              <span className="price-total-label">{t('leftWeek')}</span>
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
                    title={t('scopedLeftHint', { name })}
                  >
                    <span className="price-total-label">{t('scopedLeft', { name })}</span>
                    <span className="price-total-value">{r === null ? '—' : `${Math.round(r)}%`}</span>
                  </div>
                )
              })}
            <div
              className="price-total-item"
              title={t('sentWeekHint')}
            >
              <span className="price-total-label">{t('sentWeek')}</span>
              <span className="price-total-value">{week ? fmt.number(week.total_turns) : '—'}</span>
            </div>
            <div
              className="price-total-item"
              title={
                resetsInRange.length
                  ? resetsInRange.map((r) => `${stamp(r.t)}${r.early ? t('resetEarlyMark') : ''}`).join('\n')
                  : t('resetNone')
              }
            >
              <span className="price-total-label">{t('resets')}</span>
              <span className="price-total-value">
                {resetsInRange.length}
                {early > 0 && <span className="price-total-aside">{t('resetEarlyCount', { n: early })}</span>}
              </span>
            </div>
          </div>
          <table className="price-table">
            <thead>
              <tr>
                <th className="price-col-model">{t('colModel')}</th>
                <th title={t('col5hHint')}>{t('col5h')}</th>
                <th title={t('colWeekHint')}>{t('colWeek')}</th>
                <th title={t('colSentHint')}>{t('sentWeek')}</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.model_key}>
                  <td
                    className="price-col-model"
                    title={
                      `${r.model_key}\n${basisText(r)}` +
                      '\n' + t('avgMessage', { usd: r.usd_per_turn.toFixed(2) }) +
                      '\n' + t('fullWindow', {
                        usd: (100 / r.quota_factor).toFixed(0),
                        basis: r.factor_measured ? t('factorMeasured') : t('factorPlatform'),
                      })
                    }
                  >
                    <span className="legend-swatch" style={{ background: colorFor(r.model_key) }} />
                    {shortModelName(r.model_key)}
                  </td>
                  <td>{pair(remain5h, r.pct_per_turn)}</td>
                  <td title={weekTitle(r, snap?.windows, nowSec)}>{weekCell(r, snap?.windows, nowSec)}</td>
                  <td>{fmt.number(sent.get(r.model_key) ?? 0)}</td>
                </tr>
              ))}
              {extra.map((row) => (
                <tr key={row.model_key}>
                  <td className="price-col-model" title={`${row.model_key}\n${t('tooFewToEstimate')}`}>
                    <span className="legend-swatch" style={{ background: colorFor(row.model_key) }} />
                    {shortModelName(row.model_key)}
                  </td>
                  <td>—</td>
                  <td>—</td>
                  <td>{fmt.number(row.turns)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {/* 说明只留一行：完整口径放 hover,避免卡片底部堆成一段*/}
          <p
            className="price-note"
            title={t('msgNoteHint')}
          >
            {t('msgNote')}
            {week && week.scale === null && t('msgNoteNeedReadings')}
          </p>
        </>
      )}
    </section>
  )
}
