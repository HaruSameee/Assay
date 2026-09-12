//! 低レート帯の取り残し対策: **マッチング範囲の判定にだけ使うレート**の底上げ。
//!
//! 最下位付近の人はウィンドウ（07 §3-1）に入る相手が少なく、「あと 1〜2 人足りない」で
//! 残り続ける。走査方向のランダム化（07 §4-3）で緩和したが、そもそも近い帯に人が
//! いなければ組めない。そこで **表示レート 1486 以下の人に限り**、スキャンに取り残される
//! たびに判定用のレートを +14 する。
//!
//! **実レートには影響しない。**`player_current_ratings` は触らないし、formation に凍結する
//! `mu_at_match`（[`super::select::ProposedPlayer::rating`]）もチーム分けも実レートで行う。
//! 変わるのは「誰と同じウィンドウに入れるか」だけ。
//!
//! 回数は `queue_entries.unmatched_scans`（マッチメイカが走って、その行を見て、組まなかった
//! 回数）。止まっている間は増えず、キューを抜けると消える。

/// この表示レート**以下**の人だけ底上げする。初期値 1500 から 1 敗（−7）を 2 回ぶん。
pub const BOOST_MAX_RATING: f64 = 1486.0;

/// 取り残された 1 回あたりの底上げ幅（表示スケール）。1 勝（+7）を 2 回ぶん。
pub const BOOST_PER_UNMATCHED_SCAN: f64 = 14.0;

/// マッチング範囲の判定に使うレート。**純粋関数。**
///
/// 1486 を超えている人は実レートのまま（回数を持っていても足さない）。
/// 上限は設けない: 近い帯に人が現れるまで 1 スキャンごとに 14 ずつ上がり、
/// 組まれた時点でカウンタごと消える。
pub fn matching_rating(rating: f64, unmatched_scans: i32) -> f64 {
    if rating > BOOST_MAX_RATING || unmatched_scans <= 0 {
        return rating;
    }
    rating + BOOST_PER_UNMATCHED_SCAN * f64::from(unmatched_scans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **1486 以下だけ、取り残された回数 x 14。**
    #[test]
    fn only_low_ratings_are_lifted_by_fourteen_per_scan() {
        assert_eq!(matching_rating(1486.0, 0), 1486.0);
        assert_eq!(matching_rating(1486.0, 1), 1500.0);
        assert_eq!(matching_rating(1486.0, 3), 1528.0);
        assert_eq!(matching_rating(1400.0, 2), 1428.0);
        // 境界: 1486 は対象、1487 は対象外
        assert_eq!(matching_rating(1487.0, 5), 1487.0);
        assert_eq!(matching_rating(1500.0, 5), 1500.0);
        // 負の回数は来ないが、来ても下げない
        assert_eq!(matching_rating(1486.0, -1), 1486.0);
    }
}
