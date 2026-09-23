// 命名空间注册表：新增命名空间在此登记。
import settings from './settings'
import update from './update'
import collectors from './collectors'
import orb from './orb'
import timeline from './timeline'
import projects from './projects'
import insights from './insights'
import matrix from './matrix'
import window from './window'
import tasks from './tasks'

export const messages = {
  settings, update, collectors, orb, timeline, projects, insights, matrix, window, tasks,
}

export type Namespace = keyof typeof messages
