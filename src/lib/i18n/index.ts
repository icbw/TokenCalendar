// 界面多语言：自写类型化字典（en / zh-CN）。
// - 语言单一源 = designPrefs.locale（undefined = 跟随系统语言）;经 storage 桥跨窗口广播,
//   四个窗口的 useT / useLocale 订阅同一 store,切换即时重渲染,无需刷新。
// - 字典按命名空间分文件（messages/*.ts）,每个键是 [英文, 中文] 二元组,缺一方编译即报错。
// - 模块顶层不要拼好文案常量：切换语言后不会更新。要么在渲染时调 t,要么写成函数。
// - 数据键（'en-CA' 的 YYYY-MM-DD、排序比较）不走本模块——那是数据,不是显示文字。

import { Fragment, createElement, useSyncExternalStore, type ReactNode } from 'react'
import { getDesignPrefs, subscribeDesignPrefs, type DesignPrefs } from '../../features/settings/designPrefs'
import { messages, type Namespace } from './messages'
import { systemLocale, type Locale } from './system'

export { systemLocale, type Locale }
export const LOCALES: readonly Locale[] = ['en', 'zh-CN']

export function effectiveLocale(p: DesignPrefs = getDesignPrefs()): Locale {
  return p.locale ?? systemLocale()
}

export function subscribeLocale(fn: () => void): () => void {
  return subscribeDesignPrefs(fn)
}

export function useLocale(): Locale {
  return useSyncExternalStore(subscribeLocale, () => effectiveLocale())
}

type Vars = Record<string, string | number>
type Dict<N extends Namespace> = (typeof messages)[N]
export type MessageKey<N extends Namespace> = keyof Dict<N> & string

const IDX: Record<Locale, 0 | 1> = { en: 0, 'zh-CN': 1 }

function lookup(ns: Namespace, key: string, locale: Locale): string {
  const entry = (messages[ns] as Record<string, readonly [string, string]>)[key]
  if (!entry) {
    console.warn(`[i18n] missing key ${ns}.${key}`)
    return key
  }
  return entry[IDX[locale]]
}

function interpolate(s: string, vars?: Vars): string {
  if (!vars) return s
  return s.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m))
}

export interface Translator<N extends Namespace> {
  (key: MessageKey<N>, vars?: Vars): string
  /** 占位符可以是 React 节点（链接、加粗、按钮…）:返回片段数组。 */
  rich(key: MessageKey<N>, vars: Record<string, ReactNode>): ReactNode
  locale: Locale
}

function makeT<N extends Namespace>(ns: N, locale: Locale): Translator<N> {
  const t = ((key: MessageKey<N>, vars?: Vars) => interpolate(lookup(ns, key, locale), vars)) as Translator<N>
  t.rich = (key, vars) => {
    const parts = lookup(ns, key, locale).split(/\{(\w+)\}/g)
    return createElement(
      Fragment,
      null,
      ...parts.map((p, i) => (i % 2 === 1 ? createElement(Fragment, { key: i }, p in vars ? vars[p] : `{${p}}`) : p)),
    )
  }
  t.locale = locale
  return t
}

const cache = new Map<string, Translator<Namespace>>()
function cachedT<N extends Namespace>(ns: N, locale: Locale): Translator<N> {
  const k = `${ns}|${locale}`
  let t = cache.get(k)
  if (!t) {
    t = makeT(ns, locale) as unknown as Translator<Namespace>
    cache.set(k, t)
  }
  return t as unknown as Translator<N>
}

/** 组件内取文案：订阅语言,切换即重渲染。 */
export function useT<N extends Namespace>(ns: N): Translator<N> {
  return cachedT(ns, useLocale())
}

/** 非组件代码（服务 / 通知 / 工具函数）取文案：按调用时刻的语言。 */
export function getT<N extends Namespace>(ns: N): Translator<N> {
  return cachedT(ns, effectiveLocale())
}

// ---------- 显示格式（按当前语言;数据键勿用） ----------

const intlTag = (l: Locale): string => (l === 'zh-CN' ? 'zh-CN' : 'en-US')

export const fmt = {
  /** 千分位整数 / 小数（1,234）。 */
  number(n: number, opts?: Intl.NumberFormatOptions): string {
    return n.toLocaleString(intlTag(effectiveLocale()), opts)
  },
  /** 日期时间（显示用）。 */
  dateTime(d: Date | number, opts?: Intl.DateTimeFormatOptions): string {
    return new Date(d).toLocaleString(intlTag(effectiveLocale()), opts)
  },
  date(d: Date | number, opts?: Intl.DateTimeFormatOptions): string {
    return new Date(d).toLocaleDateString(intlTag(effectiveLocale()), opts)
  },
  time(d: Date | number, opts?: Intl.DateTimeFormatOptions): string {
    return new Date(d).toLocaleTimeString(intlTag(effectiveLocale()), opts)
  },
  /** 显示用标签排序（中文按拼音）。 */
  compare(a: string, b: string): number {
    return a.localeCompare(b, intlTag(effectiveLocale()), { numeric: true })
  },
}

// ---------- 副作用：<html lang>（CJK 字体回退）与托盘菜单语言 ----------

if (typeof window !== 'undefined') {
  let last: Locale | null = null
  const sync = (): void => {
    const l = effectiveLocale()
    if (l === last) return
    last = l
    document.documentElement.lang = l
    if ('__TAURI_INTERNALS__' in window) {
      void import('@tauri-apps/api/core')
        .then(({ invoke }) => invoke('set_ui_locale', { locale: l }))
        .catch((e) => console.error('[i18n] set_ui_locale failed:', e))
    }
  }
  sync()
  subscribeDesignPrefs(sync)
}
