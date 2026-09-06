//! ボタンとモーダルの `custom_id`。
//!
//! formation と対象プレイヤーを埋め込む。インタラクションが飛んできたら、これだけで
//! どの formation の誰宛てかが分かる（Discord 側を検索しない）。
//! 押した本人が対象プレイヤーかどうかは、アダプタが編成（`players.discord_user_id`）で照合する。
//! 古いボタンが押されても、状態機械の検証（`SubjectMismatch` など）で弾かれる。
//!
//! 形式: `<action>:<formation uuid (simple)>:<player uuid (simple)>`（68 文字。Discord の上限は 100）。

use uuid::Uuid;

use crate::lifecycle::{FormationId, PlayerId};
use crate::persistence::{Outcome, Team};

/// モーダル内のルーム ID 入力欄の `custom_id`。
pub const ROOM_CODE_INPUT_ID: &str = "room_code";

/// インタラクションの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// ホスト候補が「部屋を作る」を押した。
    HostAccept,
    /// ホスト候補が「今回はパスする」を押した。
    HostDecline,
    /// ホストが「ルーム ID を入力 / 修正」を押した（モーダルを開く）。
    CodeOpen,
    /// ルーム ID のモーダルが送信された。
    CodeSubmit,
    /// 赤チーム勝利の報告。
    ReportRed,
    /// 青チーム勝利の報告。
    ReportBlue,
    /// 無効試合の報告。
    ReportNullified,
}

impl Action {
    const ALL: [Self; 7] = [
        Self::HostAccept,
        Self::HostDecline,
        Self::CodeOpen,
        Self::CodeSubmit,
        Self::ReportRed,
        Self::ReportBlue,
        Self::ReportNullified,
    ];

    fn tag(self) -> &'static str {
        match self {
            Self::HostAccept => "ha",
            Self::HostDecline => "hd",
            Self::CodeOpen => "co",
            Self::CodeSubmit => "cs",
            Self::ReportRed => "rr",
            Self::ReportBlue => "rb",
            Self::ReportNullified => "rn",
        }
    }

    pub fn report_outcome(self) -> Option<Outcome> {
        match self {
            Self::ReportRed => Some(Outcome::Win(Team::Red)),
            Self::ReportBlue => Some(Outcome::Win(Team::Blue)),
            Self::ReportNullified => Some(Outcome::Nullified),
            _ => None,
        }
    }
}

/// `custom_id` の中身。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustomId {
    /// 種類。
    pub action: Action,
    /// 対象の formation。
    pub formation: FormationId,
    /// 対象プレイヤー（ホスト候補 / ホスト）。
    pub player: PlayerId,
}

impl CustomId {
    /// 組み立てる。
    pub fn new(action: Action, formation: FormationId, player: PlayerId) -> Self {
        Self {
            action,
            formation,
            player,
        }
    }

    /// 全参加者が使う報告ボタン。押下者は埋め込まず、編成の Discord ID から特定する。
    pub fn report(action: Action, formation: FormationId) -> Self {
        debug_assert!(action.report_outcome().is_some());
        Self::new(action, formation, PlayerId(Uuid::nil()))
    }

    /// Discord に渡す文字列。
    pub fn encode(&self) -> String {
        format!(
            "{}:{}:{}",
            self.action.tag(),
            self.formation.0.simple(),
            self.player.0.simple()
        )
    }

    /// Discord から来た文字列を読む。形式が合わなければ `None`（このアプリのものではない）。
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(':');
        let tag = parts.next()?;
        let formation = Uuid::try_parse(parts.next()?).ok()?;
        let player = Uuid::try_parse(parts.next()?).ok()?;
        if parts.next().is_some() {
            return None;
        }
        let action = Action::ALL.into_iter().find(|action| action.tag() == tag)?;
        Some(Self {
            action,
            formation: FormationId(formation),
            player: PlayerId(player),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_id_round_trips_and_fits_the_limit() {
        for action in Action::ALL {
            let id = CustomId::new(
                action,
                FormationId(Uuid::from_u128(0xf0)),
                PlayerId(Uuid::from_u128(0x7)),
            );
            let encoded = id.encode();
            assert!(encoded.len() <= 100, "{encoded}");
            assert_eq!(CustomId::parse(&encoded), Some(id));
        }
    }

    #[test]
    fn foreign_custom_ids_are_ignored() {
        assert_eq!(CustomId::parse(""), None);
        assert_eq!(CustomId::parse("ha"), None);
        assert_eq!(CustomId::parse("xx:0:0"), None);
        assert_eq!(CustomId::parse("ha:not-a-uuid:also-not"), None);
        let simple = Uuid::from_u128(1).simple().to_string();
        assert_eq!(
            CustomId::parse(&format!("ha:{simple}:{simple}:extra")),
            None
        );
    }
}
