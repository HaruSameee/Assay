//! 探索の実測。DB も Discord も要らない（[`select_matches`] は純粋関数）。
//!
//! 実行:
//!
//! ```text
//! SQLX_OFFLINE=true cargo run --release --example matchmaker_search
//! ```
//!
//! 07 §6 は「マッチメイカのパフォーマンスチューニングは初週にやらない」としているので、
//! ここは**チューニングではなく、総当たりを書いていないことの確認**である。
//! キュー 100 人規模で 1 回の実行がどれだけかかるかを見る。
//!
//! 計算量の見立て（`n` = キュー人数）:
//!
//! - ウィンドウは基準を全候補に取るので `n` 本。各ウィンドウの構築が `O(n)`
//! - 1 ウィンドウの最小費用流は、流量 10 × SPFA 1 回 `O(V·E)`。`V ≤ n + 7`、
//!   `E ≤ 4n + 5`（1 人あたり source 1 本＋希望 3 本）なので `O(n²)`
//! - 成立するたびに 10 人減らして探索をやり直すので、外側は最大 `n / 10` 周
//!
//! したがって最悪 `O(n⁴ / 10)`。100 人でも下の実測のとおり数十ミリ秒で収まる。
//! 総当たり（`C(100,10) ≈ 1.7×10¹³`）とは比較にならない。

use std::time::Instant;

use chrono::{TimeDelta, TimeZone, Utc};
use unite_matchsystem::lifecycle::PlayerId;
use unite_matchsystem::matchmaker::{Candidate, ROLES, RoleWish, select_matches};
use uuid::Uuid;

/// `count` 人のキューを作る。
///
/// `spread` はレートの散らばり（表示スケール）。0 なら全員同じレートで、幅 95 の
/// ウィンドウに全員が入る＝**探索が一番重い形**になる。
fn queue(count: usize, spread: f64) -> Vec<Candidate> {
    let now = Utc.with_ymd_and_hms(2026, 9, 12, 21, 30, 0).unwrap();
    (0..count)
        .map(|index| Candidate {
            player_id: PlayerId(Uuid::from_u128(index as u128 + 1)),
            // 決定的に散らす（乱数を持ち込まない）
            rating: 1500.0 + (index as f64 % 7.0) * spread,
            sigma: 500.0,
            down_range: 95,
            enqueued_at: now - TimeDelta::seconds((index as i64 * 13) % 1800),
            roles: vec![
                RoleWish {
                    role: ROLES[index % ROLES.len()].to_owned(),
                    priority: 1,
                },
                RoleWish {
                    role: ROLES[(index + 1) % ROLES.len()].to_owned(),
                    priority: 2,
                },
                RoleWish {
                    role: ROLES[(index + 2) % ROLES.len()].to_owned(),
                    priority: 3,
                },
            ],
            flex_compensation: index % 9 == 0,
            unmatched_scans: 0,
        })
        .collect()
}

/// 全員が同じロールしか希望しない、絶対に成立しないキュー。
///
/// 成立しないので**全ウィンドウを試して落ちる**。1 回の実行で最も無駄足が多い形。
fn hopeless(count: usize) -> Vec<Candidate> {
    queue(count, 0.0)
        .into_iter()
        .map(|mut candidate| {
            candidate.roles = vec![RoleWish {
                role: "jungle".to_owned(),
                priority: 1,
            }];
            candidate
        })
        .collect()
}

fn median_micros(candidates: &[Candidate], now: chrono::DateTime<Utc>) -> u128 {
    let mut samples: Vec<u128> = (0..5)
        .map(|_| {
            let started = Instant::now();
            let formed = select_matches(candidates, now).len();
            let elapsed = started.elapsed().as_micros();
            std::hint::black_box(formed);
            elapsed
        })
        .collect();
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn main() {
    let now = Utc.with_ymd_and_hms(2026, 9, 12, 21, 30, 0).unwrap();
    println!(
        "{:>6}  {:>7}  {:>9}  {:>10}",
        "queue", "spread", "formed", "elapsed"
    );
    for spread in [0.0, 15.0] {
        for count in [20usize, 40, 60, 80, 100, 200] {
            let candidates = queue(count, spread);
            // 1 回ぶんを 5 回測って中央値を取る（初回の分岐予測ぶれを避ける）
            let elapsed = median_micros(&candidates, now);
            let formed = select_matches(&candidates, now).len();
            println!("{count:>6}  {spread:>7.0}  {formed:>9}  {elapsed:>8}µs");
        }
    }

    println!("\n成立しないキュー（全ウィンドウを試して落ちる = 最も無駄足の多い形）");
    println!("{:>6}  {:>9}  {:>10}", "queue", "formed", "elapsed");
    for count in [40usize, 100, 200] {
        let candidates = hopeless(count);
        let elapsed = median_micros(&candidates, now);
        let formed = select_matches(&candidates, now).len();
        println!("{count:>6}  {formed:>9}  {elapsed:>8}µs");
    }
}
