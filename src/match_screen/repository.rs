//! マッチ画面の読み取りと、サイトからの操作の受け口。
//!
//! 不変条件（このモジュールで守るもの）:
//!
//! - **状態遷移そのものを変えない。**入口が Discord から HTTP に変わるだけで、積むイベントも
//!   検証も既存の [`FormationRepository::append_event`]（ロック → 検証 → INSERT）に任せる。
//! - **追記専用テーブルに UPDATE / DELETE を書かない。**`SELECT ... FOR UPDATE` も使わない
//!   （00 §5: アプリ用ロールには UPDATE 権限が無く、本番でだけ 42501 で落ちる）。
//!   直列化は `append_event` 側の `pg_advisory_xact_lock`。
//! - **他人の報告を読む経路をここに作らない。**[`MatchRepository::has_reported`] は
//!   `player_id` を必ず条件に入れ、件数も内訳も返さない。判定は既存の
//!   `evaluate_reports`（サーバ内部）だけが全報告を読む。
//! - **`player_id` はセッションからしか来ない。**この型は受け取った値をそのまま使う。
//!   照合（本人か、参加者か、ホストか）は [`routes`](super::routes) が行う。

use sqlx::PgPool;
use uuid::Uuid;

use crate::lifecycle::{FormationId, PlayerId, Timestamp};
use crate::persistence::{FormationRepository, RepositoryError};
use crate::rating::DEFAULT_DISPLAY_MU;

/// マッチ画面が読む1人分の行。**ロールは載せるが、出す相手は呼び出し側が決める。**
///
/// この型は自チームと相手チームの両方を運ぶので `role` を持つ。外へ出す形
/// （[`super::model::TeammateView`] / [`super::model::OpponentView`]）へ写す時点で、
/// 相手チームのロールは落ちる。
#[derive(Debug, Clone, PartialEq)]
pub struct MatchPlayer {
    /// `players.id`。
    pub player_id: PlayerId,
    /// 表示名。未登録なら `None`。
    pub trainer_name: Option<String>,
    /// `red` / `blue`。
    pub team: String,
    /// ロールの表示名（`roles.display_name`）。
    pub role_display_name: String,
    /// ロールの表示順。
    pub role_sort_order: i32,
    /// 現在のレート（表示スケール）。追記ログからの導出値。
    pub rating: f64,
}

/// `vc_assigned` から読んだ VC の組。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicePair {
    /// 組のラベル（`vc3-red` の `3`）。
    pub label: String,
    /// red 側のチャンネル ID。
    pub red_channel_id: String,
    /// blue 側のチャンネル ID。
    pub blue_channel_id: String,
}

impl VoicePair {
    /// `vc3-red` 形式の表示名。
    pub fn channel_name(&self, team: &str) -> String {
        format!("vc{}-{}", self.label, team)
    }

    /// チーム側のチャンネル ID。
    pub fn channel_id(&self, team: &str) -> &str {
        if team == super::model::RED {
            &self.red_channel_id
        } else {
            &self.blue_channel_id
        }
    }
}

/// 確定した自分の増減（07 §2-2）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MyOutcome {
    /// `red` / `blue` / `draw`。
    pub outcome: Outcome,
    /// 自分の増減。
    pub delta: f64,
}

/// 確定結果の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// red の勝ち。
    Red,
    /// blue の勝ち。
    Blue,
    /// 引き分け。
    Draw,
}

impl Outcome {
    /// DB の文字列。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Blue => "blue",
            Self::Draw => "draw",
        }
    }
}

/// マッチ画面の読み取り。書き込みは [`FormationRepository`] に委ねる。
#[derive(Debug, Clone)]
pub struct MatchRepository {
    pool: PgPool,
}

impl MatchRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// この formation に本人がいるか。いなければ画面自体を見せない（404）。
    ///
    /// 存在しない formation と参加していない formation を同じ `Ok(None)` に寄せる
    /// （どの試合が存在するかを外から数えられないようにする）。
    pub async fn my_team(
        &self,
        formation_id: FormationId,
        player_id: PlayerId,
    ) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query!(
            "select team from formation_players where formation_id = $1 and player_id = $2",
            formation_id.0,
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.team))
    }

    /// 10人の名前・チーム・ロール・現在レート。
    ///
    /// レートは `player_match_ratings` ビュー（追記ログからの導出）と
    /// `rating_adjustments` の合計。**現在値のテーブルは読まない**（00 §3-2）。
    /// キュー側の同じ計算（`queued_players`）と式を揃えてある。
    pub async fn roster(&self, formation_id: FormationId) -> Result<Vec<MatchPlayer>, sqlx::Error> {
        let rows = sqlx::query!(
            "select fp.player_id, fp.team, p.trainer_name, \
                    r.display_name as \"role_display_name!\", r.sort_order as \"role_sort_order!\", \
                    $2::double precision \
                        + coalesce((select sum(a.delta_mu) from season_rating_adjustments a \
                                    where a.player_id = fp.player_id), 0::double precision) \
                        + coalesce(m.delta, 0::double precision) as \"rating!\" \
             from formation_players fp \
             join players p on p.id = fp.player_id \
             join roles r on r.id = fp.role \
             left join player_match_ratings m on m.player_id = fp.player_id \
             where fp.formation_id = $1 \
             order by fp.team, r.sort_order, fp.player_id",
            formation_id.0,
            DEFAULT_DISPLAY_MU,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| MatchPlayer {
                player_id: PlayerId(row.player_id),
                trainer_name: row.trainer_name,
                team: row.team,
                role_display_name: row.role_display_name,
                role_sort_order: row.role_sort_order,
                rating: row.rating,
            })
            .collect())
    }

    /// `vc_assigned` の最新行から VC の組を読む。Discord 側は検索しない。
    pub async fn voice_pair(
        &self,
        formation_id: FormationId,
    ) -> Result<Option<VoicePair>, sqlx::Error> {
        let row = sqlx::query!(
            "select payload->'vc'->>'label' as \"label?\", \
                    payload->'vc'->>'red_channel_id' as \"red?\", \
                    payload->'vc'->>'blue_channel_id' as \"blue?\" \
             from formation_events \
             where formation_id = $1 and kind = 'vc_assigned' and payload ? 'vc' \
             order by id desc limit 1",
            formation_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let (Some(label), Some(red), Some(blue)) = (row.label, row.red, row.blue) else {
            return Ok(None);
        };
        let (label, red, blue) = (label.trim(), red.trim(), blue.trim());
        if label.is_empty() || !is_snowflake(red) || !is_snowflake(blue) {
            return Ok(None);
        }
        Ok(Some(VoicePair {
            label: label.to_owned(),
            red_channel_id: red.to_owned(),
            blue_channel_id: blue.to_owned(),
        }))
    }

    /// **自分が**報告済みか。
    ///
    /// `player_id` を条件から外せない形にしてある。件数も他人の内容も返さないので、
    /// この経路からブラインドは壊れない（00 §3-5）。
    pub async fn has_reported(
        &self,
        formation_id: FormationId,
        player_id: PlayerId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query!(
            "select exists( \
                 select 1 from match_reports \
                 where formation_id = $1 and player_id = $2 \
             ) as \"reported!\"",
            formation_id.0,
            player_id.0,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.reported)
    }

    /// 確定した自分の増減。確定していなければ `None`。
    ///
    /// 既存の `player_match_rating_deltas` ビューをそのまま引く。別の集計経路を作らない
    /// （00 §3-3）。`player_id` を条件に入れるので、他人の増減は出てこない。
    pub async fn my_outcome(
        &self,
        formation_id: FormationId,
        player_id: PlayerId,
    ) -> Result<Option<MyOutcome>, sqlx::Error> {
        let row = sqlx::query!(
            "select outcome as \"outcome!\", delta as \"delta!\" \
             from player_match_rating_deltas \
             where formation_id = $1 and player_id = $2",
            formation_id.0,
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| {
            let outcome = match row.outcome.as_str() {
                "red" => Outcome::Red,
                "blue" => Outcome::Blue,
                "draw" => Outcome::Draw,
                _ => return None,
            };
            Some(MyOutcome {
                outcome,
                delta: row.delta,
            })
        }))
    }

    /// 共通ヘッダーの導線判定に使う Discord ID。**表示のためだけ**の読み取りで、
    /// 管理者かどうかを決めるのは [`crate::registration::Audience`] の照合。
    pub async fn discord_user_id(
        &self,
        player_id: PlayerId,
    ) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query!(
            "select discord_user_id from players where id = $1",
            player_id.0
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.discord_user_id.to_string()))
    }

    /// この formation の作成時刻（見出しと期限表示に使う）。
    pub async fn created_at(
        &self,
        formation_id: FormationId,
    ) -> Result<Option<Timestamp>, sqlx::Error> {
        let row = sqlx::query!(
            "select created_at from formations where id = $1",
            formation_id.0
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.created_at))
    }
}

/// 追記の入口。既存の永続化層をそのまま使い、検証を迂回しない。
pub async fn append(
    formations: &FormationRepository,
    formation_id: FormationId,
    event: &crate::persistence::NewEvent,
) -> Result<(), RepositoryError> {
    formations
        .append_event(formation_id, event)
        .await
        .map(|_| ())
}

/// snowflake として読める文字列か（0 と非数字を弾く）。
fn is_snowflake(value: &str) -> bool {
    value.parse::<u64>().is_ok_and(|id| id != 0)
}

/// 冪等キーの材料。**同一 idempotency_key のみ no-op**（00 §5）。
///
/// 内容が違う打ち直しは別のキーになり、新しいイベントとして通る。状態ベースの冪等に
/// すると、部屋コードの打ち直しを黙って捨てることになる。
pub fn submission_key(prefix: &str, player_id: PlayerId, payload: &str, at: Timestamp) -> String {
    // 同じ内容を連打しても同じ秒なら 1 件に畳まれ、内容が変われば必ず別のキーになる
    format!(
        "{prefix}:{}:{}:{}",
        player_id.0.simple(),
        at.timestamp(),
        short_digest(payload)
    )
}

/// 冪等キーに載せるための短いダイジェスト。衝突しても「同じ秒に同じ人が違う内容を出した」
/// ときだけなので、暗号強度は要らない。
fn short_digest(value: &str) -> String {
    // FNV-1a（64bit）。依存を増やさずに済ませる
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

/// UUID をパスから読むときの補助（不正な値は `None`）。
pub fn parse_formation_id(raw: &str) -> Option<FormationId> {
    Uuid::parse_str(raw.trim()).ok().map(FormationId)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + chrono::TimeDelta::seconds(seconds)
    }

    fn player() -> PlayerId {
        PlayerId(Uuid::from_u128(7))
    }

    #[test]
    fn a_different_payload_is_never_the_same_submission() {
        // 00 §5: 状態ベースの冪等にすると部屋コードの打ち直しを黙って捨てる。
        // 内容が違えばキーが変わり、新しいイベントとして通ること
        let first = submission_key("code", player(), "1234", at(0));
        let retyped = submission_key("code", player(), "5678", at(0));
        assert_ne!(first, retyped);

        // 同じ内容・同じ秒は同じキー（連打を 1 件に畳む）
        assert_eq!(first, submission_key("code", player(), "1234", at(0)));
        // 秒が変われば別のキー（本人が意図して出し直した）
        assert_ne!(first, submission_key("code", player(), "1234", at(1)));
        // 人が変われば別のキー
        assert_ne!(
            first,
            submission_key("code", PlayerId(Uuid::from_u128(8)), "1234", at(0))
        );
    }

    #[test]
    fn the_voice_pair_names_both_sides() {
        let vc = VoicePair {
            label: "3".into(),
            red_channel_id: "101".into(),
            blue_channel_id: "102".into(),
        };
        assert_eq!(vc.channel_name("red"), "vc3-red");
        assert_eq!(vc.channel_name("blue"), "vc3-blue");
        assert_eq!(vc.channel_id("red"), "101");
        assert_eq!(vc.channel_id("blue"), "102");
    }

    #[test]
    fn only_a_real_snowflake_is_accepted() {
        assert!(is_snowflake("900000000000000006"));
        assert!(!is_snowflake("0"));
        assert!(!is_snowflake(""));
        assert!(!is_snowflake("abc"));
        assert!(!is_snowflake("-1"));
    }

    #[test]
    fn a_malformed_path_id_is_rejected_without_touching_the_database() {
        assert!(parse_formation_id("not-a-uuid").is_none());
        assert!(parse_formation_id("").is_none());
        assert!(
            parse_formation_id(" 00000000-0000-0000-0000-000000000001 ")
                .is_some_and(|id| id.0 == Uuid::from_u128(1))
        );
    }
}
