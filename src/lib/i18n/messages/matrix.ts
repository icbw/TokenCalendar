// matrix 命名空间:每个键 = [英文, 简体中文]。术语。
export default {
  // 月份缩写（热力图月标签 / 表头 / 日期拼接）
  monthJan: ['Jan', '1月'],
  monthFeb: ['Feb', '2月'],
  monthMar: ['Mar', '3月'],
  monthApr: ['Apr', '4月'],
  monthMay: ['May', '5月'],
  monthJun: ['Jun', '6月'],
  monthJul: ['Jul', '7月'],
  monthAug: ['Aug', '8月'],
  monthSep: ['Sep', '9月'],
  monthOct: ['Oct', '10月'],
  monthNov: ['Nov', '11月'],
  monthDec: ['Dec', '12月'],

  // 日期拼接（{month} = 上面的月份缩写）
  monthDay: ['{month} {day}', '{month}{day}日'],
  fullDate: ['{month} {day}, {year}', '{year}年{month}{day}日'],
  // 中文省略年份：窗口恒以今天结尾,年份自明;带年份在默认宽度下会被截断
  rangeLabel: ['{from} – {to}, {year}', '{from} – {to}'],
  weekOf: ['Week of {date}', '{date} 起一周'],

  // 格子读数 / 提示
  noUsage: ['No usage', '无用量'],
  notAvailable: ['N/A', '无数据'],
  tokensValue: ['Tokens: {n}', 'Token：{n}'],
  valueWithUnit: ['{value} {unit}', '{value} {unit}'],
  messagesPart: [' · {n} messages', ' · {n} 条消息'],
  qualityEstimated: ['Quality: estimated', '质量：估算'],
  collectorStatus: ['Collector status: {status}', '采集器状态：{status}'],
  healthAttention: ['attention', '需关注'],
  healthError: ['error', '错误'],
  total: ['Total', '合计'],
  totalValue: ['Total {n}', '合计 {n}'],
  loading: ['Loading…', '加载中…'],
  demoSuffix: [' (demo)', '（演示）'],

  // 无障碍标签
  yearGridAria: ['Year token activity', '全年 Token 活动'],
  monthGridAria: ['Monthly usage matrix', '月度用量矩阵'],

  // 挂件 chrome
  granDaily: ['Daily', '每日'],
  granWeekly: ['Weekly', '每周'],
  granCumulative: ['Cumulative', '累计'],
  viewAria: ['View: {cur}, next {next}', '视图：{cur}，下一档 {next}'],
  lock: ['Lock', '锁定'],
  unlock: ['Unlock', '解锁'],
  lockWidget: ['Lock widget', '锁定挂件'],
  unlockWidget: ['Unlock widget', '解锁挂件'],
  mainWindow: ['Main window', '主窗口'],
  openMainWindow: ['Open main window', '打开主窗口'],
  resetSize: ['Reset size', '重置尺寸'],

  // 矩阵工具栏：指标
  metricTokens: ['Tokens', 'Token'],
  metricInput: ['Input', '输入'],
  metricCacheW: ['Cache W', '缓存写'],
  metricCacheR: ['Cache R', '缓存读'],
  metricOutput: ['Output', '输出'],
  metricTokensHint: ['Total tokens = input + cache write + cache read + output', 'Token 总量 = 输入 + 缓存写入 + 缓存读取 + 输出'],
  metricInputHint: ['Input tokens not served from cache (cache miss)', '未命中缓存的输入 Token（缓存未命中）'],
  metricCacheWHint: ['Cache write: input tokens written to the prompt cache', '缓存写入：写入提示缓存的输入 Token'],
  metricCacheRHint: ['Cache read: input tokens served from the prompt cache (cache hit)', '缓存读取：由提示缓存提供的输入 Token（缓存命中）'],
  metricOutputHint: ['Output tokens', '输出 Token'],
  unitTokens: ['tokens', 'Token'],
  unitInput: ['input tokens', '输入 Token'],
  unitCacheW: ['cache-write tokens', '缓存写入 Token'],
  unitCacheR: ['cache-read tokens', '缓存读取 Token'],
  unitOutput: ['output tokens', '输出 Token'],

  // 矩阵工具栏：分组 / 粒度
  groupAgent: ['Agent', 'Agent'],
  groupModel: ['Model', '模型'],
  groupProject: ['Project', '项目'],
  groupAgentHint: ['One row per agent', '每个 Agent 一行'],
  groupModelHint: ['One row per model', '每个模型一行'],
  groupProjectHint: ['One row per project (working directory)', '每个项目一行（按工作目录）'],
  bucketCumShort: ['Cum.', '累计'],
  bucketDayHint: ['Daily buckets', '按日汇总'],
  bucketWeekHint: ['Weekly buckets', '按周汇总'],
  bucketCumHint: ['Cumulative running total', '逐日累计'],

  // 矩阵右缘开关
  scaleGlobal: ['Color scale: global (click for per row)', '色阶：全局（点击切换为逐行）'],
  scalePerRow: ['Color scale: per row (click for global)', '色阶：逐行（点击切换为全局）'],
  sortByTokens: ['Sort by tokens (click to sort by name)', '按 Token 排序（点击改为按名称）'],
  sortByName: ['Sort by name (click to sort by tokens)', '按名称排序（点击改为按 Token）'],
  familyOn: ['Grouped by model family (click to ungroup)', '已按模型家族分组（点击取消）'],
  familyOff: ['Group by model family', '按模型家族分组'],

  // 图表面板
  allSources: ['All sources', '全部来源'],
  allTotal: ['All · total', '全部 · 合计'],
  allModels: ['All models', '全部模型'],
  allProjects: ['All projects', '全部项目'],
  allAgents: ['All agents', '全部 Agent'],
  expandChart: ['Expand chart — {title}', '展开图表 — {title}'],
  stackedBars: ['Stacked bars', '堆叠柱'],
  lines: ['Lines', '曲线'],
  showPerSeries: ['Show per-series curves', '显示分系列曲线'],
  showTotalOnly: ['Show all-source total only', '仅显示全部来源合计'],
  backToPreset: ['Back to all-series preset', '返回全系列预设'],
  collapseChart: ['Collapse chart panel', '折叠图表面板'],
  dataUnavailable: ['Data unavailable (service not running)', '数据不可用（服务未运行）'],
  noUsageInWindow: ['No usage records in this window', '此窗口内无用量记录'],

  // 行明细（RowBreakdown）
  dailyModelBreakdown: ['Daily model breakdown', '每日模型明细'],
  dailyAgentBreakdown: ['Daily agent breakdown', '每日 Agent 明细'],
  lineChart: ['Line chart', '折线图'],
  stackedChart: ['Stacked chart', '堆叠图'],
  stack: ['Stack', '堆叠'],
  closeBreakdown: ['Close breakdown', '关闭明细'],
  breakdownUnavailable: ['Breakdown unavailable (demo mode or service not running)', '明细不可用（演示模式或服务未运行）'],
  noBreakdownData: ['No breakdown data in this window', '此窗口内无明细数据'],
} as const satisfies Record<string, readonly [string, string]>
