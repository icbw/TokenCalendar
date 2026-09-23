// 系统语言判定（无依赖;designPrefs 与 i18n/index 共用,避免循环引用）。

export type Locale = 'en' | 'zh-CN'

/** 系统语言：中文（任何地区）→ 简体中文,其余 → 英文（与 Rust tray:system_locale 同口径）。 */
export function systemLocale(): Locale {
  const langs = typeof navigator === 'undefined' ? [] : navigator.languages?.length ? navigator.languages : [navigator.language]
  return (langs[0] ?? '').toLowerCase().startsWith('zh') ? 'zh-CN' : 'en'
}
