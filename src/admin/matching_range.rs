//! マッチング範囲の変更（運営操作）。**追記専用で、現在値は最新行から導出する。**
//!
//! レート増減幅（[`super::rating_params`]）と形は同じだが、**効き方が違う**。
//!
//! - 増減幅は `player_match_rating_deltas` を通るので、変えると**過去の全試合が
//!   引き直される**（docs/02 不変条件 3）
//! - マッチング範囲は選抜にしか使わない。変えても**確定済みの試合も進行中の
//!   formation も何も変わらない**。効くのは次のスキャン以降だけである
//!
//! したがって、増減幅と違って「進行中の formation があれば拒否する」ガードは要らない。
//! 代わりにマッチメイカのスキャンと同じ advisory lock を取り、1 回の選抜の途中で
//! 幅が入れ替わらないようにする。
//!
//! 幅を見る場所は SQL の `current_matching_ranges` ビュー 1 か所で、
//! マッチメイカもキュー画面も管理パネルもそこだけを引く。

use chrono::{DateTime, Utc};

use crate::queue::{MAX_RANGE, MIN_RANGE, MatchingRanges};

/// 追記済みの変更 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct MatchingRangeChange {
    pub id: i64,
    /// この行が有効にした上方向の幅。
    pub up_range: i32,
    /// この行が有効にした下方向の幅。
    pub down_range: i32,
    /// 運営が書いた理由。
    pub reason: String,
    /// 操作した運営（`discord:<id>`）。
    pub created_by: String,
    /// 追記した時刻。
    pub created_at: DateTime<Utc>,
}

/// 実行前に画面へ出す値。**すべて導出値**で、どこにも保存しない。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MatchingRangePreview {
    /// いま効いている幅。
    pub current: MatchingRanges,
    /// キュー中の人数。**幅を変えると、この人たちの「範囲内で N 人」が次の
    /// ポーリングで変わる。**試合は 1 件も変わらない
    pub queued: usize,
    /// 変更履歴（新しい順、最大 20 件）。
    pub history: Vec<MatchingRangeChange>,
}

/// 追記の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchingRangeOutcome {
    /// 追記した。
    Changed {
        /// `matching_range_changes.id`。
        id: i64,
    },
    /// **同じ冪等キーで既に追記済み。**状態ベースではなくキーだけで判定する。
    Duplicate,
}

/// 入力値の検証エラー。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatchingRangeError {
    /// 数値として読めない。
    #[error("{field} は数値で入力してください")]
    NotANumber {
        /// どちらの幅か。
        field: &'static str,
    },
    /// 範囲外。
    #[error("{field} は {MIN_RANGE} 以上 {MAX_RANGE} 以下で入力してください（入力: {value}）")]
    OutOfRange {
        /// どちらの幅か。
        field: &'static str,
        /// 入力された値。
        value: i32,
    },
}

/// 画面から来た文字列を検証して [`MatchingRanges`] にする。**純粋関数。**
///
/// 下限を置くのは、狭すぎると 10 人が二度と集まらなくなるため。上限を置くのは、
/// 広すぎると事実上の無制限になり、レート帯の意味が消えるため。
/// どちらも「設定したつもりの挙動にならない」値なので、起動後に気付くのではなく
/// 入力の時点で断る。
pub fn parse_ranges(up: &str, down: &str) -> Result<MatchingRanges, MatchingRangeError> {
    Ok(MatchingRanges {
        up: one("上方向の幅", up)?,
        down: one("下方向の幅", down)?,
    })
}

fn one(field: &'static str, raw: &str) -> Result<i32, MatchingRangeError> {
    let value: i32 = raw
        .trim()
        .parse()
        .map_err(|_| MatchingRangeError::NotANumber { field })?;
    if !(MIN_RANGE..=MAX_RANGE).contains(&value) {
        return Err(MatchingRangeError::OutOfRange { field, value });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_pair_is_accepted() {
        assert_eq!(
            parse_ranges("80", "30").unwrap(),
            MatchingRanges { up: 80, down: 30 }
        );
        // 前後の空白は落とす
        assert_eq!(
            parse_ranges(" 50 ", "\t50\n").unwrap(),
            MatchingRanges::default()
        );
    }

    /// **上下は別々に置ける。**非対称にしても通る（07 §3 の片方向判定が効く）。
    #[test]
    fn the_two_directions_are_independent() {
        let ranges = parse_ranges("200", "10").unwrap();
        assert_eq!((ranges.up, ranges.down), (200, 10));
    }

    /// 狭すぎ・広すぎ・数値でない、はすべて入力の時点で断る。
    #[test]
    fn a_range_that_cannot_work_is_refused() {
        for (up, down) in [
            ("9", "50"),
            ("501", "50"),
            ("0", "50"),
            ("-50", "50"),
            ("50", "9"),
            ("50", "501"),
            ("ひろめ", "50"),
            ("", "50"),
            ("50", ""),
        ] {
            assert!(parse_ranges(up, down).is_err(), "{up:?} / {down:?}");
        }
        assert!(matches!(
            parse_ranges("9", "50").unwrap_err(),
            MatchingRangeError::OutOfRange { value: 9, .. }
        ));
        assert!(matches!(
            parse_ranges("ひろめ", "50").unwrap_err(),
            MatchingRangeError::NotANumber { .. }
        ));
    }

    /// 既定値は 10〜500 の内側。マイグレーションの CHECK と同じ境界であること。
    #[test]
    fn the_defaults_pass_their_own_validation() {
        let defaults = MatchingRanges::default();
        assert!(parse_ranges(&defaults.up.to_string(), &defaults.down.to_string()).is_ok());
        assert!((MIN_RANGE..=MAX_RANGE).contains(&defaults.up));
        assert!((MIN_RANGE..=MAX_RANGE).contains(&defaults.down));
    }
}
