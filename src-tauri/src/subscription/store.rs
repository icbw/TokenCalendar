//! 订阅快照库：`<数据根>/subscriptions.db`。
//!
//! 设计红线（/）：**凭据永不落库**——表里只有「绑定了哪些平台」
//! 的开关事实与归一化快照;token 每次轮询现读凭据原文件,内存短存。
//! 独立于 collector.db（在线账户额度 ≠ 本机用量聚合）。

use std::path::Path;

use rusqlite::Connection;

use super::model::{FetchStatus, Platform, QuotaWindow, SubscriptionSnapshot};

pub struct SubStore {
    conn: Connection,
}

impl SubStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS binding (
                 platform TEXT PRIMARY KEY,
                 enabled  INTEGER NOT NULL DEFAULT 1,
                 bound_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS snapshot (
                 platform   TEXT PRIMARY KEY,
                 plan_type  TEXT NOT NULL,
                 windows    TEXT NOT NULL,
                 fetched_at INTEGER,
                 status     TEXT NOT NULL
             );",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { conn })
    }

    // ---------- binding（只记「绑没绑」,凭据本体永远在 agent 自己的文件里） ----------

    /// 单平台绑定查询（bound_platforms 空集时的 O（1) 替代;测试与诊断用）。
    #[allow(dead_code)]
    pub fn is_bound(&self, platform: Platform) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM binding WHERE platform = ?1 AND enabled = 1",
                [platform.as_str()],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn bound_platforms(&self) -> Vec<Platform> {
        let mut out = vec![];
        let Ok(mut stmt) = self.conn.prepare("SELECT platform FROM binding WHERE enabled = 1") else {
            return out;
        };
        let rows = stmt.query_map([], |r| r.get::<_, String>(0));
        if let Ok(rows) = rows {
            for s in rows.flatten() {
                if let Some(p) = Platform::from_str(&s) {
                    out.push(p);
                }
            }
        }
        out
    }

    pub fn bind(&self, platform: Platform, now: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO binding (platform, enabled, bound_at) VALUES (?1, 1, ?2)
                 ON CONFLICT(platform) DO UPDATE SET enabled = 1, bound_at = ?2",
                [platform.as_str(), &now.to_string()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn unbind(&self, platform: Platform) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM binding WHERE platform = ?1", [platform.as_str()])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ---------- snapshot（每平台最新一条,供跨重启展示） ----------

    pub fn save_snapshot(&self, snap: &SubscriptionSnapshot) -> Result<(), String> {
        // 失败轮（无窗口 + 无 fetched_at,且不是解绑的 idle）**只推进 status**：
        // 上一轮成功的窗口/时间戳必须留着——UI 的「showing last known data」
        // 全指望这条（旧版会把 windows 覆盖成空,读数直接变 —）。
        let is_failure = snap.windows.is_empty()
            && snap.fetched_at.is_none()
            && snap.status != FetchStatus::Idle;
        if is_failure {
            let updated = self
                .conn
                .execute(
                    "UPDATE snapshot SET status = ?2 WHERE platform = ?1",
                    rusqlite::params![snap.platform.as_str(), snap.status.as_str()],
                )
                .map_err(|e| e.to_string())?;
            if updated == 0 {
                // 从未成功过（库里没行）→ 落一条占位,前端形状才稳定
                self.insert_snapshot(snap)?;
            }
            return Ok(());
        }
        self.insert_snapshot(snap)
    }

    fn insert_snapshot(&self, snap: &SubscriptionSnapshot) -> Result<(), String> {
        let windows = serde_json::to_string(&snap.windows).map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO snapshot (platform, plan_type, windows, fetched_at, status)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(platform) DO UPDATE SET
                   plan_type = ?2, windows = ?3, fetched_at = ?4, status = ?5",
                rusqlite::params![
                    snap.platform.as_str(),
                    snap.plan_type,
                    windows,
                    snap.fetched_at,
                    snap.status.as_str(),
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_snapshot(&self, platform: Platform) -> Option<SubscriptionSnapshot> {
        let (plan, windows, fetched_at, status): (String, String, Option<i64>, String) = self
            .conn
            .query_row(
                "SELECT plan_type, windows, fetched_at, status FROM snapshot WHERE platform = ?1",
                [platform.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()?;
        Some(SubscriptionSnapshot {
            platform,
            plan_type: plan,
            windows: serde_json::from_str(&windows).ok()?,
            fetched_at,
            status: FetchStatus::from_str(&status).unwrap_or(FetchStatus::Idle),
        })
    }

    /// 命令面出口:两平台快照 + 绑定态合并（未绑定平台给 idle 占位,前端形状稳定）。
    pub fn snapshots_for_frontend(&self) -> Vec<SubscriptionSnapshot> {
        let mut out = vec![];
        for platform in [Platform::Codex, Platform::Claude] {
            let snap = self.load_snapshot(platform).unwrap_or(SubscriptionSnapshot {
                platform,
                plan_type: "unknown".into(),
                windows: vec![QuotaWindow {
                    kind: "5h".into(),
                    used_percent: 0.0,
                    resets_at: None,
                }],
                fetched_at: None,
                status: FetchStatus::Idle,
            });
            out.push(snap);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> SubStore {
        SubStore::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn binding_roundtrip() {
        let s = mem_store();
        assert!(!s.is_bound(Platform::Codex));
        s.bind(Platform::Codex, 1000).unwrap();
        assert!(s.is_bound(Platform::Codex));
        assert!(!s.is_bound(Platform::Claude));
        assert_eq!(s.bound_platforms(), vec![Platform::Codex]);
        s.unbind(Platform::Codex).unwrap();
        assert!(s.bound_platforms().is_empty());
    }

    #[test]
    fn snapshot_roundtrip() {
        let s = mem_store();
        let snap = SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "pro".into(),
            windows: vec![
                QuotaWindow { kind: "5h".into(), used_percent: 12.0, resets_at: Some(1738300000) },
                QuotaWindow { kind: "7d".into(), used_percent: 40.5, resets_at: None },
            ],
            fetched_at: Some(1738299000),
            status: FetchStatus::Ok,
        };
        s.save_snapshot(&snap).unwrap();
        let loaded = s.load_snapshot(Platform::Claude).unwrap();
        assert_eq!(loaded.plan_type, "pro");
        assert_eq!(loaded.windows.len(), 2);
        assert_eq!(loaded.windows[1].used_percent, 40.5);
        assert_eq!(loaded.status, FetchStatus::Ok);
    }

    #[test]
    fn idle_placeholder_for_unbound() {
        let s = mem_store();
        let all = s.snapshots_for_frontend();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|x| x.status == FetchStatus::Idle));
    }

    fn ok_claude_snapshot() -> SubscriptionSnapshot {
        SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "max".into(),
            windows: vec![QuotaWindow { kind: "5h".into(), used_percent: 10.0, resets_at: Some(1) }],
            fetched_at: Some(1738299000),
            status: FetchStatus::Ok,
        }
    }

    #[test]
    fn failed_round_keeps_last_known_data() {
        let s = mem_store();
        s.save_snapshot(&ok_claude_snapshot()).unwrap();
        // 失败轮（无窗口/无 fetched_at）只推 status——旧数据显示面不回退
        for status in [FetchStatus::RateLimited, FetchStatus::NetworkFailed] {
            s.save_snapshot(&SubscriptionSnapshot {
                platform: Platform::Claude,
                plan_type: "unknown".into(),
                windows: vec![],
                fetched_at: None,
                status,
            })
            .unwrap();
            let loaded = s.load_snapshot(Platform::Claude).unwrap();
            assert_eq!(loaded.status, status);
            assert_eq!(loaded.plan_type, "max");
            assert_eq!(loaded.windows.len(), 1);
            assert_eq!(loaded.fetched_at, Some(1738299000));
        }
    }

    #[test]
    fn failure_before_first_success_still_lands() {
        let s = mem_store();
        s.save_snapshot(&SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "unknown".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::AuthFailed,
        })
        .unwrap();
        assert_eq!(s.load_snapshot(Platform::Claude).unwrap().status, FetchStatus::AuthFailed);
    }

    #[test]
    fn unbind_idle_clears_data() {
        let s = mem_store();
        s.save_snapshot(&ok_claude_snapshot()).unwrap();
        s.save_snapshot(&SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "unknown".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::Idle,
        })
        .unwrap();
        let loaded = s.load_snapshot(Platform::Claude).unwrap();
        assert_eq!(loaded.status, FetchStatus::Idle);
        assert!(loaded.windows.is_empty());
        assert_eq!(loaded.fetched_at, None);
    }
}
