//! 订阅快照库：`<数据根>/subscriptions.db`。
//!
//! 设计红线（/）：**凭据永不落库**——表里只有「绑定了哪些平台」
//! 的开关事实与归一化快照;token 每次轮询现读凭据原文件,内存短存。
//! 独立于 collector.db（在线账户额度 ≠ 本机用量聚合）。

use std::path::Path;

use rusqlite::Connection;

use super::model::{FetchStatus, Platform, QuotaWindow, SnapshotSource, SubscriptionSnapshot};

pub struct SubStore {
    conn: Connection,
}

impl SubStore {
    /// 只读取 `conn` 供同模块树的真库 smoke 复核（`#[cfg（test)]`,不进发布构建）。
    #[cfg(test)]
    pub(super) fn conn_for_smoke(&self) -> &Connection {
        &self.conn
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        // 建表之前先探：price_model 缺、而 usage_pair 已在 ⇒ 这是**存量库的一次结构
        // 升级**,要在动它之前落一份备份（红线：迁移前备份,见 AGENTS.md 第 3 节）。
        // 全新库（两张都没有）不算升级,不必备份空文件。
        let price_table_added =
            !Self::has_table(&conn, "price_model") && Self::has_table(&conn, "usage_pair");
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
                 status     TEXT NOT NULL,
                 -- 读数来源（api / desktop,见 model::SnapshotSource）：标定只认两端
                 -- 都是 api 的读数对,桌面端那一路由 bootstrap.rs 按样本时刻切。
                 source     TEXT NOT NULL DEFAULT 'api'
             );
             -- 标定样本（2026-09-18）：相邻两次成功读数之间的「本地代价 vs 实测涨幅」。
             -- 纯派生数据,只为算取数时机的换算系数;超出保留条数即按时间裁剪（见 prune_pairs）。
             -- breakdown 存分模型 token 明细,留作将来回过头拟合逐模型权重。
             CREATE TABLE IF NOT EXISTS usage_pair (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 platform     TEXT NOT NULL,
                 t0           INTEGER NOT NULL,
                 t1           INTEGER NOT NULL,
                 used5_0      REAL NOT NULL,
                 used5_1      REAL NOT NULL,
                 used7_0      REAL NOT NULL,
                 used7_1      REAL NOT NULL,
                 cost         REAL NOT NULL,
                 unknown_cost REAL NOT NULL,
                 breakdown    TEXT NOT NULL,
                 -- 样本来源：'online'  = 两次 API 读数之间的观测;
                 --           'desktop' = 由 Claude 桌面端采样序列按**样本时刻**切出来的
                 --                       观测（bootstrap.rs）;
                 --           'rollout' = 由 Codex 会话 rollout 里的 rate_limits 读数切出来的
                 --                       观测（codex_rollout.rs;读数与代价同一行）。
                 -- 增量续算的水位线各取自己那一路的 MAX(t1),几路互不干扰。
                 src          TEXT NOT NULL DEFAULT 'online',
                 -- 世代标记（2026-09-18）：cost 是派生值,这两列记住它是按哪把尺子量的。
                 -- weight_ver = 算 cost 时的权重表版本（cost::WEIGHT_VERSION）;价格一变
                 --   就升版,存量行按 breakdown 里的原始 token 就地重算,0 = 重算不了、存疑;
                 -- plan_type  = 该区间的套餐;套餐变了 scale 就变了,旧样本留着当档案
                 --   但不参与当前拟合（'' = 存量未知）。
                 weight_ver   INTEGER NOT NULL DEFAULT 1,
                 plan_type    TEXT NOT NULL DEFAULT '',
                 -- 5h 窗口在两端各自申报的重置时刻（2026-09-19;NULL = 该来源不提供,
                 -- 或是升级之前的存量行）。跨重置的区间两端读数不在同一个窗口里,Δ 不是
                 -- 这段消耗涨出来的 ⇒ 不参与拟合（判据见 calib::Pair::window_reset）。
                 -- **存原值而不是存「重置过没有」**：判据将来要改,这两个数还能重新判一遍。
                 resets5_0    INTEGER,
                 resets5_1    INTEGER,
                 -- 这段时间从 5h 窗尾**老掉**的代价（2026-09-19;发生在 (t0−5h, t1−5h]
                 -- 的那些调用）。5h 是滚动窗口 ⇒ 计数器的变化是「新花的 − 老掉的」,
                 -- 而不是 cost 本身。**存原始观测不存差值**：判据要改时两个数都还在。
                 -- 0 = 该来源给不出（在线路没有历史窗口）或是升级前的存量行 ⇒ 不做修正。
                 aged_cost    REAL NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS usage_pair_by_platform ON usage_pair (platform, t1);
             -- **本地读数收割表**（2026-09-18 用户定案;2026-09-19 起两个平台都用）：
             -- 平台自己留的本地读数历史都是会滚掉的（Claude 桌面端的
             -- plan-usage-history.json 只保约 14 天;Codex 的 rollout 会话文件会被归档 /
             -- 被用户清掉 / CODEX_HOME 会改指向）,这里把见过的读数**永久留下**
             -- ——样本密度只增不减,标定的可用观测随时间累积。
             -- 一行 5 个数,主键 (platform, t) ⇒ 重复收割天然幂等。
             --
             -- 表名沿用 desktop_sample 不改：改名要重建表 + 搬数据,而这张表装的正是
             -- 「丢了就再也回不来」的历史,为了名字好听去动它不划算（红线见 AGENTS.md §3）。
             CREATE TABLE IF NOT EXISTS desktop_sample (
                 platform TEXT NOT NULL,
                 t        INTEGER NOT NULL,
                 used5    REAL NOT NULL,
                 used7    REAL NOT NULL,
                 -- 该读数当时的套餐（Codex 的 rate_limits 每条自带;Claude 桌面端不提供 ⇒ ''）。
                 -- 存着才谈得上「原始输入留档」：套餐一换 scale 就换了,回头重建样本要认得出来。
                 plan_type TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (platform, t)
             );
             -- 小杂项键值（水位线一类的标量;**不存任何凭据**）。
             CREATE TABLE IF NOT EXISTS meta (
                 k TEXT PRIMARY KEY,
                 v TEXT NOT NULL
             );
             -- 官方价目快照对照表（PHASE15 S1）：**一个模型的一段生效期 = 一行**。
             -- 绝大多数模型终其生命周期只有一行;只有真的被官方降价过的模型才会有第二行
             -- ——版本化的粒度跟着真正会变的东西走,某模型降价不该把别的模型一起换代。
             --
             -- 四项 usd_* 是官方公布的**绝对价目（USD / Mtok）**,按模型名精确对准、
             -- 不做归一化（「永远不要只存派生值」:相对权重是 usd_x 除以基准模型 usd_input）。
             -- 出厂种子由 scripts/price-seed-gen.mjs 从 models.dev 第一方目录产出、编译期
             -- 嵌入,启动时按主键幂等 upsert;**不做运行时取价**（重算必须可复现）。
             --
             -- 只 upsert、不删行：更早版本留下的历史生效段必须留着,否则用户跳过若干
             -- 版本再更新时,中间那些价格段就断了（设计 4.1）。
             CREATE TABLE IF NOT EXISTS price_model (
                 platform        TEXT    NOT NULL,
                 -- 小写子串模式（opus / gpt-5-6-sol 之类）,匹配时**最长键优先**
                 match_key       TEXT    NOT NULL,
                 -- unix 秒;该价目开始生效（模型首次出现时 = 其上线时刻）
                 effective_from  INTEGER NOT NULL,
                 display_name    TEXT    NOT NULL,
                 usd_input       REAL    NOT NULL,
                 usd_output      REAL    NOT NULL,
                 usd_cache_read  REAL    NOT NULL,
                 usd_cache_write REAL    NOT NULL,
                 -- **出处 + 上游核对信息**:这张表是可审计的官方价格快照,任何时候要能
                 -- 回答「这个数从哪儿来」。「什么时候核对的」由 price_seed.json 的 git
                 -- 历史回答——定时重跑生成器 + 只在数据真动时提交,其 commit history
                 -- 恰好就是价格时间线（设计 7.4 末段）。
                 source_note     TEXT    NOT NULL DEFAULT '',
                 PRIMARY KEY (platform, match_key, effective_from)
             );",
        )
        .map_err(|e| e.to_string())?;
        Self::upgrade_schema(&conn, path, price_table_added)?;
        let store = Self { conn };
        // 出厂种子按主键幂等 upsert（每次开库都跑一遍：新装填满、升级补齐、
        // 已是最新则原样写回）。必须在 recompute_stale_costs 之前——重算要按库里的价目。
        if let Err(e) = store.upsert_price_seed(super::price::factory_seed()) {
            crate::dev_log!("[subscription] price seed upsert failed: {e}");
        }
        Ok(store)
    }

    /// 该表是否存在（`sqlite_master`;建表之前探,用来区分"存量库升级"与"新装"）。
    fn has_table(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
    }

    // ---------- 就地列升级（**只 ALTER + 回填,永不清表**,红线见 AGENTS.md） ----------

    /// 该表是否已有此列（`PRAGMA table_info`;表不存在按「没有」处理）。
    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
            return false;
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
            return false;
        };
        let names: Vec<String> = rows.flatten().collect();
        names.iter().any(|name| name == column)
    }

    /// 补齐新列。存量库走 ALTER + 默认值,
    /// **不重建表、不清空样本**——subscriptions.db 里的标定样本与桌面端采样是跨版本
    /// 累积的历史（桌面端自己只保 14 天,丢了就再也回不来）。
    fn upgrade_schema(
        conn: &Connection,
        path: &Path,
        price_table_added: bool,
    ) -> Result<(), String> {
        let need_pair_src = !Self::has_column(conn, "usage_pair", "src");
        let need_snapshot_source = !Self::has_column(conn, "snapshot", "source");
        let need_pair_epoch = !Self::has_column(conn, "usage_pair", "weight_ver");
        let need_sample_plan = !Self::has_column(conn, "desktop_sample", "plan_type");
        let need_pair_resets = !Self::has_column(conn, "usage_pair", "resets5_0");
        let need_pair_aged = !Self::has_column(conn, "usage_pair", "aged_cost");
        if !need_pair_src
            && !need_snapshot_source
            && !need_pair_epoch
            && !need_sample_plan
            && !need_pair_resets
            && !need_pair_aged
            && !price_table_added
        {
            return Ok(());
        }
        Self::backup_before_upgrade(conn, path);
        if need_pair_src {
            conn.execute_batch("ALTER TABLE usage_pair ADD COLUMN src TEXT NOT NULL DEFAULT 'online';")
                .map_err(|e| e.to_string())?;
            // 冷启动那批样本本来就是按桌面端样本时刻切的 ⇒ 归入 desktop,
            // 增量续算的水位线直接从它们接上,不会重复建同一段区间的样本。
            conn.execute(
                "UPDATE usage_pair SET src = 'desktop' WHERE breakdown LIKE '%\"src\":\"bootstrap\"%'",
                [],
            )
            .map_err(|e| e.to_string())?;
        }
        if need_snapshot_source {
            conn.execute_batch("ALTER TABLE snapshot ADD COLUMN source TEXT NOT NULL DEFAULT 'api';")
                .map_err(|e| e.to_string())?;
        }
        if need_pair_epoch {
            // 存量行的 cost 本来就是按当前（首个）权重表算的 ⇒ 默认 1 是事实,不是假设。
            conn.execute_batch(
                "ALTER TABLE usage_pair ADD COLUMN weight_ver INTEGER NOT NULL DEFAULT 1;
                 ALTER TABLE usage_pair ADD COLUMN plan_type TEXT NOT NULL DEFAULT '';",
            )
            .map_err(|e| e.to_string())?;
            // 套餐回填：只能取「当前快照的套餐」这一个已知量。区间当时的套餐无从追溯,
            // 若期间升过档会标错——但这与升级前「一律无标记」相比不会更糟,且新样本从
            // 此刻起都是准的。快照本身也不知道（unknown）时留空,由 pairs_for_fit 放行。
            conn.execute(
                "UPDATE usage_pair SET plan_type = COALESCE(
                     (SELECT s.plan_type FROM snapshot s
                       WHERE s.platform = usage_pair.platform AND s.plan_type <> 'unknown'), '')
                  WHERE plan_type = ''",
                [],
            )
            .map_err(|e| e.to_string())?;
            Self::normalize_breakdowns(conn)?;
        }
        if need_sample_plan {
            // 存量行全是 Claude 桌面端采样,那个源本来就不提供套餐 ⇒ 默认 '' 是事实,
            // 不是猜测。**一行都不动、一行都不删**。
            conn.execute_batch(
                "ALTER TABLE desktop_sample ADD COLUMN plan_type TEXT NOT NULL DEFAULT '';",
            )
            .map_err(|e| e.to_string())?;
        }
        if need_pair_resets {
            // 存量行留 NULL = **未知**,不是「没重置过」：那时根本没记这两个数。
            // calib 侧对 NULL 的处理就是退回旧判据（读数变小了才算重置）,与升级前
            // 的行为逐位相同 ⇒ **一行都不动、一行都不删**,老样本照常参与拟合。
            conn.execute_batch(
                "ALTER TABLE usage_pair ADD COLUMN resets5_0 INTEGER;
                 ALTER TABLE usage_pair ADD COLUMN resets5_1 INTEGER;",
            )
            .map_err(|e| e.to_string())?;
        }
        if need_pair_aged {
            // 存量行留 0 = **不做正**,不是「老掉的正好是 0」：那时根本没记这个数。
            // `effective_cost` 于是退化成 `cost`,与升级前的行为逐位相同
            // ⇒ **一行都不动、一行都不删**;判据升版后两条回溯路会把源还在的那一段
            // 重建出来,真实的老化量随之填上（见 calib:ADMISSION_RULE_VERSION）。
            conn.execute_batch(
                "ALTER TABLE usage_pair ADD COLUMN aged_cost REAL NOT NULL DEFAULT 0;",
            )
            .map_err(|e| e.to_string())?;
        }
        if price_table_added {
            // price_model 由上面的 CREATE TABLE IF NOT EXISTS 建好,这里只记一笔。
            // **既有行一个都没动**：新表是加的;usage_pair 的 cost 要等启动时的
            // recompute_stale_costs 按各模型在 t1 时刻生效的价目重算。
            crate::dev_log!("[subscription] price_model table added (existing rows untouched)");
        }
        crate::dev_log!(
            "[subscription] store schema upgraded in place (pair.src / snapshot.source / pair epoch / price_model / sample.plan_type / pair.resets / pair.aged_cost)"
        );
        Ok(())
    }

    // ---------- price_model（官方价目快照;**只 upsert,永不删行**,见建表注释） ----------

    /// 把出厂种子写进库（幂等,按 `（platform, match_key, effective_from)`）。
    ///
    /// 冲突时**覆盖价目与出处**：同一段生效期的数值若有正（比如发现 `-pro`
    /// 一族官方根本没有缓存读单价）,库里要跟着更正。而**不在种子里的行原样留着**
    /// ——那是更早版本下发的历史生效段,删了就断了价格时间线。
    pub fn upsert_price_seed(&self, rows: &[super::price::PriceRow]) -> Result<usize, String> {
        if rows.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO price_model (platform, match_key, effective_from, display_name,
                         usd_input, usd_output, usd_cache_read, usd_cache_write, source_note)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(platform, match_key, effective_from) DO UPDATE SET
                       display_name = ?4, usd_input = ?5, usd_output = ?6,
                       usd_cache_read = ?7, usd_cache_write = ?8, source_note = ?9",
                )
                .map_err(|e| e.to_string())?;
            for r in rows {
                stmt.execute(rusqlite::params![
                    r.platform,
                    r.match_key,
                    r.effective_from,
                    r.display_name,
                    r.usd_input,
                    r.usd_output,
                    r.usd_cache_read,
                    r.usd_cache_write,
                    r.source_note,
                ])
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(rows.len())
    }

    /// 全部价目行（启动装载进程内索引 / S2 查询面;**唯一查询源**）。
    pub fn price_rows(&self) -> Vec<super::price::PriceRow> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT platform, match_key, effective_from, display_name,
                    usd_input, usd_output, usd_cache_read, usd_cache_write, source_note
               FROM price_model ORDER BY platform, match_key, effective_from",
        ) else {
            return vec![];
        };
        let rows = stmt.query_map([], |r| {
            Ok(super::price::PriceRow {
                platform: r.get(0)?,
                match_key: r.get(1)?,
                effective_from: r.get(2)?,
                display_name: r.get(3)?,
                usd_input: r.get(4)?,
                usd_output: r.get(5)?,
                usd_cache_read: r.get(6)?,
                usd_cache_write: r.get(7)?,
                source_note: r.get(8)?,
            })
        });
        rows.map(|rs| rs.flatten().collect()).unwrap_or_default()
    }

    /// 把 `breakdown` 统一成规范的裸 map（历史上 bootstrap 写过 `{"src":…,"models":{…}}`
    /// 的包装层,`src` 现已是独立列）。形状不统一会让将来的"按新权重重算"只能恢复一半行
    /// ——而重算是"派生值可恢复"的唯一依靠,所以趁数据还少一次抹平。
    /// 逐行按 `id` UPDATE,已是规范形状的跳过 ⇒ 幂等,不产生新行。
    fn normalize_breakdowns(conn: &Connection) -> Result<(), String> {
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare("SELECT id, breakdown FROM usage_pair WHERE breakdown LIKE '%\"models\"%'")
                .map_err(|e| e.to_string())?;
            let it = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| e.to_string())?;
            it.flatten().collect()
        };
        let mut n = 0;
        for (id, body) in rows {
            let models = super::cost::parse_breakdown(&body);
            let Ok(canonical) = serde_json::to_string(&models) else { continue };
            if canonical == body {
                continue;
            }
            conn.execute(
                "UPDATE usage_pair SET breakdown = ?2 WHERE id = ?1",
                rusqlite::params![id, canonical],
            )
            .map_err(|e| e.to_string())?;
            n += 1;
        }
        if n > 0 {
            crate::dev_log!("[subscription] normalized {n} pair breakdown(s) to the canonical shape");
        }
        Ok(())
    }

    /// 升级前备份（`VACUUM INTO`,含 WAL 里未落盘的内容;与 collector.db 同口径）。
    /// 失败只记日志不拦升级——这里全是派生数据,备份是保险不是前置条件。
    fn backup_before_upgrade(conn: &Connection, path: &Path) {
        let Some(dir) = path.parent().map(|d| d.join("backups")) else { return };
        if dir.as_os_str().is_empty() || std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = dir.join(format!("subscriptions-pre-{stamp}.db"));
        if dest.exists() {
            return;
        }
        let sql = format!("VACUUM INTO '{}'", dest.display().to_string().replace("'", "''"));
        match conn.execute(&sql, []) {
            Ok(_) => crate::dev_log!("[subscription] pre-upgrade backup → {}", dest.display()),
            Err(e) => crate::dev_log!("[subscription] pre-upgrade backup failed: {e}"),
        }
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
        self.note_plan(snap);
        super::cost::set_plan(snap.platform, &snap.plan_type);
        self.conn
            .execute(
                "INSERT INTO snapshot (platform, plan_type, windows, fetched_at, status, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(platform) DO UPDATE SET
                   plan_type = ?2, windows = ?3, fetched_at = ?4, status = ?5, source = ?6",
                rusqlite::params![
                    snap.platform.as_str(),
                    snap.plan_type,
                    windows,
                    snap.fetched_at,
                    snap.status.as_str(),
                    snap.source.as_str(),
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// meta 键：**当前套餐是从哪一刻起被观测到的**（unix 秒）。
    fn plan_since_key(platform: Platform) -> String {
        format!("plan_since_{}", platform.as_str())
    }

    /// 当前套餐的起始观测时刻。`None` = 没有已知边界（存量库升上来的常态）——
    /// 调用方此时按「一直是这个套餐」处理,与加这条之前的行为一致。
    pub fn plan_since(&self, platform: Platform) -> Option<i64> {
        self.meta_i64(&Self::plan_since_key(platform))
    }

    /// 套餐名与上一条快照不同（含**从无到有**）就把边界推到取数时刻。
    ///
    /// 这条边界是给 `bootstrap` 用的：Claude 桌面端的采样历史不带套餐,回补区间只能
    /// 拿「当前快照的套餐」硬套,换过档就会把旧套餐的历史标成新套餐——而 `pairs_for_fit`
    /// 按套餐筛之后,**标错比不标更糟**（会把新套餐的估计往旧套餐拖）。有了边界,
    /// 边界之前的区间标「未知」放行、之后的才标真套餐。
    ///
    /// 只认成功轮（`fetched_at` 有值）且套餐名真的说得出来——失败轮的 "unknown"
    /// 不是换档,不能拿它把边界推到现在。
    fn note_plan(&self, snap: &SubscriptionSnapshot) {
        let Some(now) = snap.fetched_at else { return };
        if matches!(snap.plan_type.as_str(), "" | "unknown") {
            return;
        }
        let prev: Option<String> = self
            .conn
            .query_row(
                "SELECT plan_type FROM snapshot WHERE platform = ?1",
                [snap.platform.as_str()],
                |r| r.get(0),
            )
            .ok();
        if prev.as_deref() != Some(snap.plan_type.as_str()) {
            let _ = self.set_meta_i64(&Self::plan_since_key(snap.platform), now);
        }
    }

    pub fn load_snapshot(&self, platform: Platform) -> Option<SubscriptionSnapshot> {
        let (plan, windows, fetched_at, status, source): (String, String, Option<i64>, String, String) =
            self.conn
                .query_row(
                    "SELECT plan_type, windows, fetched_at, status, source FROM snapshot WHERE platform = ?1",
                    [platform.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .ok()?;
        // 出厂预设按套餐折算,而 `cost:prior_scale` 的调用方（`calib:scale`）拿不到
        // store ⇒ 每次读写快照顺手把套餐名同步进 cost 的进程内单一源。
        super::cost::set_plan(platform, &plan);
        Some(SubscriptionSnapshot {
            platform,
            plan_type: plan,
            windows: serde_json::from_str(&windows).ok()?,
            fetched_at,
            status: FetchStatus::from_str(&status).unwrap_or(FetchStatus::Idle),
            source: SnapshotSource::from_str(&source).unwrap_or_default(),
        })
    }

    /// 命令面出口:两平台快照 + 绑定态合并（未绑定平台给 idle 占位,前端形状稳定）。
    // ---------- usage_pair（标定样本;纯派生数据,见 calib.rs） ----------

    /// 保留条数（够拟合又不无限长;超出按 t1 最旧裁剪）。桌面端那一路按 15 分钟
    /// 一条的节律建样本,500 条只够十来天——提到 5000（约两个月满密度）,
    /// 真被裁掉的也能从 desktop_sample 重建,原始观测在那张表里永久留着。
    const PAIR_KEEP: i64 = 5000;

    /// 落一条标定样本（src = 'online' / 'desktop',plan_type = 该区间的套餐;
    /// weight_ver 由当前权重表版本自动带上,语义见建表注释）。
    pub fn insert_pair(
        &self,
        platform: Platform,
        pair: &super::calib::Pair,
        used7: (f64, f64),
        breakdown: &str,
        src: &str,
        plan_type: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO usage_pair
                   (platform, t0, t1, used5_0, used5_1, used7_0, used7_1, cost, unknown_cost,
                    breakdown, src, weight_ver, plan_type, resets5_0, resets5_1, aged_cost)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    platform.as_str(),
                    pair.t0,
                    pair.t1,
                    pair.used5_0,
                    pair.used5_1,
                    used7.0,
                    used7.1,
                    pair.cost,
                    pair.unknown_cost,
                    breakdown,
                    src,
                    super::cost::WEIGHT_VERSION,
                    plan_type,
                    pair.resets5_0,
                    pair.resets5_1,
                    pair.aged_cost,
                ],
            )
            .map_err(|e| e.to_string())?;
        self.prune_pairs(platform)
    }

    /// 批量落样本（收割路径一次能建上千条：逐条提交 = 上千次 WAL 提交 + 把裁剪子查询
    /// 跑上千遍,首轮补齐近两周时会把后台线程卡住好一会儿）。一次事务写完,末尾裁一次。
    /// 返回落库条数。
    pub fn insert_pairs(
        &self,
        platform: Platform,
        items: &[(super::calib::Pair, (f64, f64), String)],
        src: &str,
        plan_type: &str,
    ) -> Result<usize, String> {
        if items.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let mut n = 0;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO usage_pair
                       (platform, t0, t1, used5_0, used5_1, used7_0, used7_1, cost, unknown_cost,
                        breakdown, src, weight_ver, plan_type, resets5_0, resets5_1, aged_cost)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                )
                .map_err(|e| e.to_string())?;
            for (pair, used7, breakdown) in items {
                if stmt
                    .execute(rusqlite::params![
                        platform.as_str(),
                        pair.t0,
                        pair.t1,
                        pair.used5_0,
                        pair.used5_1,
                        used7.0,
                        used7.1,
                        pair.cost,
                        pair.unknown_cost,
                        breakdown,
                        src,
                        super::cost::WEIGHT_VERSION,
                        plan_type,
                        pair.resets5_0,
                        pair.resets5_1,
                        pair.aged_cost,
                    ])
                    .is_ok()
                {
                    n += 1;
                }
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.prune_pairs(platform)?;
        Ok(n)
    }

    /// 删掉某一路落在 `[lo, hi]` 之内的样本,返回删除条数。
    ///
    /// **只给「准入判据升版后的就地重建」用**（见 `calib:ADMISSION_RULE_VERSION`）：
    /// 调用方先按新判据把这一段重建出来,拿重建结果的首尾当边界,再删旧行、插新行。
    /// 边界之外一条不碰 ⇒ 源已经消失、这次重建不到的区间照样留在库里。
    pub fn delete_pairs_in(&self, platform: Platform, src: &str, lo: i64, hi: i64) -> usize {
        self.conn
            .execute(
                "DELETE FROM usage_pair WHERE platform = ?1 AND src = ?2 AND t0 >= ?3 AND t1 <= ?4",
                rusqlite::params![platform.as_str(), src, lo, hi],
            )
            .unwrap_or(0)
    }

    /// 超出 `PAIR_KEEP` 时按 t1 最旧裁剪（原始观测仍在 desktop_sample 里,可重建）。
    fn prune_pairs(&self, platform: Platform) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM usage_pair WHERE platform = ?1 AND id NOT IN
                   (SELECT id FROM usage_pair WHERE platform = ?1 ORDER BY t1 DESC LIMIT ?2)",
                rusqlite::params![platform.as_str(), Self::PAIR_KEEP],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 读回可用于当前拟合的标定样本（新→旧无所谓,拟合与顺序无关）。
    ///
    /// 两道筛选：
    /// - **排除存疑行**（`weight_ver = 0`）——`breakdown` 缺失 / 解析不出模型,`cost`
    ///   恢复不了,不能拿去推断;行留在库里当档案;
    /// - `plan_type` 必须与当前套餐**同一倍率类**（或空 = 存量未知,放行免得升级当天
    ///   丢光历史标定）。当前套餐本身未知时不按套餐筛——否则新装 / 取数失败期间会
    ///   一条样本都取不到。
    ///
    /// **按倍率类而不是按套餐名筛**。`scale` 是
    /// 「这个套餐一个窗口有多大」,所以分代要分的是**窗口大小**,而套餐名只是它的代号
    /// ——出厂倍率表里同一类的两个档（Codex 的 `plus` 与 `edu`）我们自己就认为窗口一样
    /// 大,它们的样本本来就可比。本机：一台机器登录两个 Codex 账号（edu / plus）
    /// 轮换使用，按名字筛会让生效系数跟着「上次用的是哪个账号」在 **8.89 ↔ 7.63**
    /// 之间跳（16%），而直接测量说两个账号的满窗只差 2%（$13.70 / $13.96）——
    /// 那个跳幅是把数据劈成两半之后各自的抽样噪声,不是真实差异。合并之后是 8.13。
    ///
    /// 倍率类的判据见 [`super:cost:same_plan_class`]：**两边都得在倍率表里查得到**
    /// 才算同类,否则退回按名字精确比——表外的档拿到的是「回落基准档」那个占位值,
    /// 拿它当依据会把一堆互不相干的档归成一类。
    ///
    /// **不再按 `weight_ver` 等于当前订号筛**。原因：价格
    /// 世代按模型走之后,每个模型按自己的时间线取价,各订号下算出来的 `cost` 都是
    /// 「该区间按**当时**官方价目的美元当量」,**量纲一致、可比**。反过来那道筛是有害的
    /// ——官方一调价就会把整段历史一次性排除掉,而正常路径下 `recompute_stale_costs`
    /// 已经把存量行按各自时刻的价目重算过了。
    ///
    /// 被筛掉的行**留在库里**：它们是档案（将来回看套餐差异 / 价格变更前后的对比）,
    /// 只是不参与当前这一版系数的推断。
    pub fn pairs_for_fit(&self, platform: Platform, plan_type: &str) -> Vec<super::calib::Pair> {
        // 倍率类是 Rust 侧的判断（子串匹配 + 倍率相等）,SQL 表达不了 ⇒ 先把库里出现过的
        // 套餐名取出来判一遍,再把同类的那几个绑进 IN。库里的 distinct 套餐名至多几个。
        let allow: Option<Vec<String>> = match plan_type.trim() {
            "" | "unknown" => None,
            plan => {
                let mut v: Vec<String> = self
                    .distinct_plans(platform)
                    .into_iter()
                    .filter(|p| p.is_empty() || super::cost::same_plan_class(platform, plan, p))
                    .collect();
                // 空串 = 存量未知,永远放行（库里一条都没有时也要带上,否则 IN 为空）
                if !v.iter().any(|p| p.is_empty()) {
                    v.push(String::new());
                }
                Some(v)
            }
        };
        let filter = match &allow {
            None => String::new(),
            Some(list) => format!(
                " AND plan_type IN ({})",
                std::iter::repeat("?").take(list.len()).collect::<Vec<_>>().join(",")
            ),
        };
        let Ok(mut stmt) = self.conn.prepare(&format!(
            "SELECT t0, t1, used5_0, used5_1, cost, unknown_cost, resets5_0, resets5_1, aged_cost
               FROM usage_pair
              WHERE platform = ? AND weight_ver <> 0{filter}"
        )) else {
            return vec![];
        };
        let mut params: Vec<String> = vec![platform.as_str().to_string()];
        params.extend(allow.unwrap_or_default());
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params),
            |r| {
            Ok(super::calib::Pair {
                t0: r.get(0)?,
                t1: r.get(1)?,
                used5_0: r.get(2)?,
                used5_1: r.get(3)?,
                cost: r.get(4)?,
                unknown_cost: r.get(5)?,
                resets5_0: r.get(6)?,
                resets5_1: r.get(7)?,
                aged_cost: r.get(8)?,
            })
        },
        );
        rows.map(|rs| rs.flatten().collect()).unwrap_or_default()
    }

    /// 库里这个平台出现过的套餐名（含空串;至多几个）。
    fn distinct_plans(&self, platform: Platform) -> Vec<String> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT DISTINCT plan_type FROM usage_pair WHERE platform = ?1")
        else {
            return vec![];
        };
        stmt.query_map([platform.as_str()], |r| r.get::<_, String>(0))
            .map(|rs| rs.flatten().collect())
            .unwrap_or_default()
    }

    /// 价格数据集升订号后的**就地重算**。
    ///
    /// 遍历 `weight_ver < 当前订号` 的行,按 `breakdown` 里的原始 token 重新折算
    /// `cost`/`unknown_cost` 并写回,`weight_ver` 置为当前订号。**全部是按 id 的 UPDATE**,
    /// 不删行、不产生新行、可重复执行。
    ///
    /// **每个模型按它在该区间 `t1` 时刻生效的价目**重算。
    /// 这是本阶段的那个错误：此前一律用**最新**价目,于是"上游 X 日调价、我们 X+14
    /// 发版"这种正常节奏下,**X 之前本来正确的样本会被算错**。按时刻取价之后：
    /// - 只有一个模型变价时,不含该模型的行重算出来的值与原值相同（幂等,无副作用）,
    ///   所以**不必挑行,全量重算最简单**（`PAIR_KEEP = 5000` 封顶,开销可忽略）;
    /// - 一个区间里的不同模型各取各的时间线,不存在"这个区间属于哪一代"。
    ///
    /// `t1` 而不是 `t0`：区间右端点就是这批 token 记账落定的时刻,与在线路传 `now`
    /// 同口径。某模型的调价时点恰好落在某个 15〜30 分钟区间内部时,**那一条** pair 里
    /// 该模型的价会偏一点——发生频率是"每次降价至多一条样本",可忽略。
    ///
    /// 恢复不了的行（`breakdown` 缺失 / 解析不出任何模型——极早期的行或写坏的行）
    /// **不删**,置 `weight_ver = 0` 表示存疑：它们仍可被查询、仍是档案的一部分,
    /// 只是不再参与推断（`pairs_for_fit` 按等于当前版本筛）。
    ///
    /// 返回 `（重算成功条数, 标为存疑条数)`。当前版本与库内一致时是一次索引查询,零开销。
    pub fn recompute_stale_costs(&self, platform: Platform) -> Result<(usize, usize), String> {
        self.recompute_to(platform, super::cost::WEIGHT_VERSION)
    }

    /// 重算到指定世代（`recompute_stale_costs` 的内层;单测据此模拟一次权重表升版,
    /// 否则常量恒为当前值,这条路永远测不到）。
    fn recompute_to(&self, platform: Platform, target: u32) -> Result<(usize, usize), String> {
        let rows: Vec<(i64, i64, String)> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, t1, breakdown FROM usage_pair
                      WHERE platform = ?1 AND weight_ver <> 0 AND weight_ver < ?2",
                )
                .map_err(|e| e.to_string())?;
            let it = stmt
                .query_map(rusqlite::params![platform.as_str(), target], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(|e| e.to_string())?;
            it.flatten().collect()
        };
        if rows.is_empty() {
            return Ok((0, 0));
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let (mut done, mut doubtful) = (0, 0);
        for (id, t1, body) in rows {
            let models = super::cost::parse_breakdown(&body);
            if models.is_empty() {
                tx.execute("UPDATE usage_pair SET weight_ver = 0 WHERE id = ?1", [id])
                    .map_err(|e| e.to_string())?;
                doubtful += 1;
                continue;
            }
            // 按该区间右端点的时刻取价——每个模型各走各的价格时间线
            let (cost, unknown) = super::cost::cost_of_breakdown(platform, &models, t1);
            tx.execute(
                "UPDATE usage_pair SET cost = ?2, unknown_cost = ?3, weight_ver = ?4 WHERE id = ?1",
                rusqlite::params![id, cost, unknown, target],
            )
            .map_err(|e| e.to_string())?;
            done += 1;
        }
        tx.commit().map_err(|e| e.to_string())?;
        crate::dev_log!(
            "[subscription] {} cost recompute → price rev {}: {} row(s) rescaled, {} left doubtful",
            platform.as_str(),
            target,
            done,
            doubtful
        );
        Ok((done, doubtful))
    }

    /// 该平台某一路样本的水位线 = 最新一条的 t1（增量续算的起点;无样本 → None）。
    pub fn latest_pair_t1(&self, platform: Platform, src: &str) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT MAX(t1) FROM usage_pair WHERE platform = ?1 AND src = ?2",
                rusqlite::params![platform.as_str(), src],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
    }

    // ---------- meta（标量水位线一类;**不存任何凭据**） ----------

    /// 读一个整数标量（键不存在 / 解析不出 → None）。
    pub fn meta_i64(&self, key: &str) -> Option<i64> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [key], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|v| v.parse().ok())
    }

    /// 读一个字符串标量。
    pub fn meta_str(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [key], |r| r.get::<_, String>(0))
            .ok()
    }

    /// 写一个字符串标量（覆盖式）。**不写任何凭据原文**——账号只存指纹。
    pub fn set_meta_str(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO meta (k, v) VALUES (?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                rusqlite::params![key, value],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// **当前账号是从哪一刻起被观测到的**（unix 秒;从没观测过 → None）。
    ///
    /// 这条边界是给 `codex_rollout` 用的：一台机器可以登录多个账号来回切,而每个账号
    /// 有**自己独立的额度窗口** ⇒ 跨在切换点上的区间,它的 Δ 是拿两个账号的读数相减
    /// 出来的,毫无意义,必须整条丢掉。`plan_type` 挡不住这件事——本机 2026-07 就出现过
    /// **两个都是 plus** 的账号交替使用。
    ///
    /// 历史补不回来,所以这条边界只对
    /// **从现在往后**有效;边界之前仍靠 `plan_type` 与窗尾判据兜着。
    pub fn account_since(&self, platform: Platform) -> Option<i64> {
        self.meta_i64(&format!("account_since_{}", platform.as_str()))
    }

    /// 记下当前账号指纹；**和上次不同**（含从无到有）就把边界推到 `now`。
    ///
    /// 两个调用点都在链路上、都不花额外开销：在线取数时从 `wham/usage` 响应的
    /// `account_id` 取，本地则从凭据文件（Codex 的 `auth.json`）取。
    pub fn note_account(&self, platform: Platform, fp: &str, now: i64) {
        if fp.is_empty() {
            return;
        }
        let key = format!("account_fp_{}", platform.as_str());
        if self.meta_str(&key).as_deref() == Some(fp) {
            return;
        }
        let first = self.meta_str(&key).is_none();
        let _ = self.set_meta_str(&key, fp);
        let _ = self.set_meta_i64(&format!("account_since_{}", platform.as_str()), now);
        crate::dev_log!(
            "[subscription] {} account {} at {now}",
            platform.as_str(),
            if first { "first seen" } else { "**changed**" }
        );
    }

    /// 写一个整数标量（覆盖式）。
    pub fn set_meta_i64(&self, key: &str, value: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO meta (k, v) VALUES (?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                rusqlite::params![key, value.to_string()],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    // ---------- desktop_sample（本地读数收割;**只增不删**,见建表注释） ----------

    /// 收割一批读数（（unix 秒, 5h 已用 %, 7d 已用 %)；套餐未知的源走这条）。
    pub fn insert_samples(
        &self,
        platform: Platform,
        samples: &[(i64, f64, f64)],
    ) -> Result<usize, String> {
        let with_plan: Vec<_> = samples.iter().map(|(t, a, b)| (*t, *a, *b, String::new())).collect();
        self.insert_samples_of(platform, &with_plan)
    }

    /// 收割一批带套餐的读数。重复的按主键忽略,返回**本次新增**的条数
    /// （0 = 源没写新读数,调用方据此少做无谓的重算）。
    ///
    /// 冲突走 `IGNORE` 而不是覆盖：同一秒里可能有多条并发会话各自回报同一份服务端
    /// 状态,先到的那条与后到的等价,没有"哪条更对"可言;而覆盖会让同一次收割的结果
    /// 依赖遍历顺序（文件发现顺序不保证稳定）。
    pub fn insert_samples_of(
        &self,
        platform: Platform,
        samples: &[(i64, f64, f64, String)],
    ) -> Result<usize, String> {
        if samples.is_empty() {
            return Ok(0);
        }
        let before = self.sample_count(platform);
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR IGNORE INTO desktop_sample (platform, t, used5, used7, plan_type)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| e.to_string())?;
            for (t, used5, used7, plan) in samples {
                stmt.execute(rusqlite::params![platform.as_str(), t, used5, used7, plan])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok((self.sample_count(platform) - before).max(0) as usize)
    }

    /// t >= since 的样本（按时间升序;since 那一条**要含进来**——它是下一个区间的
    /// 左端点,漏掉就会在水位线处断出一个空档）。
    pub fn samples_since(&self, platform: Platform, since: i64) -> Vec<(i64, f64, f64)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT t, used5, used7 FROM desktop_sample
             WHERE platform = ?1 AND t >= ?2 ORDER BY t ASC",
        ) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params![platform.as_str(), since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        });
        rows.map(|rs| rs.flatten().collect()).unwrap_or_default()
    }

    /// 已收割的样本总数。
    pub fn sample_count(&self, platform: Platform) -> i64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM desktop_sample WHERE platform = ?1",
                [platform.as_str()],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// 相邻样本间隔的**中位数**。
    ///
    /// 必须取中位数而不是 `（最新 − 最早) / （条数 − 1)`：桌面端只在自己运行时采样,
    /// 关机 / 休眠会在序列里留下十几小时的空档,
    /// 均值被这种空档拉到 1864 秒,读日志的人会以为采样器半小时才写一条——而真实节律
    /// 是 900 秒整。
    pub fn median_sample_gap(&self, platform: Platform) -> Option<i64> {
        self.conn
            .query_row(
                "WITH g AS (
                     SELECT t - LAG(t) OVER (ORDER BY t) AS d
                       FROM desktop_sample WHERE platform = ?1
                 )
                 SELECT d FROM g WHERE d IS NOT NULL ORDER BY d
                  LIMIT 1 OFFSET (SELECT COUNT(*) / 2 FROM g WHERE d IS NOT NULL)",
                [platform.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .ok()
    }

    /// 已收割样本的时间跨度 （最早, 最新)。
    pub fn sample_span(&self, platform: Platform) -> Option<(i64, i64)> {
        self.conn
            .query_row(
                "SELECT MIN(t), MAX(t) FROM desktop_sample WHERE platform = ?1",
                [platform.as_str()],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .ok()
            .and_then(|(a, b)| Some((a?, b?)))
    }

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
                source: SnapshotSource::Api,
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
            source: SnapshotSource::Api,
        };
        s.save_snapshot(&snap).unwrap();
        let loaded = s.load_snapshot(Platform::Claude).unwrap();
        assert_eq!(loaded.plan_type, "pro");
        assert_eq!(loaded.windows.len(), 2);
        assert_eq!(loaded.windows[1].used_percent, 40.5);
        assert_eq!(loaded.status, FetchStatus::Ok);
    }

    /// 套餐边界（设计 §9-10 ②）：套餐名变了（含从无到有）才推进,失败轮的 "unknown"
    /// 与重复的同名成功轮都不推进。
    #[test]
    fn plan_boundary_moves_only_when_the_plan_name_really_changes() {
        let s = mem_store();
        let p = Platform::Claude;
        let snap = |plan: &str, at: Option<i64>, status| SubscriptionSnapshot {
            platform: p,
            plan_type: plan.into(),
            windows: if at.is_some() {
                vec![QuotaWindow { kind: "5h".into(), used_percent: 1.0, resets_at: None }]
            } else {
                vec![]
            },
            fetched_at: at,
            status,
            source: SnapshotSource::Api,
        };
        assert_eq!(s.plan_since(p), None, "没取过数 → 没有边界");

        // 第一次成功取数 = 从无到有,边界落在这一刻
        s.save_snapshot(&snap("max", Some(1_000), FetchStatus::Ok)).unwrap();
        assert_eq!(s.plan_since(p), Some(1_000));

        // 同一个套餐再取数多少次都不动边界
        s.save_snapshot(&snap("max", Some(2_000), FetchStatus::Ok)).unwrap();
        assert_eq!(s.plan_since(p), Some(1_000));

        // 失败轮（unknown + 无 fetched_at）不是换档,不能把边界推到现在
        s.save_snapshot(&snap("unknown", None, FetchStatus::NetworkFailed)).unwrap();
        assert_eq!(s.plan_since(p), Some(1_000));

        // 真换档 → 边界前移
        s.save_snapshot(&snap("pro", Some(3_000), FetchStatus::Ok)).unwrap();
        assert_eq!(s.plan_since(p), Some(3_000));
        assert_eq!(s.plan_since(Platform::Codex), None, "按平台分开");
    }

    #[test]
    fn desktop_samples_accumulate_idempotently() {
        let s = mem_store();
        let p = Platform::Claude;
        assert_eq!(s.insert_samples(p, &[(1_000, 10.0, 4.0), (1_900, 12.0, 4.0)]).unwrap(), 2);
        // 重复收割（桌面端文件里同一批样本会被读到很多次）→ 主键忽略,不新增
        assert_eq!(s.insert_samples(p, &[(1_000, 10.0, 4.0), (1_900, 12.0, 4.0)]).unwrap(), 0);
        assert_eq!(s.insert_samples(p, &[(1_900, 12.0, 4.0), (2_800, 15.0, 5.0)]).unwrap(), 1);
        assert_eq!(s.sample_count(p), 3);
        assert_eq!(s.sample_span(p), Some((1_000, 2_800)));
        assert_eq!(s.median_sample_gap(p), Some(900), "两段间隔都是 900");
        assert_eq!(s.sample_count(Platform::Codex), 0, "按平台分开");
        // 水位线那一条必须含进来——它是下一个区间的左端点
        assert_eq!(s.samples_since(p, 1_900), vec![(1_900, 12.0, 4.0), (2_800, 15.0, 5.0)]);
        assert!(s.samples_since(p, 9_999).is_empty());
    }

    /// Codex 那一路的读数自带套餐;同一秒撞主键时保留先到者（语义见 insert_samples_of）。
    #[test]
    fn samples_keep_their_plan_and_first_writer_wins() {
        let s = mem_store();
        let p = Platform::Codex;
        let rows = vec![
            (1_000, 3.0, 19.0, "edu".to_string()),
            (1_060, 5.0, 19.0, "edu".to_string()),
        ];
        assert_eq!(s.insert_samples_of(p, &rows).unwrap(), 2);
        // 同一秒的第二条（并发会话回报同一份服务端状态）不覆盖先到者
        assert_eq!(
            s.insert_samples_of(p, &[(1_000, 99.0, 99.0, "plus".into())]).unwrap(),
            0
        );
        let got: (f64, String) = s
            .conn
            .query_row(
                "SELECT used5, plan_type FROM desktop_sample WHERE platform = 'codex' AND t = 1000",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(got, (3.0, "edu".to_string()));
        // 不带套餐的老入口照常可用,落空串
        assert_eq!(s.insert_samples(Platform::Claude, &[(1_000, 10.0, 4.0)]).unwrap(), 1);
        let plan: String = s
            .conn
            .query_row(
                "SELECT plan_type FROM desktop_sample WHERE platform = 'claude'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(plan, "");
    }

    /// meta 标量：读不到 → None,写了就读得回来,重复写覆盖。
    #[test]
    fn meta_scalars_round_trip() {
        let s = mem_store();
        assert_eq!(s.meta_i64("nope"), None);
        s.set_meta_i64("k", 1_789_660_803).unwrap();
        assert_eq!(s.meta_i64("k"), Some(1_789_660_803));
        s.set_meta_i64("k", 42).unwrap();
        assert_eq!(s.meta_i64("k"), Some(42));
    }

    /// 存量库里已有 desktop_sample 但没有 plan_type 列：ALTER 补列,**一行不动**。
    #[test]
    fn existing_samples_survive_the_plan_column_upgrade() {
        let dir = std::env::temp_dir().join(format!("tc_sub_plancol_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("subscriptions.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE usage_pair (id INTEGER PRIMARY KEY AUTOINCREMENT, platform TEXT NOT NULL,
                     t0 INTEGER NOT NULL, t1 INTEGER NOT NULL, used5_0 REAL NOT NULL, used5_1 REAL NOT NULL,
                     used7_0 REAL NOT NULL, used7_1 REAL NOT NULL, cost REAL NOT NULL,
                     unknown_cost REAL NOT NULL, breakdown TEXT NOT NULL);
                 CREATE TABLE desktop_sample (platform TEXT NOT NULL, t INTEGER NOT NULL,
                     used5 REAL NOT NULL, used7 REAL NOT NULL, PRIMARY KEY (platform, t));
                 INSERT INTO desktop_sample VALUES ('claude', 1000, 10.0, 4.0),
                                                   ('claude', 1900, 12.0, 4.0);",
            )
            .unwrap();
        }
        let s = SubStore::open(&path).unwrap();
        assert_eq!(s.sample_count(Platform::Claude), 2, "读数不能在升级里丢");
        assert_eq!(s.samples_since(Platform::Claude, 0).len(), 2);
        let plans: Vec<String> = {
            let mut st = s.conn.prepare("SELECT plan_type FROM desktop_sample ORDER BY t").unwrap();
            let it = st.query_map([], |r| r.get(0)).unwrap();
            it.flatten().collect()
        };
        assert_eq!(plans, vec!["".to_string(), "".to_string()], "存量源不提供套餐 ⇒ 空串");
        // 幂等：再开一次不报错、不重复 ALTER
        drop(s);
        let s2 = SubStore::open(&path).unwrap();
        assert_eq!(s2.sample_count(Platform::Claude), 2);
        drop(s2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 间隔取中位数：关机留下的长空档不能把「采样密度」这行字带偏。
    #[test]
    fn median_gap_ignores_long_outages() {
        let s = mem_store();
        let p = Platform::Claude;
        // 四段 900 秒 + 一段 22 小时空档（桌面端没跑）
        let mut samples: Vec<(i64, f64, f64)> =
            (0..5).map(|i| (1_000 + i * 900, 10.0, 4.0)).collect();
        samples.push((1_000 + 4 * 900 + 79_339, 11.0, 4.0));
        s.insert_samples(p, &samples).unwrap();
        assert_eq!(s.median_sample_gap(p), Some(900), "中位数不被空档带偏");
        // 均值会被拉到 16000 秒以上——这正是旧写法的问题
        let (first, last) = s.sample_span(p).unwrap();
        let mean = (last - first) / (s.sample_count(p) - 1);
        assert!(mean > 15_000, "均值确实被空档拉走:{mean}");
        assert!(s.median_sample_gap(Platform::Codex).is_none(), "无样本 → None");
    }

    /// 窗口重置时刻:新行存得下、取得回;不提供这个字段的源（以及升级前的存量行）
    /// 留 NULL = **未知**,判据自动退回「读数变小了」⇒ 老样本照常参与拟合。
    #[test]
    fn window_tails_round_trip_and_unknown_stays_unknown() {
        let s = mem_store();
        let p = Platform::Codex;
        let mk = |r0: Option<i64>, r1: Option<i64>| super::super::calib::Pair {
            t0: 1_000,
            t1: 1_900,
            used5_0: 10.0,
            used5_1: 12.0,
            cost: 1.0,
            unknown_cost: 0.0,
            aged_cost: 0.0,
            resets5_0: r0,
            resets5_1: r1,
        };
        let w = (4.0, 4.0);
        s.insert_pair(p, &mk(Some(19_000), Some(19_000)), w, "{}", "rollout", "edu").unwrap();
        s.insert_pair(p, &mk(None, None), w, "{}", "desktop", "edu").unwrap();
        let mut got = s.pairs_for_fit(p, "edu");
        got.sort_by_key(|x| x.resets5_0.unwrap_or(0));
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].resets5_0, got[0].resets5_1), (None, None), "源不提供 ⇒ NULL");
        assert_eq!((got[1].resets5_0, got[1].resets5_1), (Some(19_000), Some(19_000)));
        assert!(
            got.iter().all(|x| x.usable(super::super::cost::prior_scale(p))),
            "两条都照常参与拟合"
        );
    }

    /// 判据升版后的就地重建只动「这次重建覆盖到的那一段」：段外的行（源已经消失、
    /// 重建不到的那些）与别的路一行都不碰。
    #[test]
    fn rebuilding_a_span_leaves_everything_outside_it_alone() {
        let s = mem_store();
        let p = Platform::Codex;
        let mk = |t0: i64, t1: i64| super::super::calib::Pair {
            t0,
            t1,
            used5_0: 10.0,
            used5_1: 12.0,
            cost: 1.0,
            unknown_cost: 0.0,
            aged_cost: 0.0,
            resets5_0: None,
            resets5_1: None,
        };
        let w = (4.0, 4.0);
        s.insert_pair(p, &mk(1_000, 1_600), w, "{}", "rollout", "edu").unwrap(); // 段内
        s.insert_pair(p, &mk(5_000, 5_600), w, "{}", "rollout", "edu").unwrap(); // 段外
        s.insert_pair(p, &mk(1_100, 1_500), w, "{}", "online", "edu").unwrap(); // 另一路
        assert_eq!(s.delete_pairs_in(p, "rollout", 900, 2_000), 1);
        let left: Vec<i64> = {
            let mut st = s.conn.prepare("SELECT t0 FROM usage_pair ORDER BY t0").unwrap();
            let it = st.query_map([], |r| r.get(0)).unwrap();
            it.flatten().collect()
        };
        assert_eq!(left, vec![1_100, 5_000], "段外与另一路都留着");
    }

    #[test]
    fn pair_watermark_is_per_source() {
        let s = mem_store();
        let p = Platform::Claude;
        let mk = |t0: i64, t1: i64| super::super::calib::Pair {
            t0, t1, used5_0: 10.0, used5_1: 12.0, cost: 100.0, unknown_cost: 0.0, aged_cost: 0.0,
            resets5_0: None, resets5_1: None,
        };
        assert_eq!(s.latest_pair_t1(p, "desktop"), None, "空库无水位线");
        s.insert_pair(p, &mk(1_000, 1_900), (4.0, 4.0), "{}", "desktop", "max").unwrap();
        s.insert_pair(p, &mk(5_000, 7_000), (4.0, 5.0), "{}", "online", "max").unwrap();
        assert_eq!(s.latest_pair_t1(p, "desktop"), Some(1_900), "两路互不干扰");
        assert_eq!(s.latest_pair_t1(p, "online"), Some(7_000));
        s.insert_pair(p, &mk(1_900, 2_800), (4.0, 5.0), "{}", "desktop", "max").unwrap();
        assert_eq!(s.latest_pair_t1(p, "desktop"), Some(2_800), "水位线随新样本前移");
    }

    /// 存量库升级：**只 ALTER + 回填,样本一条不能少**（红线见 AGENTS.md §3）。
    #[test]
    fn legacy_schema_upgrades_in_place() {
        let dir = std::env::temp_dir().join(format!("tc_sub_upgrade_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("subscriptions.db");
        // 旧版结构（无 snapshot.source / 无 usage_pair.src / 无 desktop_sample）
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE snapshot (platform TEXT PRIMARY KEY, plan_type TEXT NOT NULL,
                     windows TEXT NOT NULL, fetched_at INTEGER, status TEXT NOT NULL);
                 CREATE TABLE usage_pair (id INTEGER PRIMARY KEY AUTOINCREMENT, platform TEXT NOT NULL,
                     t0 INTEGER NOT NULL, t1 INTEGER NOT NULL, used5_0 REAL NOT NULL, used5_1 REAL NOT NULL,
                     used7_0 REAL NOT NULL, used7_1 REAL NOT NULL, cost REAL NOT NULL,
                     unknown_cost REAL NOT NULL, breakdown TEXT NOT NULL);
                 INSERT INTO snapshot VALUES ('claude','max','[]',123,'ok');
                 INSERT INTO usage_pair (platform,t0,t1,used5_0,used5_1,used7_0,used7_1,cost,unknown_cost,breakdown)
                   VALUES ('claude',10,20,1,2,1,2,100,0,
                           '{\"src\":\"bootstrap\",\"models\":{\"claude-sonnet-5\":[1000,10,0,0]}}'),
                          ('claude',30,40,1,2,1,2,100,0,'{\"claude-opus-5\":[500,0,0,0]}');",
            )
            .unwrap();
        }
        let s = SubStore::open(&path).unwrap();
        // 旧行一条不少,新列按默认值补齐
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "样本不能在升级里丢");
        assert_eq!(s.load_snapshot(Platform::Claude).unwrap().source, SnapshotSource::Api);
        // 冷启动那批本来就是按样本时刻切的 ⇒ 归入 desktop,水位线直接接上
        assert_eq!(s.latest_pair_t1(Platform::Claude, "desktop"), Some(20));
        assert_eq!(s.latest_pair_t1(Platform::Claude, "online"), Some(40));
        // 世代两列：cost 本来就是按当前（首个）权重表算的 ⇒ 存量行 weight_ver = 1;
        // 套餐按当前快照回填,于是升级当天的历史标定不会被筛光
        let vers: Vec<i64> = {
            let mut st = s.conn.prepare("SELECT weight_ver FROM usage_pair ORDER BY id").unwrap();
            let it = st.query_map([], |r| r.get(0)).unwrap();
            it.flatten().collect()
        };
        assert_eq!(vers, vec![1, 1]);
        let plans: Vec<String> = {
            let mut st = s.conn.prepare("SELECT plan_type FROM usage_pair ORDER BY id").unwrap();
            let it = st.query_map([], |r| r.get(0)).unwrap();
            it.flatten().collect()
        };
        assert_eq!(plans, vec!["max".to_string(), "max".to_string()], "按当前快照回填套餐");
        // 就地升级只负责补列（存量 cost 是按修订号 1 的尺子量的 ⇒ 回填 1）;把它们的
        // cost 换算到当前修订号的是启动时的重算,顺序与 mod.rs 一致。
        //
        // 注意与升级前的差别：`pairs_for_fit` 不再按修订号筛,所以这两行**升级当刻就
        // 已经可用**（PHASE15 S1,设计 §4.5——那道筛会在官方调价时把整段历史一次性
        // 排除掉）。重算改的是 cost 的**数值**,不是它能不能参与拟合。
        let before: Vec<f64> =
            s.pairs_for_fit(Platform::Claude, "max").iter().map(|p| p.cost).collect();
        assert_eq!(before.len(), 2, "修订号旧但样本照常可用");
        assert!(before.iter().all(|c| *c == 100.0), "重算之前还是落库时的值");
        s.recompute_stale_costs(Platform::Claude).unwrap();
        let after = s.pairs_for_fit(Platform::Claude, "max");
        assert_eq!(after.len(), 2, "重算后历史标定仍可用");
        assert!(after.iter().all(|p| p.cost > 0.0 && p.cost != 100.0), "cost 已按官方价目重算");
        // 存量行全部抬到当前修订号
        let vers: Vec<i64> = {
            let mut st = s.conn.prepare("SELECT weight_ver FROM usage_pair ORDER BY id").unwrap();
            let it = st.query_map([], |r| r.get(0)).unwrap();
            it.flatten().collect()
        };
        assert_eq!(vers, vec![super::super::cost::WEIGHT_VERSION as i64; 2]);
        // 新表建起来了,且出厂种子已 upsert 进去（既有行一条不少,见上面的 COUNT）
        assert!(!s.price_rows().is_empty(), "price_model 由就地升级建好并填上出厂种子");
        // breakdown 统一成规范裸 map:包装层被抹平,原始 token 一个不少
        let shapes: Vec<String> = {
            let mut st = s.conn.prepare("SELECT breakdown FROM usage_pair ORDER BY id").unwrap();
            let it = st.query_map([], |r| r.get(0)).unwrap();
            it.flatten().collect()
        };
        assert_eq!(shapes[0], r#"{"claude-sonnet-5":[1000,10,0,0]}"#, "包装层抹平");
        assert_eq!(shapes[1], r#"{"claude-opus-5":[500,0,0,0]}"#, "已规范的原样不动");
        // 升级前先备份（VACUUM INTO,红线要求）
        let backups: Vec<_> = std::fs::read_dir(dir.join("backups")).unwrap().flatten().collect();
        assert_eq!(backups.len(), 1, "升级前落一份备份");
        // 幂等：再开一次不重复升级、不再备份
        drop(s);
        let s2 = SubStore::open(&path).unwrap();
        assert_eq!(s2.latest_pair_t1(Platform::Claude, "desktop"), Some(20));
        let backups2: Vec<_> = std::fs::read_dir(dir.join("backups")).unwrap().flatten().collect();
        assert_eq!(backups2.len(), 1, "无需升级时不再备份");
        drop(s2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 拟合筛选：套餐对不上的、存疑的排除,但**行还在库里**;
    /// **不按价格修订号筛**（S1 去掉了那道筛,理由见 `pairs_for_fit` 注释）。
    #[test]
    fn pairs_for_fit_filters_by_plan_and_doubt_only() {
        let s = mem_store();
        let p = Platform::Claude;
        let mk = |t0: i64, t1: i64| super::super::calib::Pair {
            t0, t1, used5_0: 10.0, used5_1: 12.0, cost: 100.0, unknown_cost: 0.0, aged_cost: 0.0,
            resets5_0: None, resets5_1: None,
        };
        s.insert_pair(p, &mk(1_000, 1_900), (4.0, 4.0), "{}", "online", "max").unwrap();
        s.insert_pair(p, &mk(2_000, 2_900), (4.0, 4.0), "{}", "online", "pro").unwrap();
        s.insert_pair(p, &mk(3_000, 3_900), (4.0, 4.0), "{}", "online", "").unwrap();
        assert_eq!(s.pairs_for_fit(p, "max").len(), 2, "当前套餐 + 存量未标记的放行");
        assert_eq!(s.pairs_for_fit(p, "pro").len(), 2);
        // Claude 的 max 与 pro 倍率不同（1.0 / 5.0）⇒ 仍然分代,不许互相串
        assert_eq!(s.pairs_for_fit(p, "max_20x").len(), 1, "0.25 那一类只剩存量未知的");
        assert_eq!(s.pairs_for_fit(p, "unknown").len(), 3, "套餐未知时不按套餐筛");
        assert_eq!(s.pairs_for_fit(p, "").len(), 3);
        // 旧修订号的行**照常参与**：价格世代按模型走之后,各修订号下的 cost 都是
        // 「按当时官方价目的美元当量」,量纲一致、可比。
        s.conn.execute("UPDATE usage_pair SET weight_ver = 1 WHERE plan_type = 'pro'", []).unwrap();
        assert_eq!(s.pairs_for_fit(p, "pro").len(), 2, "旧修订号不再被排除");
        // 存疑行（重算不了的）一律排除
        s.conn.execute("UPDATE usage_pair SET weight_ver = 0 WHERE plan_type = 'max'", []).unwrap();
        assert_eq!(s.pairs_for_fit(p, "max").len(), 1, "存疑行不参与拟合");
        let kept: i64 = s.conn.query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 3, "被筛掉的行仍留在库里当档案");
    }

    /// 按**倍率类**筛而不是按套餐名（2026-09-19 用户定案）：出厂倍率表认为窗口一样大的
    /// 两个档（Codex 的 plus / edu / business）样本可比,合成一代;倍率不同的仍分代。
    #[test]
    fn pairs_for_fit_pools_plans_of_the_same_multiplier_class() {
        let s = mem_store();
        let p = Platform::Codex;
        let mk = |t0: i64, t1: i64| super::super::calib::Pair {
            t0, t1, used5_0: 10.0, used5_1: 12.0, cost: 100.0, unknown_cost: 0.0, aged_cost: 0.0,
            resets5_0: None, resets5_1: None,
        };
        s.insert_pair(p, &mk(1_000, 1_900), (4.0, 4.0), "{}", "rollout", "edu").unwrap();
        s.insert_pair(p, &mk(2_000, 2_900), (4.0, 4.0), "{}", "rollout", "plus").unwrap();
        s.insert_pair(p, &mk(3_000, 3_900), (4.0, 4.0), "{}", "rollout", "business").unwrap();
        s.insert_pair(p, &mk(4_000, 4_900), (4.0, 4.0), "{}", "rollout", "pro").unwrap();
        s.insert_pair(p, &mk(5_000, 5_900), (4.0, 4.0), "{}", "rollout", "").unwrap();
        // 1.0 那一类：edu + plus + business + 存量未知
        assert_eq!(s.pairs_for_fit(p, "edu").len(), 4, "同倍率的三个档合成一代");
        assert_eq!(s.pairs_for_fit(p, "plus").len(), 4, "从哪个档看过去都是同一代");
        // 0.2 那一类：pro + 存量未知
        assert_eq!(s.pairs_for_fit(p, "pro").len(), 2, "倍率不同的仍然分代");
        assert_eq!(s.pairs_for_fit(p, "unknown").len(), 5, "套餐未知时不按套餐筛");
        // 表里没有的档（官方没给可比限额）退回按名字精确比,不许靠回落值归类
        s.insert_pair(p, &mk(6_000, 6_900), (4.0, 4.0), "{}", "rollout", "team").unwrap();
        assert_eq!(s.pairs_for_fit(p, "team").len(), 2, "team 只匹配自己 + 存量未知");
        assert_eq!(s.pairs_for_fit(p, "edu").len(), 4, "team 不会被并进基准档那一类");
    }

    /// 权重表升版 = 按原始 token 就地重算,不是丢样本重来。
    #[test]
    fn stale_costs_are_recomputed_in_place() {
        let s = mem_store();
        let p = Platform::Claude;
        let mk = |t0: i64, t1: i64| super::super::calib::Pair {
            t0, t1, used5_0: 10.0, used5_1: 12.0, cost: 999.0, unknown_cost: 999.0, aged_cost: 0.0,
            resets5_0: None, resets5_1: None,
        };
        // 一条有原始明细（可恢复）、一条 breakdown 是空的（恢复不了）
        let detail = r#"{"claude-opus-5":[100000,2000,0,0]}"#;
        s.insert_pair(p, &mk(1_000, 1_900), (4.0, 4.0), detail, "online", "max").unwrap();
        s.insert_pair(p, &mk(2_000, 2_900), (4.0, 4.0), "{}", "online", "max").unwrap();
        // 模拟「价格变了,世代再升一级」
        let next = super::super::cost::WEIGHT_VERSION + 1;
        let (done, doubtful) = s.recompute_to(p, next).unwrap();
        assert_eq!((done, doubtful), (1, 1));
        let expect = {
            let models = super::super::cost::parse_breakdown(detail);
            // at = 该行的 t1（= 1_900）,与 recompute 同口径
            super::super::cost::cost_of_breakdown(p, &models, 1_900).0
        };
        let (cost, ver): (f64, i64) = s
            .conn
            .query_row("SELECT cost, weight_ver FROM usage_pair WHERE t0 = 1000", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!((cost - expect).abs() < 1e-9, "cost 按新权重重算,不再是落库时的 999");
        assert_eq!(ver, next as i64);
        // 恢复不了的行:不删,标 0 表示存疑
        let (cost0, ver0): (f64, i64) = s
            .conn
            .query_row("SELECT cost, weight_ver FROM usage_pair WHERE t0 = 2000", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(ver0, 0, "存疑");
        assert_eq!(cost0, 999.0, "存疑行的原值不动");
        // 幂等：再跑一次没有可重算的行,也不会把存疑行再数一遍
        assert_eq!(s.recompute_to(p, next).unwrap(), (0, 0));
        // 且 cost 一字不变（重复重算不许让数值漂移）
        let again: f64 = s
            .conn
            .query_row("SELECT cost FROM usage_pair WHERE t0 = 1000", [], |r| r.get(0))
            .unwrap();
        assert_eq!(again, cost, "重复重算是空操作,不改数值");
    }

    /// **S1 验收的核心用例**：某模型在 T 降价,重算后
    /// T 之前的样本按旧价、T 之后按新价,而**不含该模型的样本 `cost` 一字不变**。
    #[test]
    fn a_price_cut_only_rescales_rows_containing_that_model() {
        use super::super::{cost, price};
        const T: i64 = 1_800_000_000;
        let rows = price::two_segment_fixture(T);
        price::with_rows(&rows, || {
            let s = mem_store();
            let p = Platform::Claude;
            // 三条样本:降价模型在 T 前 / T 后各一条,另一条只含全程不变价的模型
            let both = r#"{"claude-steady-1":[1000000,0,0,0],"claude-widget-1":[1000000,0,0,0]}"#;
            let steady_only = r#"{"claude-steady-1":[1000000,0,0,0]}"#;
            let cases = [(T - 3_600, both), (T + 3_600, both), (T + 7_200, steady_only)];
            for (t1, detail) in cases {
                // 落库时的 cost 按**旧价**算（模拟"调价之前录进来的样本"）
                let models = cost::parse_breakdown(detail);
                let (c, u) = cost::cost_of_breakdown(p, &models, T - 1);
                let pair = super::super::calib::Pair {
                    t0: t1 - 900,
                    t1,
                    used5_0: 10.0,
                    used5_1: 12.0,
                    resets5_0: None,
                    resets5_1: None,
                    cost: c,
                    unknown_cost: u,
                    aged_cost: 0.0,
                };
                s.insert_pair(p, &pair, (4.0, 4.0), detail, "online", "max").unwrap();
            }
            let costs = |s: &SubStore| -> Vec<(i64, f64)> {
                let mut st =
                    s.conn.prepare("SELECT t1, cost FROM usage_pair ORDER BY t1").unwrap();
                let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
                it.flatten().collect()
            };
            let before = costs(&s);
            // 旧价下:两个模型各 100 万输入 token ⇒ 3 + 10 = $13;只含 steady 的 ⇒ $3
            assert_eq!(before, vec![(T - 3_600, 13.0), (T + 3_600, 13.0), (T + 7_200, 3.0)]);

            // 价格数据集升一个修订号 ⇒ 全量重算
            let (done, doubtful) = s.recompute_to(p, cost::WEIGHT_VERSION + 1).unwrap();
            assert_eq!((done, doubtful), (3, 0), "三行全部重算,无存疑");
            let after = costs(&s);
            assert_eq!(
                after,
                vec![
                    (T - 3_600, 13.0), // T 之前:按旧价,一字不变
                    (T + 3_600, 4.0),  // T 之后:widget 降到 $1 ⇒ 3 + 1
                    (T + 7_200, 3.0),  // 不含 widget:一字不变
                ]
            );
            // 逐位相等,不是"约等于"——不含该模型的行不许因为别的模型调价而漂移
            assert_eq!(before[0], after[0]);
            assert_eq!(before[2], after[2]);
        });
    }

    /// 全新安装：建全部表 + 填出厂种子,**不落备份**（没有历史可备份,每次新装都
    /// 写一份空库副本只是噪声）。与"存量库升级要备份"是同一段代码的两条分支。
    #[test]
    fn fresh_install_seeds_prices_without_a_backup() {
        let dir = std::env::temp_dir().join(format!("tc_sub_fresh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("subscriptions.db");
        let s = SubStore::open(&path).unwrap();
        assert_eq!(
            s.price_rows().len(),
            super::super::price::factory_seed().len(),
            "新装即填满出厂价目"
        );
        assert!(!dir.join("backups").exists(), "全新库不算升级,不落备份");
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        drop(s);
        // 再开一次仍然不备份（幂等）
        let s2 = SubStore::open(&path).unwrap();
        assert!(!dir.join("backups").exists());
        drop(s2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 出厂种子 upsert：幂等、就地更正数值、**不在种子里的历史段原样留着**。
    #[test]
    fn price_seed_upsert_is_idempotent_and_keeps_history() {
        use super::super::price::PriceRow;
        let s = mem_store();
        // open() 已经 upsert 过一遍出厂种子
        let seeded = s.price_rows();
        assert_eq!(seeded.len(), super::super::price::factory_seed().len());
        // 再 upsert 一遍:行数不变（主键幂等）
        s.upsert_price_seed(super::super::price::factory_seed()).unwrap();
        assert_eq!(s.price_rows().len(), seeded.len(), "重复 upsert 不产生新行");

        // 模拟"更早版本下发的历史生效段":手插一行,再 upsert 当前种子
        let legacy = PriceRow {
            platform: "claude".into(),
            match_key: "sonnet-5".into(),
            effective_from: 1_000_000_000,
            display_name: "Claude Sonnet 5 (旧段)".into(),
            usd_input: 9.0,
            usd_output: 45.0,
            usd_cache_read: 0.9,
            usd_cache_write: 11.25,
            source_note: "旧版本".into(),
        };
        s.upsert_price_seed(&[legacy]).unwrap();
        s.upsert_price_seed(super::super::price::factory_seed()).unwrap();
        let kept: Vec<_> = s
            .price_rows()
            .into_iter()
            .filter(|r| r.match_key == "sonnet-5")
            .collect();
        assert_eq!(kept.len(), 2, "历史生效段必须留着——用户跳版本更新时靠它");
        assert_eq!(kept[0].effective_from, 1_000_000_000);
        assert_eq!(kept[0].usd_input, 9.0, "历史段的价目不被当前种子覆盖");

        // 同一段生效期的数值更正:就地覆盖
        let mut fixed = kept[0].clone();
        fixed.usd_input = 8.0;
        s.upsert_price_seed(&[fixed]).unwrap();
        let again: Vec<_> = s
            .price_rows()
            .into_iter()
            .filter(|r| r.match_key == "sonnet-5" && r.effective_from == 1_000_000_000)
            .collect();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].usd_input, 8.0, "同一段生效期的修正就地生效");
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
            source: SnapshotSource::Api,
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
                source: SnapshotSource::Api,
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
            source: SnapshotSource::Api,
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
            source: SnapshotSource::Api,
        })
        .unwrap();
        let loaded = s.load_snapshot(Platform::Claude).unwrap();
        assert_eq!(loaded.status, FetchStatus::Idle);
        assert!(loaded.windows.is_empty());
        assert_eq!(loaded.fetched_at, None);
    }
}
