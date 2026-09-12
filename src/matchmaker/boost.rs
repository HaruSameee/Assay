//! 端の取り残し対策: **マッチング範囲の判定にだけ使うレート**の底上げ / 引き下げ。
//!
//! 端の人はウィンドウ（07 §3-1）に入る相手が少なく、「あと 1〜2 人足りない」で残り続ける。
//! 走査方向のランダム化（07 §4-3）で緩和したが、そもそも近い帯に人がいなければ組めない。
//! そこでスキャンに取り残されるたびに、判定用のレートを **14 ずつ中央へ寄せる**:
//!
//! - **表示レート 1486 以下**の人は **+14 × 回数**（下の端を上へ）
//! - **そのスキャンでいちばん上**（実レートが候補の最大）の人は **−14 × 回数**（上の端を下へ）
//!
//! **実レートには影響しない。**`player_current_ratings` は触らないし、formation に凍結する
//! `mu_at_match`（[`super::select::ProposedPlayer::rating`]）もチーム分けも実レートで行う。
//! 変わるのは「誰と同じウィンドウに入れるか」だけ。
//!
//! 回数は `queue_entries.unmatched_scans`（マッチメイカが走って、その行を見て、組まなかった
//! 回数）。止まっている間は増えず、キューを抜けると消える。「いちばん上」はスキャンのたびに
//! 候補全員の実レートから決めるので、上の人が組まれれば次に高い人が引き継ぐ。

/// この表示レート**以下**の人を底上げする。初期値 1500 から 1 敗（−7）を 2 回ぶん。
pub const BOOST_MAX_RATING: f64 = 1486.0;

/// 取り残された 1 回あたりの幅（表示スケール）。1 勝（+7）を 2 回ぶん。
pub const BOOST_PER_UNMATCHED_SCAN: f64 = 14.0;

/// マッチング範囲の判定に使うレート。**純粋関数。**
///
/// `top_rating` はそのスキャンの候補全員の実レートの最大。1486 以下なら上へ、
/// 最大に並ぶなら下へ、それ以外は実レートのまま。**1486 以下が優先**（全員が 1486 以下の
/// キューで、いちばん上だけ下がると逆に離れる）。上限も下限も設けない: 近い帯に人が
/// 現れるまで 1 スキャンごとに 14 ずつ寄り、組まれた時点でカウンタごと消える。
pub fn matching_rating(rating: f64, unmatched_scans: i32, top_rating: f64) -> f64 {
    if unmatched_scans <= 0 {
        return rating;
    }
    let shift = BOOST_PER_UNMATCHED_SCAN * f64::from(unmatched_scans);
    if rating <= BOOST_MAX_RATING {
        rating + shift
    } else if rating >= top_rating {
        rating - shift
    } else {
        rating
    }
}

/// 判定用のレートが実レートと変わる人か（ログの人数用）。
pub fn is_shifted(rating: f64, top_rating: f64) -> bool {
    rating <= BOOST_MAX_RATING || rating >= top_rating
}

/// 候補全員の実レートの最大。空なら `f64::MIN`（誰も「いちばん上」にならない）。
pub fn top_rating<'a>(ratings: impl IntoIterator<Item = &'a f64>) -> f64 {
    ratings.into_iter().copied().fold(f64::MIN, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **1486 以下だけ、取り残された回数 x 14 上がる。**
    #[test]
    fn only_low_ratings_are_lifted_by_fourteen_per_scan() {
        let top = 1700.0;
        assert_eq!(matching_rating(1486.0, 0, top), 1486.0);
        assert_eq!(matching_rating(1486.0, 1, top), 1500.0);
        assert_eq!(matching_rating(1486.0, 3, top), 1528.0);
        assert_eq!(matching_rating(1400.0, 2, top), 1428.0);
        // 境界: 1486 は対象、1487 は対象外（いちばん上でもなければそのまま）
        assert_eq!(matching_rating(1487.0, 5, top), 1487.0);
        assert_eq!(matching_rating(1500.0, 5, top), 1500.0);
        // 負の回数は来ないが、来ても動かさない
        assert_eq!(matching_rating(1486.0, -1, top), 1486.0);
    }

    /// **いちばん上は取り残された回数 x 14 下がる。**2 番目以下は動かない。
    #[test]
    fn only_the_top_rating_is_pulled_down_by_fourteen_per_scan() {
        let top = 1700.0;
        assert_eq!(matching_rating(1700.0, 0, top), 1700.0);
        assert_eq!(matching_rating(1700.0, 1, top), 1686.0);
        assert_eq!(matching_rating(1700.0, 3, top), 1658.0);
        // 2 番目（1699）は何回でも動かない
        assert_eq!(matching_rating(1699.0, 5, top), 1699.0);
        // 同率首位はどちらも下がる
        assert_eq!(matching_rating(1700.0, 2, 1700.0), 1672.0);
    }

    /// **全員が 1486 以下のキューでは、いちばん上も上がる**（下げると逆に離れる）。
    #[test]
    fn a_low_queue_lifts_its_top_instead_of_dropping_it() {
        let top = 1486.0;
        assert_eq!(matching_rating(1486.0, 2, top), 1514.0);
        assert_eq!(matching_rating(1400.0, 2, top), 1428.0);
    }

    #[test]
    fn the_top_rating_is_the_maximum_and_empty_means_nobody() {
        assert_eq!(top_rating(&[1500.0, 1650.0, 1480.0]), 1650.0);
        assert_eq!(top_rating(&[]), f64::MIN);
        assert!(is_shifted(1650.0, 1650.0));
        assert!(!is_shifted(1600.0, 1650.0));
        assert!(is_shifted(1486.0, 1650.0));
    }
}
