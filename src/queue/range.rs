//! 「範囲が互いに両立する」の判定（07-マッチング仕様 §3-1）。純粋関数。
//!
//! 10人の組が成立する条件は**全ペアが相互に相手を受け入れること**であり、
//! キュー画面に出す「現在あなたの範囲内でキュー中: N人」も同じ定義で数える。
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

use super::model::UP_RANGE;

/// 範囲判定に必要な、キュー中プレイヤー1人分の値。
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedPlayer {
    /// 誰か。自分自身を数えないためだけに使う。
    pub player_id: PlayerId,
    /// 表示スケールのレート（導出値）。
    pub rating: f64,
    /// 下方向の幅。いまは 50 固定（`clamp_down_range` を通した値が入る）。
    pub down_range: i32,
    /// 第1希望のロール（`roles.id`）。希望が 1 つも無ければ `None`。
    ///
    /// **第1希望だけを持つ。**「希望のどこかに含まれる」を数えると、1 人が複数の
    /// ロールに計上されて合計が人数と合わなくなる。
    pub first_choice: Option<String>,
}

/// `viewer` から見て `other` が範囲内か。**片方向**。
///
/// 単体で人数を数える用途には使わない（[`mutually_in_range`] を使う）。相互条件の
/// 半分であることを明示するために公開してある。
///
/// **境界を含む。**`other.rating` が `viewer.rating ± 50` ちょうどでも範囲内
/// （比較は `>=` / `<=`）。
pub fn accepts(viewer: &QueuedPlayer, other: &QueuedPlayer) -> bool {
    let upper = viewer.rating + f64::from(UP_RANGE);
    let lower = viewer.rating - f64::from(viewer.down_range);
    other.rating >= lower && other.rating <= upper
}

/// 2人が**互いに**相手を受け入れるか。
pub fn mutually_in_range(a: &QueuedPlayer, b: &QueuedPlayer) -> bool {
    accepts(a, b) && accepts(b, a)
}

/// `me` の範囲内で、かつ相手からも受け入れられるキュー中プレイヤーの人数。
///
/// 自分自身は数えない（`queue` に自分が入っていてもよい）。
pub fn count_in_range(me: &QueuedPlayer, queue: &[QueuedPlayer]) -> usize {
    in_range_with(me, queue).count()
}

/// 範囲内の人だけを返す。人数と内訳を**同じ判定**から数えるための共通部分。
fn in_range_with<'a>(
    me: &'a QueuedPlayer,
    queue: &'a [QueuedPlayer],
) -> impl Iterator<Item = &'a QueuedPlayer> {
    queue
        .iter()
        .filter(move |other| other.player_id != me.player_id)
        .filter(move |other| mutually_in_range(me, other))
}

/// 範囲内の人を**第1希望のロール別**に数える。
///
/// 絞り込みは [`count_in_range`] と同じ条件（上下 50 の相互条件）。数え方は
/// 15 分サマリと同じ「第1希望がそのロールの人」で、希望のどこかに含まれる人ではない。
/// 希望を 1 つも持たない行は、どのロールにも数えない。
pub fn count_by_first_choice(me: &QueuedPlayer, queue: &[QueuedPlayer]) -> FirstChoiceRoleCounts {
    let mut counts = FirstChoiceRoleCounts::default();
    for other in in_range_with(me, queue) {
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
    use crate::queue::model::DOWN_RANGE;

    fn player(n: u128, rating: f64) -> QueuedPlayer {
        QueuedPlayer {
            player_id: PlayerId(Uuid::from_u128(n)),
            rating,
            down_range: DOWN_RANGE,
            first_choice: None,
        }
    }

    /// 第1希望つきのキュー中の人。
    fn wanting(n: u128, rating: f64, role: &str) -> QueuedPlayer {
        QueuedPlayer {
            first_choice: Some(role.to_owned()),
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

        // 中間層から見れば両方が範囲内。上下の群は互いに見えない
        let queue = [low.clone(), middle.clone(), high.clone()];
        assert_eq!(count_in_range(&middle, &queue), 2);
        assert_eq!(count_in_range(&low, &queue), 1);
        assert_eq!(count_in_range(&high, &queue), 1);
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
        assert_eq!(count_in_range(&me, &queue), 3);
    }

    /// 旧仕様の広い `down_range` が万一残っていても、丸めた値が渡る前提。
    ///
    /// ここは丸め忘れたときの挙動を記録するためのテスト。`down_range` に 200 が入れば
    /// 下方向だけ広がるが、**上方向は 50 のままなので相互条件は成立しない**。
    #[test]
    fn an_unclamped_wide_value_still_cannot_widen_the_pair() {
        let wide = QueuedPlayer {
            player_id: PlayerId(Uuid::from_u128(1)),
            rating: 1600.0,
            down_range: 200,
            first_choice: None,
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
            wanting(5, 1450.0, "bot_tank"), // -50 ちょうど。範囲内
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

        // 範囲内は 4 人（希望なしの 1 人を含む）。内訳の合計は 3
        assert_eq!(count_in_range(&me, &queue), 4);
        let total: usize = counts.all().into_iter().map(|(_, count)| count).sum();
        assert_eq!(total, 3, "希望を持たない行はどのロールにも入らない");
    }
}
