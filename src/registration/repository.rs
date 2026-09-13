//! 登録トラックの永続化。
//!
//! - `players` は**可変の参照データ**。行の作成とトレーナー名・登録時刻の UPDATE はここで行う。
//! - `player_declarations` は**追記専用**。申告し直しは新しい行で、現在値は最新行から導出する。
//!   UPDATE / DELETE は書かない（DB 側でも権限とトリガーで二重に拒否される）。
//! - `SELECT ... FOR UPDATE` は使わない。対象テーブルの UPDATE 権限を要求するため、
//!   追記専用テーブルでは**本番でだけ** 42501 で落ちる。直列化が要るなら
//!   `pg_advisory_xact_lock`（ここでは行単位の UPDATE と追記だけなので不要）。
//! - 冪等は「同一 idempotency_key のみ no-op」。申告に冪等キーは無いので、
//!   フォームの再送は**常に新しい行**になる（状態を見て「同じ内容だから捨てる」はしない）。

use serde_json::Value;
use sqlx::PgPool;

use crate::lifecycle::{FormationId, FormationState, PlayerId, Timestamp};
use crate::persistence::{FormationRepository, RepositoryError};

use super::gate::{QueueEligibility, active_formation_blocks_queue, formation_blocks_queue};
use super::model::{Declaration, NewDeclaration, PlayerProfile, PlayerStatus};

/// 登録トラックのエラー。
#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    /// Discord の snowflake として読めない。
    #[error("invalid discord user id")]
    InvalidDiscordUserId,
    /// 対象のプレイヤーが存在しない。
    #[error("player {0} not found")]
    PlayerNotFound(PlayerId),
    /// 永続化層。
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// DB エラー。
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// snowflake（文字列）を `players.discord_user_id`（bigint）に変換する。
///
/// アプリ側は仕様どおり文字列で持ち、変換は DB との境界だけで行う
/// （`players.discord_user_id` は Phase 1 の bigint。既存マイグレーションは書き直さない）。
fn snowflake_to_i64(discord_user_id: &str) -> Result<i64, RegistrationError> {
    discord_user_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .and_then(|id| i64::try_from(id).ok())
        .ok_or(RegistrationError::InvalidDiscordUserId)
}

/// 候補一覧から、進行中ゲートを立てる formation を選ぶ。
///
/// `formations_without_report_from` は `created_at` 昇順で返るので、最も古いものが選ばれる。
/// 同時に 2 つ持てる状態は作らない設計だが、万一あっても 1 つを理由として返せばよい
/// （どれか 1 つでも進行中なら塞ぐ、という判定は同じ）。
fn active_formation_of(
    candidates: &[(FormationId, FormationState)],
    report_gate_disabled: bool,
) -> Option<FormationId> {
    candidates
        .iter()
        .find(|(_, state)| active_formation_blocks_queue(state, report_gate_disabled))
        .map(|(id, _)| *id)
}

/// 登録・認証の永続化。
#[derive(Debug, Clone)]
pub struct RegistrationRepository {
    pool: PgPool,
    formations: FormationRepository,
    report_gate_disabled: bool,
}

impl RegistrationRepository {
    /// プールを包む。接続はアプリ用ロールを想定する。
    pub fn new(pool: PgPool) -> Self {
        let formations = FormationRepository::new(pool.clone());
        Self {
            pool,
            formations,
            report_gate_disabled: false,
        }
    }

    /// 緊急時に報告ゲートだけを無効化する。登録ゲートの判定は常に残る。
    pub fn with_report_gate_disabled(mut self, disabled: bool) -> Self {
        self.report_gate_disabled = disabled;
        self
    }

    /// Discord のユーザー ID からプロフィールを引く。
    pub async fn find_by_discord_user_id(
        &self,
        discord_user_id: &str,
    ) -> Result<Option<PlayerProfile>, RegistrationError> {
        let id = snowflake_to_i64(discord_user_id)?;
        let row = sqlx::query!(
            "select p.id, p.discord_user_id, p.trainer_name, p.status, p.registered_at, \
                    d.declared_at as \"declared_at?\", d.rank as \"rank?\", d.rating as \"rating?\" \
             from players p \
             left join lateral ( \
                 select declared_at, rank, rating from player_declarations \
                 where player_id = p.id order by id desc limit 1 \
             ) d on true \
             where p.discord_user_id = $1",
            id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| PlayerProfile {
            player_id: PlayerId(row.id),
            discord_user_id: row.discord_user_id.to_string(),
            trainer_name: row.trainer_name,
            status: row.status.parse().ok(),
            registered_at: row.registered_at,
            latest_declaration: row.declared_at.map(|declared_at| Declaration {
                declared_at,
                rank: row.rank.and_then(|rank| rank.parse().ok()),
                rating: row.rating,
            }),
        }))
    }

    /// `players.id` からプロフィールを引く（セッションが指すプレイヤー）。
    pub async fn find_by_player_id(
        &self,
        player_id: PlayerId,
    ) -> Result<Option<PlayerProfile>, RegistrationError> {
        let row = sqlx::query!(
            "select p.id, p.discord_user_id, p.trainer_name, p.status, p.registered_at, \
                    d.declared_at as \"declared_at?\", d.rank as \"rank?\", d.rating as \"rating?\" \
             from players p \
             left join lateral ( \
                 select declared_at, rank, rating from player_declarations \
                 where player_id = p.id order by id desc limit 1 \
             ) d on true \
             where p.id = $1",
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| PlayerProfile {
            player_id: PlayerId(row.id),
            discord_user_id: row.discord_user_id.to_string(),
            trainer_name: row.trainer_name,
            status: row.status.parse().ok(),
            registered_at: row.registered_at,
            latest_declaration: row.declared_at.map(|declared_at| Declaration {
                declared_at,
                rank: row.rank.and_then(|rank| rank.parse().ok()),
                rating: row.rating,
            }),
        }))
    }

    /// ログイン時に `players` の行を用意する。既にあればそれを返す。
    ///
    /// この時点では**未登録**（`registered_at` は NULL）。登録フォームの送信で登録済みになる。
    /// セッション Cookie に入れる識別子を確定させるために、ログインの時点で行を作る。
    ///
    /// `on conflict do nothing` は可変の参照データに対する自然キーの upsert であって、
    /// 追記ログの「状態ベースの冪等」ではない（申告は常に追記される）。
    pub async fn ensure_player(
        &self,
        discord_user_id: &str,
        now: Timestamp,
    ) -> Result<PlayerId, RegistrationError> {
        let id = snowflake_to_i64(discord_user_id)?;
        sqlx::query!(
            "insert into players (id, discord_user_id, created_at) values ($1, $2, $3) \
             on conflict (discord_user_id) do nothing",
            uuid::Uuid::new_v4(),
            id,
            now,
        )
        .execute(&self.pool)
        .await?;
        let row = sqlx::query!("select id from players where discord_user_id = $1", id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(RegistrationError::InvalidDiscordUserId)?;
        Ok(PlayerId(row.id))
    }

    /// 接続元を 1 行追記する（[`super::watch`]）。**対象かどうかの判定は呼ぶ側**が
    /// [`super::AddressWatch`] で行う。ここは渡されたら積むだけ。
    pub async fn record_address(
        &self,
        player_id: PlayerId,
        source: super::AddressSource,
        address: &super::ClientAddress,
        seen_at: Timestamp,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "insert into player_address_records \
             (player_id, source, cf_connecting_ip, x_forwarded_for, user_agent, seen_at) \
             values ($1, $2, $3, $4, $5, $6)",
            player_id.0,
            source.as_str(),
            address.cf_connecting_ip.as_deref(),
            address.x_forwarded_for.as_deref(),
            address.user_agent.as_deref(),
            seen_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 登録フォームの送信を記録する。
    ///
    /// 1 トランザクションで、`players` をUPDATE し、`player_declarations` に **1 行追記**する。
    /// `registered_at` は最初の登録時刻を保つ（`coalesce` で上書きしない）。
    /// 再送は毎回新しい申告として積む（内容が同じでも捨てない）。
    pub async fn register(&self, declaration: &NewDeclaration) -> Result<(), RegistrationError> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query!(
            "update players set trainer_name = $2, registered_at = coalesce(registered_at, $3) \
             where id = $1",
            declaration.player_id.0,
            declaration.trainer_name,
            declaration.declared_at,
        )
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(RegistrationError::PlayerNotFound(declaration.player_id));
        }
        sqlx::query!(
            "insert into player_declarations (player_id, declared_at, rank, rating, raw) \
             values ($1, $2, $3, $4, $5)",
            declaration.player_id.0,
            declaration.declared_at,
            declaration.rank.as_str(),
            declaration.rating,
            declaration.raw,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 停止処分の設定（スタッフ操作用）。`players` は可変なので UPDATE してよい。
    pub async fn set_status(
        &self,
        player_id: PlayerId,
        status: PlayerStatus,
    ) -> Result<(), RegistrationError> {
        let updated = sqlx::query!(
            "update players set status = $2 where id = $1",
            player_id.0,
            status.as_str(),
        )
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(RegistrationError::PlayerNotFound(player_id));
        }
        Ok(())
    }

    /// 報告ゲートを塞いでいる formation の一覧。
    pub async fn pending_reports(
        &self,
        player_id: PlayerId,
    ) -> Result<Vec<FormationId>, RegistrationError> {
        let candidates = self
            .formations
            .formations_without_report_from(player_id)
            .await?;
        Ok(candidates
            .into_iter()
            // 本人が報告していない候補だけが返るので、reported = false で判定する
            .filter(|(_, state)| formation_blocks_queue(state, false))
            .map(|(id, _)| id)
            .collect())
    }

    /// 進行中ゲートを塞いでいる formation（まだ試合が終わっていないもの）。
    ///
    /// 1 プレイヤーが同時に持てるのは 1 つだけなので、最初に見つかったものを返す。
    pub async fn active_formation(
        &self,
        player_id: PlayerId,
    ) -> Result<Option<FormationId>, RegistrationError> {
        let candidates = self
            .formations
            .formations_without_report_from(player_id)
            .await?;
        Ok(active_formation_of(&candidates, self.report_gate_disabled))
    }

    /// キューの 3 つのゲートをまとめて評価する（畳まずに理由を保ったまま返す）。
    ///
    /// 進行中ゲートと報告ゲートは同じ一覧から導出する（往復を 1 回で済ませる）。
    ///
    /// **`REPORT_GATE_DISABLED` は報告ゲートだけを無効化する。**進行中ゲートは縮退させず、
    /// むしろ `in_progress` まで広がる（[`super::gate::active_formation_blocks_queue`]）。
    /// 報告ゲートを切ったまま `in_progress` を素通しにすると、試合中の人がキューに入って
    /// 二重割当される——縮退運用の最中に一番分かりにくい形で出る。
    pub async fn queue_eligibility(
        &self,
        player_id: PlayerId,
    ) -> Result<QueueEligibility, RegistrationError> {
        let profile = self.find_by_player_id(player_id).await?;
        if profile.is_none() {
            return Ok(super::gate::evaluate(None, None, Vec::new()));
        }
        let candidates = self
            .formations
            .formations_without_report_from(player_id)
            .await?;
        let active = active_formation_of(&candidates, self.report_gate_disabled);
        let pending = if self.report_gate_disabled {
            Vec::new()
        } else {
            candidates
                .iter()
                // 本人が報告していない候補だけが返るので、reported = false で判定する
                .filter(|(_, state)| formation_blocks_queue(state, false))
                .map(|(id, _)| *id)
                .collect()
        };
        Ok(super::gate::evaluate(profile.as_ref(), active, pending))
    }

    /// 生の申告 JSON を作る補助（フォームの値をそのまま入れる）。
    pub fn raw_declaration(fields: &[(&str, &str)]) -> Value {
        let mut map = serde_json::Map::new();
        for (key, value) in fields {
            map.insert((*key).to_owned(), Value::String((*value).to_owned()));
        }
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflakes_are_parsed_at_the_database_boundary() {
        assert_eq!(
            snowflake_to_i64("900000000000000005").unwrap(),
            900_000_000_000_000_005
        );
        // 0 と非数字と負値と 63 ビット超は弾く
        for invalid in ["", "0", "-1", "abc", "12.3", "18446744073709551615"] {
            assert!(
                matches!(
                    snowflake_to_i64(invalid),
                    Err(RegistrationError::InvalidDiscordUserId)
                ),
                "{invalid}"
            );
        }
    }

    #[test]
    fn the_raw_declaration_keeps_every_submitted_field() {
        let raw = RegistrationRepository::raw_declaration(&[
            ("trainer_name", "トレーナー"),
            ("rank", "legend"),
            ("rating", "1500"),
        ]);
        assert_eq!(raw["trainer_name"], "トレーナー");
        assert_eq!(raw["rank"], "legend");
        // 数値に見えても、送られてきた文字列のまま残す
        assert_eq!(raw["rating"], "1500");
    }
}
