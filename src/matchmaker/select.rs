//! 10 人を選ぶアルゴリズム（07-マッチング仕様 §4-2 / §4-3）。**純粋関数**。
//!
//! Phase 2 の状態機械と同じ設計で、DB にも Discord にも触らない。時刻は引数で受ける。
//! ここがテストの主戦場になる。
//!
//! **総当たりを書かない。**キュー 40 人から 10 人を選ぶ組み合わせは 8×10⁸ 通りあり、
//! 成立しない。07 §4-3 の方針どおり、
//!
//! 1. レート降順にソートし、**幅 50 のスライディングウィンドウ**を取る
//! 2. ウィンドウ内から、各人の下方向設定を満たさない者を除く
//! 3. ロール枠への割り当てを**最小費用流**で解く（[`super::flow`]）
//! 4. 最大流が 10 に達すれば成立。達しなければ次のウィンドウへ
//!
//! で解く。ウィンドウは**その最小レートを基準**に取る。07 §3-1 の 2 条件は
//!
//! - `max(rating) − min(rating) ≤ 50`
//! - すべての i で `rating_i − min(rating) ≤ d_i`
//!
//! なので、「最小レート `m` を基準に `m ≤ rating ≤ m + 50` かつ `rating − m ≤ d`」を満たす
//! 集合を作れば、**その部分集合はどれも 2 条件を満たす**（実際の最小レートは `m` 以上なので、
//! 差はここで測った値より小さくなる）。基準を全候補について走らせるので、成立しうる組を
//! 取りこぼさない。
//!
//! チーム分けだけは成立した 10 人に対する総当たり（`C(10,5)/2 = 126` 通り）でよい。
//! 07 §4-3 のとおり、ここは十分小さい。

use std::collections::HashMap;

use crate::lifecycle::{PlayerId, Timestamp};
use crate::persistence::Team;
use crate::queue::model::UP_RANGE;

use super::flow::MinCostFlow;

/// 1 formation の人数。
pub const MATCH_SIZE: usize = 10;
/// 1 チームの人数。
pub const TEAM_SIZE: usize = MATCH_SIZE / 2;

/// ロールの正規値（00 §4 / Phase 1 マイグレーションの `roles`）。
///
/// 各チームにこの 5 つが 1 人ずつ入る。`roles` テーブルとずれると編成が作れなくなるので、
/// `tests/matchmaker.rs` が DB の値と突き合わせる。
pub const ROLES: [&str; 5] = [
    "top_carry",
    "top_support",
    "jungle",
    "bot_carry",
    "bot_tank",
];

/// マッチング時点で凍結するパラメータ版数。**選択の費用や範囲を変えたら上げること。**
///
/// 後から「どの版で組まれた試合か」を SQL だけで切り分けられるようにするための値で、
/// アルゴリズムの入力ではない。
///
/// | 版 | 下方向の範囲 | 上方向 |
/// |---|---|---|
/// | 1 | 95〜200（既定 95） | +95 固定 |
/// | 2 | 30〜100（既定 95） | +95 固定 |
/// | 3 | **50 固定** | **+50 固定** |
///
/// レーティングの版数は別物（[`crate::rating::RATING_PARAMS_VERSION`]。あちらは導出値の
/// パラメータなので凍結しない）。
pub const MATCHMAKING_PARAMS_VERSION: i32 = 3;

/// 予測勝率の凍結値。
///
/// 初週は固定増減方式（07 §2）で、勝率モデルを持たない。**無い数字を作らない**ため 0.5 を
/// 入れる。チーム平均レートの差は `formation_players.mu_at_match` から後で再現できるので、
/// 07 §7 の比較に必要な情報は失われない。
pub const PREDICTED_WIN_PROB: f32 = 0.5;

// ----------------------------------------------------------------------
// 費用（07 §4-3 の 4）。当日の校正対象。
// ----------------------------------------------------------------------

/// 待ち時間 1 秒あたりの費用の下がり幅。
///
/// 「費用: 待ち時間が長いほど低くする（＝待ち時間の合計を最大化する）」がこれ。流量は必ず
/// 10 なので、`(最大待ち − 自分の待ち)` の合計を最小化することは待ち時間の合計を
/// 最大化することと同じになる。
const WAIT_COST_PER_SECOND: i64 = 1;

/// 第 1 希望から 1 段下がるごとの費用。
///
/// 07 は希望順の一般的な優先を要求していない（要求しているのは待ち時間の合計とフレックス
/// 補償だけ）。ここは**同点の割り振りを決めるためだけ**の値で、1〜2 秒の待ち時間に相当する。
/// これより大きくすると「待ち時間の合計が最大」（§4-2 の 3）を侵し始める。
const PRIORITY_COST: i64 = 1;

/// フレックス補償の対象者が第 1 希望へ張る辺を、待ち時間 10 分ぶん安くする。
///
/// 07 §5 の「強く下げる」。**ハード制約にはしない**——費用を下げるだけなので、
/// この辺を使わないと 10 人揃わない場合は普通に第 2 希望で組まれる。
const FLEX_FIRST_CHOICE_BONUS: i64 = 600;

// ----------------------------------------------------------------------
// 入出力
// ----------------------------------------------------------------------

/// マッチメイカが見るキュー中の 1 人。
///
/// レートは**表示スケール**（1500 スタート）。導出値であり、ここでは受け取るだけ。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// 誰か。
    pub player_id: PlayerId,
    /// 表示スケールのレート。
    pub rating: f64,
    /// 表示スケールの σ。formation に凍結する。
    pub sigma: f64,
    /// 下方向の幅。いまは 50 固定（`clamp_down_range` を通した値が入る）。
    pub down_range: i32,
    /// キューに入った時刻。待ち時間の算出に使う。
    pub enqueued_at: Timestamp,
    /// 希望ロール。`priority` の昇順（1 が第 1 希望）。
    pub roles: Vec<RoleWish>,
    /// フレックス補償の対象か（07 §5。`formation_players` から導出済み）。
    pub flex_compensation: bool,
}

/// 希望ロール 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleWish {
    /// `roles.id`。
    pub role: String,
    /// 1 が第 1 希望。
    pub priority: i16,
}

/// 成立した 10 人の編成。まだ DB には何も書かれていない。
#[derive(Debug, Clone, PartialEq)]
pub struct ProposedMatch {
    /// 参加者 10 人。チーム順・ロール順に並ぶ。
    pub players: Vec<ProposedPlayer>,
}

/// 編成の 1 人。
#[derive(Debug, Clone, PartialEq)]
pub struct ProposedPlayer {
    /// 誰か。
    pub player_id: PlayerId,
    /// チーム。
    pub team: Team,
    /// 割り当てられたロール。
    pub role: String,
    /// 割り当てられたロールの希望順位（1 が第 1 希望）。
    ///
    /// formation に保存する必要はない。成立時の運用ログで、選択が希望順位をどれだけ
    /// 満たしたかを集計するためだけに使う。
    pub assigned_preference: i16,
    /// 第 1 希望以外を割り当てられたか。次回のフレックス補償の入力になる。
    pub role_was_flexed: bool,
    /// 前回のフレックスに対する補償が今回の第 1 希望へ適用されたか。
    ///
    /// これも成立時ログだけの集計値で、プレイヤーの可変状態には持たない。
    pub flex_compensation_applied: bool,
    /// 今回の成立時点での待機時間（秒）。
    pub wait_seconds: u64,
    /// 表示スケールのレート（凍結値の元）。
    pub rating: f64,
    /// 表示スケールの σ（凍結値の元）。
    pub sigma: f64,
}

impl ProposedMatch {
    /// 参加者の ID。
    pub fn player_ids(&self) -> Vec<PlayerId> {
        self.players.iter().map(|player| player.player_id).collect()
    }

    /// チームの平均レート（表示スケール）。
    pub fn team_average(&self, team: Team) -> f64 {
        let ratings: Vec<f64> = self
            .players
            .iter()
            .filter(|player| player.team == team)
            .map(|player| player.rating)
            .collect();
        ratings.iter().sum::<f64>() / ratings.len() as f64
    }

    /// 最高レートと最低レートの差。07 §3-1 の条件 1 は 50 以下（境界を含む）。
    pub fn rating_span(&self) -> f64 {
        let ratings: Vec<f64> = self.players.iter().map(|player| player.rating).collect();
        let max = ratings.iter().copied().fold(f64::MIN, f64::max);
        let min = ratings.iter().copied().fold(f64::MAX, f64::min);
        max - min
    }
}

// ----------------------------------------------------------------------
// 本体
// ----------------------------------------------------------------------

/// キューから組めるだけ組む。DB にも Discord にも触らない。
///
/// 成立した 10 人は候補から外して探索を続ける（07 §4-2「1 回の実行で組めるだけ組む」）。
/// 返る順序は決定的（レート降順・同点は `player_id` 昇順で走査した結果）。
pub fn select_matches(candidates: &[Candidate], now: Timestamp) -> Vec<ProposedMatch> {
    let mut pool: Vec<&Candidate> = candidates.iter().collect();
    // 決定的にするため、レート降順・同点は player_id 昇順で固定する
    pool.sort_by(|a, b| {
        b.rating
            .partial_cmp(&a.rating)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.player_id.0.cmp(&b.player_id.0))
    });

    let mut matches = Vec::new();
    while pool.len() >= MATCH_SIZE {
        let Some((taken, proposal)) = find_one(&pool, now) else {
            break;
        };
        // 成立した 10 人を候補から外す（添字の大きい方から消す）
        let mut taken = taken;
        taken.sort_unstable();
        for index in taken.into_iter().rev() {
            pool.remove(index);
        }
        matches.push(proposal);
    }
    matches
}

/// 幅 50 のウィンドウを上から順に試して、最初に成立した 1 組を返す。
///
/// **比較は両端を含む（`<=`）。**差ちょうど 50 は同じウィンドウに入る。
///
/// `anchor` は 0（最高レート）から順に下りるので、上のウィンドウで組めなかった候補は
/// **同じ呼び出しの中で下のウィンドウでも評価される**（飛ばさない）。
fn find_one(pool: &[&Candidate], now: Timestamp) -> Option<(Vec<usize>, ProposedMatch)> {
    let span = f64::from(UP_RANGE);
    for anchor in 0..pool.len() {
        // ウィンドウの基準はその最小レート。実際の最小はこれ以上になるので、
        // ここで 2 条件を満たしていれば部分集合はどれも満たす
        let floor = pool[anchor].rating;
        let window: Vec<usize> = (0..pool.len())
            .filter(|&i| {
                let above = pool[i].rating - floor;
                above >= 0.0 && above <= span && above <= f64::from(pool[i].down_range)
            })
            .collect();
        if window.len() < MATCH_SIZE {
            continue;
        }
        if let Some(assignment) = assign_roles(pool, &window, now) {
            let taken = assignment.iter().map(|slot| slot.pool_index).collect();
            return Some((taken, split_teams(pool, &assignment, now)));
        }
    }
    None
}

/// ロール枠への割り当て 1 件。
#[derive(Debug, Clone, Copy)]
struct Slot {
    /// `pool` の添字。
    pool_index: usize,
    /// [`ROLES`] の添字。
    role_index: usize,
    /// 割り当てられたロールの希望順（1 が第 1 希望）。
    priority: i16,
}

/// ウィンドウ内から 10 人をロール枠へ割り当てる。埋まらなければ `None`。
///
/// 枠は「5 ロール × 各 2 人」。07 §4-3 は右側を 10 枠（5 ロール × 2 チーム）と書いているが、
/// **どちらのチームに入るかは §4-2 の 4 で後から決める**ので、同じロールの 2 枠は区別が無い。
/// 容量 2 のロール頂点 1 個に畳んでも、流量も費用も同じグラフになる。
fn assign_roles(pool: &[&Candidate], window: &[usize], now: Timestamp) -> Option<Vec<Slot>> {
    let waits: Vec<i64> = window
        .iter()
        .map(|&i| (now - pool[i].enqueued_at).num_seconds().max(0))
        .collect();
    let longest = waits.iter().copied().max().unwrap_or(0);

    let source = 0;
    let player_node = |slot: usize| 1 + slot;
    let role_node = |role: usize| 1 + window.len() + role;
    let sink = 1 + window.len() + ROLES.len();

    let mut flow = MinCostFlow::new(sink + 1);
    let mut edges = Vec::new();
    for (slot, &i) in window.iter().enumerate() {
        flow.add_edge(source, player_node(slot), 1, 0);
        for wish in &pool[i].roles {
            let Some(role_index) = ROLES.iter().position(|role| *role == wish.role) else {
                // roles テーブルに無いロールは DB の FK が拒否するので通常は来ない。
                // 来たら黙って無視する（マッチメイカを止める理由にはならない）
                continue;
            };
            let cost = edge_cost(pool[i], wish, waits[slot], longest);
            let edge = flow.add_edge(player_node(slot), role_node(role_index), 1, cost);
            edges.push((edge, slot, role_index, wish.priority));
        }
    }
    for role in 0..ROLES.len() {
        flow.add_edge(role_node(role), sink, (MATCH_SIZE / ROLES.len()) as i64, 0);
    }

    let (filled, _cost) = flow.run(source, sink, MATCH_SIZE as i64);
    if filled != MATCH_SIZE as i64 {
        return None;
    }
    Some(
        edges
            .into_iter()
            .filter(|(edge, ..)| flow.used(*edge))
            .map(|(_, slot, role_index, priority)| Slot {
                pool_index: window[slot],
                role_index,
                priority,
            })
            .collect(),
    )
}

/// 1 本の「プレイヤー → ロール」の辺の費用。
///
/// 全ての辺に [`FLEX_FIRST_CHOICE_BONUS`] を下駄として履かせてあるので、値は必ず 0 以上に
/// なる。流量 1 につきこの種類の辺をちょうど 1 本通るため、下駄は最小解を変えない。
fn edge_cost(candidate: &Candidate, wish: &RoleWish, wait: i64, longest: i64) -> i64 {
    let waiting = WAIT_COST_PER_SECOND * (longest - wait);
    let preference = PRIORITY_COST * i64::from(wish.priority - 1);
    let flex = if candidate.flex_compensation && wish.priority == 1 {
        FLEX_FIRST_CHOICE_BONUS
    } else {
        0
    };
    FLEX_FIRST_CHOICE_BONUS + waiting + preference - flex
}

/// チーム平均レートの差が最小になる分け方を選ぶ（07 §4-2 の 4）。
///
/// 成立した 10 人に対する総当たり。`C(10,5)/2 = 126` 通りを、先頭のプレイヤーを red に
/// 固定して数え上げる。各チームに 5 ロールが 1 人ずつ入る分け方だけを残す。
fn split_teams(pool: &[&Candidate], assignment: &[Slot], now: Timestamp) -> ProposedMatch {
    let ratings: Vec<f64> = assignment
        .iter()
        .map(|slot| pool[slot.pool_index].rating)
        .collect();

    let mut best: Option<(f64, u32)> = None;
    for mask in 0u32..(1 << MATCH_SIZE) {
        // 先頭を red に固定して対称な分け方を 1 回だけ数える（252 / 2 = 126 通り）
        if mask & 1 == 0 || mask.count_ones() as usize != TEAM_SIZE {
            continue;
        }
        let mut red_roles = 0u32;
        let mut red_sum = 0.0;
        let mut blue_sum = 0.0;
        for (index, slot) in assignment.iter().enumerate() {
            if mask & (1 << index) != 0 {
                red_roles |= 1 << slot.role_index;
                red_sum += ratings[index];
            } else {
                blue_sum += ratings[index];
            }
        }
        // 各チームに 5 ロールが 1 人ずつ（red が 5 種類なら blue も自動的に 5 種類）
        if red_roles.count_ones() as usize != ROLES.len() {
            continue;
        }
        let difference = ((red_sum - blue_sum) / TEAM_SIZE as f64).abs();
        if best.is_none_or(|(current, _)| difference < current) {
            best = Some((difference, mask));
        }
    }
    // 割り当ては 5 ロール × 各 2 人なので、条件を満たす分け方は必ず存在する
    let (_, mask) = best.expect("a role-balanced split always exists for two players per role");

    let mut players: Vec<ProposedPlayer> = assignment
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let candidate = pool[slot.pool_index];
            ProposedPlayer {
                player_id: candidate.player_id,
                team: if mask & (1 << index) != 0 {
                    Team::Red
                } else {
                    Team::Blue
                },
                role: ROLES[slot.role_index].to_owned(),
                assigned_preference: slot.priority,
                // 第 1 希望以外を割り当てられたか。次回のフレックス補償の入力
                role_was_flexed: slot.priority != 1,
                flex_compensation_applied: candidate.flex_compensation && slot.priority == 1,
                wait_seconds: u64::try_from((now - candidate.enqueued_at).num_seconds())
                    .unwrap_or(0),
                rating: candidate.rating,
                sigma: candidate.sigma,
            }
        })
        .collect();
    players.sort_by_key(|player| {
        (
            player.team != Team::Red,
            ROLES.iter().position(|role| *role == player.role),
        )
    });
    ProposedMatch { players }
}

/// 割り当て済みのロールごとの人数（テストと不変条件の確認に使う）。
pub fn role_counts(proposal: &ProposedMatch) -> HashMap<(&str, Team), usize> {
    let mut counts = HashMap::new();
    for player in &proposal.players {
        *counts
            .entry((player.role.as_str(), player.team))
            .or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::queue::model::DOWN_RANGE;

    /// 基準時刻。すべての待ち時間はここからの逆算。
    fn now() -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 30, 0).unwrap()
    }

    /// 1 人分。`waited` 秒だけ前からキューにいる。希望は渡した順に第 1・第 2・第 3 希望。
    fn who(n: u128, rating: f64, down_range: i32, waited: i64, roles: &[&str]) -> Candidate {
        Candidate {
            player_id: PlayerId(Uuid::from_u128(n)),
            rating,
            sigma: 500.0,
            down_range,
            enqueued_at: now() - TimeDelta::seconds(waited),
            roles: roles
                .iter()
                .enumerate()
                .map(|(index, role)| RoleWish {
                    role: (*role).to_owned(),
                    priority: index as i16 + 1,
                })
                .collect(),
            flex_compensation: false,
        }
    }

    /// ロールがちょうど 2 人ずつ埋まる 10 人。全員同じレート・同じ待ち時間。
    fn full_roster(first_id: u128, rating: f64) -> Vec<Candidate> {
        (0..MATCH_SIZE)
            .map(|index| {
                who(
                    first_id + index as u128,
                    rating,
                    DOWN_RANGE,
                    600,
                    &[ROLES[index % ROLES.len()]],
                )
            })
            .collect()
    }

    fn ids(proposal: &ProposedMatch) -> Vec<u128> {
        let mut ids: Vec<u128> = proposal
            .player_ids()
            .into_iter()
            .map(|player| player.0.as_u128())
            .collect();
        ids.sort_unstable();
        ids
    }

    fn role_of(proposal: &ProposedMatch, n: u128) -> &ProposedPlayer {
        proposal
            .players
            .iter()
            .find(|player| player.player_id.0 == Uuid::from_u128(n))
            .expect("player is in the match")
    }

    /// 編成が 07 §3-1 と §4-2 の形を満たすこと。どのテストからも呼ぶ。
    fn assert_well_formed(proposal: &ProposedMatch) {
        assert_eq!(proposal.players.len(), MATCH_SIZE);
        for team in Team::ALL {
            let roles: Vec<&str> = proposal
                .players
                .iter()
                .filter(|player| player.team == team)
                .map(|player| player.role.as_str())
                .collect();
            assert_eq!(roles.len(), TEAM_SIZE, "{team} has five players");
            let mut sorted = roles.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                ROLES.len(),
                "{team} has each role once: {roles:?}"
            );
        }
        assert!(
            proposal.rating_span() <= f64::from(UP_RANGE),
            "span {} exceeds the fixed +95 window",
            proposal.rating_span()
        );
        for role in ROLES {
            for team in Team::ALL {
                assert_eq!(role_counts(proposal).get(&(role, team)), Some(&1));
            }
        }
    }

    // ------------------------------------------------------------------
    // 成立の基本
    // ------------------------------------------------------------------

    #[test]
    fn exactly_ten_players_form_one_match() {
        let matches = select_matches(&full_roster(1, 1500.0), now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        assert_eq!(ids(&matches[0]), (1..=10).collect::<Vec<u128>>());
        assert!(
            matches[0]
                .players
                .iter()
                .all(|player| !player.role_was_flexed)
        );
    }

    #[test]
    fn fewer_than_ten_players_form_nothing() {
        let mut roster = full_roster(1, 1500.0);
        roster.pop();
        assert!(select_matches(&roster, now()).is_empty());
        assert!(select_matches(&[], now()).is_empty());
    }

    #[test]
    fn the_result_is_deterministic() {
        let roster = full_roster(1, 1500.0);
        assert_eq!(
            select_matches(&roster, now()),
            select_matches(&roster, now())
        );
        let mut shuffled = roster.clone();
        shuffled.reverse();
        assert_eq!(
            select_matches(&roster, now()),
            select_matches(&shuffled, now())
        );
    }

    // ------------------------------------------------------------------
    // 待ち時間（07 §4-2 の 3）
    // ------------------------------------------------------------------

    /// 11 人目がいても、待ち時間の合計が最大になる 10 人が選ばれる。
    #[test]
    fn the_least_patient_eleventh_player_is_left_out() {
        let mut roster = full_roster(1, 1500.0);
        // jungle 希望で 10 秒しか待っていない 11 人目。既存の jungle 2 人は 600 秒待っている
        roster.push(who(11, 1500.0, DOWN_RANGE, 10, &["jungle"]));
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        assert_eq!(
            ids(&matches[0]),
            (1..=10).collect::<Vec<u128>>(),
            "the player who waited 10s must lose to the ones who waited 600s"
        );
    }

    /// 逆向き: 長く待っている 11 人目が入り、待ちの短い人が落ちる。
    #[test]
    fn a_long_waiting_newcomer_displaces_a_fresh_player() {
        let mut roster = full_roster(1, 1500.0);
        // full_roster は index % 5 でロールを配るので、3 番は jungle
        roster[2] = who(3, 1500.0, DOWN_RANGE, 5, &["jungle"]);
        roster.push(who(11, 1500.0, DOWN_RANGE, 3600, &["jungle"]));
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        let chosen = ids(&matches[0]);
        assert!(chosen.contains(&11), "{chosen:?}");
        assert!(!chosen.contains(&3), "{chosen:?}");
    }

    // ------------------------------------------------------------------
    // レート範囲（07 §3-1）
    // ------------------------------------------------------------------

    /// **境界の実測（1-2）。**差ちょうど 50 は同じ組に入る。51 は入らない。
    ///
    /// ウィンドウの条件は `above <= span`（`find_one`）なので、**両端を含む**。
    #[test]
    fn exactly_fifty_apart_share_a_match_but_fifty_one_does_not() {
        let build = |high: f64| {
            let mut roster: Vec<Candidate> = full_roster(1, 1500.0);
            for (index, candidate) in roster.iter_mut().enumerate() {
                if index % 2 == 0 {
                    candidate.rating = high;
                }
            }
            roster
        };
        assert!(
            select_matches(&build(1551.0), now()).is_empty(),
            "51 apart is outside the fixed 50 window"
        );
        let matches = select_matches(&build(1550.0), now());
        assert_eq!(matches.len(), 1, "50 apart is inside the window (<=)");
        assert_eq!(matches[0].rating_span(), 50.0);
    }

    /// 51 下の人とは組まされない。範囲は全員同じなので、条件 1 と条件 2 が一致する。
    #[test]
    fn a_player_is_never_matched_fifty_one_below() {
        let mut roster = full_roster(1, 1549.0);
        // 1 番だけ 51 上
        roster[0] = who(1, 1600.0, DOWN_RANGE, 600, &[ROLES[0]]);
        assert!(select_matches(&roster, now()).is_empty());

        // 50 下なら組める
        for candidate in roster.iter_mut().skip(1) {
            candidate.rating = 1550.0;
        }
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].rating_span(), 50.0);
    }

    /// **seed の差 100 で 1500 群と 1600 群が分離する。**今回の変更の狙い。
    #[test]
    fn the_fifteen_hundred_and_sixteen_hundred_pools_do_not_mix() {
        let mut roster = full_roster(1, 1500.0);
        for candidate in roster.iter_mut().take(5) {
            candidate.rating = 1600.0;
        }
        assert!(select_matches(&roster, now()).is_empty());
    }

    /// 中間の 1550 は、1500 群とも 1600 群とも組める（どちらも差ちょうど 50）。
    #[test]
    fn fifteen_fifty_bridges_both_pools_at_exactly_the_boundary() {
        // 1550 と 1600 の混成
        let mut upper = full_roster(1, 1550.0);
        for candidate in upper.iter_mut().take(5) {
            candidate.rating = 1600.0;
        }
        let matches = select_matches(&upper, now());
        assert_eq!(matches.len(), 1, "1550 と 1600 は差ちょうど 50");
        assert_well_formed(&matches[0]);
        assert_eq!(matches[0].rating_span(), 50.0);

        // 1550 と 1500 の混成
        let mut lower = full_roster(1, 1550.0);
        for candidate in lower.iter_mut().take(5) {
            candidate.rating = 1500.0;
        }
        let matches = select_matches(&lower, now());
        assert_eq!(matches.len(), 1, "1550 と 1500 も差ちょうど 50");
        assert_eq!(matches[0].rating_span(), 50.0);
    }

    // ------------------------------------------------------------------
    // ロール構成
    // ------------------------------------------------------------------

    /// 希望が偏っていて枠が埋まらないときは空を返す。
    #[test]
    fn a_lopsided_role_distribution_produces_nothing() {
        // 全員 jungle だけ。jungle は 2 枠しかない
        let all_jungle: Vec<Candidate> = (1..=12)
            .map(|n| who(n, 1500.0, DOWN_RANGE, 600, &["jungle"]))
            .collect();
        assert!(select_matches(&all_jungle, now()).is_empty());

        // 1 ロールだけ供給が消えても成立しない
        let mut roster = full_roster(1, 1500.0);
        roster[4] = who(5, 1500.0, DOWN_RANGE, 600, &["jungle"]);
        roster[9] = who(10, 1500.0, DOWN_RANGE, 600, &["jungle"]);
        assert!(select_matches(&roster, now()).is_empty());
    }

    /// 第 2 希望で枠を埋められるなら成立する。埋めた人は `role_was_flexed`。
    #[test]
    fn a_second_choice_fills_the_gap_and_is_recorded_as_flexed() {
        let mut roster = full_roster(1, 1500.0);
        roster[9] = who(10, 1500.0, DOWN_RANGE, 600, &["jungle", "bot_tank"]);
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        let filled = role_of(&matches[0], 10);
        assert_eq!(filled.role, "bot_tank");
        assert!(filled.role_was_flexed, "second choice is a flex");
    }

    // ------------------------------------------------------------------
    // フレックス補償（07 §5）
    // ------------------------------------------------------------------

    /// 対象者は争っている第 1 希望を取る。
    #[test]
    fn a_flex_compensated_player_wins_the_contested_first_choice() {
        let contested = |flex_target: u128| -> Vec<Candidate> {
            let mut roster = vec![
                who(1, 1500.0, DOWN_RANGE, 600, &["jungle", "bot_tank"]),
                who(2, 1500.0, DOWN_RANGE, 600, &["jungle", "bot_tank"]),
                who(3, 1500.0, DOWN_RANGE, 600, &["jungle", "bot_tank"]),
                who(4, 1500.0, DOWN_RANGE, 600, &["bot_tank"]),
                who(5, 1500.0, DOWN_RANGE, 600, &["top_carry"]),
                who(6, 1500.0, DOWN_RANGE, 600, &["top_carry"]),
                who(7, 1500.0, DOWN_RANGE, 600, &["top_support"]),
                who(8, 1500.0, DOWN_RANGE, 600, &["top_support"]),
                who(9, 1500.0, DOWN_RANGE, 600, &["bot_carry"]),
                who(10, 1500.0, DOWN_RANGE, 600, &["bot_carry"]),
            ];
            for candidate in roster.iter_mut() {
                candidate.flex_compensation = candidate.player_id.0 == Uuid::from_u128(flex_target);
            }
            roster
        };

        for target in [1u128, 2, 3] {
            let matches = select_matches(&contested(target), now());
            assert_eq!(matches.len(), 1);
            assert_well_formed(&matches[0]);
            let compensated = role_of(&matches[0], target);
            assert_eq!(
                compensated.role, "jungle",
                "the compensated player takes the first choice"
            );
            assert!(!compensated.role_was_flexed);
            // 誰か 1 人は bot_tank に回る（補償は費用であって席の保証ではない）
            assert_eq!(
                matches[0]
                    .players
                    .iter()
                    .filter(|player| player.role_was_flexed)
                    .count(),
                1
            );
        }
    }

    /// 成立しないくらいなら第 2 希望で組む。**ハード制約にしない。**
    #[test]
    fn flex_compensation_never_blocks_a_match() {
        let mut roster = vec![
            who(1, 1500.0, DOWN_RANGE, 600, &["jungle"]),
            who(2, 1500.0, DOWN_RANGE, 600, &["jungle"]),
            who(3, 1500.0, DOWN_RANGE, 600, &["top_carry", "bot_tank"]),
            who(4, 1500.0, DOWN_RANGE, 600, &["bot_tank"]),
            who(5, 1500.0, DOWN_RANGE, 600, &["top_carry"]),
            who(6, 1500.0, DOWN_RANGE, 600, &["top_carry"]),
            who(7, 1500.0, DOWN_RANGE, 600, &["top_support"]),
            who(8, 1500.0, DOWN_RANGE, 600, &["top_support"]),
            who(9, 1500.0, DOWN_RANGE, 600, &["bot_carry"]),
            who(10, 1500.0, DOWN_RANGE, 600, &["bot_carry"]),
        ];
        // 3 番は補償対象だが、第 1 希望の top_carry は 5・6 番で埋まっている
        roster[2].flex_compensation = true;
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1, "the match must still form");
        assert_well_formed(&matches[0]);
        let compensated = role_of(&matches[0], 3);
        assert_eq!(compensated.role, "bot_tank", "second choice is taken");
        assert!(compensated.role_was_flexed);
    }

    // ------------------------------------------------------------------
    // 1 回の実行で組めるだけ組む（07 §4-2）
    // ------------------------------------------------------------------

    #[test]
    fn twenty_players_form_two_matches_in_one_run() {
        let mut roster = full_roster(1, 1500.0);
        roster.extend(full_roster(11, 1500.0));
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 2);
        for proposal in &matches {
            assert_well_formed(proposal);
        }
        // 同じ人が 2 つの組に入らない
        let mut everyone: Vec<u128> = matches.iter().flat_map(ids).collect();
        everyone.sort_unstable();
        assert_eq!(everyone, (1..=20).collect::<Vec<u128>>());
    }

    #[test]
    fn nineteen_players_form_only_one_match() {
        let mut roster = full_roster(1, 1500.0);
        roster.extend(full_roster(11, 1500.0));
        roster.pop();
        assert_eq!(select_matches(&roster, now()).len(), 1);
    }

    // ------------------------------------------------------------------
    // チーム分け（07 §4-2 の 4）
    // ------------------------------------------------------------------

    /// 各ロールの 2 人の高低を交互にすると、差 0 の分け方が存在する。それが選ばれること。
    #[test]
    fn teams_are_split_to_minimise_the_average_rating_gap() {
        let roster = vec![
            who(1, 1500.0, DOWN_RANGE, 600, &["top_carry"]),
            who(2, 1540.0, DOWN_RANGE, 600, &["top_carry"]),
            who(3, 1540.0, DOWN_RANGE, 600, &["top_support"]),
            who(4, 1500.0, DOWN_RANGE, 600, &["top_support"]),
            who(5, 1500.0, DOWN_RANGE, 600, &["jungle"]),
            who(6, 1540.0, DOWN_RANGE, 600, &["jungle"]),
            who(7, 1540.0, DOWN_RANGE, 600, &["bot_carry"]),
            who(8, 1500.0, DOWN_RANGE, 600, &["bot_carry"]),
            who(9, 1520.0, DOWN_RANGE, 600, &["bot_tank"]),
            who(10, 1520.0, DOWN_RANGE, 600, &["bot_tank"]),
        ];
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        let gap = (matches[0].team_average(Team::Red) - matches[0].team_average(Team::Blue)).abs();
        assert!(
            gap < 1e-9,
            "an even split exists, so it must be found: {gap}"
        );
    }

    /// 差 0 が作れない配置では、作れる中で最小のものになる。
    ///
    /// 各ロールが 1500 と 1510 の 2 人。各チームは各ロールから 1 人ずつなので、
    /// 片方が受け取る 1510 の人数 `k` に対して平均差は `|5 − 2k| × 2`。最小は `k = 2` の 2。
    #[test]
    fn an_uneven_roster_lands_on_the_smallest_reachable_gap() {
        let roster: Vec<Candidate> = (0..MATCH_SIZE)
            .map(|index| {
                who(
                    index as u128 + 1,
                    if index < ROLES.len() { 1500.0 } else { 1510.0 },
                    95,
                    600,
                    &[ROLES[index % ROLES.len()]],
                )
            })
            .collect();
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        let gap = (matches[0].team_average(Team::Red) - matches[0].team_average(Team::Blue)).abs();
        assert!((gap - 2.0).abs() < 1e-9, "gap = {gap}");
    }

    #[test]
    fn an_unbalanceable_roster_still_produces_the_smallest_gap() {
        // 1 人だけ 50 高い（範囲の上限ちょうど）。どう割っても差は出るが、最小のものを選ぶ
        let mut roster = full_roster(1, 1500.0);
        roster[0].rating = 1550.0;
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        let gap = (matches[0].team_average(Team::Red) - matches[0].team_average(Team::Blue)).abs();
        assert!((gap - 10.0).abs() < 1e-9, "gap = {gap}");
    }

    /// **1-4 の実測。**上のウィンドウで組めなかった中間層が、同じ周期で下のウィンドウでも
    /// 候補として評価されること。
    ///
    /// 再現手順どおり 1600×6 / 1550×6 / 1500×10 を並べ、上層（1600+1550=12人）は
    /// ロール構成が埋まらないようにする。`find_one` は anchor を高い方から下ろすので、
    /// 1550 を基準にしたウィンドウが失敗しても、次の 1500 基準のウィンドウで
    /// 1550 が再び候補に入る。**スキップされない。**
    #[test]
    fn the_middle_layer_is_still_a_candidate_in_the_lower_window() {
        let mut pool: Vec<Candidate> = Vec::new();
        // 1600 が 6 人。全員 jungle しか希望しない（jungle は 2 枠）
        for n in 1..=6u128 {
            pool.push(who(n, 1600.0, DOWN_RANGE, 600, &["jungle"]));
        }
        // 1550 が 6 人。全員 bot_tank だけ
        for n in 7..=12u128 {
            pool.push(who(n, 1550.0, DOWN_RANGE, 600, &["bot_tank"]));
        }
        // 1500 が 10 人。bot_tank だけ供給が無い（top_carry が 4 人）
        let lower_roles = [
            "top_carry",
            "top_carry",
            "top_carry",
            "top_carry",
            "top_support",
            "top_support",
            "jungle",
            "jungle",
            "bot_carry",
            "bot_carry",
        ];
        for (index, role) in lower_roles.iter().enumerate() {
            pool.push(who(13 + index as u128, 1500.0, DOWN_RANGE, 600, &[role]));
        }

        let matches = select_matches(&pool, now());
        assert_eq!(matches.len(), 1, "1550 + 1500 で 1 組成立する");
        let proposal = &matches[0];
        assert_well_formed(proposal);

        // 幅は 50（1500〜1550）。1600 は 1 人も入らない
        assert_eq!(proposal.rating_span(), 50.0);
        let ratings: Vec<f64> = proposal
            .players
            .iter()
            .map(|player| player.rating)
            .collect();
        assert_eq!(
            ratings.iter().filter(|rating| **rating == 1600.0).count(),
            0,
            "1600 は窓の外"
        );
        assert_eq!(
            ratings.iter().filter(|rating| **rating == 1550.0).count(),
            2,
            "bot_tank の 2 枠を中間層が埋める"
        );
        assert_eq!(
            ratings.iter().filter(|rating| **rating == 1500.0).count(),
            8
        );

        // 上のウィンドウ（1550 基準の 12 人）は実際に埋まらないことも確かめる
        let upper: Vec<Candidate> = pool
            .iter()
            .filter(|candidate| candidate.rating >= 1550.0)
            .cloned()
            .collect();
        assert_eq!(upper.len(), 12);
        assert!(
            select_matches(&upper, now()).is_empty(),
            "上層だけではロール構成が埋まらない"
        );
    }
}
