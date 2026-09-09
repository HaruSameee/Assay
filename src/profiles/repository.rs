//! プロフィールの永続化。**追記だけ。**現在値は最新行から導出する。
//!
//! 不変条件:
//! - `player_discord_snapshots` / `player_profiles` は追記専用。UPDATE も DELETE も書かない
//!   （アプリ用ロールには権限が無く、所有者にもトリガーが効く）
//! - **現在値の列を持たない。**「いまの表示名」も「いまの公開可否」も最新行から導出する
//! - レートと試合数は既存の導出経路（`player_match_rating_deltas` /
//!   `season_rating_adjustments`）をそのまま読む。集計表もキャッシュも作らない

use sqlx::PgPool;
use uuid::Uuid;

use crate::lifecycle::PlayerId;
use crate::rating::DEFAULT_DISPLAY_MU;

use super::model::{DiscordAppearance, ProfileEdit, PublicProfile, avatar_url};

/// プロフィールの読み書き。接続はアプリ用ロールを想定する。
#[derive(Debug, Clone)]
pub struct ProfileRepository {
    pool: PgPool,
}

/// 編集画面に出す現在値。**公開ビューとは別の型**（担当ロールと公開可否をそのまま持つ）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnProfile {
    pub bio: String,
    pub x_id: Option<String>,
    pub main_roles: Vec<String>,
    /// **本人にだけ見せる。**公開ビューには載せない。
    pub main_roles_public: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerDiscordIdentity {
    pub player_id: PlayerId,
    pub discord_user_id: i64,
}

impl ProfileRepository {
    /// プールを包む。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// ログインのたびに Discord 側の見た目を積む。
    ///
    /// **失敗してもログインを止めない**のは呼ぶ側の責任（ここは Err を返すだけ）。
    /// 同じ内容でも毎回積む — 「いつ見に来たか」も記録の一部で、
    /// 変化が無いことを後から言えるようにしておく。
    pub async fn record_appearance(
        &self,
        player_id: PlayerId,
        display_name: &str,
        avatar_hash: Option<&str>,
        captured_at: crate::lifecycle::Timestamp,
    ) -> Result<(), sqlx::Error> {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            // CHECK に触れるので、そもそも積まない（ログインは通す）
            return Ok(());
        }
        sqlx::query!(
            "insert into player_discord_snapshots \
             (player_id, display_name, avatar_hash, captured_at) values ($1, $2, $3, $4)",
            player_id.0,
            display_name,
            avatar_hash,
            captured_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// いまの見た目。**最新行から導出する。**まだ 1 行も無ければ `None`。
    pub async fn appearance(
        &self,
        player_id: PlayerId,
    ) -> Result<Option<DiscordAppearance>, sqlx::Error> {
        let row = sqlx::query!(
            "select display_name, avatar_hash from player_discord_snapshots \
             where player_id = $1 order by id desc limit 1",
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| DiscordAppearance {
            display_name: row.display_name,
            avatar_hash: row.avatar_hash,
        }))
    }

    /// Existing players without any appearance snapshot. This only identifies
    /// rows for a one-time fetch; writes still use [`Self::record_appearance`].
    pub async fn players_without_appearance(
        &self,
    ) -> Result<Vec<PlayerDiscordIdentity>, sqlx::Error> {
        let rows = sqlx::query!(
            "select p.id, p.discord_user_id from players p \
             where not exists ( \
                 select 1 from player_discord_snapshots s where s.player_id = p.id \
             ) order by p.id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| PlayerDiscordIdentity {
                player_id: PlayerId(row.id),
                discord_user_id: row.discord_user_id,
            })
            .collect())
    }

    /// 編集内容を追記する。**上書きしない。**
    pub async fn save(
        &self,
        player_id: PlayerId,
        edit: &ProfileEdit,
        edited_at: crate::lifecycle::Timestamp,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "insert into player_profiles \
             (player_id, bio, x_id, main_roles, main_roles_public, edited_at) \
             values ($1, $2, $3, $4, $5, $6)",
            player_id.0,
            edit.bio,
            edit.x_id.as_deref(),
            serde_json::to_value(&edit.main_roles).unwrap_or_else(|_| serde_json::json!([])),
            edit.main_roles_public,
            edited_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 本人の編集画面に出す現在値。**最新行から導出する。**
    pub async fn own(&self, player_id: PlayerId) -> Result<OwnProfile, sqlx::Error> {
        let Some(row) = sqlx::query!(
            "select bio, x_id, main_roles, main_roles_public from player_profiles \
             where player_id = $1 order by id desc limit 1",
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(OwnProfile::default());
        };
        Ok(OwnProfile {
            bio: row.bio,
            x_id: row.x_id,
            main_roles: serde_json::from_value(row.main_roles).unwrap_or_default(),
            main_roles_public: row.main_roles_public,
        })
    }

    /// 公開プロフィール。**担当ロールは公開を選んでいるときだけ載せる。**
    ///
    /// 未設定でも非公開でも `main_roles` は `None` になり、JSON からキーごと消える。
    /// ここが唯一の組み立て口なので、公開側だけ除外し忘れる余地が無い。
    pub async fn public_profile(
        &self,
        player_id: PlayerId,
    ) -> Result<Option<PublicProfile>, sqlx::Error> {
        let Some(player) = sqlx::query!(
            "select p.discord_user_id, p.trainer_name, \
                    $2::double precision \
                        + coalesce((select sum(a.delta_mu) from season_rating_adjustments a \
                                    where a.player_id = p.id), 0::double precision) \
                        + coalesce((select sum(d.delta) from player_match_rating_deltas d \
                                    where d.player_id = p.id), 0::double precision) \
                        as \"rating!\", \
                    coalesce((select count(*) from player_match_rating_deltas d \
                              where d.player_id = p.id), 0)::bigint as \"matches!\" \
             from players p where p.id = $1",
            player_id.0,
            DEFAULT_DISPLAY_MU,
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };

        let appearance = self.appearance(player_id).await?;
        let own = self.own(player_id).await?;
        Ok(Some(PublicProfile {
            player_id: player_id.0,
            display_name: appearance
                .as_ref()
                .map_or_else(|| "（未取得）".to_owned(), |a| a.display_name.clone()),
            avatar_url: avatar_url(
                player.discord_user_id,
                appearance.as_ref().and_then(|a| a.avatar_hash.as_deref()),
            ),
            trainer_name: player.trainer_name,
            rating: player.rating,
            matches: player.matches,
            bio: own.bio,
            x_id: own.x_id,
            // **公開を選んでいて、かつ空でないときだけ。**
            // 非公開と未設定はどちらも None（キーごと消える）
            main_roles: (own.main_roles_public && !own.main_roles.is_empty())
                .then_some(own.main_roles),
        }))
    }

    /// `roles` の識別子。検証で使う一覧をアプリ側に複製しないため。
    pub async fn known_roles(&self) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query!("select id from roles order by sort_order, id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|row| row.id).collect())
    }

    /// UUID からプレイヤーを引く。存在しなければ `None`（404 にする）。
    pub async fn find_player(&self, player_id: Uuid) -> Result<Option<PlayerId>, sqlx::Error> {
        let row = sqlx::query!("select id from players where id = $1", player_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| PlayerId(row.id)))
    }
}
