// 项目展示名缓存:别名 / Scratch / 合并目标名只有后端知道（project_meta 解析层）。
// 项目维取数封装（usageService / taskService）把响应里的 key → label 记到这里,
// analytics.projectDisplayName 优先读缓存,未命中再退回路径末段——既有调用点不必逐个改签名。
// 后端标签按结果集做同名消歧,缓存以最近一次响应为准;管理面板改名后发 usage:changed,各视图重取即覆盖。

const labels = new Map<string, string>()

export function rememberProjectLabels(pairs: Iterable<[string, string]>): void {
  for (const [key, label] of pairs) {
    if (key && label) labels.set(key, label)
  }
}

export function cachedProjectLabel(key: string): string | undefined {
  return labels.get(key)
}
