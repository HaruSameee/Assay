//! レート増減幅の変更（運営操作）。**追記専用で、現在値は最新行から導出する。**
//!
//! docs/02 の不変条件 1〜3 はすべて維持される。
//!
//! - 不変条件 1（試合ログが唯一の正・追記専用）— 何も消さない。変更も追記
//! - 不変条件 2（レーティングは導出値）— 現在値のテーブルは無いまま。増減幅も列に持たない
//! - 不変条件 3（recompute が唯一の計算経路）— **変更すると過去の全試合が新しい値で
//!   引き直される。**これは互換レイヤの欠落ではなく、不変条件 3 そのものである
//!   （「常に現行 params を全履歴に適用する」）
//!
//! 増減幅を適用する条件は SQL の `current_rating_params` ビュー 1 か所にあり、
//! `player_match_rating_deltas` がそれを引く。Rust 側に条件式は無い。

use chrono::{DateTime, Utc};

/// 現在効いているレート params。**最新行から導出した値**で、保存された「現在値」ではない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatingParams {
    /// 勝ちの増減幅（表示スケール）。負けは同じ幅の逆符号、引き分けは 0。
    pub base_delta: f64,
    /// 版数。変更のたびに 1 つ上がる。
    pub params_version: i32,
}

/// 追記済みの変更 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct RatingParamChange {
    pub id: i64,
    /// この行が有効にした増減幅。
    pub base_delta: f64,
    /// この行で採番された版数。
    pub params_version: i32,
    /// 運営が書いた理由。
    pub reason: String,
    /// 操作した運営（`discord:<id>`）。
    pub created_by: String,
    /// 追記した時刻。
    pub created_at: DateTime<Utc>,
}

/// 実行前に数えて画面に出す値。**すべて導出値**で、どこにも保存しない。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RatingParamsPreview {
    /// いま効いている増減幅と版数。
    pub current_base_delta: f64,
    pub current_params_version: i32,
    /// **引き直される試合数。**いま導出に入っている確定済みの formation。
    pub matches_affected: usize,
    /// **影響を受けるプレイヤー数。**その試合に参加していた人。
    pub players_affected: usize,
    /// 進行中の formation 数（matched / awaiting_host / awaiting_code / in_progress）。
    /// **1 件でもあれば実行を拒否する。**
    pub active_formations: usize,
    /// 未報告の試合数。**`active_formations` の部分集合**（未報告なら必ず `in_progress`）
    /// なので、これ自体は拒否の条件にしていない。画面に内訳として出すだけ。
    pub pending_reports: usize,
    /// 係争中の試合数。**拒否はしないが警告として出す。**裁定の時点で新しい値が
    /// 適用されるので、いま変えると「揉めていた試合だけ別の幅で計算される」ことはない
    /// ——が、運営が把握しないまま変えると説明できなくなる。
    pub disputed: usize,
    /// 直近の変更履歴（新しい順）。空なら一度も変えていない。
    pub history: Vec<RatingParamChange>,
}

impl RatingParamsPreview {
    /// 実行してよいか。**進行中の formation が 1 件でもあれば拒否する。**
    ///
    /// 増減幅は 1 行積んだ瞬間に全プレイヤーのレートを変える。進行中の試合が
    /// あるあいだに変えると、その試合が始まったときのレートと確定時の計算が
    /// 食い違って見え、キュー中の人の範囲判定（±50）も同時に動く。
    /// シーズン境界（[`super::season::SeasonPreview::can_start`]）と同じ条件にしてある。
    pub fn can_change(&self) -> bool {
        self.active_formations == 0
    }
}

/// 変更を追記した結果。
#[derive(Debug, Clone, PartialEq)]
pub enum RatingParamsOutcome {
    /// 追記した。
    Changed {
        /// 追記された行の id。
        id: i64,
        /// 新しい版数。
        params_version: i32,
    },
    /// 同一 `idempotency_key` の行が既にある（no-op）。**状態ベースの判定はしない。**
    Duplicate,
    /// 進行中の formation があるので実行しなかった。**断ったことは記録に残す。**
    Blocked {
        /// 進行中の件数。画面に出して止める。
        active_formations: usize,
    },
}

/// 増減幅の下限（この値そのものは許可しない。0 以下を拒否する）。
///
/// 0 だと勝っても負けてもレートが動かず、レーティングが機能しなくなる。
pub const MIN_BASE_DELTA: f64 = 0.0;
/// 増減幅の上限（この値は許可する）。
///
/// 初期レートが 1500、範囲が上下 50 なので、50 は「1 試合でマッチング範囲を丸ごと
/// 飛び出す」幅にあたる。これ以上を許すと 1 試合で順位が壊れる。
pub const MAX_BASE_DELTA: f64 = 50.0;

/// 入力された増減幅を検証する。使えない値なら `None`。
///
/// **DB の CHECK と同じ範囲**（`base_delta > 0 and base_delta <= 50`）にしてある。
/// アプリ側で弾くのは画面に理由を出すためで、最後の砦は DB 側。
pub fn normalize_base_delta(raw: &str) -> Option<f64> {
    let value: f64 = raw.trim().parse().ok()?;
    if !value.is_finite() || value <= MIN_BASE_DELTA || value > MAX_BASE_DELTA {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usable_delta_is_accepted() {
        for raw in ["7", "7.0", " 12 ", "0.5", "50"] {
            assert!(normalize_base_delta(raw).is_some(), "{raw} が拒否された");
        }
        assert_eq!(normalize_base_delta("7.5"), Some(7.5));
    }

    /// **0 以下と 50 超は拒否する。**設定ミスが黙って通る形にしない。
    #[test]
    fn a_meaningless_delta_is_rejected() {
        for raw in [
            "0",
            "-1",
            "-7",
            "50.1",
            "51",
            "1000",
            "",
            "  ",
            "たくさん",
            "NaN",
            "inf",
        ] {
            assert!(
                normalize_base_delta(raw).is_none(),
                "{raw} が通ってしまった"
            );
        }
    }
}
