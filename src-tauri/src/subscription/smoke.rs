//! 订阅链路真库 smoke（仅本机手动验证,`cargo test -- --ignored` 触发）。
//!
//! 与 `collector:smoke` 同一套约定：**不触碰真实 subscriptions.db**——先把它整份
//! 复制到临时路径,一切升级 / 重算 / 拟合都发生在副本上。真库路径由环境变量
//! `TC_SUB_DB` 指定（默认取 dev 构建的数据根 `target/debug/data/subscriptions.db`）,
//! 临时目录由 `TC_SMOKE_OUT` 指定,事后可只读 SQL 复核。

use std::path::PathBuf;

use super::model::Platform;
use super::store::SubStore;

fn real_db() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TC_SUB_DB") {
        return Some(PathBuf::from(p));
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/debug/data/subscriptions.db");
    p.exists().then_some(p)
}

/// 把真库（含 -wal / -shm）整份复制到工作目录,返回副本路径。
fn clone_db(src: &PathBuf, out: &PathBuf) -> PathBuf {
    std::fs::create_dir_all(out).expect("create out dir");
    let dst = out.join("subscriptions.db");
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", src.display()));
        if from.exists() {
            std::fs::copy(&from, format!("{}{suffix}", dst.display())).expect("copy db");
        }
    }
    dst
}

/// **读数时间序列两层的真库迁移**：在真库副本上跑一遍开库顺序,
/// 核对 `desktop_sample` 一行不动、 已收割的读数全部就地回填进 `quota_reading`、
///  `quota_daily` 由它完整长出来且与整表重建逐位相同、 再开一次什么都不变。
#[test]
#[ignore]
fn quota_reading_layers_migrate_real_db() {
    let Some(src) = real_db() else {
        eprintln!("没有找到真库,跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_quota_smoke");
    let _ = std::fs::remove_dir_all(&out);
    let dst = clone_db(&src, &out);
    println!("真库副本 → {}", dst.display());

    // 迁移前：库里有多少条已收割读数,以及有没有 quota_reading
    let (samples_before, had_table) = {
        let c = rusqlite::Connection::open(&dst).unwrap();
        let had: bool = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='quota_reading'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        let mut st = c
            .prepare("SELECT platform, COUNT(*) FROM desktop_sample GROUP BY 1 ORDER BY 1")
            .unwrap();
        let it = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap();
        (it.flatten().collect::<Vec<_>>(), had)
    };
    println!("--- 迁移前 ---");
    println!("  quota_reading 表存在: {had_table}");
    for (p, n) in &samples_before {
        println!("  desktop_sample {p}: {n} 行");
    }

    let s = SubStore::open(&dst).expect("open");
    println!("--- 迁移后 ---");
    for (p, n_before) in &samples_before {
        let platform = Platform::from_str(p).unwrap();
        let after = s.sample_count(platform);
        assert_eq!(after, *n_before, "desktop_sample 一行都不能动（{p}）");
        let readings = s.quota_reading_count(platform);
        println!(
            "  {p}: desktop_sample {after} 行（不变） → quota_reading {readings} 行（去重前）"
        );
        assert!(
            readings >= n_before * 2,
            "每条样本两个窗口 ⇒ 至少 {} 行,实得 {readings}",
            n_before * 2
        );
        if let Some((first, last)) = s.quota_reading_span(platform) {
            println!(
                "     跨度 {:.1} 天 [{first}, {last}]",
                (last - first) as f64 / 86_400.0
            );
        }
    }

    // 日级汇总：逐日打印最近两周,并与整表重建对拍
    for platform in [Platform::Claude, Platform::Codex] {
        for kind in ["5h", "7d"] {
            let days = s.quota_days(platform, kind, "0000-00-00", "9999-99-99");
            if days.is_empty() {
                continue;
            }
            let total_n: i64 = days.iter().map(|d| d.n).sum();
            let total_gain: f64 = days.iter().map(|d| d.gain_pct).sum();
            let total_drop: f64 = days.iter().map(|d| d.drop_pct).sum();
            let resets: i64 = days.iter().map(|d| d.resets).sum();
            println!(
                "  quota_daily {} {}: {} 天 / {} 条读数 / Σ涨 {:.0}% / Σ掉 {:.0}% / 重置 {} 次",
                platform.as_str(),
                kind,
                days.len(),
                total_n,
                total_gain,
                total_drop,
                resets
            );
            for d in days.iter().rev().take(10).rev() {
                println!(
                    "     {} n={:<4} {:.0}%→{:.0}% (max {:.0}) 涨 {:.0} 掉 {:.0} 重置 {} 攒入 {}s",
                    d.day,
                    d.n,
                    d.used_first,
                    d.used_last,
                    d.used_max,
                    d.gain_pct,
                    d.drop_pct,
                    d.resets,
                    d.carry_secs
                );
            }
            // 去重后的读数条数必须与汇总里的 n 对得上
            let deduped = s
                .quota_readings(platform, kind, i64::MIN / 2, i64::MAX / 2)
                .len() as i64;
            assert_eq!(total_n, deduped, "汇总的 n 之和 = 去重后的读数条数");
        }
    }

    // 增量 ↔ 整表重建对拍（纯派生层的定义）
    for platform in [Platform::Claude, Platform::Codex] {
        let before: Vec<_> = ["5h", "7d"]
            .iter()
            .map(|k| s.quota_days(platform, k, "0000-00-00", "9999-99-99"))
            .collect();
        s.refresh_quota_daily(platform, None).unwrap();
        let after: Vec<_> = ["5h", "7d"]
            .iter()
            .map(|k| s.quota_days(platform, k, "0000-00-00", "9999-99-99"))
            .collect();
        assert_eq!(before, after, "{} 的增量结果与整表重建必须逐位相同", platform.as_str());
    }
    println!("  增量汇总 ↔ 整表重建：逐位相同");

    // 幂等：再开一次,两层都不变
    let counts: Vec<i64> =
        [Platform::Claude, Platform::Codex].iter().map(|p| s.quota_reading_count(*p)).collect();
    drop(s);
    let s2 = SubStore::open(&dst).expect("reopen");
    let counts2: Vec<i64> =
        [Platform::Claude, Platform::Codex].iter().map(|p| s2.quota_reading_count(*p)).collect();
    assert_eq!(counts, counts2, "再开一次不该重复回填");
    println!("  再开一次：quota_reading 行数不变 {counts2:?} ⇒ 迁移幂等");
}

/// 世代分布快照：`（platform, weight_ver, plan_type, 行数)`。
fn generations(s: &SubStore) -> Vec<(String, i64, String, i64)> {
    let mut st = s
        .conn_for_smoke()
        .prepare(
            "SELECT platform, weight_ver, plan_type, COUNT(*)
               FROM usage_pair GROUP BY 1,2,3 ORDER BY 1,2,3",
        )
        .unwrap();
    let it = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap();
    it.flatten().collect()
}

/// 每行的 `（id, t1, cost, breakdown)`（重算前后对拍用）。
fn pair_costs(s: &SubStore) -> Vec<(i64, i64, f64, String)> {
    let mut st = s
        .conn_for_smoke()
        .prepare("SELECT id, t1, cost, breakdown FROM usage_pair ORDER BY id")
        .unwrap();
    let it = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap();
    it.flatten().collect()
}

/// `price_model` 就地迁移 + 按时刻取价 + 代价单位改美元当量的真库。
///
/// 在真库**副本**上跑一遍启动顺序（open → price:load_from → recompute → refit）,核对：
/// - `price_model` 由就地迁移建好并填上出厂种子,**既有行一条不少**;
/// - 世代 2 的存量行,`cost` 是一次**纯量纲换算**：旧值 × 0.002 = 新值（旧单位 1 = $0.002）,
///   逐行核对;偏离的行单独列出来（那就是真的改了价的模型）;
/// - `scale` 相应地 ×500,于是 `cost × scale`（= 预计消耗百分点）**逐行不变** ⇒ 零回归;
/// - 更早世代的行按当前价目重算,不留在旧世代;
/// - 重算后标定仍拿得到样本（有可用行的平台 n > 0）;
/// - 重算幂等,存疑行不删。
#[test]
#[ignore]
fn price_model_migration_on_real_db() {
    let Some(src) = real_db() else {
        eprintln!("没有找到真库,跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_price_smoke");
    let _ = std::fs::remove_dir_all(&out);
    let dst = clone_db(&src, &out);
    println!("真库副本 → {}", dst.display());

    // ---- 升级前：旧构建看到的库长什么样 ----
    // 这个库可能是还没迁移过的旧库,也可能是已经跑过一次的新库,两种都要能跑：
    // 「迁移前必落备份」「旧值 × 0.002 = 新值」只在真的发生迁移时成立,
    // 无条件断言会让本用例对同一个库只能跑一次。
    // 想重新走一遍迁移路径：TC_SUB_DB 指向 data/backups/subscriptions-pre-*.db。
    // **两件独立的事**,别混：结构升级（缺表 / 缺列,要备份）与派生值重算（不改结构）。
    // 恢复一份"迁移中"的备份就会出现"结构已齐、行还旧"——那时该重算不该备份。
    // 结构缺口直接问 `SubStore:schema_gaps`（开库用的同一个判据）,不在这里另抄一份。
    let (needs_schema, pre_ver, legacy_pairs): (bool, std::collections::BTreeMap<i64, i64>, i64) = {
        let c = rusqlite::Connection::open(&dst).unwrap();
        let gaps = SubStore::schema_gaps(&c);
        let has_ver: bool = c
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('usage_pair') WHERE name = 'weight_ver'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        // 没有 weight_ver 列的库,升级时 ALTER 的默认值是 1（首个权重表）
        let sql = if has_ver {
            "SELECT id, weight_ver FROM usage_pair"
        } else {
            "SELECT id, 1 FROM usage_pair"
        };
        let mut st = c.prepare(sql).unwrap();
        let pre_ver: std::collections::BTreeMap<i64, i64> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().flatten().collect();
        let n = pre_ver.len() as i64;
        println!("--- 升级前 --- 结构缺口={gaps:?}");
        (gaps.any(), pre_ver, n)
    };
    let needs_recompute =
        pre_ver.values().any(|v| *v != 0 && *v < super::cost::WEIGHT_VERSION as i64);
    println!("  ⇒ 需要结构升级={needs_schema} 需要重算={needs_recompute}");

    // ---- open：就地迁移（建表 + upsert 种子 + 迁移前备份） ----
    let s = SubStore::open(&dst).expect("open");
    println!("--- 就地迁移后 ---");
    let after_rows: i64 =
        s.conn_for_smoke().query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0)).unwrap();
    assert_eq!(legacy_pairs, after_rows, "迁移不许动既有行");
    println!("  usage_pair {legacy_pairs} → {after_rows} 行（一条不少）");
    let backups = std::fs::read_dir(out.join("backups"))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    println!("  迁移前备份：{backups} 份");
    if needs_schema {
        assert!(backups >= 1, "结构升级前必须落备份（红线）");
    } else {
        assert_eq!(backups, 0, "没有结构升级就不该无谓备份");
    }

    let price_rows = s.price_rows();
    println!("  price_model {} 行", price_rows.len());
    assert_eq!(price_rows.len(), super::price::factory_seed().len(), "出厂种子全部落库");
    for p in [Platform::Claude, Platform::Codex] {
        let n = price_rows.iter().filter(|r| r.platform == p.as_str()).count();
        let multi = price_rows
            .iter()
            .filter(|r| r.platform == p.as_str())
            .fold(std::collections::BTreeMap::<String, usize>::new(), |mut m, r| {
                *m.entry(r.match_key.clone()).or_default() += 1;
                m
            })
            .into_iter()
            .filter(|(_, c)| *c > 1)
            .count();
        println!("    {}: {n} 行,其中有第二段生效期的模型 {multi} 个", p.as_str());
    }

    // 价目索引从库装载（启动顺序里排在重算之前）
    super::price::load_from(&s);
    assert!(super::price::is_loaded_from_db(), "索引必须来自库,不是编译期种子兜底");

    // ---- 重算前的 cost（= 旧构建按世代 2 相对权重算出来的值） ----
    let before = pair_costs(&s);

    // 演示**为什么 mod.rs 必须把重算排在标定装载之前**：此刻库里的 cost 还是旧
    // 单位（1 单位 = $0.002）,而 PRIOR_SCALE 已经是新单位的 1 %/美元 —— 两者相差 500
    // 倍,于是 calib 的"隐含比值可信带"会把绝大多数样本判成离谱值踢掉。
    // 这是一个**不该出现的中间态**,启动顺序保证它不会发生。
    for p in [Platform::Claude, Platform::Codex] {
        super::calib::refit_from_store(&s, p);
        println!(
            "  refit(**重算之前**,仅为演示顺序的必要性) {}: scale={:.6} n={}",
            p.as_str(),
            super::calib::scale(p),
            super::calib::sample_count(p)
        );
    }

    // ---- 重算 ----
    println!("--- 重算（每个模型按它在该区间 t1 时刻生效的价目）---");
    let mut doubtful_total = 0usize;
    for p in [Platform::Claude, Platform::Codex] {
        let (done, doubtful) = s.recompute_stale_costs(p).expect("recompute");
        doubtful_total += doubtful;
        println!("  {}: 重算 {done} 行,存疑 {doubtful} 行", p.as_str());
    }
    let after = pair_costs(&s);
    assert_eq!(before.len(), after.len(), "重算不许丢行");

    // ---- 逐行核对量纲换算 ----
    // 旧代价单位 1 = 1000 个 Sonnet 5 输入 token 当量 = 1000 × 2 USD/Mtok ÷ 1e6 = $0.002。
    // 只有世代 2（相对权重）→ 美元当量是纯量纲换算;更早的世代权重本身不同,重算是真的改价,
    // 只核对它们被重算到了当前世代。
    const OLD_UNIT_USD: f64 = 0.002;
    const UNIT_CHANGE_FROM: i64 = 2;
    let mut exact = 0usize;
    let mut drifted = vec![];
    let mut repriced = 0usize;
    for ((id, t1, old_cost, body), (_, _, new_cost, _)) in before.iter().zip(after.iter()) {
        if super::cost::parse_breakdown(body).is_empty() {
            continue; // 存疑行的原值不动,不参与对拍
        }
        match pre_ver.get(id).copied().unwrap_or(0) {
            UNIT_CHANGE_FROM => {}
            v if v > 0 && v < UNIT_CHANGE_FROM => {
                repriced += 1;
                continue;
            }
            _ => continue,
        }
        let want = old_cost * OLD_UNIT_USD;
        let rel = if want != 0.0 { ((new_cost - want) / want).abs() } else { new_cost.abs() };
        if rel < 1e-9 {
            exact += 1;
        } else {
            drifted.push((*id, *t1, *old_cost, want, *new_cost, rel));
        }
    }
    let behind: i64 = s
        .conn_for_smoke()
        .query_row(
            "SELECT COUNT(*) FROM usage_pair WHERE weight_ver <> 0 AND weight_ver < ?1",
            [super::cost::WEIGHT_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    println!("--- 早于世代 {UNIT_CHANGE_FROM} 的行按当前价目重算 {repriced} 行,仍落后于当前世代 {behind} 行 ---");
    assert_eq!(behind, 0, "重算之后不许有行停在旧世代");
    if needs_recompute {
        println!("--- 量纲换算对拍（旧值 × {OLD_UNIT_USD} 应等于新值）---");
        println!("  逐位相符 {exact} 行,偏离 {} 行", drifted.len());
        for (id, t1, old_cost, want, got, rel) in drifted.iter().take(20) {
            println!(
                "    id={id} t1={t1} 旧={old_cost:.4} 期望={want:.6} 实得={got:.6} 相对差={:.4}%",
                rel * 100.0
            );
        }
        assert!(
            drifted.is_empty(),
            "有 {} 行的 cost 不是纯量纲换算 —— 这些模型的价目真的变了,须逐条确认",
            drifted.len()
        );
    } else {
        // 已迁移的库:before / after 是同一批值,重算是空操作 ⇒ 逐行相等即可
        let same = before.iter().zip(after.iter()).all(|(b, a)| b.2 == a.2);
        println!("--- 重算幂等对拍 --- 每行 cost 与重算前完全相同：{same}");
        assert!(same, "已是当前修订号时,重算必须一个数都不改");
    }

    // ---- 样本准入在换单位前后必须一致 ----
    //
    // 这是零回归的第二个支点,而且**与这台机器有多少样本无关**：`Pair:usable` 的
    // 那道"隐含比值可信带"是**带心**的倍数区间,而 Δ用量 / 代价与带心同步 ×500
    // ⇒ 判据不变。逐行两边各算一遍,逐个核对 usable 的结论相同。
    //
    // **只隔离"换单位"这一件事**：两边调的是**同一个** `usable`,只是一边喂旧量纲的
    // 代价配旧量纲的带心、另一边喂新的。不另抄一份旧判据,否则判据本身一改就会把
    // 两件不相干的改动混成一笔。
    let usable_flips: Vec<i64> = {
        let mut st = s
            .conn_for_smoke()
            .prepare(
                "SELECT id, t0, t1, used5_0, used5_1, cost, unknown_cost, resets5_0, resets5_1,
                        platform
                   FROM usage_pair
                  WHERE weight_ver <> 0 ORDER BY id",
            )
            .unwrap();
        let it = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    super::calib::Pair {
                        t0: r.get(1)?,
                        t1: r.get(2)?,
                        used5_0: r.get(3)?,
                        used5_1: r.get(4)?,
                        cost: r.get(5)?,
                        unknown_cost: r.get(6)?,
                        aged_cost: 0.0,
                        resets5_0: r.get(7)?,
                        resets5_1: r.get(8)?,
                    },
                    if r.get::<_, String>(9)? == "codex" { Platform::Codex } else { Platform::Claude },
                ))
            })
            .unwrap();
        it.flatten()
            .filter(|(_, new_pair, plat)| {
                // 同一行的"旧单位"版本 = 把两项代价除回 0.002
                let old_pair = super::calib::Pair {
                    cost: new_pair.cost / OLD_UNIT_USD,
                    unknown_cost: new_pair.unknown_cost / OLD_UNIT_USD,
                    aged_cost: 0.0,
                    ..*new_pair
                };
                // 旧构建的带心是该平台先验的旧量纲值、配旧代价;新构建是新量纲配新代价。
                // 纯量纲换算 ⇒ 两边结论必须逐行相同（与先验取什么数无关）。
                let prior = super::cost::prior_scale(*plat);
                old_pair.usable(prior * OLD_UNIT_USD) != new_pair.usable(prior)
            })
            .map(|(id, _, _)| id)
            .collect()
    };
    println!("--- 样本准入对拍 ---");
    println!("  换单位前后 usable 结论不同的行：{}", usable_flips.len());
    assert!(usable_flips.is_empty(), "样本准入不许因为换单位而改变：{usable_flips:?}");

    // ---- 读数量化在真库上的实际影响（诊断,不改变任何行为） ----
    //
    // 读数是整数百分比 ⇒ 一段消耗若不够动一个百分点,Δ 就是 0,而 `usable` 目前要求
    // Δ > 0 ⇒ 这条样本被整条丢掉,**它的 cost 也一起离开了分母**。这会把 ΣΔ/Σcost
    // 抬高。下面把两种算法都打出来,供筛选层的取舍用（不靠抬采样下界解决：区间长度
    // 同时是监测节奏的量,不该拿它当筛子）。
    {
        let mut st = s
            .conn_for_smoke()
            .prepare(
                "SELECT t1 - t0, used5_1 - used5_0, cost FROM usage_pair
                  WHERE weight_ver <> 0 AND cost > 0 AND used5_1 >= used5_0
                    AND t1 - t0 BETWEEN ?1 AND ?2",
            )
            .unwrap();
        let rows: Vec<(i64, f64, f64)> = st
            .query_map([super::calib::MIN_PAIR_SECS, super::calib::MAX_PAIR_SECS], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .flatten()
            .collect();
        let ratio = |v: &[(i64, f64, f64)]| {
            let (d, c): (f64, f64) = v.iter().fold((0.0, 0.0), |a, x| (a.0 + x.1, a.1 + x.2));
            if c > 0.0 { d / c } else { 0.0 }
        };
        let flat: Vec<_> = rows.iter().copied().filter(|r| r.1 <= 0.0).collect();
        let moved: Vec<_> = rows.iter().copied().filter(|r| r.1 > 0.0).collect();
        println!("--- 读数量化诊断 ---");
        println!(
            "  Δ=0（消耗不够动一个百分点）{} 行 / Δ>0 {} 行",
            flat.len(),
            moved.len()
        );
        println!(
            "  现口径 ΣΔ/Σcost（只算 Δ>0）= {:.4};把 Δ=0 的 cost 也计入分母 = {:.4}",
            ratio(&moved),
            ratio(&rows)
        );
        if !flat.is_empty() {
            println!(
                "  ⇒ 丢掉 Δ=0 让系数偏高 {:.0}%（它们的 cost 是真实消耗,只是没被整数读数捕捉到）",
                (ratio(&moved) / ratio(&rows) - 1.0) * 100.0
            );
        }
        let total: i64 = s
            .conn_for_smoke()
            .query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, after.len() as i64, "诊断只读,不许动行");
    }

    // ---- 重算后：样本准入与逐行换算都已对拍 ⇒ 拟合出的 scale 恰好 ×500 ----
    println!("--- 重算后 ---");
    for g in generations(&s) {
        println!("  platform={} weight_ver={} plan={} rows={}", g.0, g.1, g.2, g.3);
    }
    for p in [Platform::Claude, Platform::Codex] {
        super::calib::refit_from_store(&s, p);
        let sa = super::calib::scale(p);
        let na = super::calib::sample_count(p);
        println!("  refit(重算后) {}: scale={:.6} %/美元 n={}", p.as_str(), sa, na);
        let rows: i64 = s
            .conn_for_smoke()
            .query_row(
                "SELECT COUNT(*) FROM usage_pair WHERE platform = ?1 AND weight_ver <> 0",
                [p.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(rows == 0 || na > 0, "{} 有可用行,标定却一条样本都拿不到", p.as_str());
        println!("    折回旧单位 = {:.8} %/旧代价单位", sa * OLD_UNIT_USD);
        // 预计消耗 = cost × scale ⇒ 两个因子一个 ×0.002 一个 ×500,乘积不变
        println!("    ⇒ cost × scale 不变 ⇒ 取数时机零回归");
    }

    // ---- 幂等 ----
    for p in [Platform::Claude, Platform::Codex] {
        let (done, _) = s.recompute_stale_costs(p).expect("recompute again");
        assert_eq!(done, 0, "{} 第二次重算必须是空操作", p.as_str());
    }
    println!("幂等：第二次重算 0 行");
    println!("存疑（breakdown 恢复不了）共 {doubtful_total} 行——不删,留库当档案");

    // ---- 诊断：有多少行的 t1 早于其模型在上游登记的上线时刻（取首段的那种情形） ----
    let mut clamped = 0usize;
    let mut models_seen: std::collections::BTreeMap<String, usize> = Default::default();
    for (_, t1, _, body) in &after {
        for model in super::cost::parse_breakdown(body).keys() {
            *models_seen.entry(model.clone()).or_default() += 1;
            for p in [Platform::Claude, Platform::Codex] {
                if let Some(r) = super::price::price_at(p, model, *t1) {
                    if *t1 < r.effective_from {
                        clamped += 1;
                    }
                    break;
                }
            }
        }
    }
    println!("--- 诊断 ---");
    println!("  样本里出现过的模型 {} 个：{:?}", models_seen.len(), models_seen);
    println!("  t1 早于模型上线时刻、按首段取价的 (行,模型) 对：{clamped}");
}

/// Claude 那一路每行的 `（id, t1, cost, breakdown)`（Codex 回溯前后逐行对拍用）。
fn claude_pairs(s: &SubStore) -> Vec<(i64, i64, f64, String)> {
    let mut st = s
        .conn_for_smoke()
        .prepare(
            "SELECT id, t1, cost, breakdown FROM usage_pair WHERE platform = 'claude' ORDER BY id",
        )
        .unwrap();
    let it = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap();
    it.flatten().collect()
}

/// Codex 回溯路的真库：在真库**副本**上跑一遍
/// `codex_rollout:ingest`,用本机真实的 `~/.codex` rollout 文件,核对：
///
/// - Claude 那一路的样本**一行不动**（两路水位线互不干扰）;
/// - Codex 从 0 条样本变成有样本,且 `scale` 由出厂预设 1.0 变成值;
/// - 读数收割进 `desktop_sample` 且带上套餐;
/// - **第二次 ingest 是空操作**（水位线生效,不重复建同一段区间的样本）;
/// - 首次扫描读了多少文件 / 多少字节 / 花了多久——`HORIZON_DAYS` 的取值依据来自这里。
#[test]
#[ignore]
fn codex_rollout_backfill_on_real_db() {
    let Some(src) = real_db() else {
        eprintln!("没有找到真库,跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_codex_rollout_smoke");
    let _ = std::fs::remove_dir_all(&out);
    let dst = clone_db(&src, &out);
    println!("真库副本 → {}", dst.display());

    let s = SubStore::open(&dst).expect("open");
    super::price::load_from(&s);
    for p in [Platform::Claude, Platform::Codex] {
        let _ = s.recompute_stale_costs(p);
        super::calib::refit_from_store(&s, p);
    }
    // 把副本上的 Codex 快照时间**倒拨**,好让「rollout 读数比上次取数新」这个条件成立。
    // 真机上它要等下一次真的用 Codex 才出现,而这条路（零请求推进快照）正是本用例要验的；
    // 倒拨的是副本,用的是真实 rollout 文件,跑的是同一份代码。
    if let Some(mut sn) = s.load_snapshot(Platform::Codex) {
        sn.fetched_at = Some(0);
        s.save_snapshot(&sn).expect("rewind snapshot");
        println!("--- 已把副本的 codex 快照 fetched_at 倒拨到 0（真机需等下次用 Codex）---");
    }
    // 判据版本也倒拨一格。真机上它已经是当前版本、水位线也扫到了最新 ⇒ 不倒拨的话
    // 这一轮整段跳过（scan 一个文件都不读）,什么都验不到。倒拨之后走的正是
    // 「判据升版 → 就地重建」那条路,连带把零请求推进快照也跑出来。
    let _ =
        s.set_meta_i64("codex_rollout_pair_rule", super::calib::ADMISSION_RULE_VERSION - 1);
    let codex_rows_before: i64 = s
        .conn_for_smoke()
        .query_row("SELECT COUNT(*) FROM usage_pair WHERE platform='codex'", [], |r| r.get(0))
        .unwrap_or(0);

    let claude_before = claude_pairs(&s);
    let claude_rows_before = claude_before.len();
    let claude_scale_before = super::calib::scale(Platform::Claude);
    let claude_n_before = super::calib::sample_count(Platform::Claude);
    println!("--- 回溯前 ---");
    println!(
        "  claude: {claude_rows_before} 行, scale={claude_scale_before:.6} n={claude_n_before}"
    );
    println!(
        "  codex : {} 行, scale={:.6} n={}, 已收割读数 {} 条",
        s.conn_for_smoke()
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM usage_pair WHERE platform='codex'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        super::calib::scale(Platform::Codex),
        super::calib::sample_count(Platform::Codex),
        s.sample_count(Platform::Codex)
    );

    // ---- 第一次回溯（首次全扫）----
    let now = chrono::Utc::now().timestamp();
    let t = std::time::Instant::now();
    let (harvested, pairs, snap_changed) = super::codex_rollout::ingest(&s, now);
    let elapsed = t.elapsed();
    println!("--- 第一次 ingest ---");
    println!(
        "  收割读数 {harvested} 条 / 新建样本 {pairs} 条, 耗时 {:.2}s, 快照被推进={snap_changed}",
        elapsed.as_secs_f64()
    );
    if let Some(sn) = s.load_snapshot(Platform::Codex) {
        println!(
            "  快照 source={} plan={} windows={} fetched_at={:?}",
            sn.source.as_str(),
            sn.plan_type,
            serde_json::to_string(&sn.windows).unwrap_or_default(),
            sn.fetched_at
        );
    }
    println!(
        "  codex refit: scale={:.4} %/美元 n={}  ⇒ 满 5h 窗 ≈ ${:.1} 等价用量",
        super::calib::scale(Platform::Codex),
        super::calib::sample_count(Platform::Codex),
        100.0 / super::calib::scale(Platform::Codex)
    );

    assert!(pairs > 0, "判据版本倒拨之后必须真的重建出样本");
    let codex_rows_after: i64 = s
        .conn_for_smoke()
        .query_row("SELECT COUNT(*) FROM usage_pair WHERE platform='codex'", [], |r| r.get(0))
        .unwrap_or(0);
    println!("  就地重建：{codex_rows_before} 行 → {codex_rows_after} 行（+{pairs} 条新样本）");

    // 按套餐看一眼样本分布（本机历史里 plan_type 在 plus / edu 之间跳过很多次）
    println!("--- codex 样本分布 ---");
    for g in generations(&s).into_iter().filter(|g| g.0 == "codex") {
        println!("  weight_ver={} plan={} rows={}", g.1, g.2, g.3);
    }
    let (rmin, rmax): (i64, i64) = s
        .conn_for_smoke()
        .query_row(
            "SELECT MIN(t0), MAX(t1) FROM usage_pair WHERE platform='codex' AND src='rollout'",
            [],
            |r| Ok((r.get(0).unwrap_or(0), r.get(1).unwrap_or(0))),
        )
        .unwrap_or((0, 0));
    println!("  区间跨度 {:.1} 天", (rmax - rmin) as f64 / 86_400.0);
    let plans: Vec<(String, i64)> = {
        let mut st = s
            .conn_for_smoke()
            .prepare(
                "SELECT plan_type, COUNT(*) FROM desktop_sample
                  WHERE platform='codex' GROUP BY 1 ORDER BY 2 DESC",
            )
            .unwrap();
        let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        it.flatten().collect()
    };
    println!("  收割的读数按套餐: {plans:?}");

    // ---- Claude 那一路必须原样不动（逐行对拍,不只是数行数）----
    let claude_after = claude_pairs(&s);
    let claude_rows_after = claude_after.len();
    assert_eq!(claude_rows_before, claude_rows_after, "Codex 回溯不许动 Claude 的样本");
    assert_eq!(claude_before, claude_after, "Claude 每一行的 (id,t1,cost,breakdown) 都必须逐位相同");
    super::calib::refit_from_store(&s, Platform::Claude);
    println!("--- Claude 对照 ---");
    println!(
        "  {claude_rows_after} 行（逐行不变）, scale={:.6} n={}（回溯前 {:.6} / {}）",
        super::calib::scale(Platform::Claude),
        super::calib::sample_count(Platform::Claude),
        claude_scale_before,
        claude_n_before
    );
    assert!(
        (super::calib::scale(Platform::Claude) - claude_scale_before).abs() < 1e-12,
        "Claude 的系数必须逐位不变"
    );

    // ---- 第二次 ingest：水位线生效,必须是空操作 ----
    let t = std::time::Instant::now();
    let (h2, p2, snap2) = super::codex_rollout::ingest(&s, now);
    println!("--- 第二次 ingest ---");
    println!(
        "  收割 {h2} 条 / 新建 {p2} 条, 快照再变={snap2}, 耗时 {:.2}s（水位线生效）",
        t.elapsed().as_secs_f64()
    );
    assert_eq!(p2, 0, "第二次不许重复建同一段区间的样本");
    assert!(!snap2, "第二次没有更新的读数 ⇒ 快照不许再动");

    // ---- 零请求推进的那条快照必须是真的、且标对来源 ----
    let sn = s.load_snapshot(Platform::Codex).expect("snapshot");
    let newest: (i64, f64, f64) = s
        .conn_for_smoke()
        .query_row(
            "SELECT t, used5, used7 FROM desktop_sample WHERE platform='codex'
              ORDER BY t DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("newest reading");
    assert_eq!(sn.source, super::model::SnapshotSource::Rollout, "来源要标成 rollout");
    assert_eq!(sn.fetched_at, Some(newest.0), "快照时刻 = 最新那条读数的时刻");
    let used = |kind: &str| sn.windows.iter().find(|w| w.kind == kind).unwrap().used_percent;
    assert_eq!((used("5h"), used("7d")), (newest.1, newest.2), "两个窗口都要照搬");
    assert!(
        sn.windows.iter().all(|w| w.resets_at.is_some()),
        "窗尾要从 rollout 带过来——悬浮球靠它显示重置时刻"
    );
    println!(
        "--- 零请求快照 --- source={} 5h={:.0}% 7d={:.0}% fetched_at={:?} resets={:?}",
        sn.source.as_str(),
        used("5h"),
        used("7d"),
        sn.fetched_at,
        sn.windows.iter().map(|w| w.resets_at).collect::<Vec<_>>()
    );

    // ---- 重算幂等：回溯落库的行本来就是按当前订号算的 ----
    let (done, doubtful) = s.recompute_stale_costs(Platform::Codex).expect("recompute");
    println!("--- 重算 --- codex: {done} 行, 存疑 {doubtful} 行");
    assert_eq!(done, 0, "刚落库的样本就是当前修订号,不该被重算");
    assert_eq!(doubtful, 0, "breakdown 是本模块自己写的,不该有恢复不了的");
}

/// 本机 collector.db（`bootstrap` 那一路的另一半原料）。
fn real_collector_db() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TC_COLLECTOR_DB") {
        return Some(PathBuf::from(p));
    }
    // 默认与 `real_db` 同一个数据根
    let p = real_db()?.with_file_name("collector.db");
    p.exists().then_some(p)
}

/// 套餐差异三件事的真库（倍率表、套餐边界、冷启动次序）：
///
/// - **零回归**：本机当前套餐（Claude Max 5x / Codex edu）的倍率是 1.0 ⇒ `prior_scale`
///   与真库上拟合出来的 `scale` 逐位不变;
/// - **倍率表**：把各档折算出来的预设打出来,核对方向（配额小的档系数更大）;
/// - **套餐边界**：真库里 `plan_since` 缺席（换档才写）⇒ `bootstrap` 的标注与没有这条
///   时逐行相同,一行都不该变成「套餐未知」;
/// - **冷启动次序**：模拟全新安装——空的 subscriptions.db + 还没写盘的 collector.db,
///   核对「读数收割进去了、样本一条建不出来、水位线仍是 None」（正是这个 None 让
///   `mod.rs` 的闸门保持冷启动态、跟着 collector 首扫写盘重试）,再用真 collector.db
///   跑第二轮,核对补得上。
#[test]
#[ignore]
fn plan_aware_priors_on_real_db() {
    let Some(src) = real_db() else {
        eprintln!("没有找到真库,跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_plan_prior_smoke");
    let _ = std::fs::remove_dir_all(&out);
    let dst = clone_db(&src, &out);
    println!("真库副本 → {}", dst.display());

    let s = SubStore::open(&dst).expect("open");
    super::price::load_from(&s);

    // ---- 倍率表：各档折算出来的预设 ----
    println!("--- 出厂预设按套餐折算 ---");
    for (platform, plans) in [
        (Platform::Claude, ["pro", "max", "max_20x", "unknown"].as_slice()),
        (Platform::Codex, ["plus", "edu", "pro", "unknown"].as_slice()),
    ] {
        for plan in plans {
            super::cost::set_plan(platform, plan);
            println!(
                "  {:>6} {:>8} → prior_scale = {:.4} %/美元",
                platform.as_str(),
                plan,
                super::cost::prior_scale(platform)
            );
        }
    }

    // ---- 零回归：本机当前套餐下,预设与拟合值逐位不变 ----
    println!("--- 本机当前套餐 ---");
    for platform in [Platform::Claude, Platform::Codex] {
        // 先把进程全局清成「没设过」,拿到不折算的基准值
        super::cost::set_plan(platform, "");
        super::cost::set_plan_tier(platform, "");
        let base = super::cost::prior_scale(platform);
        // 再按库里快照说的套餐设一遍（真机路径:store 读写快照时自动做这一步）
        let plan = s.load_snapshot(platform).map(|x| x.plan_type).unwrap_or_default();
        super::cost::set_plan(platform, &plan);
        if platform == Platform::Claude {
            // 真机取数时由 claude.rs 从凭据带进来
            super::cost::set_plan_tier(platform, "default_claude_max_5x");
        }
        let now = super::cost::prior_scale(platform);
        super::calib::refit_from_store(&s, platform);
        println!(
            "  {:>6} plan={:<8} prior {base:.4} → {now:.4}（×{:.3}）, 拟合后 scale={:.5} n={}",
            platform.as_str(),
            plan,
            now / base,
            super::calib::scale(platform),
            super::calib::sample_count(platform)
        );
        assert_eq!(now, base, "本机这两档都是基准档,预设必须逐位不变");
    }

    // ---- 套餐边界：真库里缺席 ⇒ 标注一行不变 ----
    let before = generations(&s);
    println!("--- 套餐边界 ---");
    println!("  plan_since(claude) = {:?}（None = 没换过档,按旧行为整批标当前套餐）",
        s.plan_since(Platform::Claude));
    let Some(collector) = real_collector_db() else {
        eprintln!("没有找到 collector.db,跳过后半段（设 TC_COLLECTOR_DB=<路径>）");
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let (h, p) = super::bootstrap::ingest(&s, &collector, now);
    println!("  一轮 ingest：收割 {h} 条读数 / 新建 {p} 条样本");
    let after = generations(&s);
    let unknown_before: i64 =
        before.iter().filter(|(pl, _, plan, _)| pl == "claude" && plan.is_empty()).map(|r| r.3).sum();
    let unknown_after: i64 =
        after.iter().filter(|(pl, _, plan, _)| pl == "claude" && plan.is_empty()).map(|r| r.3).sum();
    println!("  claude 标「套餐未知」的行数：{unknown_before} → {unknown_after}");
    assert_eq!(unknown_before, unknown_after, "没有边界时不该有新的「套餐未知」行");

    // ---- 冷启动次序：全新安装的第一轮 ----
    println!("--- 冷启动次序（全新安装模拟）---");
    let fresh = SubStore::open(&out.join("fresh.db")).expect("fresh store");
    super::price::load_from(&fresh);
    let not_yet = out.join("collector-not-written-yet.db");
    let (h1, p1) = super::bootstrap::ingest(&fresh, &not_yet, now);
    println!("  第一轮（collector 首扫还没写盘）：收割 {h1} 条读数 / 新建 {p1} 条样本");
    assert!(h1 > 0, "读数照样收割得到——它只读桌面端自己的历史文件");
    assert_eq!(p1, 0, "没有本地轮记录就建不出样本,这正是那个次序问题");
    assert_eq!(
        fresh.latest_pair_t1(Platform::Claude, super::bootstrap::PAIR_SRC),
        None,
        "水位线仍是 None ⇒ mod.rs 的闸门保持冷启动态,跟着 collector 写盘重试"
    );
    let (h2, p2) = super::bootstrap::ingest(&fresh, &collector, now);
    println!("  第二轮（首扫写完了）：收割 {h2} 条读数 / 新建 {p2} 条样本");
    assert_eq!(h2, 0, "同一批读数不重复收割");
    assert!(p2 > 0, "首扫一写完就该补得上——修的就是这一格");
    let fresh_gen = generations(&fresh);
    println!("  全新安装建出来的样本分档：{fresh_gen:?}");
    assert!(
        fresh_gen.iter().all(|(pl, _, plan, _)| pl != "claude" || plan.is_empty()),
        "全新安装还没取过数 ⇒ 套餐未知,不能硬套成某一档"
    );

    // ---- 的**差分情形**：快照里套餐已知,而回补区间比边界更早 ----
    // 上一步的全新安装里 Claude 还没绑定 ⇒ 快照为空 ⇒ 不管改不改都标空串,验不出差别。
    // 这里补上：给副本写一条 Claude 快照,取数时刻**故意放在两天前**,于是这批回补区间
    // 一半落在边界之前、一半之后 —— 之前的必须标空串,之后的才标真套餐。
    println!("--- 套餐边界的差分情形 ---");
    let boundary = now - 2 * 86_400;
    fresh
        .save_snapshot(&super::model::SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "max".into(),
            windows: vec![super::model::QuotaWindow {
                kind: "5h".into(),
                used_percent: 1.0,
                resets_at: None,
            }],
            fetched_at: Some(boundary),
            status: super::model::FetchStatus::Ok,
            source: super::model::SnapshotSource::Api,
        })
        .expect("save snapshot");
    assert_eq!(fresh.plan_since(Platform::Claude), Some(boundary), "从无到有 ⇒ 边界落在这一刻");
    // 判据 meta 倒拨一格 ⇒ 这一路把「读数还在的那一段」整段重建,重走一遍标注
    let _ = fresh
        .set_meta_i64("claude_desktop_pair_rule", super::calib::ADMISSION_RULE_VERSION - 1);
    let (_, p3) = super::bootstrap::ingest(&fresh, &collector, now);
    println!("  按边界重建：{p3} 条样本");
    let split = generations(&fresh);
    println!("  重建后的样本分档：{split:?}");
    let count = |plan: &str| -> i64 {
        split.iter().filter(|(pl, _, pt, _)| pl == "claude" && pt == plan).map(|r| r.3).sum()
    };
    println!("  claude: 套餐未知 {} 条 / 标 max {} 条", count(""), count("max"));
    assert!(count("") > 0, "边界之前的区间必须标「套餐未知」——这正是 ② 要修的");
    assert!(count("max") > 0, "边界之后的区间照常标真套餐");
    let earliest_max: Option<i64> = fresh
        .conn_for_smoke()
        .query_row(
            "SELECT MIN(t0) FROM usage_pair WHERE platform='claude' AND plan_type='max'",
            [],
            |r| r.get(0),
        )
        .ok();
    assert!(
        earliest_max.is_some_and(|t| t >= boundary),
        "标了真套餐的区间一条都不能比边界早"
    );
}

/// **一个 5h 窗口值多少钱——按套餐分开直接数**（不依赖任何回归）。
///
/// 本机登录了两个 Codex 订阅账号（edu / plus），用完一个就切另一个。两个账号
/// **各有各的 5h 窗口**，而 rollout 把它们的读数写在同一条时间线上，只能靠 `plan_type`
/// 区分。于是「窗口值多少钱」必须**按套餐分开量**，否则量到的是两个窗口的混合。
///
/// 做法：**同一个 `resets5` 就是同一个窗口实例**。
/// 把某套餐的读数按窗尾分组，每组取首尾算涨幅、把区间内的调用折成美元；
/// 组内若夹着**另一个套餐的读数**（= 那段时间人在另一个账号上）就判为污染、整组不要
/// ——那段涨幅是本账号的，代价却混进了别人的。
#[test]
#[ignore]
fn codex_window_dollars_by_plan_on_real_db() {
    /// 只看涨幅够大的窗口实例：读数是整数百分比，涨幅小的实例相对量化误差太大。
    const MIN_CLIMB_PCT: f64 = 50.0;

    let now = chrono::Utc::now().timestamp();
    let scan = super::codex_rollout::scan(now - 120 * 86_400, 0, &Default::default());
    println!(
        "扫到 {} 条读数 / {} 次调用（{}/{} 个文件, {:.1} MB）",
        scan.readings.len(),
        scan.calls.len(),
        scan.files_read,
        scan.files_total,
        scan.bytes_read as f64 / 1e6
    );
    // 价目要按库里的（与标定同一把尺子）
    if let Some(src) = real_db() {
        let out = std::env::var_os("TC_SMOKE_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("tc_window_dollars");
        let _ = std::fs::remove_dir_all(&out);
        super::price::load_from(&SubStore::open(&clone_db(&src, &out)).expect("open"));
    }

    let plans: std::collections::BTreeSet<String> = scan
        .readings
        .iter()
        .filter(|r| !r.plan.is_empty())
        .map(|r| r.plan.clone())
        .collect();
    println!("读数里出现过的套餐：{plans:?}");

    // ---- 对照：在**混合**时间线上找「used5 从 ≤2% 爬到 ≥95%」的连续段。
    // 两个账号各有各的窗口却写在同一条线上,段首段尾可能分属两个账号 ⇒ 量到的不是任何一个窗口。
    {
        let cost_between = |t0: i64, t1: i64| -> f64 {
            let mut b: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
            for c in scan.calls.iter().filter(|c| c.t > t0 && c.t <= t1) {
                let slot = b.entry(c.model.clone()).or_insert([0; 4]);
                for (k, v) in c.tokens.iter().enumerate() {
                    slot[k] += v;
                }
            }
            super::cost::cost_of_breakdown(Platform::Codex, &b, t1).0
        };
        let mut segs: Vec<(f64, bool)> = vec![]; // (满窗美元, 是否跨账号)
        let mut start: Option<usize> = None;
        for (i, r) in scan.readings.iter().enumerate() {
            match start {
                None if r.used5 <= 2.0 => start = Some(i),
                Some(s0) if r.used5 >= 95.0 => {
                    let (a, b) = (&scan.readings[s0], r);
                    let cost = cost_between(a.t, b.t);
                    let mixed = scan.readings[s0..=i]
                        .iter()
                        .filter(|x| !x.plan.is_empty())
                        .any(|x| x.plan != a.plan);
                    if cost > 0.0 {
                        segs.push((cost / (b.used5 - a.used5) * 100.0, mixed));
                    }
                    start = None;
                }
                Some(_) if r.used5 <= 2.0 => start = Some(i),
                _ => {}
            }
        }
        let mut all: Vec<f64> = segs.iter().map(|(c, _)| *c).collect();
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mixed = segs.iter().filter(|(_, m)| *m).count();
        if !all.is_empty() {
            println!(
                "--- 对照:旧办法(混合时间线上的 2%→95% 连续段) --- {} 段,其中 {} 段跨账号
                       满 5h 窗中位 ${:.2} ⇒ scale = {:.2} %/美元",
                all.len(),
                mixed,
                all[all.len() / 2],
                100.0 / all[all.len() / 2]
            );
        }
    }

    println!("--- 按套餐分开:同一个 resets5 = 同一个窗口实例 ---");
    for plan in &plans {
        let mine: Vec<&super::codex_rollout::Reading> =
            scan.readings.iter().filter(|r| &r.plan == plan && r.resets5.is_some()).collect();
        // 按窗尾分组 = 按窗口实例分组（同一个窗口里窗尾不变）
        let mut groups: Vec<(usize, usize)> = vec![];
        let mut i = 0;
        while i < mine.len() {
            let mut j = i;
            while j + 1 < mine.len() && mine[j + 1].resets5 == mine[i].resets5 {
                j += 1;
            }
            groups.push((i, j));
            i = j + 1;
        }
        let mut kept: Vec<(f64, f64)> = vec![]; // (涨幅 %, 代价 $)
        let (mut too_small, mut polluted) = (0usize, 0usize);
        for (a, b) in groups {
            let (r0, r1) = (mine[a], mine[b]);
            let climb = r1.used5 - r0.used5;
            if climb < MIN_CLIMB_PCT {
                too_small += 1;
                continue;
            }
            // 组内夹着别的套餐的读数 = 这段时间人在另一个账号上 ⇒ 代价不全是本账号的
            if scan
                .readings
                .iter()
                .any(|r| !r.plan.is_empty() && &r.plan != plan && r.t > r0.t && r.t < r1.t)
            {
                polluted += 1;
                continue;
            }
            let mut breakdown: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
            for c in scan.calls.iter().filter(|c| c.t > r0.t && c.t <= r1.t) {
                let slot = breakdown.entry(c.model.clone()).or_insert([0; 4]);
                for (k, v) in c.tokens.iter().enumerate() {
                    slot[k] += v;
                }
            }
            let (cost, _) = super::cost::cost_of_breakdown(Platform::Codex, &breakdown, r1.t);
            if cost > 0.0 {
                kept.push((climb, cost));
            }
        }
        if kept.is_empty() {
            println!("  {plan:<5} 没有可用的窗口实例（涨幅够小的 {too_small} 个 / 被另一账号污染的 {polluted} 个）");
            continue;
        }
        // 满窗美元 = 代价 / 涨幅 × 100;取中位数（均值会被个别长尾拉走）
        let mut full: Vec<f64> = kept.iter().map(|(d, c)| c / d * 100.0).collect();
        full.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = full[full.len() / 2];
        println!(
            "  {plan:<5} 可用窗口实例 {:>2} 个（涨幅小的丢 {too_small} / 被另一账号污染丢 {polluted}）             
        满 5h 窗中位 ${:.2}（{:.2}〜{:.2}）⇒ 直接测量 scale = {:.2} %/美元",
            kept.len(),
            median,
            full[0],
            full[full.len() - 1],
            100.0 / median
        );
    }
}

/// **按套餐筛对标定的影响**：同一批行，只切换 `pairs_for_fit` 的套餐参数。
///
/// 本机登录了两个 Codex 账号（edu / plus），用完一个切另一个，而 `pairs_for_fit` 按
/// **当前快照的套餐**筛 ⇒ 生效的系数会跟着「上一次用的是哪个账号」跳。这一条把跳幅
/// 量出来，与 `codex_window_dollars_by_plan_on_real_db` 的直接测量对读：两个账号的窗口
/// 若一样大，那这个跳幅就是**纯粹的抽样噪声**，不是真实差异。
#[test]
#[ignore]
fn codex_fit_by_plan_on_real_db() {
    let Some(src) = real_db() else {
        eprintln!("没有找到真库,跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_acct_switch_probe");
    let _ = std::fs::remove_dir_all(&out);
    let dst = clone_db(&src, &out);
    let s = SubStore::open(&dst).expect("open");
    super::price::load_from(&s);
    let _ = s.recompute_stale_costs(Platform::Codex);

    println!("--- 同一批行,只切换 pairs_for_fit 的套餐参数 ---");
    for plan in ["edu", "plus", ""] {
        let pairs = s.pairs_for_fit(Platform::Codex, plan);
        let (scale, n) = super::calib::fit(Platform::Codex, &pairs);
        let cost: f64 = pairs.iter().map(|p| p.cost).sum();
        println!(
            "  plan={:<5} 取到 {:>4} 行（总代价 ${:>8.2}）→ scale={:.5} %/美元, 参与拟合 n={}",
            if plan.is_empty() { "(全)" } else { plan },
            pairs.len(),
            cost,
            scale,
            n
        );
    }
}

/// **抽样偏差：标定用的区间 vs 全部区间**（按套餐分开）。
///
/// 回归只吃「端点链上、两端同套餐、没跨重置、`usable` 放行」的区间；直接测量吃的是
/// 整个窗口。两者的差来自代价与涨幅的时序错位——这里把它拆成
/// 三层，同一批读数、同一把价目，只改纳入范围：
///
/// - **全部**：端点链上两端同套餐、没跨重置的区间（物理上有意义的全体）；
/// - **≤30 分钟**：再加上 `MAX_PAIR_SECS` 这道闸；
/// - **usable**：再加上比值可信带与 Δ=0 的准入（= 真正进拟合的那批）。
#[test]
#[ignore]
fn codex_pair_sampling_bias_by_plan_on_real_db() {
    let now = chrono::Utc::now().timestamp();
    let scan = super::codex_rollout::scan(now - 120 * 86_400, 0, &Default::default());
    if let Some(src) = real_db() {
        let out = std::env::var_os("TC_SMOKE_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("tc_sampling_bias");
        let _ = std::fs::remove_dir_all(&out);
        super::price::load_from(&SubStore::open(&clone_db(&src, &out)).expect("open"));
    }

    // 端点链与 build_pairs 完全一致（相邻端点至少隔 MIN_PAIR_SECS，跳过的读数成为内点）
    let mut ends: Vec<&super::codex_rollout::Reading> = vec![];
    for r in &scan.readings {
        match ends.last() {
            None => ends.push(r),
            Some(prev) if r.t - prev.t >= super::calib::MIN_PAIR_SECS => ends.push(r),
            _ => {}
        }
    }
    println!("端点 {} 个（读数 {} 条）", ends.len(), scan.readings.len());

    let plans: std::collections::BTreeSet<String> =
        scan.readings.iter().filter(|r| !r.plan.is_empty()).map(|r| r.plan.clone()).collect();
    let mut cur = 0usize;
    // （plan, dt, delta, cost) —— 一次遍历把三层要的东西都算出来
    let mut rows: Vec<(String, i64, f64, f64, bool)> = vec![];
    for w in ends.windows(2) {
        let (a, b) = (w[0], w[1]);
        while cur < scan.calls.len() && scan.calls[cur].t <= a.t {
            cur += 1;
        }
        let mut breakdown: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
        let mut k = cur;
        while k < scan.calls.len() && scan.calls[k].t <= b.t {
            let slot = breakdown.entry(scan.calls[k].model.clone()).or_insert([0; 4]);
            for (i, v) in scan.calls[k].tokens.iter().enumerate() {
                slot[i] += v;
            }
            k += 1;
        }
        if a.plan != b.plan || a.plan.is_empty() || breakdown.is_empty() {
            continue;
        }
        let (cost, unknown) = super::cost::cost_of_breakdown(Platform::Codex, &breakdown, b.t);
        if cost <= 0.0 {
            continue;
        }
        let pair = super::calib::Pair {
            t0: a.t,
            t1: b.t,
            used5_0: a.used5,
            used5_1: b.used5,
            resets5_0: a.resets5,
            resets5_1: b.resets5,
            cost,
            unknown_cost: unknown,
            aged_cost: 0.0,
        };
        // 跨重置 / 换账号的区间两端不在同一个窗口里,Δ 没有意义——这是物理,不是筛子。
        // 判据与线上同一段代码（`Pair:window_changed`）。
        if pair.window_changed() {
            continue;
        }
        rows.push((
            a.plan.clone(),
            b.t - a.t,
            b.used5 - a.used5,
            cost,
            pair.usable(super::calib::scale(Platform::Codex)),
        ));
    }

    for plan in &plans {
        println!("--- {plan} ---");
        let mine: Vec<&(String, i64, f64, f64, bool)> = rows.iter().filter(|r| &r.0 == plan).collect();
        let report = |label: &str, sel: &dyn Fn(&&(String, i64, f64, f64, bool)) -> bool| {
            let v: Vec<_> = mine.iter().filter(|r| sel(r)).collect();
            let d: f64 = v.iter().map(|r| r.2).sum();
            let c: f64 = v.iter().map(|r| r.3).sum();
            println!(
                "  {label:<14} {:>5} 段, Σ涨幅 {:>7.0} 点 / Σ代价 ${:>8.2} ⇒ 比值 {:.2} %/美元",
                v.len(),
                d,
                c,
                if c > 0.0 { d / c } else { 0.0 }
            );
        };
        report("全部", &|_| true);
        report("≤30 分钟", &|r| r.1 <= super::calib::MAX_PAIR_SECS);
        report("usable()", &|r| r.4);
    }
}

/// **账号纪元**：真机的 `~/.codex/auth.json` 能不能给出账号指纹，
/// 以及 `note_account` 的「变了才推进边界」语义。**不打印指纹原值也不打印账号 id**。
#[test]
#[ignore]
fn codex_account_fingerprint_from_real_auth_json() {
    let Some(fp) = super::credentials::read_credential(Platform::Codex)
        .and_then(|c| c.account_fp)
    else {
        eprintln!("本机 ~/.codex/auth.json 取不到 tokens.account_id，跳过");
        return;
    };
    println!("真机 auth.json 给出了账号指纹（{} 位十六进制）", fp.len());
    assert_eq!(fp.len(), 16, "FNV-1a 64 位 → 16 位十六进制");

    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_account_epoch");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("mkdir");
    let s = SubStore::open(&out.join("subscriptions.db")).expect("open");
    assert_eq!(s.account_since(Platform::Codex), None, "没记过 → 没有边界");
    s.note_account(Platform::Codex, &fp, 1_000);
    assert_eq!(s.account_since(Platform::Codex), Some(1_000), "从无到有 ⇒ 边界落在这一刻");
    s.note_account(Platform::Codex, &fp, 2_000);
    assert_eq!(s.account_since(Platform::Codex), Some(1_000), "同一个账号不动边界");
    s.note_account(Platform::Codex, "0000000000000000", 3_000);
    assert_eq!(s.account_since(Platform::Codex), Some(3_000), "换了账号 ⇒ 边界前移");
    s.note_account(Platform::Codex, "", 4_000);
    assert_eq!(s.account_since(Platform::Codex), Some(3_000), "空指纹什么都不做");
    assert_eq!(s.account_since(Platform::Claude), None, "按平台分开");
    println!("note_account 的边界语义通过（从无到有 / 不变 / 换账号 / 空值）");
}

/// **同套餐双账号的结构性痕迹**（本机历史上有过两个 plus 账号并用的时期）。
///
/// rollout 不带任何账号字段，但**窗尾骗不了人**：一个账号的 `resets_at` 在窗口内不变、
/// 窗口滚动时向前跳，**永远不会往回走**（空窗时它 ≈ `now + 窗长`，仍是向前）。所以
/// 同一个 `plan_type` 的相邻读数里出现**窗尾后退**，只能是换了另一个账号。
///
/// 这一条只扫 rollout 读数，按套餐、按日期数出四类痕迹：窗尾不变而用量下跌、一天内过多的
/// 7d 窗尾、窗尾乒乓、后退幅度分档。结论已落在线上：后退超过 `calib:TAIL_DRIFT_SLACK_SECS`
/// 的区间由 `Pair:window_changed` 判为换账号丢弃，账号指纹纪元见 `store:account_since`。
#[test]
#[ignore]
fn codex_same_plan_two_accounts_trace() {
    let now = chrono::Utc::now().timestamp();
    let scan = super::codex_rollout::scan(now - 200 * 86_400, 0, &Default::default());
    println!(
        "扫到 {} 条读数（{}/{} 个文件）",
        scan.readings.len(),
        scan.files_read,
        scan.files_total
    );
    let day = |t: i64| {
        chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default()
    };

    let plans: std::collections::BTreeSet<String> =
        scan.readings.iter().filter(|r| !r.plan.is_empty()).map(|r| r.plan.clone()).collect();
    for plan in &plans {
        let mine: Vec<&super::codex_rollout::Reading> =
            scan.readings.iter().filter(|r| &r.plan == plan).collect();
        let span = match (mine.first(), mine.last()) {
            (Some(a), Some(b)) => format!("{} 〜 {}", day(a.t), day(b.t)),
            _ => "—".into(),
        };
        println!("--- {plan}：{} 条读数，{span} ---", mine.len());
        // ⓪-pre **覆盖度**：没有痕迹和没有数据是两回事。先把每天的读数条数摊开。
        {
            let mut by_day: std::collections::BTreeMap<String, usize> = Default::default();
            for r in &mine {
                *by_day.entry(day(r.t)).or_insert(0) += 1;
            }
            println!(
                "  有读数的天数 {}，逐日条数：{}",
                by_day.len(),
                by_day.iter().map(|(d, n)| format!("{}×{n}", &d[5..])).collect::<Vec<_>>().join(" ")
            );
        }
        // ⓪ **最硬的一条**：窗尾一模一样（= 同一个窗口实例）而已用百分比却掉了。
        //    一个账号在同一个窗口里用量只增不减,掉下去只可能是**另一个账号的读数插了进来**
        //    ——所以这里数的是「窗尾不变、用量下跌」这个组合本身。
        {
            let mut drops: Vec<(i64, f64, f64)> = vec![];
            for w in mine.windows(2) {
                let (a, b) = (w[0], w[1]);
                if a.resets5.is_some() && a.resets5 == b.resets5 && b.used5 < a.used5 - 0.5 {
                    drops.push((b.t, a.used5, b.used5));
                }
            }
            let mut by_day: std::collections::BTreeMap<String, usize> = Default::default();
            for (t, _, _) in &drops {
                *by_day.entry(day(*t)).or_insert(0) += 1;
            }
            let big = drops.iter().filter(|(_, a, b)| a - b >= 20.0).count();
            println!(
                "  **窗尾不变而用量下跌** {} 次（其中跌 ≥20 点的 {} 次）{}",
                drops.len(),
                big,
                if by_day.is_empty() { String::new() } else { format!("，跨 {} 天", by_day.len()) }
            );
            if !by_day.is_empty() {
                println!(
                    "      {}",
                    by_day.iter().map(|(d, n)| format!("{d}×{n}")).collect::<Vec<_>>().join("  ")
                );
                let (t, a, b) = drops[0];
                println!("      最早一次：{} {:.0}% → {:.0}%", day(t), a, b);
                for (t, a, b) in drops.iter().filter(|(_, a, b)| a - b >= 20.0) {
                    println!("      跌 ≥20 点：{} {:.0}% → {:.0}%", day(*t), a, b);
                }
            }
        }
        // -bis 每天出现过几个不同的 7d 窗尾（一个账号一天至多 2 个：至多滚一次）
        {
            let mut by_day: std::collections::BTreeMap<String, std::collections::BTreeSet<i64>> =
                Default::default();
            for r in &mine {
                if let Some(v) = r.resets7 {
                    // 7d 窗尾按小时归一,吸收空窗时那点漂移
                    by_day.entry(day(r.t)).or_default().insert(v / 3_600);
                }
            }
            let many: Vec<String> = by_day
                .iter()
                .filter(|(_, v)| v.len() > 2)
                .map(|(d, v)| format!("{d}×{}", v.len()))
                .collect();
            println!(
                "  每天不同的 7d 窗尾（>2 个 = 一天滚不了那么多次）：{} 天超标{}",
                many.len(),
                if many.is_empty() { String::new() } else { format!("  {}", many.join("  ")) }
            );
        }
        //  乒乓：窗尾回到**之前已经出现过**的某个值。一个账号的窗尾只会向前推,
        //    回到旧值只能是两条独立的窗口序列在交替 ⇒ 两个账号。
        for (label, pick) in [("5h 窗尾", 0usize), ("7d 窗尾", 1usize)] {
            let get = |r: &super::codex_rollout::Reading| if pick == 0 { r.resets5 } else { r.resets7 };
            let mut seen: std::collections::BTreeSet<i64> = Default::default();
            let mut last: Option<i64> = None;
            let mut pong: std::collections::BTreeMap<String, usize> = Default::default();
            for r in &mine {
                let Some(v) = get(r) else { continue };
                if last != Some(v) {
                    if seen.contains(&v) {
                        *pong.entry(day(r.t)).or_insert(0) += 1;
                    }
                    seen.insert(v);
                    last = Some(v);
                }
            }
            let total: usize = pong.values().sum();
            println!(
                "  {label}乒乓 {total} 次（回到已出现过的窗尾）{}",
                if pong.is_empty() { String::new() } else { format!("，跨 {} 天", pong.len()) }
            );
            if !pong.is_empty() {
                println!(
                    "      {}",
                    pong.iter().map(|(d, n)| format!("{d}×{n}")).collect::<Vec<_>>().join("  ")
                );
            }
        }
        //  后退幅度分档：一个账号内部的后退只会是「空窗时报 now+窗长、开始用了才snap 回
        //    真窗首」那一种,幅度小于窗长;跨账号的后退可以是任意幅度。
        for (label, pick, buckets) in [
            ("5h 窗尾", 0usize, [600i64, 3_600, 3 * 3_600, 5 * 3_600].as_slice()),
            ("7d 窗尾", 1usize, [3_600, 86_400, 3 * 86_400, 7 * 86_400].as_slice()),
        ] {
            let get = |r: &super::codex_rollout::Reading| if pick == 0 { r.resets5 } else { r.resets7 };
            let mut back: Vec<(i64, i64, i64)> = vec![]; // (时刻, 前一个窗尾, 这个窗尾)
            for w in mine.windows(2) {
                if let (Some(a), Some(b)) = (get(w[0]), get(w[1])) {
                    if b < a {
                        back.push((w[1].t, a, b));
                    }
                }
            }
            let mut by_day: std::collections::BTreeMap<String, usize> = Default::default();
            for (t, _, _) in &back {
                *by_day.entry(day(*t)).or_insert(0) += 1;
            }
            let mut hist = vec![0usize; buckets.len() + 1];
            for (_, a, b) in &back {
                let d = a - b;
                let i = buckets.iter().position(|x| d <= *x).unwrap_or(buckets.len());
                hist[i] += 1;
            }
            let big: Vec<&(i64, i64, i64)> =
                back.iter().filter(|(_, a, b)| a - b > buckets[1]).collect();
            println!(
                "  {label}后退 {} 次，幅度分档 {:?}（档位 {:?} 秒）；**超过 {} 秒的 {} 次**",
                back.len(),
                hist,
                buckets,
                buckets[1],
                big.len()
            );
            if !big.is_empty() {
                let mut by_day2: std::collections::BTreeMap<String, usize> = Default::default();
                for (t, _, _) in &big {
                    *by_day2.entry(day(*t)).or_insert(0) += 1;
                }
                println!(
                    "      大幅后退按日：{}",
                    by_day2.iter().map(|(d, n)| format!("{d}×{n}")).collect::<Vec<_>>().join("  ")
                );
                let (t, a, b) = big[0];
                println!(
                    "      最早一次：{} 窗尾 {} → {}（往回 {:.1} 小时）",
                    day(*t),
                    day(*a),
                    day(*b),
                    (a - b) as f64 / 3600.0
                );
            }
            let _ = &by_day;
        }
    }
}

/// **拆抽样偏差：被扔掉的是哪一类区间**。
///
/// 同一批读数、同一把价目、同一段判据代码（`Pair:reject_reason`），把端点链上每个
/// 区间按**第一条触发的判据**归档，并按套餐分开。三张表：
///
/// - 按原因：段数 / Σ代价 / Σ涨幅 / 比值 —— 看钱被哪一类吃掉；
/// - 按窗口水位（`used5_0` 分档）：看是不是「窗口见顶之后还在花钱」；
/// - 按模型：看被扔掉的代价集中在哪些模型上。
#[test]
#[ignore]
fn codex_rejected_intervals_by_reason_on_real_db() {
    let now = chrono::Utc::now().timestamp();
    let scan = super::codex_rollout::scan(now - 120 * 86_400, 0, &Default::default());
    if let Some(src) = real_db() {
        let out = std::env::var_os("TC_SMOKE_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("tc_reject_reasons");
        let _ = std::fs::remove_dir_all(&out);
        let s = SubStore::open(&clone_db(&src, &out)).expect("open");
        super::price::load_from(&s);
        super::calib::refit_from_store(&s, Platform::Codex);
    }
    let center = super::calib::scale(Platform::Codex);
    println!("参照系数 center = {center:.5} %/美元");

    let mut ends: Vec<&super::codex_rollout::Reading> = vec![];
    for r in &scan.readings {
        match ends.last() {
            None => ends.push(r),
            Some(prev) if r.t - prev.t >= super::calib::MIN_PAIR_SECS => ends.push(r),
            _ => {}
        }
    }
    // （plan, 原因, used5_0, 代价, 涨幅, 分模型代价)
    type Row = (String, &'static str, f64, f64, f64, std::collections::BTreeMap<String, f64>);
    let mut rows: Vec<Row> = vec![];
    // （plan, 原因, t0, dt, used5_0, used5_1, 有效代价) —— 逐条看最贵的那些
    let mut dump: Vec<(String, &'static str, i64, i64, f64, f64, f64)> = vec![];
    let mut cur = 0usize;
    for w in ends.windows(2) {
        let (a, b) = (w[0], w[1]);
        while cur < scan.calls.len() && scan.calls[cur].t <= a.t {
            cur += 1;
        }
        let mut breakdown: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
        let mut k = cur;
        while k < scan.calls.len() && scan.calls[k].t <= b.t {
            let slot = breakdown.entry(scan.calls[k].model.clone()).or_insert([0; 4]);
            for (i, v) in scan.calls[k].tokens.iter().enumerate() {
                slot[i] += v;
            }
            k += 1;
        }
        if a.plan != b.plan || a.plan.is_empty() || breakdown.is_empty() {
            continue;
        }
        let (cost, unknown) = super::cost::cost_of_breakdown(Platform::Codex, &breakdown, b.t);
        let per_model: std::collections::BTreeMap<String, f64> = breakdown
            .iter()
            .map(|(m, t)| {
                let one = std::collections::BTreeMap::from([(m.clone(), *t)]);
                (m.clone(), super::cost::cost_of_breakdown(Platform::Codex, &one, b.t).0)
            })
            .collect();
        // 老化量：发生在 （t0−5h, t1−5h] 的调用（与 codex_rollout:build_pairs 同口径）
        let aged = {
            let (lo, hi) = (a.t - 5 * 3_600, b.t - 5 * 3_600);
            let i = scan.calls.partition_point(|c| c.t <= lo);
            let j = scan.calls.partition_point(|c| c.t <= hi);
            let mut ob: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
            for c in &scan.calls[i..j] {
                let slot = ob.entry(c.model.clone()).or_insert([0; 4]);
                for (x, v) in c.tokens.iter().enumerate() {
                    slot[x] += v;
                }
            }
            super::cost::cost_of_breakdown(Platform::Codex, &ob, hi).0
        };
        let pair = super::calib::Pair {
            t0: a.t,
            t1: b.t,
            used5_0: a.used5,
            used5_1: b.used5,
            resets5_0: a.resets5,
            resets5_1: b.resets5,
            cost,
            unknown_cost: unknown,
            aged_cost: aged,
        };
        rows.push((
            a.plan.clone(),
            pair.reject_reason(center).unwrap_or("**采纳**"),
            a.used5,
            pair.effective_cost().max(0.0),
            b.used5 - a.used5,
            per_model,
        ));
        dump.push((
            a.plan.clone(),
            pair.reject_reason(center).unwrap_or("**采纳**"),
            a.t,
            b.t - a.t,
            a.used5,
            b.used5,
            pair.effective_cost(),
        ));
    }

    for plan in ["edu", "plus"] {
        let mine: Vec<&Row> = rows.iter().filter(|r| r.0 == plan).collect();
        let total_cost: f64 = mine.iter().map(|r| r.3).sum();
        println!("\n=== {plan}：{} 段，Σ代价 ${total_cost:.2} ===", mine.len());
        //  按原因
        let mut by: std::collections::BTreeMap<&str, (usize, f64, f64)> = Default::default();
        for r in &mine {
            let e = by.entry(r.1).or_insert((0, 0.0, 0.0));
            e.0 += 1;
            e.1 += r.3;
            e.2 += r.4;
        }
        let mut v: Vec<_> = by.into_iter().collect();
        v.sort_by(|a, b| b.1 .1.partial_cmp(&a.1 .1).unwrap());
        println!("  {:<16} {:>6} {:>11} {:>9} {:>9}", "原因", "段数", "Σ代价", "Σ涨幅", "比值");
        for (why, (n, c, d)) in &v {
            println!(
                "  {why:<16} {n:>6} {:>10.2}$ {:>8.0}点 {:>8.2}  （占代价 {:>4.1}%）",
                c,
                d,
                if *c > 0.0 { d / c } else { 0.0 },
                c / total_cost * 100.0
            );
        }
        // -bis 最贵的被拒区间逐条
        let mut worst: Vec<&(String, &str, i64, i64, f64, f64, f64)> =
            dump.iter().filter(|d| d.0 == plan && d.1 != "**采纳**" && d.1 != "间隔过长").collect();
        worst.sort_by(|a, b| b.6.partial_cmp(&a.6).unwrap());
        println!("  —— 最贵的被拒区间（不含「间隔过长」）——");
        for d in worst.iter().take(6) {
            println!(
                "    ${:>6.2}  {:<16} {}  dt={:>5}s  used5 {:>5.1} → {:>5.1}",
                d.6,
                d.1,
                chrono::DateTime::from_timestamp(d.2, 0)
                    .map(|x| x.format("%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default(),
                d.3,
                d.4,
                d.5
            );
        }
        //  被扔掉的区间按窗口水位分档
        println!("  —— 被扔掉的区间按**区间起点的窗口水位** ——");
        for (lo, hi) in [(0.0, 50.0), (50.0, 90.0), (90.0, 99.0), (99.0, 101.0)] {
            let sel: Vec<&&Row> = mine
                .iter()
                .filter(|r| r.1 != "**采纳**" && r.2 >= lo && r.2 < hi)
                .collect();
            let kept: Vec<&&Row> =
                mine.iter().filter(|r| r.1 == "**采纳**" && r.2 >= lo && r.2 < hi).collect();
            println!(
                "    used5_0 {lo:>3.0}〜{hi:<3.0}  丢 {:>4} 段 / ${:>7.2}   留 {:>4} 段 / ${:>7.2}",
                sel.len(),
                sel.iter().map(|r| r.3).sum::<f64>(),
                kept.len(),
                kept.iter().map(|r| r.3).sum::<f64>()
            );
        }
        //  被扔掉的代价按模型
        let mut drop_m: std::collections::BTreeMap<String, f64> = Default::default();
        let mut keep_m: std::collections::BTreeMap<String, f64> = Default::default();
        for r in &mine {
            let t = if r.1 == "**采纳**" { &mut keep_m } else { &mut drop_m };
            for (m, c) in &r.5 {
                *t.entry(m.clone()).or_insert(0.0) += c;
            }
        }
        let mut dm: Vec<_> = drop_m.iter().collect();
        dm.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        println!("  —— 被扔掉的代价按模型（前 6）——");
        for (m, c) in dm.iter().take(6) {
            let kept = keep_m.get(*m).copied().unwrap_or(0.0);
            println!(
                "    {m:<22} 丢 ${:>7.2}  留 ${:>7.2}  （丢掉 {:>4.1}%）",
                c,
                kept,
                *c / (*c + kept).max(1e-9) * 100.0
            );
        }
    }
}

/// **量缓存读在额度口径里的系数**。
///
/// 存在按官方价目折出大额「等价用量」、5h 计数器却一个点都没动的区间,而这类区间
/// 以缓存读为主 ⇒ 要么缓存读在额度口径里比在价目口径里便宜得多,要么是计数器滞后。
///
/// 把每个区间的有效代价拆成两半做一次**过原点的两元回归**：
///
/// ```text
///   Δ用量% = a × 非缓存代价（输入 + 输出 + 缓存写） + b × 缓存读代价
/// ```
///
/// - `b ≈ 0` ⇒ 缓存读在额度口径里不计（或几乎不计），`cost.rs` 要分出
///   **价目口径**与**额度口径**两把尺子。
/// - `b ≈ a` ⇒ 假设不成立，那几个异常区间是**计数器滞后**而不是缓存便宜。
///
/// 这条测量能把两种解释分开：**滞后不会与缓存占比相关，缓存便宜会**。
/// **只量，不改生效逻辑。**
///
/// 三层证据，互相独立：
/// 1. 过原点两元 OLS + 标准误（`b` 与 0、与 `a` 各差几个标准误）；
/// 2. 按**缓存读占代价的比例**分档，看隐含比值 `ΣΔ / Σ有效代价` 随占比怎么走
///    ——这一层不依赖任何回归假设；
/// 3. 单元回归对照：只用非缓存代价、与只用总代价，看残差平方和差多少。
///
/// 两套样本各跑一遍：**拟合口径**（`usable`，与线上一致）与**放宽口径**
/// （多收「Δ=0 但代价过大」那一桶）——识别 `b` 靠的正是那一桶的高缓存占比区间，
/// 拟合口径把它们挡在门外。
#[test]
#[ignore]
fn codex_cache_read_quota_coefficient_on_real_db() {
    /// 5h 窗口长度（秒）。
    const WINDOW: i64 = 5 * 3_600;

    let now = chrono::Utc::now().timestamp();
    let scan = super::codex_rollout::scan(now - 120 * 86_400, 0, &Default::default());
    if let Some(src) = real_db() {
        let out = std::env::var_os("TC_SMOKE_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("tc_cache_coef");
        let _ = std::fs::remove_dir_all(&out);
        let s = SubStore::open(&clone_db(&src, &out)).expect("open");
        super::price::load_from(&s);
        super::calib::refit_from_store(&s, Platform::Codex);
    }
    let center = super::calib::scale(Platform::Codex);
    println!(
        "参照系数 center = {center:.5} %/美元；读数 {} 条、调用 {} 次",
        scan.readings.len(),
        scan.calls.len()
    );

    // 把一段调用折成 （非缓存代价, 缓存读代价)。价目按给定时刻的生效价取。
    let split_cost = |lo: i64, hi: i64, at: i64| -> (f64, f64) {
        if hi <= lo {
            return (0.0, 0.0);
        }
        let i = scan.calls.partition_point(|c| c.t <= lo);
        let j = scan.calls.partition_point(|c| c.t <= hi);
        let mut nc: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
        let mut cr: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
        for c in &scan.calls[i..j] {
            let a = nc.entry(c.model.clone()).or_insert([0; 4]);
            a[0] += c.tokens[0];
            a[1] += c.tokens[1];
            a[3] += c.tokens[3];
            let b = cr.entry(c.model.clone()).or_insert([0; 4]);
            b[2] += c.tokens[2];
        }
        (
            super::cost::cost_of_breakdown(Platform::Codex, &nc, at).0,
            super::cost::cost_of_breakdown(Platform::Codex, &cr, at).0,
        )
    };

    let mut ends: Vec<&super::codex_rollout::Reading> = vec![];
    for r in &scan.readings {
        match ends.last() {
            None => ends.push(r),
            Some(prev) if r.t - prev.t >= super::calib::MIN_PAIR_SECS => ends.push(r),
            _ => {}
        }
    }

    // 一条观测：（plan, 原因, t0, dt, used5_0, Δ, 非缓存有效代价, 缓存读有效代价)
    type Obs = (String, &'static str, i64, i64, f64, f64, f64, f64);
    let mut obs: Vec<Obs> = vec![];
    for w in ends.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a.plan != b.plan || a.plan.is_empty() {
            continue;
        }
        let (nc, cr) = split_cost(a.t, b.t, b.t);
        if nc + cr <= 0.0 {
            continue;
        }
        // 老化量同样拆两半（与 build_pairs 同口径：发生在 （t0−5h, t1−5h] 的调用）
        let (anc, acr) = split_cost(a.t - WINDOW, b.t - WINDOW, b.t - WINDOW);
        // 未知模型占比按整段算，与 Pair 判据一致
        let mut whole: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
        {
            let i = scan.calls.partition_point(|c| c.t <= a.t);
            let j = scan.calls.partition_point(|c| c.t <= b.t);
            for c in &scan.calls[i..j] {
                let slot = whole.entry(c.model.clone()).or_insert([0; 4]);
                for (x, v) in c.tokens.iter().enumerate() {
                    slot[x] += v;
                }
            }
        }
        let (cost, unknown) = super::cost::cost_of_breakdown(Platform::Codex, &whole, b.t);
        let pair = super::calib::Pair {
            t0: a.t,
            t1: b.t,
            used5_0: a.used5,
            used5_1: b.used5,
            resets5_0: a.resets5,
            resets5_1: b.resets5,
            cost,
            unknown_cost: unknown,
            aged_cost: anc + acr,
        };
        obs.push((
            a.plan.clone(),
            pair.reject_reason(center).unwrap_or("**采纳**"),
            a.t,
            b.t - a.t,
            a.used5,
            b.used5 - a.used5,
            nc - anc,
            cr - acr,
        ));
    }

    // 过原点两元 OLS：min Σ（δ − a·x − b·y)²。返回 （a, b, SE_a, SE_b, SE_（b−a), n, R²)。
    fn ols2(rows: &[(f64, f64, f64)]) -> Option<(f64, f64, f64, f64, f64, usize, f64)> {
        let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
        let (mut sxd, mut syd, mut sdd) = (0.0, 0.0, 0.0);
        for (x, y, d) in rows {
            sxx += x * x;
            sxy += x * y;
            syy += y * y;
            sxd += x * d;
            syd += y * d;
            sdd += d * d;
        }
        let det = sxx * syy - sxy * sxy;
        if rows.len() < 3 || det.abs() < 1e-12 {
            return None;
        }
        let a = (syy * sxd - sxy * syd) / det;
        let b = (sxx * syd - sxy * sxd) / det;
        let rss = sdd - a * sxd - b * syd;
        let sigma2 = (rss / (rows.len() as f64 - 2.0)).max(0.0);
        // M⁻¹ = 1/det × [[syy, −sxy], [−sxy, sxx]]
        let (va, vb, cov) = (sigma2 * syy / det, sigma2 * sxx / det, sigma2 * -sxy / det);
        let r2 = if sdd > 0.0 { 1.0 - rss / sdd } else { 0.0 };
        Some((
            a,
            b,
            va.max(0.0).sqrt(),
            vb.max(0.0).sqrt(),
            (va + vb - 2.0 * cov).max(0.0).sqrt(),
            rows.len(),
            r2,
        ))
    }
    // 过原点单元 OLS：返回 （系数, RSS)。
    fn ols1(rows: &[(f64, f64)]) -> (f64, f64) {
        let sxx: f64 = rows.iter().map(|(x, _)| x * x).sum();
        let sxd: f64 = rows.iter().map(|(x, d)| x * d).sum();
        let sdd: f64 = rows.iter().map(|(_, d)| d * d).sum();
        let k = if sxx > 0.0 { sxd / sxx } else { 0.0 };
        (k, sdd - k * sxd)
    }

    let strict = |why: &str| why == "**采纳**";
    // 放宽口径：多收「Δ=0 但代价过大」那一桶——识别 b 靠的就是它。
    let relaxed = |why: &str| matches!(why, "**采纳**" | "Δ=0 但代价过大");

    for plan in ["edu", "plus", "合并"] {
        let mine: Vec<&Obs> = obs.iter().filter(|o| plan == "合并" || o.0 == plan).collect();
        if mine.is_empty() {
            continue;
        }
        let tot_nc: f64 = mine.iter().map(|o| o.6.max(0.0)).sum();
        let tot_cr: f64 = mine.iter().map(|o| o.7.max(0.0)).sum();
        println!(
            "\n════ {plan}：{} 段；Σ非缓存 ${tot_nc:.2}  Σ缓存读 ${tot_cr:.2}（缓存读占 {:.1}%）",
            mine.len(),
            tot_cr / (tot_nc + tot_cr).max(1e-9) * 100.0
        );

        let variants: [(&str, &dyn Fn(&&Obs) -> bool); 4] = [
            ("拟合口径（usable）", &|o: &&Obs| strict(o.1)),
            ("放宽口径（含 Δ=0 代价过大）", &|o: &&Obs| relaxed(o.1)),
            // 下面两条是排混淆：低缓存占比的区间往往也是小区间，而读数只到整数百分点，
            // 小区间的 Δ 被量化吃掉的比例更大 ⇒ b>a 可能是量化假象而不是口径。
            ("拟合口径 + 只留 Δ>0", &|o: &&Obs| strict(o.1) && o.5 > 0.0),
            ("拟合口径 + 预期涨幅 ≥ 3 点（量化占比 <1/3）", &|o: &&Obs| {
                strict(o.1) && (o.6 + o.7) * center >= 3.0
            }),
        ];
        for (label, keep) in variants {
            let sel: Vec<&&Obs> = mine.iter().filter(|o| keep(o)).collect();
            if sel.len() < 3 {
                continue;
            }
            let rows: Vec<(f64, f64, f64)> = sel.iter().map(|o| (o.6, o.7, o.5)).collect();
            let snc: f64 = sel.iter().map(|o| o.6).sum();
            let scr: f64 = sel.iter().map(|o| o.7).sum();
            let sd: f64 = sel.iter().map(|o| o.5).sum();
            println!(
                "\n  ── {label} ──  {} 段  Σ涨幅 {sd:.0} 点  Σ非缓存 ${snc:.2}  Σ缓存读 ${scr:.2}（占 {:.1}%）",
                sel.len(),
                scr / (snc + scr).max(1e-9) * 100.0
            );
            // 共线诊断：两列相关性太高时 b 的标准误会炸，先看清楚
            let n = sel.len() as f64;
            let (mx, my) = (snc / n, scr / n);
            let (mut cxy, mut cxx, mut cyy) = (0.0, 0.0, 0.0);
            for o in &sel {
                cxy += (o.6 - mx) * (o.7 - my);
                cxx += (o.6 - mx).powi(2);
                cyy += (o.7 - my).powi(2);
            }
            println!("     corr(非缓存, 缓存读) = {:.3}", cxy / (cxx * cyy).max(1e-18).sqrt());
            let mut ratio_ba = f64::NAN; // 拟合出来的 b/a，下面分档表要用
            match ols2(&rows) {
                None => println!("     样本不足或两列共线，回归无解"),
                Some((a, b, sa, sb, sba, nn, r2)) => {
                    ratio_ba = b / a;
                    println!(
                        "     a（非缓存）= {a:>7.3} ± {sa:.3}   b（缓存读）= {b:>7.3} ± {sb:.3}   n={nn}  R²={r2:.3}"
                    );
                    println!(
                        "     b/a = {:>6.3}   |b−0| = {:.1}σ   |b−a| = {:.1}σ",
                        b / a,
                        (b / sb.max(1e-12)).abs(),
                        ((b - a) / sba.max(1e-12)).abs()
                    );
                    let (k_all, rss_all) =
                        ols1(&rows.iter().map(|r| (r.0 + r.1, r.2)).collect::<Vec<_>>());
                    let (k_nc, rss_nc) =
                        ols1(&rows.iter().map(|r| (r.0, r.2)).collect::<Vec<_>>());
                    println!(
                        "     单元对照：只用总代价 k={k_all:.3}（RSS {rss_all:.0}）  只用非缓存 k={k_nc:.3}（RSS {rss_nc:.0}）"
                    );
                    println!(
                        "     和之比对照：ΣΔ/Σ总有效 = {:.3}   ΣΔ/Σ非缓存 = {:.3}",
                        sd / (snc + scr).max(1e-9),
                        sd / snc.max(1e-9)
                    );
                }
            }
            // 不依赖回归的一层：按缓存读占比分档看隐含比值
            println!("     —— 按**缓存读占有效代价的比例**分档 ——");
            println!(
                "     （最后一列 = 额度口径尺子：缓存读按拟合出的 b/a = {ratio_ba:.2} 加权；它如果真是口径问题，这一列应当沿行拉平；b/a ≤ 0 时这一列无意义，打 NaN）"
            );
            println!(
                "     {:<12} {:>5} {:>11} {:>11} {:>9} {:>10} {:>12}",
                "缓存占比", "段数", "Σ非缓存", "Σ缓存读", "Σ涨幅", "ΣΔ/Σ总", "ΣΔ/额度尺"
            );
            for (lo, hi) in [(0.0, 0.2), (0.2, 0.4), (0.4, 0.6), (0.6, 0.8), (0.8, 1.01)] {
                let bin: Vec<&&&Obs> = sel
                    .iter()
                    .filter(|o| {
                        let f = o.7 / (o.6 + o.7).max(1e-9);
                        f >= lo && f < hi
                    })
                    .collect();
                if bin.is_empty() {
                    continue;
                }
                let x: f64 = bin.iter().map(|o| o.6).sum();
                let y: f64 = bin.iter().map(|o| o.7).sum();
                let d: f64 = bin.iter().map(|o| o.5).sum();
                println!(
                    "     {:<12} {:>5} {:>10.2}$ {:>10.2}$ {:>8.0}点 {:>9.3} {:>12.3}  （只除非缓存 {:.3}）",
                    format!("{:.0}〜{:.0}%", lo * 100.0, hi.min(1.0) * 100.0),
                    bin.len(),
                    x,
                    y,
                    d,
                    d / (x + y).max(1e-9),
                    if ratio_ba > 0.0 { d / (x + ratio_ba * y).max(1e-9) } else { f64::NAN },
                    d / x.max(1e-9)
                );
            }
        }
    }

    // 最后把「Δ=0 但代价过大」的大额区间逐条摊开,看各自的缓存占比
    println!("\n════ 最贵的「Δ=0 但代价过大」区间（缓存占比逐条）════");
    let mut worst: Vec<&Obs> = obs.iter().filter(|o| o.1 == "Δ=0 但代价过大").collect();
    worst.sort_by(|a, b| (b.6 + b.7).partial_cmp(&(a.6 + a.7)).unwrap());
    for o in worst.iter().take(8) {
        println!(
            "  {:<5} ${:>7.2}（非缓存 ${:>6.2} + 缓存读 ${:>6.2}，缓存占 {:>4.1}%）  {}  dt={:>5}s  used5_0={:.0}",
            o.0,
            o.6 + o.7,
            o.6,
            o.7,
            o.7 / (o.6 + o.7).max(1e-9) * 100.0,
            chrono::DateTime::from_timestamp(o.2, 0)
                .map(|x| x.format("%m-%d %H:%M:%S").to_string())
                .unwrap_or_default(),
            o.3,
            o.4
        );
    }
}

/// **额度口径的 token 权重**。
///
/// 上一条（[`codex_cache_read_quota_coefficient_on_real_db`]）按**美元**拆两半，但「每美元」
/// 本身是价目口径的量，两个自变量里混着四种单价与多个模型，`a` 到底是输入还是输出的
/// 系数说不清。
///
/// 这一条换到**token 空间**直接问：额度计数器按什么权重数 token？
///
/// ```text
///   Δ用量% = α×输入（Mtok) + β×输出（Mtok) + γ×缓存读（Mtok) + δ×缓存写（Mtok)
/// ```
///
/// 三个对照假设，一次量清楚：
/// - **按价目**（价目口径）：`α:β:γ:δ` 应当等于官方单价比（`gpt-5.6-sol` = 4:20:0.4:5）；
/// - **按 token 数**（不分种类）：四个系数应当**相等**；
/// - **按轮次**：token 解释不了，`calls` 那一列才显著 —— 所以附一版把**调用次数**
///   当第五个自变量。
///
/// 为了不让「模型贵贱」混进权重，**只取单一模型占 token 九成以上的区间**，按主模型
/// 分组各拟合一次。老化量在 token 空间同样逐通道扣掉，准入判据仍走 `Pair:reject_reason`
/// （与线上同一段代码）。**只量，不改生效逻辑。**
#[test]
#[ignore]
fn codex_quota_token_weights_on_real_db() {
    /// 5h 窗口长度（秒）。
    const WINDOW: i64 = 5 * 3_600;
    /// 「单一模型主导」的门槛：主模型要占区间 token 的这么多。
    const DOMINANT: f64 = 0.90;

    let now = chrono::Utc::now().timestamp();
    let scan = super::codex_rollout::scan(now - 120 * 86_400, 0, &Default::default());
    if let Some(src) = real_db() {
        let out = std::env::var_os("TC_SMOKE_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("tc_token_weights");
        let _ = std::fs::remove_dir_all(&out);
        let s = SubStore::open(&clone_db(&src, &out)).expect("open");
        super::price::load_from(&s);
        super::calib::refit_from_store(&s, Platform::Codex);
    }
    let center = super::calib::scale(Platform::Codex);
    println!("参照系数 center = {center:.5} %/美元");

    // 任意时段的分模型 token 与调用次数。
    let slice = |lo: i64, hi: i64| -> (std::collections::BTreeMap<String, [i64; 4]>, usize) {
        if hi <= lo {
            return (Default::default(), 0);
        }
        let i = scan.calls.partition_point(|c| c.t <= lo);
        let j = scan.calls.partition_point(|c| c.t <= hi);
        let mut m: std::collections::BTreeMap<String, [i64; 4]> = Default::default();
        for c in &scan.calls[i..j] {
            let slot = m.entry(c.model.clone()).or_insert([0; 4]);
            for (x, v) in c.tokens.iter().enumerate() {
                slot[x] += v;
            }
        }
        (m, j - i)
    };

    let mut ends: Vec<&super::codex_rollout::Reading> = vec![];
    for r in &scan.readings {
        match ends.last() {
            None => ends.push(r),
            Some(prev) if r.t - prev.t >= super::calib::MIN_PAIR_SECS => ends.push(r),
            _ => {}
        }
    }

    // 一条观测：（主模型, plan, 原因, Δ, [输入,输出,缓存读,缓存写] 净 Mtok, 净调用数)
    type Obs = (String, String, &'static str, f64, [f64; 4], f64);
    let mut obs: Vec<Obs> = vec![];
    // 顺带统计全局 token 份额，好知道「主模型」到底覆盖多少
    let mut share: std::collections::BTreeMap<String, i64> = Default::default();
    for w in ends.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a.plan != b.plan || a.plan.is_empty() {
            continue;
        }
        let (whole, ncalls) = slice(a.t, b.t);
        if whole.is_empty() {
            continue;
        }
        for (m, v) in &whole {
            *share.entry(m.clone()).or_insert(0) += v.iter().sum::<i64>();
        }
        let (aged, acalls) = slice(a.t - WINDOW, b.t - WINDOW);
        let (cost, unknown) = super::cost::cost_of_breakdown(Platform::Codex, &whole, b.t);
        let aged_cost = super::cost::cost_of_breakdown(Platform::Codex, &aged, b.t - WINDOW).0;
        let pair = super::calib::Pair {
            t0: a.t,
            t1: b.t,
            used5_0: a.used5,
            used5_1: b.used5,
            resets5_0: a.resets5,
            resets5_1: b.resets5,
            cost,
            unknown_cost: unknown,
            aged_cost,
        };
        let why = pair.reject_reason(center).unwrap_or("**采纳**");
        // 主模型：token 占九成以上才算
        let total: i64 = whole.values().map(|v| v.iter().sum::<i64>()).sum();
        let Some((top, tv)) = whole.iter().max_by_key(|(_, v)| v.iter().sum::<i64>()) else {
            continue;
        };
        if total <= 0 || (tv.iter().sum::<i64>() as f64) < DOMINANT * total as f64 {
            continue;
        }
        let az = aged.get(top).copied().unwrap_or([0; 4]);
        let net = [
            (tv[0] - az[0]) as f64 / 1e6,
            (tv[1] - az[1]) as f64 / 1e6,
            (tv[2] - az[2]) as f64 / 1e6,
            (tv[3] - az[3]) as f64 / 1e6,
        ];
        obs.push((
            top.clone(),
            a.plan.clone(),
            why,
            b.used5 - a.used5,
            net,
            ncalls as f64 - acalls as f64,
        ));
    }

    let mut sh: Vec<_> = share.iter().collect();
    sh.sort_by_key(|(_, v)| -**v);
    let all: i64 = share.values().sum();
    println!("\n全局 token 份额（前 6）：");
    for (m, v) in sh.iter().take(6) {
        println!("  {m:<24} {:>14} tok  （{:>5.1}%）", v, **v as f64 / all as f64 * 100.0);
    }

    /// 过原点多元 OLS（正规方程 + Gauss-Jordan 求逆）。返回 （系数, 标准误, R²)。
    fn ols(x: &[Vec<f64>], y: &[f64]) -> Option<(Vec<f64>, Vec<f64>, f64)> {
        let k = x.first()?.len();
        if x.len() <= k + 1 {
            return None;
        }
        let mut m = vec![vec![0.0; k]; k];
        let mut v = vec![0.0; k];
        let mut syy = 0.0;
        for (row, d) in x.iter().zip(y) {
            for i in 0..k {
                v[i] += row[i] * d;
                for j in 0..k {
                    m[i][j] += row[i] * row[j];
                }
            }
            syy += d * d;
        }
        // [m | I] → [I | m⁻¹]
        let mut aug: Vec<Vec<f64>> = (0..k)
            .map(|i| {
                let mut r = m[i].clone();
                r.extend((0..k).map(|j| if i == j { 1.0 } else { 0.0 }));
                r
            })
            .collect();
        for col in 0..k {
            let piv = (col..k).max_by(|&a, &b| {
                aug[a][col].abs().partial_cmp(&aug[b][col].abs()).unwrap()
            })?;
            if aug[piv][col].abs() < 1e-18 {
                return None; // 奇异：某一列被别的列线性表出
            }
            aug.swap(col, piv);
            let p = aug[col][col];
            for x in aug[col].iter_mut() {
                *x /= p;
            }
            for r in 0..k {
                if r != col {
                    let f = aug[r][col];
                    if f != 0.0 {
                        for c in 0..2 * k {
                            aug[r][c] -= f * aug[col][c];
                        }
                    }
                }
            }
        }
        let inv: Vec<Vec<f64>> = aug.iter().map(|r| r[k..].to_vec()).collect();
        let beta: Vec<f64> =
            (0..k).map(|i| (0..k).map(|j| inv[i][j] * v[j]).sum()).collect();
        let rss = syy - (0..k).map(|i| beta[i] * v[i]).sum::<f64>();
        let s2 = (rss / (x.len() - k) as f64).max(0.0);
        let se: Vec<f64> = (0..k).map(|i| (s2 * inv[i][i]).max(0.0).sqrt()).collect();
        Some((beta, se, if syy > 0.0 { 1.0 - rss / syy } else { 0.0 }))
    }

    let names4 = ["输入", "输出", "缓存读", "缓存写"];
    for model in sh.iter().take(3).map(|(m, _)| (*m).clone()) {
        for plan in ["edu", "plus", "合并"] {
            let sel: Vec<&Obs> = obs
                .iter()
                .filter(|o| o.0 == model && o.2 == "**采纳**" && (plan == "合并" || o.1 == plan))
                .collect();
            if sel.len() < 20 {
                continue;
            }
            let sums: [f64; 4] = (0..4)
                .map(|i| sel.iter().map(|o| o.4[i]).sum::<f64>())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            let sd: f64 = sel.iter().map(|o| o.3).sum();
            println!(
                "\n──── {model} / {plan}：{} 段（单一模型占 token ≥{:.0}%，拟合口径）  Σ涨幅 {sd:.0} 点",
                sel.len(),
                DOMINANT * 100.0
            );
            // 官方单价（$/Mtok）：拿单通道一百万 token 过一遍 cost.rs，不另拄一份价目表
            let unit = |t: [i64; 4]| {
                super::cost::cost_of_breakdown(
                    Platform::Codex,
                    &std::collections::BTreeMap::from([(model.clone(), t)]),
                    now,
                )
                .0
            };
            let p = [
                unit([1_000_000, 0, 0, 0]),
                unit([0, 1_000_000, 0, 0]),
                unit([0, 0, 1_000_000, 0]),
                unit([0, 0, 0, 1_000_000]),
            ];
            let dollars: f64 = (0..4).map(|i| sums[i] * p[i]).sum();
            println!(
                "     净 Mtok：输入 {:.1}  输出 {:.1}  缓存读 {:.1}  缓存写 {:.1}   ⇒ 净代价 ${:.2}",
                sums[0], sums[1], sums[2], sums[3], dollars
            );
            println!(
                "     价目口径隐含系数 Σ涨幅/Σ代价 = {:.3} %/美元（全库 center = {center:.3}）",
                sd / dollars.max(1e-9)
            );
            let y: Vec<f64> = sel.iter().map(|o| o.3).collect();
            // 缓存写在 rollout 里恒为 0（上游不报），全零列会把正规方程弄奇异 ⇒ 退化列不进回归。
            let cols: Vec<usize> = (0..4)
                .filter(|&i| sel.iter().map(|o| o.4[i] * o.4[i]).sum::<f64>() > 1e-12)
                .collect();
            let x4: Vec<Vec<f64>> =
                sel.iter().map(|o| cols.iter().map(|&i| o.4[i]).collect()).collect();
            println!(
                "     参与回归的通道：{}（其余通道恒为 0）",
                cols.iter().map(|&i| names4[i]).collect::<Vec<_>>().join(" / ")
            );
            match ols(&x4, &y) {
                None => println!("     多元回归奇异"),
                Some((bta, se, r2)) => {
                    println!("     多元（%/Mtok）  R²={r2:.3}");
                    for (k, &i) in cols.iter().enumerate() {
                        println!(
                            "       {:<6} {:>9.3} ± {:>6.3}   （= 输入的 {:>6.3} 倍，{:>5.1}σ）",
                            names4[i],
                            bta[k],
                            se[k],
                            bta[k] / bta[0],
                            (bta[k] / se[k].max(1e-12)).abs()
                        );
                    }
                    // 回填成四通道，下面与官方单价对读时不必再分情况
                    let bta: Vec<f64> = (0..4)
                        .map(|i| cols.iter().position(|&c| c == i).map(|k| bta[k]).unwrap_or(0.0))
                        .collect();
                    println!(
                        "       官方单价（$/Mtok）：{:.2} / {:.2} / {:.2} / {:.2}  ⇒ 价目口径比 1 : {:.2} : {:.3} : {:.2}",
                        p[0],
                        p[1],
                        p[2],
                        p[3],
                        p[1] / p[0],
                        p[2] / p[0],
                        p[3] / p[0]
                    );
                    println!(
                        "       额度/价目 的每 token 权重比：输出 {:>6.2}  缓存读 {:>6.2}  缓存写 {:>6.2}  （1 = 与价目一致）",
                        (bta[1] / bta[0]) / (p[1] / p[0]).max(1e-9),
                        (bta[2] / bta[0]) / (p[2] / p[0]).max(1e-9),
                        (bta[3] / bta[0]) / (p[3] / p[0]).max(1e-9)
                    );
                }
            }
            // 「按 token 数不分种类」对照：单元回归，自变量 = 总 token
            let xt: Vec<Vec<f64>> = sel.iter().map(|o| vec![o.4.iter().sum::<f64>()]).collect();
            if let Some((bta, se, r2)) = ols(&xt, &y) {
                println!(
                    "     对照·只用总 token：{:.3} ± {:.3} %/Mtok   R²={r2:.3}",
                    bta[0], se[0]
                );
            }
            // 「按轮次」对照：加一列调用次数
            let xc: Vec<Vec<f64>> = sel
                .iter()
                .map(|o| {
                    let mut r = o.4.to_vec();
                    r.push(o.5);
                    r
                })
                .collect();
            if let Some((bta, se, r2)) = ols(&xc, &y) {
                println!(
                    "     对照·加一列调用次数：调用 {:.5} ± {:.5} %/次（{:.1}σ）  R²={r2:.3}；加进来之后 输入 {:.3} 缓存读 {:.3}",
                    bta[4],
                    se[4],
                    (bta[4] / se[4].max(1e-12)).abs(),
                    bta[0],
                    bta[2]
                );
            }
        }
    }
}

// ============================================================================
// 只读查询面的真库核对
// ============================================================================

/// 把 collector.db（含 -wal / -shm）整份复制到工作目录。
fn clone_collector(src: &PathBuf, out: &PathBuf) -> PathBuf {
    std::fs::create_dir_all(out).expect("create out dir");
    let dst = out.join("collector.db");
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", src.display()));
        if from.exists() {
            std::fs::copy(&from, format!("{}{suffix}", dst.display())).expect("copy db");
        }
    }
    dst
}

/// 本地日 `day` 的第 `hour` 小时起点（unix 秒;独立复算用,**故意不复用**
/// `query:local_hour_ts`——复用就成了自己对自己）。
fn local_hour(day: &str, hour: u8) -> i64 {
    use chrono::TimeZone;
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(hour as u32, 0, 0))
        .and_then(|dt| chrono::Local.from_local_datetime(&dt).earliest())
        .map(|dt| dt.timestamp())
        .expect("local hour")
}

fn mmdd(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.format("%m-%d").to_string())
        .unwrap_or_default()
}

/// **查询面真库核对**：五条命令的本体各跑一遍真库副本,核对
///  价目表出得来,且某个时刻每个键恰好一行;
///  分模型用量对 `hourly_usage` **token 守恒**、对 `cost_of` **代价逐位相同**;
///  读数两层按序、条数与库里的 `SELECT COUNT（*)` 对得上。
#[test]
#[ignore]
fn s2_query_surface_on_real_db() {
    use super::price;
    use super::query;

    let Some(sub_src) = real_db() else {
        eprintln!("没有找到订阅真库,跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_s2_smoke");
    let _ = std::fs::remove_dir_all(&out);
    let sub_dst = clone_db(&sub_src, &out);
    let sub = SubStore::open(&sub_dst).expect("open subscriptions.db");
    // 价目索引从库装载（启动顺序里这一步排在重算之前,这里照做）
    price::load_from(&sub);
    println!("订阅库副本 -> {}", sub_dst.display());
    println!("价目索引来自库: {}", price::is_loaded_from_db());

    // ---------- （1) 价目表 ----------
    let now = chrono::Utc::now().timestamp();
    for platform in [Platform::Codex, Platform::Claude] {
        let all = price::rows_for(platform);
        let at_now = price::rows_at(platform, now);
        let keys: std::collections::BTreeSet<&str> =
            all.iter().map(|r| r.match_key.as_str()).collect();
        println!(
            "\n=== {} 价目：{} 行 / {} 个键;此刻有效 {} 行 ===",
            platform.as_str(),
            all.len(),
            keys.len(),
            at_now.len()
        );
        assert_eq!(at_now.len(), keys.len(), "某时刻每个键必须恰好一行");
        let mut seen = std::collections::BTreeSet::new();
        for r in &at_now {
            assert!(seen.insert(r.match_key.clone()), "{} 出了两行", r.match_key);
        }
        let multi: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|k| all.iter().filter(|r| r.match_key == *k).count() > 1)
            .collect();
        println!(
            "  有第二段生效期的键（= 被官方降过价的模型）：{}",
            if multi.is_empty() { "（无）".to_string() } else { multi.join(", ") }
        );
        for r in at_now.iter().take(3) {
            println!(
                "  {:<16} {:<24} in {:>7.3} out {:>7.3} cr {:>7.4} cw {:>7.3}  自 {}",
                r.match_key,
                r.display_name,
                r.usd_input,
                r.usd_output,
                r.usd_cache_read,
                r.usd_cache_write,
                chrono::DateTime::from_timestamp(r.effective_from, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default()
            );
        }
    }

    // ---------- （2) 分模型用量 ----------
    let Some(col_src) = real_collector_db() else {
        eprintln!("没有找到 collector.db,分模型用量这一段跳过（设 TC_COLLECTOR_DB=<路径>）");
        return;
    };
    let col_dst = clone_collector(&col_src, &out);
    let col = crate::collector::store::Store::open(&col_dst).expect("open collector.db");
    println!("\n采集库副本 -> {}", col_dst.display());

    for platform in [Platform::Codex, Platform::Claude] {
        let got = query::model_usage(&col, platform, None, None);
        println!(
            "\n=== {} 分模型用量 {} 〜 {}：{} 个模型,合计 ${:.2}（其中价目不可信 ${:.2}）===",
            platform.as_str(),
            got.from,
            got.to,
            got.rows.len(),
            got.usd_total,
            got.usd_unknown
        );
        println!(
            "  {:<22} {:>6} {:>13} {:>12} {:>14} {:>10} {:>4}",
            "模型", "轮次", "输入", "输出", "缓存读", "美元当量", "段"
        );
        for r in &got.rows {
            println!(
                "  {:<22} {:>6} {:>13} {:>12} {:>14} {:>10.2} {:>4}{}",
                r.model_key,
                r.requests,
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.usd,
                r.segments.len(),
                if r.known { "" } else { "  <- 价目不可信" }
            );
        }

        // token 守恒：查询面的四项合计 == 小时表原始合计
        let agent = platform.collector_source();
        let raw: (i64, i64, i64, i64) = rusqlite::Connection::open(&col_dst)
            .unwrap()
            .query_row(
                "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                        COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_write_tokens),0)
                   FROM hourly_usage WHERE agent_key = ?1",
                [agent],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        let sum = |f: fn(&query::ModelUsageRow) -> i64| got.rows.iter().map(f).sum::<i64>();
        assert_eq!(
            (
                sum(|r| r.input_tokens),
                sum(|r| r.output_tokens),
                sum(|r| r.cache_read_tokens),
                sum(|r| r.cache_write_tokens)
            ),
            raw,
            "{} 查询面的 token 必须与小时表逐项相等",
            platform.as_str()
        );
        for r in &got.rows {
            assert_eq!(
                r.input_tokens,
                r.segments.iter().map(|s| s.input_tokens).sum::<i64>(),
                "{} 段内 token 不守恒",
                r.model_key
            );
            assert_eq!(
                r.total_tokens,
                r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_write_tokens
            );
        }

        // 代价一致：逐小时用 cost_of 自己再算一遍,必须与查询面逐位相同
        let mut independent = 0.0f64;
        for (day, hour, model, t) in col.model_usage_hours(agent, &got.from, &got.to) {
            let tokens = super::cost::Tokens {
                input: t[0],
                output: t[1],
                cache_read: t[2],
                cache_write: t[3],
            };
            independent += super::cost::cost_of(platform, &model, &tokens, local_hour(&day, hour)).0;
        }
        println!(
            "  逐小时 cost_of 独立复算：${independent:.6}   查询面：${:.6}",
            got.usd_total
        );
        assert!(
            (independent - got.usd_total).abs() < 1e-6,
            "{} 查询面与 cost_of 必须给出同一个数",
            platform.as_str()
        );
    }

    // ---------- （3) 读数两层 ----------
    let conn = rusqlite::Connection::open(&sub_dst).unwrap();
    for platform in [Platform::Codex, Platform::Claude] {
        let kinds = sub.quota_kinds(platform);
        println!("\n=== {} 读数两层（种类 {:?}）===", platform.as_str(), kinds);
        let mut readings_total = 0usize;
        for k in &kinds {
            let rs = sub.quota_readings(platform, k, i64::MIN / 2, i64::MAX / 2);
            let ds = sub.quota_days(platform, k, "0000-00-00", "9999-99-99");
            readings_total += rs.len();
            let gain: f64 = ds.iter().map(|d| d.gain_pct).sum();
            let drop: f64 = ds.iter().map(|d| d.drop_pct).sum();
            let resets: i64 = ds.iter().map(|d| d.resets).sum();
            let carry_max = ds.iter().map(|d| d.carry_secs).max().unwrap_or(0);
            println!(
                "  {k:<7} 读数 {:>6} 条（{}）  日行 {:>3} 天  涨 {:>7.1} / 掉 {:>7.1} 点  重置 {resets:>3} 次  最长攒账 {:.1} 天",
                rs.len(),
                rs.first()
                    .zip(rs.last())
                    .map(|(a, b)| format!("{} 〜 {}", mmdd(a.t), mmdd(b.t)))
                    .unwrap_or_else(|| "空".to_string()),
                ds.len(),
                gain,
                drop,
                carry_max as f64 / 86_400.0
            );
            assert!(rs.windows(2).all(|w| w[0].t <= w[1].t), "读数必须按时刻升序");
            assert!(ds.windows(2).all(|w| w[0].day <= w[1].day), "日行必须按日期升序");
        }
        // 「kind 省略 = 全部种类」：各种类条数之和 == 库里按 （kind, t) 去重后的条数
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (SELECT DISTINCT kind, t FROM quota_reading WHERE platform = ?1)",
                [platform.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            readings_total as i64, rows,
            "{} 去重后的读数条数对不上",
            platform.as_str()
        );
    }
    println!("\n全部核对通过。");
}
