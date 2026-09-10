//! 運営の当日操作（`admin_actions`）の型と検証。**純粋**。DB も HTTP も知らない。
//!
//! 設計（マイグレーション `20260907120000_admin_actions` の冒頭と対）:
//!
//! - **現在の状態は列に持たない。**「マッチメイカが停止中か」「ブロック枠が何枠か」は
//!   追記された最新行から導出する（`formation_events` と同じ）。停止フラグを可変列に
//!   すると、誰がいつ止めたかが上書きで消える。
//! - **停止状態をプロセス内に持たない。**メモリに持つと再起動で勝手に再開する。
//!   「止めたまま再起動したら動き出した」が当日いちばん危ない。
//! - 操作の記録は追記専用。取り消しは「反対の操作を追記する」（再開・枠の再設定）。

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// ブロック枠の既定値。行が 1 つも無いプレイヤーはこの枠数を持つ。
pub const DEFAULT_BLOCK_ALLOWANCE: i32 = 1;
/// ブロック枠の上限。DB の CHECK（0〜99）と同じ値にしてある。
pub const MAX_BLOCK_ALLOWANCE: i32 = 99;

/// 運営操作の種別。DB の CHECK と同じ 4 種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminActionKind {
    /// マッチメイカの停止。
    MatchmakerPaused,
    /// マッチメイカの再開。
    MatchmakerResumed,
    /// キューからの強制離脱。
    QueueRemoved,
    /// ブロック枠の変更。
    BlockAllowanceSet,
    /// シーズン境界の追記（導出の起点を進めた）。
    SeasonStarted,
    /// クールダウンを無視したブロックの管理解除。
    BlockOverrideRemoved,
    /// 未報告のプレイヤーに運営が報告を入れた。
    ReportEntered,
    /// レートの増減幅の変更を断った（進行中の formation があった）。
    RatingParamsRejected,
    /// レートの増減幅を変えた（過去の全試合が引き直される）。
    RatingParamsChanged,
    /// 運営の報告入力を断った（係争中・確定済みの試合）。
    ReportRejected,
}

impl AdminActionKind {
    /// 全種別。マイグレーションの `kind` CHECK と一致していることを `tests/schema.rs` が検査する
    /// （片方だけ変わる事故を防ぐ。`FormationEventKind::ALL` と同じ役割）。
    pub const ALL: [Self; 10] = [
        Self::MatchmakerPaused,
        Self::MatchmakerResumed,
        Self::QueueRemoved,
        Self::BlockAllowanceSet,
        Self::SeasonStarted,
        Self::RatingParamsChanged,
        Self::RatingParamsRejected,
        Self::BlockOverrideRemoved,
        Self::ReportEntered,
        Self::ReportRejected,
    ];

    /// DB に入る文字列。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MatchmakerPaused => "matchmaker_paused",
            Self::MatchmakerResumed => "matchmaker_resumed",
            Self::QueueRemoved => "queue_removed",
            Self::BlockAllowanceSet => "block_allowance_set",
            Self::SeasonStarted => "season_started",
            Self::RatingParamsChanged => "rating_params_changed",
            Self::RatingParamsRejected => "rating_params_rejected",
            Self::BlockOverrideRemoved => "block_override_removed",
            Self::ReportEntered => "report_entered",
            Self::ReportRejected => "report_rejected",
        }
    }

    /// 読み取り側。**知らない値は `None`** にして落とす（追記ログを読む側は止まらない）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "matchmaker_paused" => Some(Self::MatchmakerPaused),
            "matchmaker_resumed" => Some(Self::MatchmakerResumed),
            "queue_removed" => Some(Self::QueueRemoved),
            "block_allowance_set" => Some(Self::BlockAllowanceSet),
            "season_started" => Some(Self::SeasonStarted),
            "rating_params_changed" => Some(Self::RatingParamsChanged),
            "rating_params_rejected" => Some(Self::RatingParamsRejected),
            "block_override_removed" => Some(Self::BlockOverrideRemoved),
            "report_entered" => Some(Self::ReportEntered),
            "report_rejected" => Some(Self::ReportRejected),
            _ => None,
        }
    }

    /// 運用の言葉。管理パネルの履歴表に出す。
    pub fn label(self) -> &'static str {
        match self {
            Self::MatchmakerPaused => "マッチメイカ停止",
            Self::MatchmakerResumed => "マッチメイカ再開",
            Self::QueueRemoved => "キューから強制離脱",
            Self::BlockAllowanceSet => "ブロック枠の変更",
            Self::SeasonStarted => "シーズン開始（導出の起点を追記）",
            Self::RatingParamsChanged => "レート増減幅の変更（全履歴を引き直し）",
            Self::RatingParamsRejected => "レート増減幅の変更を拒否（進行中の試合あり）",
            Self::BlockOverrideRemoved => "ブロックの管理解除",
            Self::ReportEntered => "運営による報告の入力",
            Self::ReportRejected => "運営による報告の入力を拒否",
        }
    }
}

/// マッチメイカの停止状態。`matchmaker_paused` / `matchmaker_resumed` の**最新行**から導出する。
///
/// 「止まっている」ことだけでなく**誰がいつ止めたか**を持つ。運営ログの定期通知に出して、
/// 止めたまま忘れることを防ぐ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseState {
    /// 止めた運営の Discord ID。
    pub actor_discord_user_id: i64,
    /// 止めた時刻。
    pub since: DateTime<Utc>,
    /// 運営が書いた理由（任意）。
    pub reason: Option<String>,
}

/// 本人のキュー画面に出す「運営に外された」通知。`queue_removed` の最新行から導出する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRemoval {
    /// 外された時刻。
    pub removed_at: DateTime<Utc>,
    /// 運営が書いた理由（任意）。**本人にそのまま見せる。**
    pub reason: Option<String>,
}

/// 運営操作ログの 1 行。管理パネルの履歴表に出す。
#[derive(Debug, Clone, PartialEq)]
pub struct AdminActionRow {
    pub id: i64,
    /// 読めない種別は `None`（表示だけ落として一覧は止めない）。
    pub kind: Option<AdminActionKind>,
    /// DB に入っていた生の種別。
    pub raw_kind: String,
    pub actor_discord_user_id: i64,
    pub target_player_id: Option<Uuid>,
    /// 対象プレイヤーのトレーナー名（表示用。未設定なら `None`）。
    pub target_trainer_name: Option<String>,
    pub reason: Option<String>,
    /// `block_allowance_set` のときの枠数。
    pub allowance: Option<i32>,
    pub occurred_at: DateTime<Utc>,
}

/// 理由の入力を正規化する。空・空白だけは「理由なし」（任意項目なので拒否しない）。
///
/// DB 側は `btrim(reason) <> ''` を要求するので、ここで空文字を `None` に畳んでおく。
pub fn normalize_reason(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// ブロック枠の入力を読む。0〜[`MAX_BLOCK_ALLOWANCE`] の整数だけを受ける。
///
/// 範囲外・非数は拒否する（DB の CHECK と同じ範囲。先に弾いて運用の言葉で返す）。
pub fn parse_block_allowance(raw: &str) -> Option<i32> {
    let value: i32 = raw.trim().parse().ok()?;
    (0..=MAX_BLOCK_ALLOWANCE).contains(&value).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_the_database_string() {
        for kind in AdminActionKind::ALL {
            assert_eq!(AdminActionKind::parse(kind.as_str()), Some(kind));
            assert!(!kind.label().is_empty());
        }
        // 読み手は知らない種別で止まらない
        assert_eq!(AdminActionKind::parse("matchmaker_exploded"), None);
    }

    #[test]
    fn a_blank_reason_is_no_reason() {
        assert_eq!(normalize_reason("  "), None);
        assert_eq!(normalize_reason(""), None);
        assert_eq!(normalize_reason(" 通信不良 "), Some("通信不良".to_owned()));
    }

    #[test]
    fn the_block_allowance_stays_within_the_database_check() {
        assert_eq!(parse_block_allowance("0"), Some(0));
        assert_eq!(parse_block_allowance(" 3 "), Some(3));
        assert_eq!(parse_block_allowance("99"), Some(MAX_BLOCK_ALLOWANCE));
        for invalid in ["-1", "100", "", "２", "3.5", "three"] {
            assert_eq!(parse_block_allowance(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn the_default_allowance_is_one() {
        assert_eq!(DEFAULT_BLOCK_ALLOWANCE, 1);
    }
}
