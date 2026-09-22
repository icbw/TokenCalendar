// 离开阈值控件 + 空档分布直方图。
// 直方图:get_gap_histogram 的 26 个对数桶等宽排布（横轴即对数刻度）,阈值竖线按桶内对数插值定位,
// 两侧计数 / 时长与合计取后端 within / beyond（与 daily_project.idle_ms 同判据）。
// 改阈值的唯一路径:set_idle_threshold（Rust 合并写 prefs.json + 同步重算 + emit usage:changed）,
// 成功后再 setDesignPrefs（{ idleThresholdMin })——designPrefs 持久化的是整份内存快照,
// 不同步内存值,下一次任意偏好保存会把旧阈值写回 prefs.json。
import { useEffect, useState } from 'react'
import { taskService } from '../../services'
import type { DayRange, GapHistogram } from '../../services'
import { formatFull } from '../matrix/matrixScale'
import { formatDuration } from '../insights/analytics'
import { getDesignPrefs, setDesignPrefs } from '../settings/designPrefs'

const FALLBACK = { minutes: 30, defaultMinutes: 30, minMinutes: 1, maxMinutes: 1440 }

/** 轴刻度用短格式:12s / 17m / 3h / 12d。 */
function shortGap(ms: number): string {
  const s = ms / 1000
  if (s < 60) return `${Math.round(s)}s`
  if (s < 3600) return `${Math.round(s / 60)}m`
  if (s < 86400) return `${Math.round(s / 3600)}h`
  return `${Math.round(s / 86400)}d`
}

function thresholdLabel(ms: number): string {
  const min = Math.round(ms / 60_000)
  return min % 60 === 0 && min >= 60 ? `${min / 60}h` : `${min}m`
}

function GapChart({ hist }: { hist: GapHistogram }) {
  const width = 760
  const height = 150
  const margin = { top: 18, right: 12, bottom: 22, left: 40 }
  const plot = { w: width - margin.left - margin.right, h: height - margin.top - margin.bottom }
  const n = hist.buckets.length
  const slot = n > 0 ? plot.w / n : plot.w
  const maxCount = Math.max(1, ...hist.buckets.map((b) => b.count))
  const t = hist.thresholdMs

  // 阈值 → 横坐标:首桶 [0,1s) 线性,其余桶内按对数插值;开放末桶沿用前一桶的对数步长
  const xOfMs = (ms: number): number => {
    for (let i = 0; i < n; i++) {
      const b = hist.buckets[i]
      if (b.hiMs !== null && ms >= b.hiMs) continue
      let frac: number
      if (b.loMs <= 0) frac = b.hiMs ? ms / b.hiMs : 0
      else if (b.hiMs === null) {
        const prev = hist.buckets[i - 1]
        const step = prev && prev.loMs > 0 ? Math.log(b.loMs / prev.loMs) : Math.log(10) / 4
        frac = Math.min(1, Math.log(ms / b.loMs) / step)
      } else frac = Math.log(ms / b.loMs) / Math.log(b.hiMs / b.loMs)
      return margin.left + (i + Math.max(0, Math.min(1, frac))) * slot
    }
    return margin.left + plot.w
  }
  const tx = xOfMs(t)

  return (
    <svg viewBox={`0 0 ${width} ${height}`} style={{ width: '100%', display: 'block' }} role="img" aria-label="Gap distribution">
      <line x1={margin.left} x2={margin.left + plot.w} y1={margin.top + plot.h} y2={margin.top + plot.h} stroke="var(--border)" />
      <text x={margin.left - 6} y={margin.top + 4} textAnchor="end" fontSize={10} fill="var(--text-faint)">{formatFull(maxCount)}</text>
      <text x={margin.left - 6} y={margin.top + plot.h} textAnchor="end" fontSize={10} fill="var(--text-faint)">0</text>
      {hist.buckets.map((b, i) => {
        const h = (b.count / maxCount) * plot.h
        const x = margin.left + i * slot + 1
        // 桶的几何中点落在阈值左侧 = 计入 human（跨阈值的那一桶按中点归属着色）
        const mid = b.hiMs === null ? b.loMs : b.loMs > 0 ? Math.sqrt(b.loMs * b.hiMs) : b.hiMs / 2
        const within = mid <= t
        const range = b.hiMs === null ? `≥ ${shortGap(b.loMs)}` : `${b.loMs === 0 ? '0' : shortGap(b.loMs)} – ${shortGap(b.hiMs)}`
        return (
          <g key={i}>
            <rect
              x={x}
              y={margin.top + plot.h - h}
              width={Math.max(1, slot - 2)}
              height={h}
              rx={2}
              className={within ? 'gap-bar is-within' : 'gap-bar is-beyond'}
            />
            {/* 整桶透明命中区:矮柱 / 空桶也能 hover 出计数*/}
            <rect x={margin.left + i * slot} y={margin.top} width={slot} height={plot.h} fill="transparent">
              <title>{`${range}: ${formatFull(b.count)} gaps`}</title>
            </rect>
          </g>
        )
      })}
      {/* 十倍刻度:1s / 10s / 100s … 在桶左缘（k 为 4 的倍数）*/}
      <text x={margin.left} y={height - 6} textAnchor="middle" fontSize={10} fill="var(--text-faint)">0</text>
      {hist.buckets.map((b, i) =>
        i > 0 && (i - 1) % 4 === 0 ? (
          <text key={`t${i}`} x={margin.left + i * slot} y={height - 6} textAnchor="middle" fontSize={10} fill="var(--text-faint)">
            {shortGap(b.loMs)}
          </text>
        ) : null,
      )}
      <line x1={tx} x2={tx} y1={margin.top - 4} y2={margin.top + plot.h} stroke="var(--danger)" strokeWidth={1.5} strokeDasharray="4 3" />
      <text x={tx} y={margin.top - 7} textAnchor="middle" fontSize={10} fontWeight={700} fill="var(--danger)">
        {thresholdLabel(t)}
      </text>
    </svg>
  )
}

export default function GapHistogramCard({ range, refreshTick }: { range: DayRange; refreshTick: number }) {
  const [info, setInfo] = useState(FALLBACK)
  const [input, setInput] = useState(() => String(getDesignPrefs().idleThresholdMin ?? FALLBACK.minutes))
  const [hist, setHist] = useState<GapHistogram | null | undefined>(undefined)
  const [applying, setApplying] = useState(false)
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null)
  const [histTick, setHistTick] = useState(0)

  // 运行时阈值以 Rust 为准（prefs.json 载入值;designPrefs 仅作首帧占位）
  useEffect(() => {
    let cancelled = false
    void taskService.getIdleThreshold().then((res) => {
      if (cancelled || !res) return
      setInfo(res)
      setInput(String(res.minutes))
    })
    return () => {
      cancelled = true
    }
  }, [])

  useEffect(() => {
    let cancelled = false
    void taskService.getGapHistogram(range).then((res) => {
      if (!cancelled) setHist(res)
    })
    return () => {
      cancelled = true
    }
  }, [range, refreshTick, histTick])

  const parsed = /^\d+$/.test(input.trim()) ? Number(input.trim()) : NaN
  const valid = Number.isInteger(parsed) && parsed >= info.minMinutes && parsed <= info.maxMinutes
  const dirty = valid && parsed !== info.minutes

  const apply = async () => {
    if (!valid || applying) return
    if (!dirty) {
      setStatus(null)
      return
    }
    setApplying(true)
    setStatus(null)
    const res = await taskService.setIdleThreshold(parsed)
    setApplying(false)
    if (!res) {
      setStatus({ ok: false, text: 'Could not apply the threshold' })
      return
    }
    // 双写:Rust 已落 prefs.json,这里只同步内存快照（persist 会写回同值,无副作用）
    setDesignPrefs({ idleThresholdMin: res.minutes })
    setInfo((prev) => ({ ...prev, minutes: res.minutes }))
    setInput(String(res.minutes))
    setStatus({ ok: true, text: `Applied · recomputed ${formatFull(res.recomputedDays)} agent-days in ${formatFull(res.elapsedMs)} ms` })
    setHistTick((x) => x + 1)
  }

  return (
    <section className="insight-card gap-card">
      <header className="insight-card-header gap-card-header">
        <span className="insight-card-title">Idle gaps</span>
        <span className="insight-card-sub">Gaps between turns up to the threshold count as human time; longer gaps count as away</span>
      </header>
      <div className="gap-controls">
        <label className="gap-threshold" title={`Idle threshold in minutes (${info.minMinutes}–${info.maxMinutes}, default ${info.defaultMinutes})`}>
          Idle threshold
          <input
            type="number"
            min={info.minMinutes}
            max={info.maxMinutes}
            step={1}
            value={input}
            onChange={(e) => {
              setInput(e.target.value)
              setStatus(null)
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void apply()
            }}
            aria-invalid={!valid || undefined}
          />
          min
        </label>
        <button
          className={`seg gap-apply${dirty && !applying ? ' is-active' : ''}${!dirty || applying ? ' is-disabled' : ''}`}
          aria-disabled={!dirty || applying || undefined}
          title={!valid ? `Enter a whole number of minutes between ${info.minMinutes} and ${info.maxMinutes}` : dirty ? 'Recompute human time with this threshold' : 'Threshold unchanged'}
          onClick={() => void apply()}
        >
          {applying ? 'Applying…' : 'Apply'}
        </button>
        {!valid && <span className="gap-status is-error">{info.minMinutes}–{info.maxMinutes} minutes</span>}
        {valid && status && <span className={`gap-status${status.ok ? '' : ' is-error'}`}>{status.text}</span>}
      </div>

      {hist === undefined ? (
        <div className="insight-empty">Loading…</div>
      ) : hist === null ? (
        <div className="insight-empty">Data unavailable (service not running)</div>
      ) : hist.total === 0 ? (
        <div className="insight-empty">No gaps between turns in this range</div>
      ) : (
        <>
          <GapChart hist={hist} />
          <div className="gap-stats">
            <div className="gap-stat">
              <span className="legend-swatch gap-swatch-within" />
              <span className="gap-stat-label">≤ {thresholdLabel(hist.thresholdMs)} · human</span>
              <span className="gap-stat-value">{formatFull(hist.withinCount)} gaps · {formatDuration(hist.withinMs)}</span>
            </div>
            <div className="gap-stat">
              <span className="legend-swatch gap-swatch-beyond" />
              <span className="gap-stat-label">&gt; {thresholdLabel(hist.thresholdMs)} · away</span>
              <span className="gap-stat-value">{formatFull(hist.beyondCount)} gaps · {formatDuration(hist.beyondMs)}</span>
            </div>
            <div className="gap-stat">
              <span className="gap-stat-label">Total</span>
              <span className="gap-stat-value">{formatFull(hist.total)} gaps · {formatDuration(hist.withinMs + hist.beyondMs)}</span>
            </div>
          </div>
        </>
      )}
    </section>
  )
}
