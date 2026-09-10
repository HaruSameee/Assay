//! レーティングの導出。**現在レートを保存しない。**
//!
//! 現在の表示レートは、追記済みのデータだけから毎回組み立てる。
//!
//! ```text
//! 現在レート = 1500 + Σ rating_adjustments.delta_mu + Σ 確定した試合の増減（07 §2-2）
//! ```
//!
//! 不変条件（00 §3）:
//! - **レーティングは導出値**。現在値のテーブルを持たない。
//! - **recompute が唯一の計算経路**。インクリメンタル更新を作らない。1 試合ぶんの増減は
//!   [`match_delta`] が凍結値だけから決めるので、既存の値を読んで書き戻す経路が要らない。
//! - **初期値は一律 1500**。1600 は `rating_adjustments` への `+100` の追記であって、
//!   第二の初期値ではない（07 §2-1）。
//!
//! **畳み込みは順序に依存しない。**増減の入力は `formation_players.mu_at_match`
//! （その試合の開始時点の凍結値）だけで、途中結果を参照しない。したがって全体は単なる総和で、
//! 同じログからは必ず同じ値が出る。07 §7 が心配している「同一時刻の tiebreak」は、
//! レートの導出には影響しない。
//!
//! SQL 側の同じ式は `migrations/20260906160000_rating_from_matches` のビューにある
//! （キューとマッチメイカが人数ぶんまとめて読むため）。両者が一致することは
//! `tests/rating.rs` が検査する。

use sqlx::PgPool;

use crate::lifecycle::{FormationId, PlayerId};

/// 表示値からライブラリの内部値への変換係数。
///
/// 運営は psql で表示スケールを扱う。例: `1500 / 60 = 25`。
pub const DISPLAY_SCALE: f64 = 60.0;
/// 調整前の表示 mu。内部 mu は 25。
pub const DEFAULT_DISPLAY_MU: f64 = 1500.0;
/// 調整前の表示 sigma。内部 sigma は 25 / 3。
pub const DEFAULT_DISPLAY_SIGMA: f64 = 500.0;

// ----------------------------------------------------------------------
// 初週の固定増減方式（07 §2-2）
// ----------------------------------------------------------------------

/// 固定増減方式のパラメータ版数。**下の値を変えたら必ず上げること。**
///
/// | 版 | 基本 | 格上・格下の補正 |
/// |---|---|---|
/// | 1 | 11 | ±1（しきい値 50） |
/// | 2 | 7 | ±1（しきい値 50） |
/// | 3 | 7 | **なし（フラット）** |
///
/// **`formations` には凍結しない。**レーティングは導出値で、`recompute` が唯一の計算経路
/// （00 §3-2 / §3-3）。params を変えれば過去の全試合が**新しい params で引き直される**のが
/// 意図した挙動であり、「当時の params で再生する」互換レイヤは作らない。したがって
/// 試合ごとに版数を凍結すると、どこからも読まれないうえに「当時の値で再生できる」と
/// 誤読させる列になる。ここは「いまの recompute が使っている版」を 1 か所に置くだけ。
///
/// マッチメイキング側の版数は別物（[`crate::matchmaker::MATCHMAKING_PARAMS_VERSION`]。
/// あちらは選択の費用と範囲の版で、`formations` に凍結する）。
pub const RATING_PARAMS_VERSION: i32 = 3;

/// 増減幅（表示スケール）。**勝ち +7 / 負け −7 のフラット。**
///
/// v3 で格上・格下の補正を削除したので、しきい値も補正幅も、境界比較のための
/// イプシロンも要らなくなった（`RATING_GAP_THRESHOLD` / `UPSET_ADJUSTMENT` /
/// `GAP_EPSILON` は削除済み）。相手チーム平均との差は**増減に影響しない**。
pub const BASE_DELTA: f64 = 7.0;

/// 確定した 1 試合の、そのプレイヤーから見た結果。
///
/// `nullified` / `voided` / `abandoned` / `disputed` はここに現れない——試合が成立して
/// いないか、まだ確定していないので、レートに影響しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    /// 自分のチームが勝った。
    Won,
    /// 相手のチームが勝った。
    Lost,
    /// 引き分け。増減 0。
    Drawn,
}

/// 07 §2-2 の増減式（params v3）。表示スケールの差分を返す。
///
/// | 条件 | 勝ち | 負け |
/// |---|---|---|
/// | **相手チーム平均との差に関わらず** | +7 | −7 |
///
/// v3 で格上・格下の分岐を削除した。**引数の `own` / `opponent_mean` は増減を変えない。**
/// 残してあるのは、試合ごとの入力（[`PlayedMatch`]）が凍結値だけから作られていることを
/// 型で示し続けるためと、07 §7 の比較で μ+σ 方式へ同じログを流すときに同じ形の入力が
/// 要るため。**現在レートは使わない**（これがインクリメンタル更新を不要にしている）。
///
/// 分岐が無いので、境界（差ちょうど 50）の比較演算子も浮動小数点の誤差も関係しない。
/// **増減幅は実行時の値を渡すこと。**運営が管理パネルから変えられるようになったので、
/// [`BASE_DELTA`] は「まだ一度も変えていないときの既定値」でしかない。正は
/// `current_rating_params`（`rating_param_changes` の最新行から導出）で、
/// アプリはそれを読んで渡す。
pub fn match_delta(_own: f64, _opponent_mean: f64, result: MatchResult, base_delta: f64) -> f64 {
    match result {
        MatchResult::Drawn => 0.0,
        MatchResult::Won => base_delta,
        MatchResult::Lost => -base_delta,
    }
}

/// 確定した 1 試合ぶんの入力。すべて凍結値から作る。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayedMatch {
    /// どの formation か。順序には使わない（総和なので不要）。読み手の照合用。
    pub formation_id: FormationId,
    /// 自分の表示レート（`mu_at_match * DISPLAY_SCALE`）。
    pub own_rating: f64,
    /// 相手チーム 5 人の表示レートの平均。
    pub opponent_mean_rating: f64,
    /// 自分から見た結果。
    pub result: MatchResult,
}

impl PlayedMatch {
    /// この試合ぶんの増減。**現行の増減幅を渡すこと。**
    pub fn delta(&self, base_delta: f64) -> f64 {
        match_delta(
            self.own_rating,
            self.opponent_mean_rating,
            self.result,
            base_delta,
        )
    }
}

/// 現在の表示レート。初期値（調整込み）に確定した試合の増減を足す。
///
/// **総和なので、`matches` の順序は結果を変えない。**
///
/// `base_delta` は現行の増減幅。**過去の試合にも同じ値を適用する**（当時の値では
/// 再生しない。docs/02 の不変条件 3）。
pub fn display_rating(initial: InitialRating, matches: &[PlayedMatch], base_delta: f64) -> f64 {
    initial.mu * DISPLAY_SCALE
        + matches
            .iter()
            .map(|played| played.delta(base_delta))
            .sum::<f64>()
}

/// 永続化された調整の、再計算に必要な列。
#[derive(Debug, Clone, PartialEq)]
pub struct RatingAdjustment {
    /// 挿入順の識別子。
    pub id: i64,
    /// 表示スケールの mu 差分。
    pub delta_mu: f64,
    /// 表示スケールの sigma 差分。
    pub delta_sigma: f64,
    /// 打ち消す元の調整。DB トリガーが逆符号の差分を保証する。
    pub reverts_id: Option<i64>,
}

/// 現行のレーティングアルゴリズムに渡す初期分布（内部スケール）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InitialRating {
    pub mu: f64,
    pub sigma: f64,
}

/// rating_adjustments を表示スケールの合計から内部スケールの初期分布に変換する。
///
/// 打ち消し行はDBで元の逆符号を要求されるため、全行を合算するだけで対象を相殺する。
/// 読み取り順は `id` だが、結果は順序に依存しない。純粋関数なので再計算を繰り返しても同じ入力から同じ値になる。
pub fn initial_rating(adjustments: impl IntoIterator<Item = RatingAdjustment>) -> InitialRating {
    let (mu, sigma) = adjustments.into_iter().fold(
        (DEFAULT_DISPLAY_MU, DEFAULT_DISPLAY_SIGMA),
        |(mu, sigma), adjustment| (mu + adjustment.delta_mu, sigma + adjustment.delta_sigma),
    );
    InitialRating {
        mu: mu / DISPLAY_SCALE,
        sigma: sigma / DISPLAY_SCALE,
    }
}

/// レーティング再計算が読む調整ログ。
#[derive(Debug, Clone)]
pub struct RatingAdjustmentRepository {
    pool: PgPool,
}

impl RatingAdjustmentRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 1 プレイヤーの、**現在のシーズンに入る**調整を挿入順で読む。
    ///
    /// 読むのは `season_rating_adjustments` ビュー（`rating_adjustments` を直接読まない）。
    /// 境界より前の調整はログに残ったまま、ここに現れない。
    ///
    /// ビュー越しなので sqlx が NOT NULL を推論できない。元の列はすべて NOT NULL なので
    /// `!` で明示する（`reverts_id` だけは元から nullable）。
    pub async fn load(&self, player_id: PlayerId) -> Result<Vec<RatingAdjustment>, sqlx::Error> {
        let rows = sqlx::query!(
            "select id as \"id!\", delta_mu as \"delta_mu!\", \
                    delta_sigma as \"delta_sigma!\", reverts_id \
             from season_rating_adjustments where player_id = $1 order by id",
            player_id.0,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| RatingAdjustment {
                id: row.id,
                delta_mu: row.delta_mu,
                delta_sigma: row.delta_sigma,
                reverts_id: row.reverts_id,
            })
            .collect())
    }

    /// 全調整を読んで初期分布を導出する。レーティングの更新結果は保存しない。
    pub async fn initial_rating(&self, player_id: PlayerId) -> Result<InitialRating, sqlx::Error> {
        Ok(initial_rating(self.load(player_id).await?))
    }

    /// このプレイヤーの、確定した試合の一覧（07 §2-2 の入力）。
    ///
    /// `player_match_rating_deltas` ビューが確定判定・相手平均・凍結値の取り出しを行う。
    /// ビューは実体を持たないので、呼ぶたびに追記ログから計算し直される。
    pub async fn played_matches(
        &self,
        player_id: PlayerId,
    ) -> Result<Vec<PlayedMatch>, sqlx::Error> {
        let rows = sqlx::query!(
            "select formation_id as \"formation_id!\", team as \"team!\", \
                    outcome as \"outcome!\", own_rating as \"own_rating!\", \
                    opponent_mean_rating as \"opponent_mean_rating!\" \
             from player_match_rating_deltas where player_id = $1 \
             order by formation_id",
            player_id.0,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| PlayedMatch {
                formation_id: FormationId(row.formation_id),
                own_rating: row.own_rating,
                opponent_mean_rating: row.opponent_mean_rating,
                result: match row.outcome.as_str() {
                    "draw" => MatchResult::Drawn,
                    winner if winner == row.team => MatchResult::Won,
                    _ => MatchResult::Lost,
                },
            })
            .collect())
    }

    /// 現在の表示レート。
    ///
    /// **合算は `player_current_ratings` に集約してある。**ここで式を組み立て直さない
    /// ——キュー・マッチメイカ・マッチ画面・管理パネル・プロフィール・ランキングも
    /// すべて同じビューを読む（導出の入口が増えたときに 1 か所だけ直せば済む）。
    /// 純粋関数の [`display_rating`] は式そのものの単体テスト用に残してある。
    pub async fn display_rating(&self, player_id: PlayerId) -> Result<f64, sqlx::Error> {
        // ビューは `players` を主にしているので、行が無いのは players に居ないときだけ
        let rating = sqlx::query_scalar!(
            "select rating as \"rating!\" from player_current_ratings where player_id = $1",
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(rating.unwrap_or(DEFAULT_DISPLAY_MU))
    }

    /// 現行の増減幅。`current_rating_params`（`rating_param_changes` の最新行）から導出する。
    ///
    /// 環境変数もコード上の定数も、ここでは参照しない。[`BASE_DELTA`] は
    /// 「まだ一度も変更していないとき」に SQL 側の `coalesce` が返す既定値でしかない。
    pub async fn base_delta(&self) -> Result<f64, sqlx::Error> {
        let row = sqlx::query!("select base_delta as \"base_delta!\" from current_rating_params")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.base_delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_scale_adjustments_are_converted_after_summing() {
        let rating = initial_rating([
            RatingAdjustment {
                id: 1,
                delta_mu: 180.0,
                delta_sigma: -60.0,
                reverts_id: None,
            },
            RatingAdjustment {
                id: 2,
                delta_mu: -180.0,
                delta_sigma: 60.0,
                reverts_id: Some(1),
            },
            RatingAdjustment {
                id: 3,
                delta_mu: 60.0,
                delta_sigma: -30.0,
                reverts_id: None,
            },
        ]);

        assert_eq!(rating.mu, 26.0);
        assert_eq!(rating.sigma, 47.0 / 6.0);
    }

    // ------------------------------------------------------------------
    // 07 §2-2 の増減式
    // ------------------------------------------------------------------

    fn played(own: f64, opponent_mean: f64, result: MatchResult) -> PlayedMatch {
        PlayedMatch {
            formation_id: FormationId(uuid::Uuid::from_u128(1)),
            own_rating: own,
            opponent_mean_rating: opponent_mean,
            result,
        }
    }

    /// 差が 50 未満なら ±7（v2）。
    #[test]
    fn an_even_match_moves_the_base_amount() {
        for gap in [0.0, 1.0, -1.0, 49.0, -49.0, 49.999] {
            assert_eq!(
                match_delta(1500.0, 1500.0 + gap, MatchResult::Won, BASE_DELTA),
                7.0,
                "gap {gap}"
            );
            assert_eq!(
                match_delta(1500.0, 1500.0 + gap, MatchResult::Lost, BASE_DELTA),
                -7.0,
                "gap {gap}"
            );
        }
    }

    /// **格上でも増減は変わらない。**v3 で分岐を削除したことをここが守る。
    #[test]
    fn beating_a_stronger_opponent_pays_exactly_the_same() {
        for gap in [50.0, 51.0, 300.0, 1000.0] {
            assert_eq!(
                match_delta(1500.0, 1500.0 + gap, MatchResult::Won, BASE_DELTA),
                7.0,
                "gap {gap}"
            );
            assert_eq!(
                match_delta(1500.0, 1500.0 + gap, MatchResult::Lost, BASE_DELTA),
                -7.0,
                "gap {gap}"
            );
        }
    }

    /// **格下でも増減は変わらない。**
    #[test]
    fn beating_a_weaker_opponent_pays_exactly_the_same() {
        for gap in [-50.0, -51.0, -300.0, -1000.0] {
            assert_eq!(
                match_delta(1500.0, 1500.0 + gap, MatchResult::Won, BASE_DELTA),
                7.0,
                "gap {gap}"
            );
            assert_eq!(
                match_delta(1500.0, 1500.0 + gap, MatchResult::Lost, BASE_DELTA),
                -7.0,
                "gap {gap}"
            );
        }
    }

    /// 引き分けは双方とも増減 0（07 §2-3）。格差があっても動かない。
    #[test]
    fn a_draw_moves_nobody() {
        for gap in [0.0, 200.0, -200.0] {
            assert_eq!(
                match_delta(1500.0, 1500.0 + gap, MatchResult::Drawn, BASE_DELTA),
                0.0
            );
        }
    }

    /// ちょうど 50 は「50 以上」に入る（境界）。
    #[test]
    fn there_is_no_threshold_left_to_be_inclusive_about() {
        // かつて境界だった 50 の周辺でも、値は一切変わらない
        for opponent_mean in [1449.9, 1450.0, 1450.1, 1549.9, 1550.0, 1550.1] {
            assert_eq!(
                match_delta(1500.0, opponent_mean, MatchResult::Won, BASE_DELTA),
                BASE_DELTA,
                "{opponent_mean}"
            );
            assert_eq!(
                match_delta(1500.0, opponent_mean, MatchResult::Lost, BASE_DELTA),
                -BASE_DELTA,
                "{opponent_mean}"
            );
        }
    }

    // ------------------------------------------------------------------
    // 畳み込み
    // ------------------------------------------------------------------

    /// 1 勝した 1500 は 1507 になる。**範囲 50 では 1600 群にまだ届かない。**
    ///
    /// 範囲を 95 から 50 に絞ったので、07 §3-2 の「1 勝でプールが融合する」は成立しなくなった。
    /// 1600 と両立するには 1550 以上、つまり**8 勝**が要る。seed の差 100 で 2 群を分離する、
    /// という今回の狙いどおりの帰結であり、意図した挙動。
    #[test]
    fn one_win_is_not_enough_to_reach_the_upper_pool() {
        let initial = initial_rating([]);
        assert_eq!(initial.mu * DISPLAY_SCALE, 1500.0);
        let rating = display_rating(
            initial,
            &[played(1500.0, 1500.0, MatchResult::Won)],
            BASE_DELTA,
        );
        assert_eq!(rating, 1507.0);
        assert!(1600.0 - rating > 50.0, "1 勝では 1600 と両立しない");

        // 8 勝（+56）で 1556。1600 との差 44 が範囲 50 の内側に入る
        let eight_wins: Vec<PlayedMatch> = (0..8)
            .map(|_| played(1500.0, 1500.0, MatchResult::Won))
            .collect();
        let climbed = display_rating(initial_rating([]), &eight_wins, BASE_DELTA);
        assert_eq!(climbed, 1556.0);
        assert!(1600.0 - climbed <= 50.0);
    }

    /// 総和なので、畳む順序は結果を変えない。同じログからは必ず同じ値。
    #[test]
    fn the_fold_does_not_depend_on_the_order() {
        let matches = [
            played(1500.0, 1600.0, MatchResult::Won),
            played(1512.0, 1400.0, MatchResult::Lost),
            played(1500.0, 1500.0, MatchResult::Drawn),
            played(1500.0, 1500.0, MatchResult::Won),
        ];
        let forward = display_rating(initial_rating([]), &matches, BASE_DELTA);
        let mut reversed = matches;
        reversed.reverse();
        let backward = display_rating(initial_rating([]), &reversed, BASE_DELTA);
        assert_eq!(forward, backward);
        // +7, -7, 0, +7
        assert_eq!(forward, 1500.0 + 7.0);
    }

    /// 実績考慮の +100 は第二の初期値ではなく、調整ログの 1 行として効く（07 §2-1）。
    #[test]
    fn the_manual_adjustment_shifts_the_starting_point_only() {
        let seeded = initial_rating([RatingAdjustment {
            id: 1,
            delta_mu: 100.0,
            delta_sigma: 0.0,
            reverts_id: None,
        }]);
        assert_eq!(seeded.mu * DISPLAY_SCALE, 1600.0);
        // 1600 の人が同格に勝てば 1607
        assert_eq!(
            display_rating(
                seeded,
                &[played(1600.0, 1600.0, MatchResult::Won)],
                BASE_DELTA
            ),
            1607.0
        );
    }

    /// 試合が無ければ初期値のまま。
    #[test]
    fn a_player_who_never_played_keeps_the_starting_rating() {
        assert_eq!(display_rating(initial_rating([]), &[], BASE_DELTA), 1500.0);
    }
}
