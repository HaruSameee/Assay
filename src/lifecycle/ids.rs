//! 識別子と時刻の型。

use std::fmt;

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// 時刻。DB の `timestamptz` に対応する。
pub type Timestamp = DateTime<Utc>;

/// プレイヤー ID（`players.id`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlayerId(pub Uuid);

/// formation ID（`formations.id`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FormationId(pub Uuid);

impl fmt::Display for PlayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for FormationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
