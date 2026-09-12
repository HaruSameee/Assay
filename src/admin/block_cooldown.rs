//! ブロック解除の待機日数（クールタイム）の変更（運営操作）。**追記専用で、現在値は
//! 最新行から導出する。**
//!
//! 形はマッチング範囲（[`super::matching_range`]）と同じ。効き方も似ていて、
//! 試合には何も影響しない。違いは **既にあるブロックにも効く**こと: 解除できる時刻は
//! `blocked_at + 現在の日数` を読むたびに導出するので（`player_blocks` は書き換えない）、
//! 日数を縮めれば待っている人がすぐ解除できるようになり、伸ばせば解除できたはずの人が
//! また待つことになる。運営の管理解除（`block_override_removed`）は引き続き日数に関係なく効く。
//!
//! 日数を見る場所は SQL の `current_block_cooldown` ビュー 1 か所で、
//! ブロック画面もブロック解除の検証も管理パネルもそこだけを引く。

use chrono::{DateTime, Utc};

use crate::blocks::MAX_BLOCK_COOLDOWN_DAYS;

/// 追記済みの変更 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockCooldownChange {
    pub id: i64,
    /// この行が有効にした日数。
    pub cooldown_days: i32,
    /// 運営が書いた理由。
    pub reason: String,
    /// 操作した運営（`discord:<id>`）。
    pub created_by: String,
    /// 追記した時刻。
    pub created_at: DateTime<Utc>,
}

/// 実行前に画面へ出す値。**すべて導出値**で、どこにも保存しない。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockCooldownPreview {
    /// いま効いている日数。
    pub current_days: i64,
    /// いま有効なブロックの件数。**日数を変えると、この人たちの解除可能時刻が変わる。**
    pub active_blocks: usize,
    /// 変更履歴（新しい順、最大 20 件）。
    pub history: Vec<BlockCooldownChange>,
}

/// 追記の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockCooldownOutcome {
    /// 追記した。
    Changed {
        /// `block_cooldown_changes.id`。
        id: i64,
    },
    /// **同じ冪等キーで既に追記済み。**状態ベースではなくキーだけで判定する。
    Duplicate,
}

/// 入力値の検証エラー。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockCooldownError {
    /// 数値として読めない。
    #[error("待機日数は数値で入力してください")]
    NotANumber,
    /// 範囲外。
    #[error("待機日数は 0 以上 {MAX_BLOCK_COOLDOWN_DAYS} 以下で入力してください（入力: {value}）")]
    OutOfRange {
        /// 入力された値。
        value: i64,
    },
}

/// 画面から来た文字列を検証して日数にする。**純粋関数。**0 は「待たない」。
pub fn parse_cooldown_days(raw: &str) -> Result<i32, BlockCooldownError> {
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| BlockCooldownError::NotANumber)?;
    if !(0..=MAX_BLOCK_COOLDOWN_DAYS).contains(&value) {
        return Err(BlockCooldownError::OutOfRange { value });
    }
    i32::try_from(value).map_err(|_| BlockCooldownError::OutOfRange { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_count_within_the_bounds_is_accepted() {
        assert_eq!(parse_cooldown_days("5"), Ok(5));
        assert_eq!(parse_cooldown_days(" 0 "), Ok(0), "0 は「待たない」");
        assert_eq!(parse_cooldown_days("365"), Ok(365));
    }

    /// 負・上限超え・数値でない、はすべて入力の時点で断る。**SQL の CHECK と同じ範囲。**
    #[test]
    fn a_day_count_that_cannot_be_stored_is_refused() {
        assert_eq!(
            parse_cooldown_days("-1"),
            Err(BlockCooldownError::OutOfRange { value: -1 })
        );
        assert_eq!(
            parse_cooldown_days("366"),
            Err(BlockCooldownError::OutOfRange { value: 366 })
        );
        for bad in ["", "5日", "5.5", "五"] {
            assert_eq!(
                parse_cooldown_days(bad),
                Err(BlockCooldownError::NotANumber),
                "{bad}"
            );
        }
    }
}
