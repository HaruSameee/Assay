use chrono::{Duration, Utc};
use thiserror::Error;
use uuid::Uuid;

use crate::lifecycle::Timestamp;

pub const BLOCK_COOLDOWN_DAYS: i64 = 5;

pub fn cooldown_ends_at(blocked_at: Timestamp) -> Timestamp {
    blocked_at + Duration::days(BLOCK_COOLDOWN_DAYS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Blocked,
    Unblocked,
}

impl BlockKind {
    pub const ALL: [Self; 2] = [Self::Blocked, Self::Unblocked];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Unblocked => "unblocked",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "blocked" => Some(Self::Blocked),
            "unblocked" => Some(Self::Unblocked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentBlock {
    pub blocked_player_id: Uuid,
    pub display_name: Option<String>,
    pub trainer_name: Option<String>,
    pub blocked_at: Timestamp,
}

impl CurrentBlock {
    pub fn cooldown_ends_at(&self) -> Timestamp {
        cooldown_ends_at(self.blocked_at)
    }

    pub fn can_unblock_at(&self, now: Timestamp) -> bool {
        now >= self.cooldown_ends_at()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSearchResult {
    pub player_id: Uuid,
    /// Discord の表示名。スナップショットがまだ無い相手では `None`。
    pub display_name: Option<String>,
    /// サイト上のトレーナー名。**利用者が打つのはこちら。**
    pub trainer_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingBlock {
    pub player_id: Uuid,
    pub display_name: Option<String>,
    pub trainer_name: Option<String>,
    pub blocked_at: Timestamp,
}

/// 運営だけが見る、現在受信している味方ブロックの集計行。
///
/// 一覧は件数の多い順に出す。利用者向けの API や公開プロフィールには載せない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedBlockCount {
    pub player_id: Uuid,
    pub display_name: Option<String>,
    pub trainer_name: Option<String>,
    pub count: usize,
}

#[derive(Debug, Error)]
pub enum UserBlockError {
    #[error("a player cannot block themselves")]
    SelfTarget,
    #[error("this player is already blocked")]
    AlreadyBlocked,
    #[error("the block allowance of {allowance} is full")]
    AllowanceFull { allowance: i32 },
    #[error("this player is not currently blocked")]
    NotBlocked,
    #[error("this block cannot be removed until {available_at}")]
    Cooldown { available_at: chrono::DateTime<Utc> },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Error)]
pub enum AdminBlockOverrideError {
    #[error("this player is not currently blocked")]
    NotBlocked,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Admin(#[from] crate::admin::AdminStoreError),
}
