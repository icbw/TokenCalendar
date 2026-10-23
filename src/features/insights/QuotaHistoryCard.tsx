// Insights 价格面板第三张卡：额度历史（读数日级汇总 → 日末已用柱状图）。
//
// 「读数两层」的界面:数据层 quota_reading / quota_daily 早已就位
//这张卡消费日级汇总层——
// get_quota_days 一年 ~365 行,只读、零网络,不扫几千行的读数层。
//
// 范围两档:近一月（30 天）/ 近一年（365 天）——数据一次拉一年,月档只是切片,切换零查询。
// 目测反馈:曲线在缺日场景断线碎,改柱状——无读数的日无柱,
// 空档不伪装成 0;「年月切换」补近一月档细看。
//
// 口径红线（与 subscriptionService.QuotaDay 注释一字不松）：
// - gain_pct = 当日观测到的额度消耗,**是下界不是账单**——滚动窗口里消耗与过期同时发生;
// - drop_pct = 窗口回收,与消耗分开存、不合成净值;
// - carry_secs = 当天第一条读数与它前面那条的间隔——空档之后的第一笔涨幅整笔记在
//   后一天,tooltip 里超过 6 小时的空档单独提示;
// - 无读数的日是空档:无柱,不伪装成 0。
import { useEffect, useMemo, useState } from 'react'
import { events, subscriptionService } from '../../services'
import type { SubscriptionPlatform } from '../../services'
import type { QuotaDay } from '../../services/subscriptionService'
import { colorFor, QuotaBars, type QuotaPoint } from './charts'
import { Seg } from './Seg'
import { useT } from '../../lib/i18n'

const PLATFORM_LABEL: Record<SubscriptionPlatform, string> = { codex: 'Codex', claude: 'Claude' }

const pad2 = (v: number): string => `${v}`.padStart(2, '0')
const dayStr = (d: Date): string => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`

/** 窗口种类固定顺序:5h → 7d → 专属周窗（7d_*）。 */
const kindRank = (k: string): number => (k === '5h' ? 0 : k === '7d' ? 1 : 2)
/** Seg 显示:数据键原样;专属周窗取派生名（与 orb / MessagesCard 同款:7d_opus → Opus）。 */
const kindLabel = (k: string): string => (k.startsWith('7d_') ? k.slice(3).replace(/_/g, ' ') : k)

export default function QuotaHistoryCard({ platform, refreshTick }: {
  platform: SubscriptionPlatform
  refreshTick: number
}) {
  const t = useT('insights')
  const [rows, setRows] = useState<QuotaDay[] | null>(null)
  const [kind, setKind] = useState('7d')
  const [range, setRange] = useState<'month' | 'year'>('year')
  const [subTick, setSubTick] = useState(0)

  // 读数一变（subscription:changed）就重查——quota_daily 随读数增量重算,本地只读零请求
  useEffect(() => {
    let timer = 0
    let off: (() => void) | null = null
    void events.onSubscriptionChanged(() => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => setSubTick((v) => v + 1), 300)
    }).then((unlisten) => {
      off = unlisten
    })
    return () => {
      window.clearTimeout(timer)
      off?.()
    }
  }, [])

  // 最近一年（含今日,共 365 个日历日;未来日不会出现,endDay = 今天）
  useEffect(() => {
    let stale = false
    const to = new Date()
    const from = new Date(to)
    from.setDate(from.getDate() - 364)
    void subscriptionService.getQuotaDays(platform, dayStr(from), dayStr(to)).then((res) => {
      if (!stale) setRows(res)
    })
    return () => {
      stale = true
    }
  }, [platform, subTick, refreshTick])

  // kinds 数据驱动（Codex: 5h / 7d;Claude 另有专属周窗）;当前 kind 不在数据里时回落首个
  const kinds = useMemo(
    () => [...new Set((rows ?? []).map((r) => r.kind))].sort((a, b) => kindRank(a) - kindRank(b) || a.localeCompare(b)),
    [rows],
  )
  const curKind = kinds.includes(kind) ? kind : (kinds[0] ?? kind)

  const byDay = useMemo(() => {
    const m = new Map<string, QuotaDay>()
    for (const r of rows ?? []) if (r.kind === curKind) m.set(r.day, r)
    return m
  }, [rows, curKind])

  const { days, points } = useMemo(() => {
    const span = range === 'month' ? 29 : 364 // 含今日共 30 / 365 个日历日
    const today = new Date()
    const days: string[] = []
    for (let i = span; i >= 0; i--) {
      const d = new Date(today)
      d.setDate(d.getDate() - i)
      days.push(dayStr(d))
    }
    const points: (QuotaPoint | null)[] = days.map((d) => {
      const r = byDay.get(d)
      return r === undefined
        ? null
        : {
            day: d,
            usedFirst: r.used_first,
            usedLast: r.used_last,
            usedMax: r.used_max,
            usedMin: r.used_min,
            gain: r.gain_pct,
            drop: r.drop_pct,
            resets: r.resets,
            carrySecs: r.carry_secs,
            n: r.n,
          }
    })
    return { days, points }
  }, [byDay, range])

  return (
    <section className="insight-card">
      <header className="insight-card-header">
        <span className="insight-card-title">{t('quotaHistoryTitle', { p: PLATFORM_LABEL[platform] })}</span>
        <span className="insight-card-sub">
          {t('quotaAxisNote')} · {t(range === 'month' ? 'quotaRangeMonth' : 'quotaRangeYear')}
        </span>
        <span style={{ marginLeft: 'auto', alignSelf: 'center', display: 'flex', gap: 6 }}>
          <Seg
            value={range}
            options={[
              { v: 'month' as const, label: t('quotaSegMonth'), hint: t('quotaSegMonthHint') },
              { v: 'year' as const, label: t('quotaSegYear'), hint: t('quotaSegYearHint') },
            ]}
            onChange={setRange}
          />
          {kinds.length > 1 && (
            <Seg
              value={curKind}
              options={kinds.map((k) => ({
                v: k,
                label: kindLabel(k),
                hint: k === '5h' ? t('quotaKind5hHint') : k === '7d' ? t('quotaKind7dHint') : t('quotaKindScopedHint'),
              }))}
              onChange={setKind}
            />
          )}
        </span>
      </header>
      {rows === null ? (
        <div className="insight-empty">{t('dataUnavailable')}</div>
      ) : rows.length === 0 ? (
        <div className="insight-empty">{t('quotaNoData', { p: PLATFORM_LABEL[platform] })}</div>
      ) : (
        <>
          <QuotaBars
            days={days}
            points={points}
            color={colorFor(platform)}
            text={{
              first: t('quotaTipFirst'),
              high: t('quotaTipHigh'),
              low: t('quotaTipLow'),
              gain: t('quotaGain'),
              drop: t('quotaDrop'),
              resets: t('quotaResets'),
              readings: t('quotaTipReadings'),
              carry: (h) => t('quotaCarry', { h }),
            }}
          />
          <p className="price-note" title={t('quotaNoteHint')}>
            {t('quotaNote')}
          </p>
        </>
      )}
    </section>
  )
}
