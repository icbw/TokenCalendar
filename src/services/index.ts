// services 统一出口：装配层只 import 这里，不直接触碰 @tauri-apps/api。

import * as usageService from './usageService'
import * as collectorService from './collectorService'
import * as windowService from './windowService'
import * as exportService from './exportService'
import * as dataService from './dataService'
import * as subscriptionService from './subscriptionService'
import * as events from './events'

export { usageService, collectorService, windowService, exportService, dataService, subscriptionService, events }
export { inTauri } from './tauri'
export type { WindowVisibility, OrbDockState } from './windowService'
export type { UsageResult, UsageRow, SourceSummary, ExportResult, QueryOptions } from './types'
export type { BreakdownDay, BreakdownSlice, ChangedKeys } from './contract'
export type { CreditSummary, CreditModelDaySlice } from './usageService'
export type { RangeSeriesQuery, RangeSeriesResult, RangeSeriesPoint } from './usageService'
export type { DataInfo, ImportFileResult } from './dataService'
export type { SubscriptionSnapshot, CredentialInfo, QuotaWindow, SubscriptionPlatform } from './subscriptionService'
