// mock 数据：10 agents × 2026-08（31 天），含模型构成、估算、未来日、真实零。
// 仅在真实取数失败时作降级（UsageMatrixView）。

export interface MockModelSlice {
  modelKey: string
  displayName: string
  tokens: number
  quality: 'reported' | 'estimated'
}

export interface MockDayCell {
  total: number
  models: MockModelSlice[]
  quality: 'reported' | 'estimated' | 'mixed'
  health: 'healthy' | 'attention' | 'error'
}

export interface MockAgentRow {
  key: string
  label: string
  // days[d] 为 null 表示未来/不可用（不渲染），0 由 cell.total===0 表示
  days: (MockDayCell | null)[]
  health: 'healthy' | 'attention' | 'error'
}

export const MOCK_MONTH = '2026-08'
export const MOCK_TODAY = 27 // 模拟"今天"= 2026-08-27，其后为未来
export const MOCK_DAY_COUNT = 31

const MODELS = [
  { key: 'deepseek/deepseek-v4/v4/flash/default', displayName: 'DeepSeek V4 Flash', base: 80000 },
  { key: 'deepseek/deepseek-v4/v4/pro/default', displayName: 'DeepSeek V4 Pro', base: 60000 },
  { key: 'zhipu/glm-5.3/5.3/flash/default', displayName: 'GLM-5.3 Flash', base: 30000 },
  { key: 'zhipu/glm-5.3/5.3/pro/default', displayName: 'GLM-5.3 Pro', base: 20000 },
  { key: 'anthropic/claude-sonnet/4/sonnet/default', displayName: 'Claude Sonnet 4', base: 15000 },
]

const AGENTS: { key: string; label: string; weight: number; health: MockAgentRow['health'] }[] = [
  { key: 'zcode', label: 'ZCode', weight: 1.0, health: 'healthy' },
  { key: 'workbuddy', label: 'WorkBuddy', weight: 0.85, health: 'healthy' },
  { key: 'claude', label: 'Claude Code', weight: 0.7, health: 'healthy' },
  { key: 'opencode', label: 'OpenCode', weight: 0.5, health: 'healthy' },
  { key: 'codex', label: 'Codex', weight: 0.4, health: 'attention' },
  { key: 'cursor', label: 'Cursor', weight: 0.35, health: 'healthy' },
  { key: 'copilot', label: 'Copilot', weight: 0.3, health: 'healthy' },
  { key: 'gemini-cli', label: 'Gemini CLI', weight: 0.25, health: 'attention' },
  { key: 'aws-q', label: 'AWS Q', weight: 0.15, health: 'error' },
  { key: 'other', label: 'Other', weight: 0.1, health: 'healthy' },
]

// 确定性伪随机（不用 Math.random，便于 golden 对照）
function mulberry32(seed: number) {
  return function () {
    seed |= 0
    seed = (seed + 0x6d2b79f5) | 0
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

export function generateMockData(): MockAgentRow[] {
  const rng = mulberry32(20260801)
  return AGENTS.map((agent, ai) => {
    const days: (MockDayCell | null)[] = []
    for (let d = 0; d < MOCK_DAY_COUNT; d++) {
      if (d >= MOCK_TODAY + 2) {
        days.push(null) // 未来（留 1 天"今天"余量）
        continue
      }
      // 周末/工作日节奏 + 间歇
      const weekend = d % 7 === 0 || d % 7 === 6
      const activity = weekend ? 0.35 : 1
      const slump = (ai * 3 + d * 7) % 11 === 0 ? 0.15 : 1 // 偶尔低谷（含真实 0 天）
      const total = Math.floor(2000000 * agent.weight * activity * slump * (0.5 + rng()))
      if (total === 0) {
        days.push({ total: 0, models: [], quality: 'reported', health: agent.health })
        continue
      }
      // 模型构成（top 2-3 + 可能 estimated）
      const count = 2 + Math.floor(rng() * 2)
      const picked = [...MODELS].sort(() => rng() - 0.5).slice(0, count)
      const weights = picked.map(() => 0.3 + rng())
      const wSum = weights.reduce((a, b) => a + b, 0)
      let remaining = total
      const models = picked.map((m, i) => {
        const tokens = i === picked.length - 1 ? remaining : Math.floor(total * (weights[i] / wSum))
        remaining -= tokens
        const estimated = rng() > 0.85
        return { modelKey: m.key, displayName: m.displayName, tokens, quality: estimated ? 'estimated' as const : 'reported' as const }
      })
      const hasEstimated = models.some((m) => m.quality === 'estimated')
      days.push({
        total,
        models,
        quality: hasEstimated ? 'mixed' : 'reported',
        health: agent.health,
      })
    }
    return { key: agent.key, label: agent.label, days, health: agent.health }
  })
}
