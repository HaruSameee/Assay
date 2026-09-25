//! 端の取り残し対策: **マッチング範囲の判定にだけ使うレート**の底上げ / 引き下げ。
//!
//! 端の人はウィンドウ（07 §3-1）に入る相手が少なく、「あと 1〜2 人足りない」で残り続ける。
//! 走査方向のランダム化（07 §4-3）で緩和したが、そもそも近い帯に人がいなければ組めない。
//! そこでスキャンに取り残されるたびに、判定用のレートを **14 ずつ中央へ寄せる**:
//!
//! - **表示レート 1486 以下**の人は **+14 × 回数**（下の端を上へ）。**1430 以下は 1430 から**始める
//! - **そのスキャンでいちばん上**（実レートが候補の最大）の人は **−14 × 回数**（上の端を下へ）。
//!   実レート 1600 以上は **1550 まで**、1600 未満は **最大 65** まで
//!
//! 寄せた後のレートは**実レートに加えて**試す判定で、置き換えではない。選抜はまず実レートで
//! 組めるかを探し、組めなかった人だけを寄せたレートで見直す（`select::select_matches_inner`）。
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

/// いちばん上の人が下がる幅の上限（表示スケール）。**14 × 4 回 = 56 の次、5 回目で頭打ち。**
///
/// 際限なく下げると、組めない理由がロールの都合だったときに帯を通り過ぎて下へ抜ける。
/// 65 は「ウィンドウ 50 + 1 勝 2 回ぶん」で、隣の帯までは届き、その先へは行かない。
/// **実レート 1600 以上の首位には使わない**（[`HIGH_TOP_FLOOR`] まで下がる）。
pub const TOP_DROP_MAX: f64 = 65.0;

/// 実レートがこれ**以下**の人は、判定用レートを最初から [`LOW_FLOOR`] に置く。
pub const LOW_FLOOR: f64 = 1430.0;

/// これ以上の実レートの首位は、65 の上限の代わりに [`HIGH_TOP_FLOOR`] まで下がる。
pub const HIGH_TOP_MIN_RATING: f64 = 1600.0;

/// 実レート 1600 以上の首位が下がる下限。
pub const HIGH_TOP_FLOOR: f64 = 1550.0;

/// マッチング範囲の判定に使うレート。**純粋関数。**
///
/// `bounds` はそのスキャンの候補全員の実レートの最小と最大（[`Bounds`]）。
///
/// - **実レート 1430 以下**は、取り残されていなくても判定用レートを **1430 から始める**。
///   取り残されるたびの +14 も 1430 に足す（1400 の人が 2 回取り残されれば 1458）
/// - **1431〜1486** は実レート + 14 × 回数
/// - 上げる側はどちらも候補の最大で止める（それ以上上げても意味がない。ただし 1430 は下回らない）
/// - **いちばん上**（実レートが候補の最大）は −14 × 回数。実レート **1600 以上なら 1550 まで**、
///   1600 未満なら **最大 65** まで下がる
/// - それ以外は実レートのまま。**1486 以下が優先**（全員が 1486 以下のキューで、
///   いちばん上だけ下がると逆に離れる）
///
/// **寄せた後のレートで組めなくても、実レートでは組めるかを選抜が必ず先に試す**
/// （`select::select_matches_inner` の 2 周）ので、寄せ込みが取りこぼしを作ることはない。
/// 組まれた時点でカウンタごと消える。
pub fn matching_rating(rating: f64, unmatched_scans: i32, bounds: Bounds) -> f64 {
    let shift = BOOST_PER_UNMATCHED_SCAN * f64::from(unmatched_scans.max(0));
    if rating <= BOOST_MAX_RATING {
        let base = rating.max(LOW_FLOOR);
        (base + shift).min(bounds.top.max(base))
    } else if unmatched_scans > 0 && rating >= bounds.top {
        if rating >= HIGH_TOP_MIN_RATING {
            (rating - shift).max(HIGH_TOP_FLOOR)
        } else {
            rating - shift.min(TOP_DROP_MAX)
        }
    } else {
        rating
    }
}

/// そのスキャンの候補全員の実レートの最小と最大。空なら誰も動かない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// 最小の実レート。上の端はここまで下がる。
    pub bottom: f64,
    /// 最大の実レート。「いちばん上」の判定にも、下の端がここまで上がるのにも使う。
    pub top: f64,
}

impl Bounds {
    /// 候補の実レートから作る。
    pub fn of<'a>(ratings: impl IntoIterator<Item = &'a f64>) -> Self {
        let mut bounds = Self {
            bottom: f64::MAX,
            top: f64::MIN,
        };
        for rating in ratings {
            bounds.bottom = bounds.bottom.min(*rating);
            bounds.top = bounds.top.max(*rating);
        }
        bounds
    }
}

/// 判定用のレートが実レートと変わりうる人か（ログの人数用）。
pub fn is_shifted(rating: f64, bounds: Bounds) -> bool {
    rating <= BOOST_MAX_RATING || rating >= bounds.top
}

#[cfg(test)]
mod tests {
    use super::*;

    fn between(bottom: f64, top: f64) -> Bounds {
        Bounds { bottom, top }
    }

    /// **1486 以下だけ、取り残された回数 x 14 上がる。**
    #[test]
    fn only_low_ratings_are_lifted_by_fourteen_per_scan() {
        let bounds = between(1400.0, 1700.0);
        assert_eq!(matching_rating(1486.0, 0, bounds), 1486.0);
        assert_eq!(matching_rating(1486.0, 1, bounds), 1500.0);
        assert_eq!(matching_rating(1486.0, 3, bounds), 1528.0);
        assert_eq!(matching_rating(1440.0, 2, bounds), 1468.0);
        // 境界: 1486 は対象、1487 は対象外（いちばん上でもなければそのまま）
        assert_eq!(matching_rating(1487.0, 5, bounds), 1487.0);
        assert_eq!(matching_rating(1500.0, 5, bounds), 1500.0);
        // 負の回数は来ないが、来ても下げない
        assert_eq!(matching_rating(1486.0, -1, bounds), 1486.0);
    }

    /// **1430 以下は、取り残されていなくても 1430 から。**加算も 1430 に足す。
    #[test]
    fn ratings_at_or_below_1430_start_from_1430() {
        let bounds = between(1300.0, 1700.0);
        assert_eq!(matching_rating(1400.0, 0, bounds), 1430.0, "0 回でも 1430");
        assert_eq!(matching_rating(1300.0, 0, bounds), 1430.0);
        assert_eq!(matching_rating(1430.0, 0, bounds), 1430.0);
        assert_eq!(matching_rating(1400.0, 2, bounds), 1458.0, "1430 + 28");
        assert_eq!(
            matching_rating(1300.0, 2, bounds),
            1458.0,
            "元が低くても同じ"
        );
        // 1431 は実レートから
        assert_eq!(matching_rating(1431.0, 0, bounds), 1431.0);
        assert_eq!(matching_rating(1431.0, 2, bounds), 1459.0);
        // 候補の最大で止まるが、1430 は下回らない
        let low_queue = between(1300.0, 1420.0);
        assert_eq!(matching_rating(1300.0, 0, low_queue), 1430.0);
        assert_eq!(matching_rating(1300.0, 9, low_queue), 1430.0);
        assert!(is_shifted(1300.0, bounds));
    }

    /// **いちばん上は取り残された回数 x 14 下がる。**2 番目以下は動かない。
    #[test]
    fn only_the_top_rating_is_pulled_down_by_fourteen_per_scan() {
        let bounds = between(1500.0, 1700.0);
        assert_eq!(matching_rating(1700.0, 0, bounds), 1700.0);
        assert_eq!(matching_rating(1700.0, 1, bounds), 1686.0);
        assert_eq!(matching_rating(1700.0, 3, bounds), 1658.0);
        // 2 番目（1699）は何回でも動かない
        assert_eq!(matching_rating(1699.0, 5, bounds), 1699.0);
        // 同率首位はどちらも下がる
        assert_eq!(matching_rating(1700.0, 2, between(1500.0, 1700.0)), 1672.0);
    }

    /// **実レート 1600 以上の首位は 1550 まで**下がる（65 の上限は使わない）。
    /// 1600 未満の首位は従来どおり最大 65。
    #[test]
    fn a_top_at_or_above_1600_drops_to_1550() {
        let bounds = between(1400.0, 1700.0);
        assert_eq!(
            matching_rating(1700.0, 5, bounds),
            1630.0,
            "70 下がる（65 で止めない）"
        );
        assert_eq!(matching_rating(1700.0, 10, bounds), 1560.0);
        assert_eq!(
            matching_rating(1700.0, 11, bounds),
            1550.0,
            "1546 ではなく 1550"
        );
        assert_eq!(matching_rating(1700.0, 50, bounds), 1550.0);
        let top_1600 = between(1400.0, 1600.0);
        assert_eq!(matching_rating(1600.0, 3, top_1600), 1558.0);
        assert_eq!(
            matching_rating(1600.0, 4, top_1600),
            1550.0,
            "1544 ではなく 1550"
        );
        // 1599 の首位は 65 まで（1534 で止まる）
        let top_1599 = between(1400.0, 1599.0);
        assert_eq!(matching_rating(1599.0, 4, top_1599), 1543.0);
        assert_eq!(matching_rating(1599.0, 5, top_1599), 1534.0);
        assert_eq!(matching_rating(1599.0, 30, top_1599), 1534.0);
    }

    /// 1486 以下は候補の最大で止まる。
    #[test]
    fn the_lift_stops_at_the_top_of_the_candidates() {
        let bounds = between(1486.0, 1560.0);
        assert_eq!(matching_rating(1486.0, 5, bounds), 1556.0);
        assert_eq!(
            matching_rating(1486.0, 6, bounds),
            1560.0,
            "1570 まで上がらない"
        );
        // 自分 1 人だけのキュー: 首位は下がる（実レートでの探索が別にある）、1486 は動かない
        let alone = between(1650.0, 1650.0);
        assert_eq!(matching_rating(1650.0, 9, alone), 1550.0);
        let alone_low = between(1486.0, 1486.0);
        assert_eq!(matching_rating(1486.0, 9, alone_low), 1486.0);
    }

    /// **全員が 1486 以下のキューでは、いちばん上も上がる側**（下げると逆に離れる）。
    /// ただし候補の最大より上へは行かないので、首位そのものは動かない。
    #[test]
    fn a_low_queue_lifts_its_top_instead_of_dropping_it() {
        let bounds = between(1440.0, 1486.0);
        assert_eq!(
            matching_rating(1486.0, 2, bounds),
            1486.0,
            "首位は最大に張り付く"
        );
        assert_eq!(matching_rating(1440.0, 2, bounds), 1468.0);
        assert_eq!(
            matching_rating(1440.0, 9, bounds),
            1486.0,
            "首位まで上がって止まる"
        );
    }

    #[test]
    fn bounds_are_the_extremes_and_empty_means_nobody() {
        let bounds = Bounds::of(&[1500.0, 1650.0, 1480.0]);
        assert_eq!(bounds, between(1480.0, 1650.0));
        let empty = Bounds::of(&[]);
        assert_eq!(empty.top, f64::MIN);
        assert_eq!(empty.bottom, f64::MAX);
        assert!(is_shifted(1650.0, bounds));
        assert!(!is_shifted(1600.0, bounds));
        assert!(is_shifted(1486.0, bounds));
    }
}
