// CollectorHealth：采集器健康状态条（数据来自 CollectorService.ListSources）
import { useEffect, useState } from 'react'
import { collectorService, type SourceSummary } from '../../services'
import './health.css'

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

  useEffect(() => {
    let cancelled = false
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
    }
    load()
    // 每 30s 轮询一次（窗口隐藏时 Chromium 自动节流后台页定时器，无需额外处理）
    const timer = setInterval(load, 30000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [])

  if (!loaded) return <footer className="health-footer"><span className="health-summary">Loading collectors…</span></footer>

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
