// 本地时间工具：月度视图/导出的当前月口径（本地时区日界线）。

export function currentMonth(): string {
  const n = new Date()
  return `${n.getFullYear()}-${String(n.getMonth() + 1).padStart(2, '0')}`
}

export function todayDate(): number {
  return new Date().getDate()
}

export function daysInCurrentMonth(): number {
  const n = new Date()
  return new Date(n.getFullYear(), n.getMonth() + 1, 0).getDate()
}
