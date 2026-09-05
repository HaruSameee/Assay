//! 登録トラックの型。本家ランク、プレイヤーの登録状態、申告。

use std::fmt;
use std::str::FromStr;

use serde_json::Value;

use crate::lifecycle::UnknownValue;
use crate::lifecycle::{PlayerId, Timestamp};

/// 本家（ポケモンユナイト）のランク。低い順。
///
/// **要確認（9/10 の登録受付開始まで）**: 05-登録トラック仕様 §3 は「ビギナー〜レジェンドの選択」と
/// しか書いておらず、中間段位の一覧はどのドキュメントにも無い。ここはその読みで置いた 7 段で、
/// [`Rank::as_str`] の識別子が `player_declarations.rank` に**永久に**残る。段位の増減や名前の
/// 誤りがあれば、登録を開ける前に直すこと（表示名 [`Rank::display_name`] だけなら後から変えてよい）。
///
/// 順序はキューの登録ゲート（[`super::gate`]）が「レジェンド未満」を判定するのに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    /// ビギナー
    Beginner,
    /// スーパー
    Super,
    /// ハイパー
    Hyper,
    /// エリート
    Elite,
    /// エキスパート
    Expert,
    /// マスター
    Master,
    /// レジェンド。参加資格の下限（00-現在地 §4、02 A-1）。
    Legend,
}

impl Rank {
    /// 全ランク。低い順。フォームの選択肢はこの順で出す。
    pub const ALL: [Self; 7] = [
        Self::Beginner,
        Self::Super,
        Self::Hyper,
        Self::Elite,
        Self::Expert,
        Self::Master,
        Self::Legend,
    ];

    /// 参加資格の下限。これ未満は登録ゲートで弾く（記録はする）。
    pub const ENTRY_REQUIREMENT: Self = Self::Legend;

    /// `player_declarations.rank` に入る不変の識別子。**永久ログに残るので変えない。**
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Beginner => "beginner",
            Self::Super => "super",
            Self::Hyper => "hyper",
            Self::Elite => "elite",
            Self::Expert => "expert",
            Self::Master => "master",
            Self::Legend => "legend",
        }
    }

    /// フォームに出す表示名。表示専用なので変えてよい。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Beginner => "ビギナー",
            Self::Super => "スーパー",
            Self::Hyper => "ハイパー",
            Self::Elite => "エリート",
            Self::Expert => "エキスパート",
            Self::Master => "マスター",
            Self::Legend => "レジェンド",
        }
    }

    /// 本家レートの申告を伴うランクか（05-登録トラック仕様 §3「マスター／レジェンドのとき」）。
    pub fn requires_rating(self) -> bool {
        matches!(self, Self::Master | Self::Legend)
    }

    /// 参加資格を満たすか。満たさなくても登録は受け付けて記録する（生データを捨てない）。
    pub fn meets_entry_requirement(self) -> bool {
        self >= Self::ENTRY_REQUIREMENT
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Rank {
    type Err = UnknownValue;

    fn from_str(s: &str) -> Result<Self, UnknownValue> {
        Self::ALL
            .into_iter()
            .find(|rank| rank.as_str() == s)
            .ok_or_else(|| UnknownValue {
                what: "rank",
                value: s.to_owned(),
            })
    }
}

/// `players.status`。可変の参照データなので UPDATE してよい。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayerStatus {
    /// 通常。
    Active,
    /// 停止処分。キューの登録ゲートで弾く。
    Suspended,
}

impl PlayerStatus {
    /// 全状態。
    pub const ALL: [Self; 2] = [Self::Active, Self::Suspended];

    /// DB の値。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
        }
    }
}

impl fmt::Display for PlayerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PlayerStatus {
    type Err = UnknownValue;

    fn from_str(s: &str) -> Result<Self, UnknownValue> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| UnknownValue {
                what: "player status",
                value: s.to_owned(),
            })
    }
}

/// 最新の申告（`player_declarations` の (player_id, id) 最新行から導出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// 申告時刻。
    pub declared_at: Timestamp,
    /// 申告された本家ランク。読めない識別子（体系が変わった後の古い行）は `None`。
    pub rank: Option<Rank>,
    /// 申告された本家レート。マスター / レジェンド以外は `None`。
    pub rating: Option<i32>,
}

/// プレイヤーの登録状態。可変列と、そこから導出した最新の申告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerProfile {
    /// `players.id`。セッション Cookie に入るのはこれと発行時刻だけ。
    pub player_id: PlayerId,
    /// Discord のユーザー ID。snowflake は文字列で持つ。
    pub discord_user_id: String,
    /// トレーナー名。未登録なら `None`。
    pub trainer_name: Option<String>,
    /// 状態。読めない値は `None`（登録ゲートは安全側に倒して弾く）。
    pub status: Option<PlayerStatus>,
    /// 登録フォームを最初に送信した時刻。`None` は「ログインしたが未登録」。
    pub registered_at: Option<Timestamp>,
    /// 最新の申告。
    pub latest_declaration: Option<Declaration>,
}

impl PlayerProfile {
    /// 登録フォームを送信済みか。
    pub fn is_registered(&self) -> bool {
        self.registered_at.is_some()
    }
}

/// 追記する申告。`raw` は受け取ったフォームの生データ。
#[derive(Debug, Clone, PartialEq)]
pub struct NewDeclaration {
    /// 対象プレイヤー。
    pub player_id: PlayerId,
    /// 申告時刻。
    pub declared_at: Timestamp,
    /// トレーナー名（`players` 側を UPDATE する値）。
    pub trainer_name: String,
    /// 申告された本家ランク。
    pub rank: Rank,
    /// 申告された本家レート。
    pub rating: Option<i32>,
    /// 送信されたフォームの生データ。パースに失敗した項目も含めてそのまま保存する。
    pub raw: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_is_ordered_and_legend_is_the_entry_requirement() {
        assert!(Rank::ALL.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(*Rank::ALL.last().unwrap(), Rank::Legend);
        assert!(Rank::Legend.meets_entry_requirement());
        assert!(!Rank::Master.meets_entry_requirement());
        assert!(!Rank::Beginner.meets_entry_requirement());
    }

    #[test]
    fn rank_identifiers_round_trip_and_are_distinct() {
        let mut ids: Vec<&str> = Rank::ALL.iter().map(|rank| rank.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "identifiers must be distinct");
        for rank in Rank::ALL {
            assert_eq!(rank.as_str().parse::<Rank>(), Ok(rank));
            assert!(!rank.display_name().is_empty());
        }
        assert!("diamond".parse::<Rank>().is_err());
    }

    #[test]
    fn only_the_top_two_ranks_carry_a_rating() {
        assert!(Rank::Master.requires_rating());
        assert!(Rank::Legend.requires_rating());
        for rank in Rank::ALL.iter().take(5) {
            assert!(!rank.requires_rating(), "{rank}");
        }
    }

    #[test]
    fn player_status_round_trips() {
        for status in PlayerStatus::ALL {
            assert_eq!(status.as_str().parse::<PlayerStatus>(), Ok(status));
        }
        assert!("banned".parse::<PlayerStatus>().is_err());
    }
}
