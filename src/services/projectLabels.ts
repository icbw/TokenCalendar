// 项目展示名缓存:别名 / Scratch / 合并目标名只有后端知道（project_meta 解析层）。
// 项目维取数封装（usageService / taskService）把响应里的 key → label 记到这里,
// analytics.projectDisplayName 优先读缓存,未命中再退回路径末段——既有调用点不必逐个改签名。
// 后端标签按结果集做同名消歧,缓存以最近一次响应为准;管理面板改名后发 usage:changed,各视图重取即覆盖。
//
// 后端内置伪项目的默认名（Rust project_meta:SCRATCH_LABEL "Scratch" / UNKNOWN_LABEL "Unknown project"）
// 是英文常量:读出时按当前语言换成显示名（localizeProjectLabel）;用户起的别名、目录名原样不译。
// 缓存里存的是后端原文,切换语言后下次读取即跟随。

import { getT, type Translator } from '../lib/i18n'

const labels = new Map<string, string>()

/** 与 Rust project_meta:SCRATCH_KEY / turns:UNKNOWN_PROJECT 及其默认标签对齐。 */
const BUILTIN_DEFAULT_LABELS: Record<string, string> = { __scratch: 'Scratch', unknown: 'Unknown project' }

/** 后端标签 → 显示名:内置伪项目且仍是后端默认名 → 当前语言的名称;其余（别名 / 目录名）原样。
 * 组件内可传入 useT（'projects') 的 t（便于作 memo 依赖）,缺省按调用时刻语言。 */
export function localizeProjectLabel(key: string, label: string, t: Translator<'projects'> = getT('projects')): string {
  if (BUILTIN_DEFAULT_LABELS[key] !== label) return label
  return key === 'unknown' ? t('unknownProject') : t('scratch')
}

export function rememberProjectLabels(pairs: Iterable<[string, string]>): void {
  for (const [key, label] of pairs) {
    if (key && label) labels.set(key, label)
  }
}

export function cachedProjectLabel(key: string): string | undefined {
  const label = labels.get(key)
  return label === undefined ? undefined : localizeProjectLabel(key, label)
}
