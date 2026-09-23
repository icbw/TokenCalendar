// orb 命名空间:每个键 = [英文, 简体中文]。术语。
// 悬浮球是 110px 小表盘：表盘内与底部两行是定宽槽位,中文取短写法（倒计时单位紧凑不加空格）。
export default {
  // 状态文案（statusHint;hover 提示）
  statusActive: ['Active', '正常'],
  statusPlanInactive: ['Subscription inactive — resumes after renewal', '订阅未生效——续费后自动恢复'],
  statusAuthFailed: ['Credentials expired — run the agent CLI to refresh', '凭据已失效——运行 Agent CLI 以刷新'],
  statusRateLimited: ['Rate limited — retrying automatically', '请求受限——正在自动重试'],
  statusNetworkFailed: ['Network error — showing last known data', '网络错误——显示最近一次数据'],
  statusParseFailed: ['Upstream response unrecognized', '上游响应无法识别'],
  statusNotBound: ['Not bound — manage in Settings', '未绑定——前往设置管理'],

  // 倒计时（表盘底部周重置行 + 提示;定宽槽位,中文紧凑写法）
  countdownDHM: ['{d}d {h}h {m}m', '{d}天{h}时{m}分'],
  countdownHM: ['{h}h {m}m', '{h}时{m}分'],
  countdownM: ['{m}m', '{m}分'],

  // 套餐行（平台名 + 套餐名;无套餐名时的兜底）
  planSubscription: ['Subscription', '订阅'],
  planNotBound: ['Not bound', '未绑定'],

  // hover 提示：口径名
  tipWeeklyQuota: ['Weekly quota', '周额度'],
  tipFiveHourQuota: ['5-hour quota', '5 小时额度'],
  tipWeeklyReset: ['Weekly reset', '周重置'],
  tipSubscriptions: ['Subscriptions', '订阅'],
  // hover 提示：读数行
  tipIdle: ['idle — updates after first use', '未使用——首次使用后更新'],
  tipLeft: ['{pct} / 100 left', '剩余 {pct} / 100'],
  // 窗口已开始、整数读数仍为 0（服务端只报整数百分比）
  tipUnderOne: ['over 99 / 100 left — under 1% used', '剩余 99 以上 / 100——已用不足 1%'],
  tipScopedLeft: ['{name} limit {pct} / 100 left', '{name} 限额剩余 {pct} / 100'],
  tipResetsIn: ['resets in {t}', '{t}后重置'],
  tipResetsAt: ['resets at {t}', '{t} 重置'],
  tipSwitch: ['Switch · {i} / {n}', '切换 · {i} / {n}'],
  tipOnlyOne: ['Only one subscription bound', '仅绑定了一个订阅'],

  // 悬挂按钮（提示 + aria-label）
  btnSettings: ['Subscription settings', '订阅设置'],
  btnRefresh: ['Refresh now', '立即刷新'],
  btnCollapseTip: ['Collapse to strip', '收起为竖条'],
  btnCollapse: ['Collapse', '收起'],
  btnSwitch: ['Switch subscription', '切换订阅'],

  // 表盘内读数（定宽槽位）
  gaugeIdle: ['idle', '未使用'],
  weekIdle: ['7d idle', '本周未使用'],

  // 右键菜单
  menuHide: ['Hide orb', '隐藏悬浮球'],
} as const satisfies Record<string, readonly [string, string]>
