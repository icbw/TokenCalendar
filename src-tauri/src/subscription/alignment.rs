//! 5h 额度读数 × 任务执行双轨的窗口重建。
//!
//! 只读 `quota_reading`，不改快照、日汇总或标定判据。边界只来自服务端申报的窗尾；
//! 缺窗尾、套餐变化和窗尾异常都保留为证据断点，不用读数下降补造重置。

use serde::Serialize;

use super::model::{Platform, QuotaReading};

pub const FIVE_HOURS_SECS: i64 = 5 * 3600;
pub const MAX_DISPLAY_READINGS: usize = 1200;
const CODEX_ROLLING_RESET_TOLERANCE_SECS: i64 = 120;
const SAME_WINDOW_TAIL_SECS: i64 = 120;
const TAIL_REGRESSION_SECS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowEvidence {
    AnnouncedTail,
    TailMinus5h,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscontinuityReason {
    Reset,
    WindowStarted,
    PlanChanged,
    TailRegressed,
    TailChanged,
    EvidenceMissing,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaDiscontinuity {
    pub from_t: i64,
    pub to_t: i64,
    /// 旧窗已经申报、且落在两条读数之间的重置点；其余情况不在读数间伪造精确时刻。
    pub at: Option<i64>,
    pub reason: DiscontinuityReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaWindowSegment {
    pub id: usize,
    pub start_at: Option<i64>,
    pub end_at: Option<i64>,
    pub start_evidence: WindowEvidence,
    pub end_evidence: WindowEvidence,
    pub first_reading_at: i64,
    pub last_reading_at: i64,
    /// 与视口外的前驱 / 后继仍属同一证据段，画到边缘时不伪造成观测空档。
    pub continues_left: bool,
    pub continues_right: bool,
    pub readings: Vec<QuotaReading>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaTimeline {
    pub windows: Vec<QuotaWindowSegment>,
    pub discontinuities: Vec<QuotaDiscontinuity>,
    pub raw_readings: usize,
    pub returned_readings: usize,
    pub downsampled: bool,
}

#[derive(Clone)]
struct ContextReading {
    row: QuotaReading,
    /// -1 = 左前驱，0 = 区间内，1 = 右后继。
    scope: i8,
}

struct WorkingSegment {
    key: Option<i64>,
    rows: Vec<ContextReading>,
}

/// 单条读数能否证明一个已开始的 5h 窗口及其窗尾。
/// Codex 未开始时也报窗尾，但它约为 `t + 5h` 并随时间漂移，不能据此生成假窗口。
fn announced_end(platform: Platform, row: &QuotaReading) -> Option<i64> {
    let end = row.resets_at.filter(|&end| end > row.t)?;
    if row.used_percent > 0.0 {
        return Some(end);
    }
    match platform {
        Platform::Claude => Some(end),
        Platform::Codex => {
            (end - row.t < FIVE_HOURS_SECS - CODEX_ROLLING_RESET_TOLERANCE_SECS).then_some(end)
        }
    }
}

/// 同一服务窗口的窗尾可能只有秒级取整 / 时钟抖动；120 秒内仍归同一证据段。
fn same_window_key(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= SAME_WINDOW_TAIL_SECS as u64,
        _ => a == b,
    }
}

fn break_between(
    prev: &QuotaReading,
    prev_key: Option<i64>,
    curr: &QuotaReading,
    curr_key: Option<i64>,
) -> Option<(DiscontinuityReason, Option<i64>)> {
    if !prev.plan_type.is_empty() && !curr.plan_type.is_empty() && prev.plan_type != curr.plan_type
    {
        return Some((DiscontinuityReason::PlanChanged, None));
    }
    if let (Some(a), Some(b)) = (prev.resets_at, curr.resets_at) {
        if b + TAIL_REGRESSION_SECS < a {
            return Some((DiscontinuityReason::TailRegressed, None));
        }
    }
    if same_window_key(prev_key, curr_key) {
        return None;
    }
    if let Some(old_end) = prev_key {
        if old_end > prev.t && old_end <= curr.t {
            return Some((DiscontinuityReason::Reset, Some(old_end)));
        }
    }
    Some(match (prev_key, curr_key) {
        (Some(_), None) if curr.resets_at.is_none() => (DiscontinuityReason::EvidenceMissing, None),
        (None, Some(_)) => (DiscontinuityReason::WindowStarted, None),
        _ => (DiscontinuityReason::TailChanged, None),
    })
}

fn downsample_rows(rows: &[QuotaReading], limit: usize) -> Vec<QuotaReading> {
    if rows.len() <= limit {
        return rows.to_vec();
    }
    let buckets = (limit / 4).max(1);
    let mut out = Vec::with_capacity((buckets * 4).min(rows.len()));
    for bucket in 0..buckets {
        let from = bucket * rows.len() / buckets;
        let to = (bucket + 1) * rows.len() / buckets;
        if from >= to {
            continue;
        }
        let slice = &rows[from..to];
        let min = slice
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.used_percent.total_cmp(&b.used_percent))
            .map(|(i, _)| from + i)
            .unwrap_or(from);
        let max = slice
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.used_percent.total_cmp(&b.used_percent))
            .map(|(i, _)| from + i)
            .unwrap_or(from);
        let mut indices = vec![from, min, max, to - 1];
        indices.sort_unstable();
        indices.dedup();
        out.extend(indices.into_iter().map(|i| rows[i].clone()));
    }
    out
}

/// 重建与 `[from, to]` 相交的读数证据段。前驱 / 后继只提供边界上下文，不作为区间内读数返回。
/// 原始行全部参与分段后才抽样，避免先 LIMIT 把重置证据或极值丢掉。
pub fn rebuild_5h(
    platform: Platform,
    before: Option<QuotaReading>,
    readings: Vec<QuotaReading>,
    after: Option<QuotaReading>,
    from: i64,
    to: i64,
) -> QuotaTimeline {
    let raw_readings = readings.len();
    let mut chain: Vec<ContextReading> = before
        .into_iter()
        .map(|row| ContextReading { row, scope: -1 })
        .chain(
            readings
                .into_iter()
                .map(|row| ContextReading { row, scope: 0 }),
        )
        .chain(
            after
                .into_iter()
                .map(|row| ContextReading { row, scope: 1 }),
        )
        .collect();
    chain.sort_by(|a, b| a.row.t.cmp(&b.row.t));

    let mut segments: Vec<WorkingSegment> = Vec::new();
    let mut discontinuities = Vec::new();
    let mut previous: Option<(QuotaReading, Option<i64>)> = None;
    for item in chain {
        let reported = announced_end(platform, &item.row);
        let key = match (segments.last().map(|segment| segment.key), reported) {
            (Some(current), Some(_)) if same_window_key(current, reported) => current,
            _ => reported,
        };
        let should_break = previous.as_ref().and_then(|(prev, prev_key)| {
            break_between(prev, *prev_key, &item.row, key).map(|(reason, at)| {
                discontinuities.push(QuotaDiscontinuity {
                    from_t: prev.t,
                    to_t: item.row.t,
                    at,
                    reason,
                });
            })
        });
        if should_break.is_some() || segments.last().is_some_and(|segment| segment.key != key) {
            segments.push(WorkingSegment {
                key,
                rows: Vec::new(),
            });
        }
        if segments.is_empty() {
            segments.push(WorkingSegment {
                key,
                rows: Vec::new(),
            });
        }
        segments
            .last_mut()
            .expect("segment just created")
            .rows
            .push(item.clone());
        previous = Some((item.row, key));
    }

    let mut windows: Vec<QuotaWindowSegment> = segments
        .into_iter()
        .filter_map(|segment| {
            let inside: Vec<QuotaReading> = segment
                .rows
                .iter()
                .filter(|item| item.scope == 0)
                .map(|item| item.row.clone())
                .collect();
            let first = inside.first()?.t;
            let last = inside.last()?.t;
            Some(QuotaWindowSegment {
                id: 0,
                start_at: segment.key.map(|end| end - FIVE_HOURS_SECS),
                end_at: segment.key,
                start_evidence: segment
                    .key
                    .map_or(WindowEvidence::Unknown, |_| WindowEvidence::TailMinus5h),
                end_evidence: segment
                    .key
                    .map_or(WindowEvidence::Unknown, |_| WindowEvidence::AnnouncedTail),
                first_reading_at: first,
                last_reading_at: last,
                continues_left: segment.rows.iter().any(|item| item.scope < 0),
                continues_right: segment.rows.iter().any(|item| item.scope > 0),
                readings: inside,
            })
        })
        .collect();
    discontinuities.retain(|d| d.to_t >= from && d.from_t <= to);

    for (id, window) in windows.iter_mut().enumerate() {
        window.id = id;
    }
    if raw_readings > MAX_DISPLAY_READINGS {
        for window in &mut windows {
            let limit = ((window.readings.len() * MAX_DISPLAY_READINGS) / raw_readings).max(4);
            window.readings = downsample_rows(&window.readings, limit);
        }
    }
    let returned_readings = windows.iter().map(|window| window.readings.len()).sum();
    QuotaTimeline {
        windows,
        discontinuities,
        raw_readings,
        returned_readings,
        downsampled: returned_readings < raw_readings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::model::SnapshotSource;

    fn reading(t: i64, used: f64, reset: Option<i64>) -> QuotaReading {
        QuotaReading {
            t,
            kind: "5h".into(),
            used_percent: used,
            resets_at: reset,
            plan_type: "pro".into(),
            source: SnapshotSource::Api,
        }
    }

    #[test]
    fn fixed_tail_builds_window_but_missing_tail_stays_unknown() {
        let known = rebuild_5h(
            Platform::Codex,
            None,
            vec![reading(1_000, 2.0, Some(18_000))],
            None,
            0,
            2_000,
        );
        assert_eq!(known.windows[0].start_at, Some(0));
        assert_eq!(known.windows[0].end_at, Some(18_000));
        assert_eq!(known.windows[0].start_evidence, WindowEvidence::TailMinus5h);

        let unknown = rebuild_5h(
            Platform::Claude,
            None,
            vec![reading(1_000, 40.0, None)],
            None,
            0,
            2_000,
        );
        assert_eq!(
            (unknown.windows[0].start_at, unknown.windows[0].end_at),
            (None, None)
        );
        assert_eq!(unknown.windows[0].start_evidence, WindowEvidence::Unknown);
    }

    #[test]
    fn codex_rolling_empty_tail_does_not_build_a_window() {
        let rolling = rebuild_5h(
            Platform::Codex,
            None,
            vec![reading(1_000, 0.0, Some(19_000))],
            None,
            0,
            2_000,
        );
        assert_eq!(rolling.windows[0].end_at, None);
        let fixed = rebuild_5h(
            Platform::Codex,
            None,
            vec![reading(1_000, 0.0, Some(17_000))],
            None,
            0,
            2_000,
        );
        assert_eq!(fixed.windows[0].end_at, Some(17_000));
    }

    #[test]
    fn one_second_tail_jitter_does_not_split_the_window() {
        let rows = vec![
            reading(1_000, 8.0, Some(20_000)),
            reading(2_000, 9.0, Some(20_001)),
            reading(3_000, 10.0, Some(19_999)),
            reading(4_000, 11.0, Some(20_000)),
        ];
        let out = rebuild_5h(Platform::Claude, None, rows, None, 0, 5_000);
        assert_eq!(out.windows.len(), 1);
        assert_eq!(out.windows[0].readings.len(), 4);
        assert!(out.discontinuities.is_empty());
    }

    #[test]
    fn announced_reset_is_found_even_when_usage_rises() {
        let rows = vec![
            reading(1_000, 80.0, Some(5_000)),
            reading(2_000, 81.0, Some(5_000)),
            reading(6_000, 90.0, Some(23_000)),
        ];
        let out = rebuild_5h(Platform::Codex, None, rows, None, 0, 7_000);
        assert_eq!(out.windows.len(), 2);
        assert_eq!(out.discontinuities.len(), 1);
        assert_eq!(out.discontinuities[0].reason, DiscontinuityReason::Reset);
        assert_eq!(out.discontinuities[0].at, Some(5_000));
    }

    #[test]
    fn plan_change_and_tail_regression_cut_evidence_segments() {
        let mut changed_plan = reading(2_000, 10.0, Some(20_000));
        changed_plan.plan_type = "plus".into();
        let out = rebuild_5h(
            Platform::Claude,
            None,
            vec![reading(1_000, 5.0, Some(20_000)), changed_plan],
            None,
            0,
            3_000,
        );
        assert_eq!(
            out.discontinuities[0].reason,
            DiscontinuityReason::PlanChanged
        );
        assert_eq!(out.windows.len(), 2);

        let out = rebuild_5h(
            Platform::Codex,
            None,
            vec![
                reading(1_000, 5.0, Some(20_000)),
                reading(2_000, 5.0, Some(10_000)),
            ],
            None,
            0,
            3_000,
        );
        assert_eq!(
            out.discontinuities[0].reason,
            DiscontinuityReason::TailRegressed
        );
    }

    #[test]
    fn predecessor_and_successor_only_mark_continuation() {
        let out = rebuild_5h(
            Platform::Codex,
            Some(reading(500, 1.0, Some(20_000))),
            vec![reading(1_000, 2.0, Some(20_000))],
            Some(reading(1_500, 3.0, Some(20_000))),
            1_000,
            1_000,
        );
        assert_eq!(out.windows.len(), 1);
        assert_eq!(out.windows[0].readings.len(), 1);
        assert!(out.windows[0].continues_left);
        assert!(out.windows[0].continues_right);
    }

    #[test]
    fn downsampling_keeps_first_last_and_extrema() {
        let rows: Vec<QuotaReading> = (0..100)
            .map(|i| {
                reading(
                    i,
                    if i == 37 {
                        99.0
                    } else if i == 72 {
                        1.0
                    } else {
                        20.0
                    },
                    Some(20_000),
                )
            })
            .collect();
        let out = downsample_rows(&rows, 8);
        assert!(out.len() <= 8);
        assert_eq!(out.first().unwrap().t, 0);
        assert_eq!(out.last().unwrap().t, 99);
        assert!(out.iter().any(|row| row.t == 37));
        assert!(out.iter().any(|row| row.t == 72));
    }
}
