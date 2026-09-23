// collectors 命名空间:每个键 = [英文, 简体中文]。术语。
// 覆盖：主窗口底部采集器健康状态条（CollectorHealth）。
export default {
  // 汇总
  loading: ['Loading collectors…', '正在加载采集器…'],
  paused: ['Collection paused', '采集已暂停'],
  collecting: ['Collecting…', '采集中…'],
  summary: ['Collectors {ok}/{total} OK', '采集器 {ok}/{total} 正常'],

  // 单源短注
  notePartial: ['Partial', '部分可用'],
  noteSchemaUnknown: ['Schema unknown', '格式未知'],
  noteNoSource: ['No source', '无数据源'],
  noteStale: ['Stale', '未更新'],
} as const satisfies Record<string, readonly [string, string]>
