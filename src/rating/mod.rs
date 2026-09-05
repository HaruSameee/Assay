//! レーティング再計算の入力。
//!
//! 現在レートを保存しない。ここでは、追記専用の `rating_adjustments` から
//! アルゴリズムが使う初期分布を導出するだけで、対戦結果の更新式は持たない。

use sqlx::PgPool;

use crate::lifecycle::PlayerId;

/// 表示値からライブラリの内部値への変換係数。
///
/// 運営は psql で表示スケールを扱う。例: `1500 / 60 = 25`。
pub const DISPLAY_SCALE: f64 = 60.0;
/// 調整前の表示 mu。内部 mu は 25。
pub const DEFAULT_DISPLAY_MU: f64 = 1500.0;
/// 調整前の表示 sigma。内部 sigma は 25 / 3。
pub const DEFAULT_DISPLAY_SIGMA: f64 = 500.0;

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

    /// 1 プレイヤーの全調整を挿入順で読む。現在値の保存や更新は行わない。
    pub async fn load(&self, player_id: PlayerId) -> Result<Vec<RatingAdjustment>, sqlx::Error> {
        let rows = sqlx::query!(
            "select id, delta_mu, delta_sigma, reverts_id \
             from rating_adjustments where player_id = $1 order by id",
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
}
