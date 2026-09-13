//! 「範囲が互いに両立する」の判定（07-マッチング仕様 §3-1）。純粋関数。
//!
//! 10人の組が成立する条件は**全ペアが相互に相手を受け入れること**であり、
//! キュー画面に出す「現在あなたと同じ試合に入れる人: N人」も同じ定義で数える
//! （自分を含む幅 50 のウィンドウのうち、いちばん人が多いもの。±50 ではない）。
//! 範囲が上下とも 50 固定になった現在、相互条件は「差が 50 以内」と同値だが、
//! **片方向の判定を残してある**（範囲がまた非対称になったときに、ここだけで直せる）。
//!
//! **境界は両端とも含む（`<=` / `>=`）。**差ちょうど 50 は組める側。seed の差 100 と
//! 組み合わせると、1500 と 1600 は組めず、1550 は 1500 とも 1600 とも組める。
//!
//! レートは**表示スケール**（1500 スタート）。レーティングは導出値なので、ここには
//! 現在値を保存する仕組みを置かない——呼び出し側が導出したものを渡す。

use crate::lifecycle::PlayerId;
use crate::matchmaker::FirstChoiceRoleCounts;

/// 範囲判定に必要な、キュー中プレイヤー1人分の値。
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedPlayer {
    /// 誰か。自分自身を数えないためだけに使う。
    pub player_id: PlayerId,
    /// 表示スケールのレート（導出値）。
    pub rating: f64,
    /// 上方向の幅。運営が変えられる（`current_matching_ranges.up_range`）。
    ///
    /// 全員に同じ値が入る。それでも 1 人ずつ持たせているのは、**片方向の判定を
    /// 残してある**のと同じ理由で、上下が非対称になったときにここだけで直せるため。
    pub up_range: i32,
    /// 下方向の幅（[`crate::queue::MatchingRanges::clamp_down`] を通した値が入る）。
    pub down_range: i32,
    /// 第1希望のロール（`roles.id`）。希望が 1 つも無ければ `None`。
    ///
    /// **第1希望だけを持つ。**「希望のどこかに含まれる」を数えると、1 人が複数の
    /// ロールに計上されて合計が人数と合わなくなる。
    pub first_choice: Option<String>,
    /// IP のシャドウバン中か（`queue_entries.shadow_banned`）。他の人の「範囲内 N 人」には
    /// 数えない（マッチメイカも候補から外す）。本人の画面ではふつうに見せる。
    pub shadow_banned: bool,
}

/// `viewer` から見て `other` が範囲内か。**片方向**。
///
/// 単体で人数を数える用途には使わない（[`mutually_in_range`] を使う）。相互条件の
/// 半分であることを明示するために公開してある。
///
/// **境界を含む。**`other.rating` が `viewer.rating ± 50` ちょうどでも範囲内
/// （比較は `>=` / `<=`）。
pub fn accepts(viewer: &QueuedPlayer, other: &QueuedPlayer) -> bool {
    let upper = viewer.rating + f64::from(viewer.up_range);
    let lower = viewer.rating - f64::from(viewer.down_range);
    other.rating >= lower && other.rating <= upper
}

/// 2人が**互いに**相手を受け入れるか。
pub fn mutually_in_range(a: &QueuedPlayer, b: &QueuedPlayer) -> bool {
    accepts(a, b) && accepts(b, a)
}

/// **`me` と同じ試合に入れる人数。**自分自身は数えない（`queue` に自分が入っていてもよい）。
///
/// 数えるのは「自分を含む幅 50 のウィンドウ（07 §3-1）のうち、いちばん人が多いもの」に
/// 入る人。**自分の ±50 に入る人の数ではない。**±50 で数えると、自分の 45 上と 45 下の
/// 人は両方「範囲内」だが、その 2 人は互いに 90 離れていて同じ試合には入れない。
/// マッチメイカが見るのは max − min ≤ 50 の集合なので、画面もそれに揃える
/// （「範囲内 10 人」と出ているのに何度も流れる、という食い違いを作らない）。
pub fn count_in_range(me: &QueuedPlayer, queue: &[QueuedPlayer]) -> usize {
    best_window(me, queue).len()
}

/// 自分を含む幅 `up_range` のウィンドウのうち、いちばん人が多いものに入る**他の人**。
///
/// ウィンドウの下端は「自分か、自分の下 `up_range` 以内にいる誰か」のレートで、
/// その下端から `up_range` 上までを 1 つの組み合わせとして数える（マッチメイカの
/// `find_one` と同じ形）。相互条件（[`mutually_in_range`]）を満たさない人は最初から外す。
/// 同数のウィンドウが複数あれば下端が低いものを採る（決定的にするため）。
fn best_window<'a>(me: &'a QueuedPlayer, queue: &'a [QueuedPlayer]) -> Vec<&'a QueuedPlayer> {
    let candidates: Vec<&QueuedPlayer> = queue
        .iter()
        .filter(|other| other.player_id != me.player_id)
        .filter(|other| mutually_in_range(me, other))
        .collect();
    let span = f64::from(me.up_range);
    // 下端の候補: 自分と、自分の下 span 以内の人のレート
    let mut floors: Vec<f64> = candidates
        .iter()
        .map(|other| other.rating)
        .filter(|rating| *rating <= me.rating && me.rating - *rating <= span)
        .chain(std::iter::once(me.rating))
        .collect();
    floors.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    floors.dedup();
    let mut best: Vec<&QueuedPlayer> = Vec::new();
    for floor in floors {
        let window: Vec<&QueuedPlayer> = candidates
            .iter()
            .copied()
            .filter(|other| {
                let above = other.rating - floor;
                above >= 0.0 && above <= span && above <= f64::from(other.down_range)
            })
            .collect();
        if window.len() > best.len() {
            best = window;
        }
    }
    best
}

/// 範囲内の人を**第1希望のロール別**に数える。
///
/// 絞り込みは [`count_in_range`] と同じ（自分を含む幅 50 のいちばん多いウィンドウ）。数え方は
/// 15 分サマリと同じ「第1希望がそのロールの人」で、希望のどこかに含まれる人ではない。
/// 希望を 1 つも持たない行は、どのロールにも数えない。
pub fn count_by_first_choice(me: &QueuedPlayer, queue: &[QueuedPlayer]) -> FirstChoiceRoleCounts {
    let mut counts = FirstChoiceRoleCounts::default();
    for other in best_window(me, queue) {
        if let Some(role) = other.first_choice.as_deref() {
            counts.increment(role);
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::queue::model::{DEFAULT_DOWN_RANGE, DEFAULT_UP_RANGE};

    fn player(n: u128, rating: f64) -> QueuedPlayer {
        QueuedPlayer {
            player_id: PlayerId(Uuid::from_u128(n)),
            rating,
            up_range: DEFAULT_UP_RANGE,
            down_range: DEFAULT_DOWN_RANGE,
            first_choice: None,
            shadow_banned: false,
        }
    }

    /// 第1希望つきのキュー中の人。
    fn wanting(n: u128, rating: f64, role: &str) -> QueuedPlayer {
        QueuedPlayer {
            first_choice: Some(role.to_owned()),
            shadow_banned: false,
            ..player(n, rating)
        }
    }

    /// 上下とも 50。**境界ちょうどは範囲内**（比較は `>=` / `<=`）。
    #[test]
    fn the_range_is_fifty_in_both_directions_and_includes_the_boundary() {
        let me = player(1, 1500.0);
        assert!(accepts(&me, &player(2, 1550.0)), "+50 ちょうどは範囲内");
        assert!(!accepts(&me, &player(3, 1550.1)), "+50 を超えたら外");
        assert!(accepts(&me, &player(4, 1450.0)), "-50 ちょうどは範囲内");
        assert!(!accepts(&me, &player(5, 1449.9)), "-50 を超えたら外");
    }

    /// **1-2 の実測対象。**seed の差 100 と幅 50 の噛み合わせ。
    #[test]
    fn the_seed_gap_of_one_hundred_separates_the_two_pools() {
        let low = player(1, 1500.0);
        let middle = player(2, 1550.0);
        let high = player(3, 1600.0);

        // 1550 と 1600 は差ちょうど 50 → 組める
        assert!(mutually_in_range(&middle, &high), "1550 と 1600（差 50）");
        // 1550 と 1500 も差ちょうど 50 → 組める
        assert!(mutually_in_range(&middle, &low), "1550 と 1500（差 50）");
        // 1500 と 1600 は差 100 → 組めない
        assert!(!mutually_in_range(&low, &high), "1500 と 1600（差 100）");

        // 中間層から見ると両方が ±50 に入るが、**同じ試合には片方しか入らない**
        // （1500 と 1600 は 100 離れている）。数えるのは幅 50 のウィンドウなので 1
        let queue = [low.clone(), middle.clone(), high.clone()];
        assert_eq!(count_in_range(&middle, &queue), 1);
        assert_eq!(count_in_range(&low, &queue), 1);
        assert_eq!(count_in_range(&high, &queue), 1);
    }

    /// **±50 に入る人数ではなく、自分を含む幅 50 の組み合わせで数える。**
    ///
    /// 「範囲内 10 人」と出ているのに何度も流れる、の原因: 自分の 45 上と 45 下は
    /// どちらも範囲内だが、互いに 90 離れていて同じ試合には入れない。
    #[test]
    fn the_count_is_the_largest_fifty_wide_window_that_contains_me() {
        let me = player(1, 1600.0);
        let mut queue = vec![me.clone()];
        // 上に 4 人（1640〜1643）、下に 5 人（1555〜1560）。±50 なら 9 人だが、
        // 上の群と下の群は 80 以上離れていて 1 つの試合には入らない
        for n in 0..4u128 {
            queue.push(player(10 + n, 1640.0 + n as f64));
        }
        for n in 0..5u128 {
            queue.push(player(20 + n, 1555.0 + n as f64));
        }
        // 自分を含む幅 50: [1555, 1605] に下の 5 人、[1600, 1650] に上の 4 人 → 5
        assert_eq!(count_in_range(&me, &queue), 5);
        // 上の群にもう 2 人足せば [1600, 1650] が 6 人になり、そちらが採られる
        queue.push(player(30, 1649.0));
        queue.push(player(31, 1650.0));
        assert_eq!(count_in_range(&me, &queue), 6);
        // 全員が幅 50 に収まっていれば、±50 と同じ数になる
        let tight: Vec<QueuedPlayer> = (0..10u128).map(|n| player(n, 1600.0 + n as f64)).collect();
        assert_eq!(count_in_range(&tight[0], &tight), 9);
        assert_eq!(count_in_range(&tight[9], &tight), 9);
    }

    /// 相互条件は対称。範囲が全員同じなので、片方向と結論が一致する。
    #[test]
    fn the_condition_is_symmetric() {
        let a = player(1, 1500.0);
        for rating in [1400.0, 1449.9, 1450.0, 1500.0, 1550.0, 1550.1, 1600.0] {
            let b = player(2, rating);
            assert_eq!(
                mutually_in_range(&a, &b),
                mutually_in_range(&b, &a),
                "{rating}"
            );
            assert_eq!(accepts(&a, &b), accepts(&b, &a), "{rating}");
        }
    }

    #[test]
    fn the_count_excludes_yourself_and_out_of_range_players() {
        let me = player(1, 1500.0);
        let queue = [
            me.clone(),        // 自分は数えない
            player(2, 1500.0), // 同レート
            player(3, 1550.0), // +50 ちょうど
            player(4, 1550.1), // +50 を超える
            player(5, 1450.0), // -50 ちょうど
            player(6, 1449.9), // -50 を超える
            player(7, 1600.0), // 上の群
        ];
        // ±50 には 3 人（1500 / 1550 / 1450）入るが、1450 と 1550 は同じ試合に入れない。
        // 幅 50 のウィンドウは [1450, 1500] か [1500, 1550] のどちらかで、どちらも 2 人
        assert_eq!(count_in_range(&me, &queue), 2);
    }

    /// 旧仕様の広い `down_range` が万一残っていても、丸めた値が渡る前提。
    ///
    /// ここは丸め忘れたときの挙動を記録するためのテスト。`down_range` に 200 が入れば
    /// 下方向だけ広がるが、**上方向は 50 のままなので相互条件は成立しない**。
    #[test]
    fn an_unclamped_wide_value_still_cannot_widen_the_pair() {
        let wide = QueuedPlayer {
            player_id: PlayerId(Uuid::from_u128(1)),
            up_range: DEFAULT_UP_RANGE,
            rating: 1600.0,
            down_range: 200,
            first_choice: None,
            shadow_banned: false,
        };
        let below = player(2, 1450.0);
        assert!(accepts(&wide, &below), "片方向では通る");
        assert!(!accepts(&below, &wide), "上方向 +50 の外");
        assert!(!mutually_in_range(&wide, &below));
        assert_eq!(count_in_range(&wide, &[wide.clone(), below]), 0);
    }

    #[test]
    fn an_empty_queue_counts_zero() {
        let me = player(1, 1500.0);
        assert_eq!(count_in_range(&me, &[]), 0);
        assert_eq!(count_in_range(&me, std::slice::from_ref(&me)), 0);
    }

    /// **内訳は第1希望だけを数える。**希望のどこかに含まれる人ではない。
    ///
    /// 絞り込みは [`count_in_range`] と同じ条件なので、内訳の合計は範囲内の人数と
    /// 一致する（希望を持たない行があるぶんだけ内訳が少なくなる）。
    #[test]
    fn the_breakdown_counts_only_the_first_choice_of_players_in_range() {
        let me = wanting(1, 1500.0, "jungle");
        let queue = [
            me.clone(),                     // 自分は数えない
            wanting(2, 1500.0, "jungle"),   // 範囲内
            wanting(3, 1550.0, "jungle"),   // +50 ちょうど。範囲内
            wanting(4, 1550.1, "jungle"),   // 範囲外
            wanting(5, 1500.0, "bot_tank"), // 同レート。範囲内
            wanting(6, 1449.9, "bot_tank"), // 範囲外
            player(7, 1500.0),              // 希望なし。どのロールにも入らない
        ];
        let counts = super::count_by_first_choice(&me, &queue);
        assert_eq!(counts.jungle, 2, "範囲外の 1550.1 を数えていない");
        assert_eq!(counts.bot_tank, 1, "範囲外の 1449.9 を数えていない");
        // 0 人のロールも 0 として返る（画面が行ごと落とさないため）
        assert_eq!(counts.top_carry, 0);
        assert_eq!(counts.top_support, 0);
        assert_eq!(counts.bot_carry, 0);

        // 幅 50 のウィンドウ [1500, 1550] に 4 人（希望なしの 1 人を含む）。内訳の合計は 3
        assert_eq!(count_in_range(&me, &queue), 4);
        let total: usize = counts.all().into_iter().map(|(_, count)| count).sum();
        assert_eq!(total, 3, "希望を持たない行はどのロールにも入らない");
    }
}
