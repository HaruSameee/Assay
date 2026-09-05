//! 「範囲が互いに両立する」の判定（07-マッチング仕様 §3-1）。純粋関数。
//!
//! 10人の組が成立する条件は**全ペアが相互に相手を受け入れること**であり、
//! キュー画面に出す「現在あなたの範囲内でキュー中: N人」も同じ定義で数える。
//! **片方向で数えないこと。**自分の下方向を 200 まで広げても、相手の下方向が 95 のままなら
//! そのペアは成立しない。片方向で数えると、実際には組めない人数を画面に出すことになり、
//! 「範囲を広げたのに待たされる」という形で効く。
//!
//! レートは**表示スケール**（1500 スタート）。レーティングは導出値なので、ここには
//! 現在値を保存する仕組みを置かない——呼び出し側が導出したものを渡す。

use crate::lifecycle::PlayerId;

use super::model::UP_RANGE;

/// 範囲判定に必要な、キュー中プレイヤー1人分の値。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueuedPlayer {
    /// 誰か。自分自身を数えないためだけに使う。
    pub player_id: PlayerId,
    /// 表示スケールのレート（導出値）。
    pub rating: f64,
    /// 本人が設定した下方向の幅。95〜200。
    pub down_range: i32,
}

/// `viewer` から見て `other` が範囲内か。**片方向**。
///
/// 単体で人数を数える用途には使わない（[`mutually_in_range`] を使う）。相互条件の
/// 半分であることを明示するために公開してある。
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
    queue
        .iter()
        .filter(|other| other.player_id != me.player_id)
        .filter(|other| mutually_in_range(me, other))
        .count()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn player(n: u128, rating: f64, down_range: i32) -> QueuedPlayer {
        QueuedPlayer {
            player_id: PlayerId(Uuid::from_u128(n)),
            rating,
            down_range,
        }
    }

    #[test]
    fn the_upward_range_is_fixed_at_ninety_five() {
        let me = player(1, 1500.0, 200);
        // +95 ちょうどは範囲内、+96 は外。下方向をいくら広げても上方向は動かない
        assert!(accepts(&me, &player(2, 1595.0, 95)));
        assert!(!accepts(&me, &player(3, 1596.0, 95)));
    }

    #[test]
    fn the_downward_range_is_what_the_player_set() {
        let narrow = player(1, 1500.0, 95);
        let wide = player(2, 1500.0, 200);
        assert!(accepts(&narrow, &player(3, 1405.0, 95)));
        assert!(!accepts(&narrow, &player(4, 1404.0, 95)));
        assert!(accepts(&wide, &player(5, 1300.0, 95)));
    }

    /// 片方向で数えると通ってしまうケース。**これが相互条件の意味。**
    #[test]
    fn a_one_directional_count_would_overcount_the_wide_player() {
        // 自分は 1600 で下方向 200。相手は 1450 で下方向 95（既定のまま）
        let me = player(1, 1600.0, 200);
        let below = player(2, 1450.0, 95);

        // 自分から見れば範囲内（1600 - 200 = 1400 ≦ 1450）
        assert!(accepts(&me, &below), "one direction passes");
        // だが相手から見ると、自分は上方向 +95 の外（1450 + 95 = 1545 < 1600）
        assert!(!accepts(&below, &me), "the other direction fails");

        assert!(!mutually_in_range(&me, &below));
        assert_eq!(
            count_in_range(&me, &[me, below]),
            0,
            "counting one-directionally would report 1 here"
        );
        // 相手側から数えても同じ 0 でなければならない（対称）
        assert_eq!(count_in_range(&below, &[me, below]), 0);
    }

    #[test]
    fn widening_only_helps_when_the_other_side_is_close_enough() {
        let me = player(1, 1600.0, 200);
        // 相手が下方向を広げれば、同じレート差でも両立する
        let widened = player(2, 1450.0, 200);
        assert!(!accepts(&widened, &me), "up is still +95 for them");
        assert!(!mutually_in_range(&me, &widened), "the cap is symmetric");

        // 上方向 95 に収まる相手なら、下方向の設定に関わらず両立する
        let near = player(3, 1520.0, 95);
        assert!(mutually_in_range(&me, &near));
    }

    #[test]
    fn the_count_excludes_yourself_and_out_of_range_players() {
        let me = player(1, 1500.0, 150);
        let queue = [
            me,                     // 自分は数えない
            player(2, 1500.0, 95),  // 同レート
            player(3, 1595.0, 95),  // +95 ちょうど
            player(4, 1596.0, 95),  // +96。自分の上方向の外
            player(5, 1405.0, 95),  // -95。相手の上方向 1405+95=1500 でちょうど両立
            player(6, 1404.0, 95),  // 相手の上方向 1499 < 1500 なので相互には外
            player(7, 1404.0, 200), // 自分の下方向 1350 の内。相手は 1404+95=1499 < 1500 で外
        ];
        assert_eq!(count_in_range(&me, &queue), 3);

        // 相互条件は対称: どちらから数えても同じ結論になる
        for other in &queue[1..] {
            assert_eq!(
                mutually_in_range(&me, other),
                mutually_in_range(other, &me),
                "{other:?}"
            );
        }
    }

    #[test]
    fn an_empty_queue_counts_zero() {
        let me = player(1, 1500.0, 95);
        assert_eq!(count_in_range(&me, &[]), 0);
        assert_eq!(count_in_range(&me, &[me]), 0);
    }
}
