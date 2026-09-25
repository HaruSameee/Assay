//! レート増減の補正（運営操作）。**追記専用。**
//!
//! 増減幅（[`super::rating_params`]）を変えると全試合が引き直される。こちらは**期間を
//! 区切って**増減の大きさに上乗せ / 差し引きする: 成立時刻が `[starts_at, ends_at)` に
//! 入る試合で、勝ち `+(基本 + 補正)`・負け `−(基本 + 補正)`。期間が重なる補正は合算し、
//! 大きさは 0 を下回らない。「シーズン終盤の 3 日間だけ ±3」のように使う。
//!
//! 適用の条件は SQL の `player_match_rating_deltas` 1 か所にあり、Rust 側に式は無い。
//! 期間に入っていれば過去の試合にも効く（docs/02 不変条件 3）。取り消しは
//! `rating_delta_correction_revocations` に 1 行積む。

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};

/// 補正の大きさの上限（絶対値）。SQL の CHECK と同じ。
pub const MAX_CORRECTION: f64 = 50.0;

/// JST。運営は日本時間で期間を言う。
const JST_OFFSET_SECONDS: i32 = 9 * 3600;

/// 補正 1 件（効いているもの、または取り消し済み）。
#[derive(Debug, Clone, PartialEq)]
pub struct RatingCorrection {
    pub id: i64,
    /// 増減の大きさへ足す値。正なら大きく、負なら小さく。
    pub amount: f64,
    pub starts_at: DateTime<Utc>,
    /// `None` なら終わりなし。
    pub ends_at: Option<DateTime<Utc>>,
    pub reason: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    /// 取り消し済みなら、その時刻。
    pub revoked_at: Option<DateTime<Utc>>,
    /// 期間に入っている確定済みの試合の数（現在のシーズン）。**導出値。**
    pub matches_in_period: usize,
}

impl RatingCorrection {
    /// いま効いているか（取り消されていない）。
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// 画面に出す値。**すべて導出値。**
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RatingCorrectionsPreview {
    /// いまの基本の増減幅。
    pub base_delta: f64,
    /// 補正（新しい順。取り消し済みも含む、最大 50 件）。
    pub corrections: Vec<RatingCorrection>,
}

/// 追記の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RatingCorrectionOutcome {
    /// 追記した。
    Changed {
        /// 補正（または取り消し）の id。
        id: i64,
    },
    /// 同じ冪等キーで既に追記済み。
    Duplicate,
    /// 取り消そうとした補正が無い、または既に取り消し済み。
    NotActive,
}

/// 入力値の検証エラー。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RatingCorrectionError {
    #[error("補正値は数値で入力してください（例: +3 / -2 / 1.5）")]
    NotANumber,
    #[error("補正値は 0 以外、絶対値 {MAX_CORRECTION} 以下で入力してください")]
    OutOfRange,
    #[error("開始日時の形式が不正です")]
    BadStart,
    #[error("終了日時の形式が不正です")]
    BadEnd,
    #[error("終了日時は開始日時より後にしてください")]
    EndBeforeStart,
}

/// 検証済みの入力。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorrectionInput {
    pub amount: f64,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
}

/// 画面から来た値を検証する。**純粋関数。**日時は JST の `YYYY-MM-DDTHH:MM`
/// （`<input type="datetime-local">` の形）。終了は空なら「終わりなし」。
pub fn parse_correction(
    amount: &str,
    starts_at: &str,
    ends_at: &str,
) -> Result<CorrectionInput, RatingCorrectionError> {
    let amount: f64 = amount
        .trim()
        .trim_start_matches('+')
        .parse()
        .map_err(|_| RatingCorrectionError::NotANumber)?;
    if !amount.is_finite() || amount == 0.0 || amount.abs() > MAX_CORRECTION {
        return Err(RatingCorrectionError::OutOfRange);
    }
    let starts_at = parse_jst(starts_at).ok_or(RatingCorrectionError::BadStart)?;
    let ends_at = if ends_at.trim().is_empty() {
        None
    } else {
        Some(parse_jst(ends_at).ok_or(RatingCorrectionError::BadEnd)?)
    };
    if ends_at.is_some_and(|end| end <= starts_at) {
        return Err(RatingCorrectionError::EndBeforeStart);
    }
    Ok(CorrectionInput {
        amount,
        starts_at,
        ends_at,
    })
}

/// JST の `YYYY-MM-DDTHH:MM`（秒付きも可）を UTC にする。
pub fn parse_jst(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    FixedOffset::east_opt(JST_OFFSET_SECONDS)?
        .from_local_datetime(&naive)
        .single()
        .map(|at| at.with_timezone(&Utc))
}

/// 画面の `datetime-local` に入れる形（JST）。
pub fn jst_input_value(at: DateTime<Utc>) -> String {
    at.with_timezone(&FixedOffset::east_opt(JST_OFFSET_SECONDS).expect("valid"))
        .format("%Y-%m-%dT%H:%M")
        .to_string()
}

/// 補正込みの増減の大きさ。**SQL と同じ式**（`greatest(base + amount, 0)`）。
pub fn corrected_delta(base_delta: f64, amount: f64) -> f64 {
    (base_delta + amount).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_amount_and_a_jst_period_are_accepted() {
        let input = parse_correction("+3", "2026-09-27T21:00", "2026-09-28T02:00").unwrap();
        assert_eq!(input.amount, 3.0);
        assert_eq!(
            input.starts_at,
            Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap(),
            "JST 21:00 = UTC 12:00"
        );
        assert_eq!(
            input.ends_at,
            Some(Utc.with_ymd_and_hms(2026, 9, 27, 17, 0, 0).unwrap())
        );
        let open = parse_correction("-2.5", "2026-09-27T21:00", " ").unwrap();
        assert_eq!(open.amount, -2.5);
        assert_eq!(open.ends_at, None, "空なら終わりなし");
    }

    #[test]
    fn nonsense_is_refused() {
        assert_eq!(
            parse_correction("abc", "2026-09-27T21:00", ""),
            Err(RatingCorrectionError::NotANumber)
        );
        assert_eq!(
            parse_correction("0", "2026-09-27T21:00", ""),
            Err(RatingCorrectionError::OutOfRange)
        );
        assert_eq!(
            parse_correction("51", "2026-09-27T21:00", ""),
            Err(RatingCorrectionError::OutOfRange)
        );
        assert_eq!(
            parse_correction("3", "昨日", ""),
            Err(RatingCorrectionError::BadStart)
        );
        assert_eq!(
            parse_correction("3", "2026-09-27T21:00", "x"),
            Err(RatingCorrectionError::BadEnd)
        );
        assert_eq!(
            parse_correction("3", "2026-09-27T21:00", "2026-09-27T21:00"),
            Err(RatingCorrectionError::EndBeforeStart)
        );
    }

    #[test]
    fn the_corrected_delta_never_goes_below_zero() {
        assert_eq!(corrected_delta(7.0, 3.0), 10.0);
        assert_eq!(corrected_delta(7.0, -2.0), 5.0);
        assert_eq!(corrected_delta(7.0, -10.0), 0.0);
    }

    #[test]
    fn the_input_value_round_trips() {
        let at = Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap();
        assert_eq!(jst_input_value(at), "2026-09-27T21:00");
        assert_eq!(parse_jst(&jst_input_value(at)), Some(at));
    }
}
