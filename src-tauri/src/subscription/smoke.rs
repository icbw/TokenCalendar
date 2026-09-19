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

/// 权重表升版后的存量重算：在真库副本上跑一遍启动顺序（open → recompute → refit）,
/// 核对「一行不丢、可恢复的全部抬到当前世代、标定仍拿得到样本」。
#[test]
#[ignore]
fn weight_version_bump_recomputes_real_db() {
    let Some(src) = real_db() else {
        eprintln!("没有找到真库,跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tc_sub_smoke");
    let _ = std::fs::remove_dir_all(&out);
    let dst = clone_db(&src, &out);
    println!("真库副本 → {}", dst.display());

    let before_rows: i64 = {
        let s = SubStore::open(&dst).expect("open");
        // open 已经跑完就地升级;先记下升级后、重算前的世代分布
        println!("--- 升级后 / 重算前 ---");
        for g in generations(&s) {
            println!("  platform={} weight_ver={} plan={} rows={}", g.0, g.1, g.2, g.3);
        }
        for p in [Platform::Claude, Platform::Codex] {
            super::calib::refit_from_store(&s, p);
            println!(
                "  refit(旧世代) {}: scale={:.6} n={}",
                p.as_str(),
                super::calib::scale(p),
                super::calib::sample_count(p)
            );
        }
        s.conn_for_smoke()
            .query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0))
            .unwrap()
    };

    let s = SubStore::open(&dst).expect("reopen");
    println!("--- 重算 ---");
    let mut doubtful_total = 0usize;
    for p in [Platform::Claude, Platform::Codex] {
        let (done, doubtful) = s.recompute_stale_costs(p).expect("recompute");
        doubtful_total += doubtful;
        println!("  {}: 重算 {done} 行,存疑 {doubtful} 行", p.as_str());
        super::calib::refit_from_store(&s, p);
        println!(
            "  refit(新世代) {}: scale={:.6} n={}",
            p.as_str(),
            super::calib::scale(p),
            super::calib::sample_count(p)
        );
    }
    println!("--- 重算后 ---");
    for g in generations(&s) {
        println!("  platform={} weight_ver={} plan={} rows={}", g.0, g.1, g.2, g.3);
    }

    let after_rows: i64 = s
        .conn_for_smoke()
        .query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before_rows, after_rows, "重算不许丢行");

    // 幂等：再跑一遍应当是空操作
    for p in [Platform::Claude, Platform::Codex] {
        let (done, _) = s.recompute_stale_costs(p).expect("recompute again");
        assert_eq!(done, 0, "{} 第二次重算必须是空操作", p.as_str());
    }
    println!("幂等：第二次重算 0 行");
    println!("存疑（breakdown 恢复不了）共 {doubtful_total} 行——不删,留库当档案");
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

/// 的真库：`price_model` 就地迁移 + 按时刻取价 + 代价单位改美元当量。
///
/// 在真库**副本**上跑一遍启动顺序（open → price:load_from → recompute → refit）,核对：
/// - `price_model` 由就地迁移建好并填上出厂种子,**既有行一条不少**;
/// - 存量行的 `cost` 是一次**纯量纲换算**：旧值 × 0.002 = 新值（旧单位 1 = $0.002）,
///   逐行核对;偏离的行单独列出来（那就是真的改了价的模型）;
/// - `scale` 相应地 ×500,于是 `cost × scale`（= 预计消耗百分点）**逐行不变** ⇒ 零回归;
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
    // 这个库是**还没迁移过**的旧库,还是已经被跑过一次的新库？两种都要能跑：
    // 「迁移前必落备份」「旧值 × 0.002 = 新值」这两条只在真的发生迁移时成立,
    // 无条件断言会让本用例对同一个库**只能跑一次**。
    // 想重新走一遍迁移路径：TC_SUB_DB 指向 data/backups/subscriptions-pre-*.db。
    let (needs_schema, needs_recompute, legacy_pairs): (bool, bool, i64) = {
        let c = rusqlite::Connection::open(&dst).unwrap();
        let has_price: bool = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='price_model'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        let stale: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM usage_pair WHERE weight_ver <> 0 AND weight_ver < ?1",
                [super::cost::WEIGHT_VERSION],
                |r| r.get(0),
            )
            .unwrap_or(0);
        println!("--- 升级前 --- price_model 存在={has_price}, 待重算行={stale}");
        let n = c.query_row("SELECT COUNT(*) FROM usage_pair", [], |r| r.get(0)).unwrap();
        // **两件独立的事**,别混：建表是结构变更（要备份）,重算是派生值刷新（不改结构）。
        // 恢复一份"迁移中"的备份就会出现"表已在、行还旧"——那时该重算不该备份。
        (!has_price, stale > 0, n)
    };
    println!("  ⇒ 需要建表={needs_schema} 需要重算={needs_recompute}");

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
        assert!(backups >= 1, "结构变更前必须落备份（红线）");
    } else {
        assert_eq!(backups, 0, "没有结构变更就不该无谓备份");
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

    // 顺手演示**为什么 mod.rs 必须把重算排在标定装载之前**：此刻库里的 cost 还是旧
    // 单位（1 单位 = $0.002）,而 PRIOR_SCALE 已经是新单位的 1 %/美元 —— 两者相差 500
    // 倍,于是 calib 的"隐含比值可信带"会把绝大多数样本判成离谱值踢掉。
    // 这不是回归,是一个**不该出现的中间态**:启动顺序保证它不会发生。
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
    // 旧代价单位 1 = 1000 个 Sonnet 5 输入 token 当量 = 1000 × 2 USD/Mtok ÷ 1e6 = $0.002
    const OLD_UNIT_USD: f64 = 0.002;
    let mut exact = 0usize;
    let mut drifted = vec![];
    for ((id, t1, old_cost, body), (_, _, new_cost, _)) in before.iter().zip(after.iter()) {
        if super::cost::parse_breakdown(body).is_empty() {
            continue; // 存疑行的原值不动,不参与对拍
        }
        let want = old_cost * OLD_UNIT_USD;
        let rel = if want != 0.0 { ((new_cost - want) / want).abs() } else { new_cost.abs() };
        if rel < 1e-9 {
            exact += 1;
        } else {
            drifted.push((*id, *t1, *old_cost, want, *new_cost, rel));
        }
    }
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
    // 代价配旧量纲的带心、另一边喂新的。此前这里手抄了一份旧判据,于是判据本身一改
    // 就会把两件不相干的改动混成一笔——改成调同一个函数之后
    // 这道对拍对将来的判据改动自动免疫。
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
    // 抬高。下面把两种算法都打出来,供筛选层的取舍用（抬采样下界那个方案
    // 已被否决——区间长度同时是监测节奏的量,不该拿它当筛子）。
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

    // ---- 重算后：scale 与旧构建的值互为量纲换算 ----
    println!("--- 重算后 ---");
    for g in generations(&s) {
        println!("  platform={} weight_ver={} plan={} rows={}", g.0, g.1, g.2, g.3);
    }
    // 旧构建在**同一个库**上的输出。
    // 拿它当跨构建的锚：新 scale × $0.002/旧单位应当还原成这个数。
    // 带宽给 5%——这台机器每天都在新增样本,拟合值会小幅漂移,但不该整体位移。
    const CLAUDE_SCALE_OLD_BUILD: f64 = 0.001852;
    for p in [Platform::Claude, Platform::Codex] {
        super::calib::refit_from_store(&s, p);
        let sa = super::calib::scale(p);
        let na = super::calib::sample_count(p);
        println!("  refit(重算后) {}: scale={:.6} %/美元 n={}", p.as_str(), sa, na);
        println!("    折回旧单位 = {:.8} %/旧代价单位", sa * OLD_UNIT_USD);
        if needs_recompute && matches!(p, Platform::Claude) && na > 0 {
            let back = sa * OLD_UNIT_USD;
            println!(
                "    旧构建实测 {CLAUDE_SCALE_OLD_BUILD:.6} ⇒ 相对差 {:.3}%",
                (back / CLAUDE_SCALE_OLD_BUILD - 1.0) * 100.0
            );
            assert!(
                (back / CLAUDE_SCALE_OLD_BUILD - 1.0).abs() < 0.05,
                "scale 折回旧单位后应还原旧构建实测值（{CLAUDE_SCALE_OLD_BUILD}）,实得 {back}"
            );
        }
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
/// - 首次扫描读了多少文件 / 多少字节 / 花了多久——这是"回溯上限"那个常量的依据,
///   不能靠推测（`HORIZON_DAYS` 的注释直接引用这里跑出来的数）。
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
    // 真机上它要等下一次真的用 Codex 才出现,而这条路（零请求推进快照）正是要验的；
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
