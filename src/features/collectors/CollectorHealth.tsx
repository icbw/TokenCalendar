// CollectorHealth：采集器健康状态条（各源健康 = list_sources;采集进度 = get_collect_status + collector:status）
//
// 各源健康读的是 source_state（上次运行留下的记录）,启动后首轮跑完前它说的是过去:
// 首轮未完 / 一轮跑得久（大量回填）时显示「Collecting…」,收轮即重取健康,热力图同步由 usage:changed 刷新。
import { useEffect, useState } from 'react'
import { collectorService, events, type SourceSummary } from '../../services'
import { toCollectStatus, type CollectStatus } from '../../services/collectorService'
import './health.css'

/** 已追上之后,单轮超过这么久才亮「Collecting…」（常规一轮毫秒级,避免每轮闪一下）。 */
const LONG_ROUND_MS = 1500

type StatusLevel = 'healthy' | 'attention' | 'error'

function levelOf(s: SourceSummary): StatusLevel {
  if (s.probeStatus === 'ready') {
    return s.stale || s.lastErrorMessage ? 'attention' : 'healthy'
  }
  if (s.probeStatus === 'partial') return 'attention'
  if (s.probeStatus === 'no_source') return 'attention'
  return 'error'
}

function noteOf(s: SourceSummary): string | undefined {
  if (s.lastErrorMessage) return s.lastErrorMessage.slice(0, 60)
  if (s.probeStatus === 'partial') return 'Partial'
  if (s.probeStatus === 'unsupported_schema') return 'Schema unknown'
  if (s.probeStatus === 'no_source') return 'No source'
  if (s.stale) return 'Stale'
  return undefined
}

export default function CollectorHealth() {
  const [sources, setSources] = useState<SourceSummary[]>([])
  const [loaded, setLoaded] = useState(false)
  const [status, setStatus] = useState<CollectStatus | null>(null)
  const [paused, setPaused] = useState(false)
  const [longRound, setLongRound] = useState(false)

  useEffect(() => {
    let cancelled = false
    let off: (() => void) | null = null
    const load = () => {
      collectorService
        .listSources()
        .then((list: SourceSummary[] | null) => {
          if (cancelled) return
          setSources(list || [])
          setLoaded(true)
        })
        .catch(() => {
          if (cancelled) return
          setLoaded(true)
        })
      void collectorService.getPaused().then((p) => {
        if (!cancelled && p !== null) setPaused(p)
      })
    }
    load()
    void collectorService.getCollectStatus().then((s) => {
      if (!cancelled && s) setStatus(s)
    })
    void events
      .onCollectStatus((raw) => {
        if (cancelled) return
        const s = toCollectStatus(raw)
        setStatus(s)
        if (s.source === null) load() // 收轮:健康记录刚更新
      })
      .then((unlisten) => {
        if (cancelled) unlisten()
        else off = unlisten
      })
    // 每 30s 轮询一次兜底（窗口隐藏时 Chromium 自动节流后台页定时器，无需额外处理）
    const timer = setInterval(load, 30000)
    return () => {
      cancelled = true
      clearInterval(timer)
      off?.()
    }
  }, [])

  // 一轮跑过 LONG_ROUND_MS 仍未收 → 亮「Collecting…」
  const roundStartedAt = status?.roundStartedAt ?? null
  useEffect(() => {
    setLongRound(false)
    if (roundStartedAt === null) return
    const wait = Math.max(0, roundStartedAt + LONG_ROUND_MS - Date.now())
    const timer = window.setTimeout(() => setLongRound(true), wait)
    return () => window.clearTimeout(timer)
  }, [roundStartedAt])

  if (!loaded) return <footer className="health-footer"><span className="health-summary">Loading collectors…</span></footer>

  if (paused) {
    return (
      <footer className="health-footer">
        <span className="health-summary"><span className="health-dot is-attention" />Collection paused</span>
      </footer>
    )
  }

  const collecting = status !== null && (!status.firstRoundDone || (status.source !== null && longRound))
  if (collecting) {
    const name = sources.find((s) => s.id === status.source)?.adapterName ?? status.source
    return (
      <footer className="health-footer">
        <span className="health-summary">
          <span className="health-dot is-collecting" />
          Collecting…{name ? <span className="health-note">{name}</span> : null}
        </span>
      </footer>
    )
  }

  const items = sources.map((s) => ({ ...s, level: levelOf(s), note: noteOf(s) }))
  const ok = items.filter((s) => s.level === 'healthy').length

  return (
    <footer className="health-footer">
      <span className="health-summary">
        <span className={`health-dot ${ok === items.length && items.length > 0 ? 'is-healthy' : 'is-attention'}`} />
        Collectors {ok}/{items.length} OK
      </span>
      {items.filter((s) => s.level !== 'healthy').map((s) => (
        <span key={s.id} className="health-item" title={s.id}>
          <span className={`health-dot is-${s.level}`} />
          {s.adapterName}
          {s.note ? <span className="health-note">{s.note}</span> : null}
        </span>
      ))}
    </footer>
  )
}
