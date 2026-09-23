// window 命名空间:每个键 = [英文, 简体中文]。术语。
export default {
  // 标题栏视图 / 窗口开关
  orbit: ['Orbit', '悬浮球'],
  showOrb: ['Show orbit orb', '显示悬浮球'],
  hideOrb: ['Hide orbit orb', '隐藏悬浮球'],
  widget: ['Widget', '挂件'],
  showWidget: ['Show widget window', '显示挂件窗口'],
  hideWidget: ['Hide widget window', '隐藏挂件窗口'],
  matrix: ['Matrix', '矩阵'],
  matrixHint: ['Matrix view', '矩阵视图'],
  insights: ['Insights', '洞察'],
  insightsHint: ['Insights charts', '洞察图表'],
  tasks: ['Tasks', '任务'],
  tasksHint: ['Tasks: per-session turns, steps and time', '任务：每个会话的轮次、步骤与时间'],
  settings: ['Settings', '设置'],
  settingsUpdateReady: ['Settings — TokenCalendar {version} is ready to install', '设置 — TokenCalendar {version} 已可安装'],
  updateReady: ['Update ready', '更新已就绪'],

  // 窗口控制
  minimize: ['Minimize', '最小化'],
  maximize: ['Maximize', '最大化'],
  restore: ['Restore', '还原'],
  close: ['Close', '关闭'],
} as const satisfies Record<string, readonly [string, string]>
