// timeline 命名空间:每个键 = [英文, 简体中文]。术语。
export default {
  // 日期轴（左侧日标签列宽固定 46px:十、十一、十二月的两位日号改用短写,避免被裁）
  dayHeadMonth: ['{mon} {d}', '{m}月{d}日'],
  dayHeadMonthNarrow: ['{mon} {d}', '{m}/{d}'],
  dayTitle: ['{date}', '{date} {weekday}'],

  // 会话格
  itemTurnsOne: ['{n} turn', '{n} 轮'],
  itemTurnsOther: ['{n} turns', '{n} 轮'],
  itemTip: [
    '{label}\n{start} – {end} · {agent}\n{turns} turns · {tokens} tokens\nDouble-click to open in {agent}',
    '{label}\n{start} – {end} · {agent}\n{turns} 轮 · {tokens} Token\n双击在 {agent} 中打开',
  ],

  // 距今时长
  agoUnderMinute: ['<1m', '不到 1 分钟'],
  agoMinutes: ['{n}m', '{n} 分钟'],
  agoHours: ['{n}h', '{n} 小时'],

  // 项目列头 / 条态提示
  inactiveBadge: ['{n}d', '{n} 天'],
  inactiveTip: ['No activity for {n} days', '已 {n} 天无活动'],
  tipWaiting: [
    'An agent is waiting for your reply (click to bring its window to the front)',
    'Agent 正在等你回复（点击将其窗口前置）',
  ],
  tipPending: [
    'A tool call has been pending for a while, maybe an approval (click to bring its window to the front)',
    '有工具调用已等待一段时间，可能需要批准（点击将其窗口前置）',
  ],
  tipStale: [
    'Last stopped here: the agent window is gone (click to open the folder)',
    '上次停在这里：Agent 窗口已关闭（点击打开文件夹）',
  ],
  stripWaiting: ['An agent is waiting for your reply', 'Agent 正在等你回复'],
  stripPending: ['A tool call has been pending for a while', '有工具调用已等待一段时间'],
  stripClickHint: [' (click to bring its window to the front)', '（点击将其窗口前置）'],
  dotWaiting: ['Waiting for reply', '等待回复'],
  dotPending: ['Tool pending', '工具等待中'],
  dotStale: ['Last stopped here', '上次停在这里'],

  // 顶栏 / 条态
  foldTitle: [
    'Fold into a strip at the top of the screen (double-click the strip to expand)',
    '折叠为屏幕顶部的条态（双击条态展开）',
  ],
  expandTitle: ['Expand the board', '展开看板'],
  stripEmpty: ['Timeline', '时间轴'],

  // 空态
  serviceNotRunning: ['Service not running', '服务未运行'],
  noActivityYet: ['No project activity yet', '暂无项目活动'],

  // hover 状态卡
  status: ['Status', '状态'],
  idle: ['Idle', '空闲'],
  statusWaiting: ['{n} waiting', '{n} 个等待'],
  statusRunning: ['{n} running', '{n} 个运行中'],
  reply: ['Reply', '回复'],
  tool: ['Tool', '工具'],
  today: ['Today', '今天'],
  todayValue: ['{turns} turns · {tokens}', '{turns} 轮 · {tokens}'],
  noActivity: ['No activity', '无活动'],
  lastDay: ['Last day', '最近活动'],
  daysAgo: [' ({n}d ago)', '（{n} 天前）'],
  span: ['Span', '跨度'],
  inWindow: ['In window', '时间窗内'],
  sessionsOne: ['{n} session', '{n} 个会话'],
  sessionsOther: ['{n} sessions', '{n} 个会话'],
  daysOne: ['{n} day', '{n} 天'],
  daysOther: ['{n} days', '{n} 天'],
  agents: ['Agents', 'Agent'],
  openFolder: ['Open Folder', '打开文件夹'],
  openInExplorer: ['Open in File Explorer', '在文件资源管理器中打开'],
  folderNotFound: ['Folder not found on this machine', '本机上找不到该文件夹'],
} as const satisfies Record<string, readonly [string, string]>
