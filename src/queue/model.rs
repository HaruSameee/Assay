//! キューの型と、投入リクエストの検証（純粋関数）。
//!
//! マッチング範囲の値は 07-マッチング仕様 §3 に従う。**上方向は +95 固定**で
//! ユーザーが変更できないため、定数として持ち DB の列にはしない。下方向だけを
//! [`QueueEntry::down_range`] に持ち、DB 側でも CHECK で 95〜200 を強制する。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::lifecycle::{PlayerId, Timestamp};

/// 上方向のマッチング範囲。**固定**。ユーザーは変更できない（07 §3）。
///
/// 高レート側が低レートだけを狙う設定を防ぐため、狭められない。列にしないのは、
/// 保存すると「保存値と定数のどちらが正か」が後から分からなくなるため。
pub const UP_RANGE: i32 = 95;
/// 下方向の最小値（＝デフォルト）。
pub const MIN_DOWN_RANGE: i32 = 95;
/// 下方向の最大値。ここまで本人が広げられる。
pub const MAX_DOWN_RANGE: i32 = 200;
/// 希望ロールの最大数（07 §4-5）。
pub const MAX_ROLE_PREFERENCES: usize = 3;

/// `queue_entries.id`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueEntryId(pub Uuid);

impl std::fmt::Display for QueueEntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// 希望ロール1つ。`priority` は 1 が第1希望。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolePreference {
    /// `roles.id`。存在しない値は DB の FK が拒否する（ここで一覧を複製しない）。
    pub role: String,
    /// 1 が第1希望。1〜3。
    pub priority: i16,
}

/// キューのエントリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    /// `queue_entries.id`。
    pub id: QueueEntryId,
    /// 所有者。エンドポイントはセッションからしか埋めない。
    pub player_id: PlayerId,
    /// 最初にキューへ入った時刻。設定変更では動かさない。
    pub enqueued_at: Timestamp,
    /// 設定を最後に変えた時刻。
    pub updated_at: Timestamp,
    /// 下方向のマッチング範囲。
    pub down_range: i32,
    /// 希望ロール。`priority` の昇順。
    pub roles: Vec<RolePreference>,
}

/// 検証済みの投入リクエスト。DB へ渡せるのはこの型だけ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRequest {
    /// 下方向のマッチング範囲。95〜200。
    pub down_range: i32,
    /// 希望ロール。`priority` の昇順で 1..=n が詰まっている。
    pub roles: Vec<RolePreference>,
}

/// 投入リクエストが受け付けられない理由。**値そのものはメッセージに載せてよい**
/// （秘密ではなく、本人が送った設定なので）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueRequestError {
    /// 下方向の範囲が 95〜200 の外。
    #[error("down_range must be between {MIN_DOWN_RANGE} and {MAX_DOWN_RANGE}, got {0}")]
    DownRangeOutOfBounds(i32),
    /// 希望ロールが 0 個。
    #[error("at least one role preference is required")]
    NoRoles,
    /// 希望ロールが多すぎる。
    #[error("at most {MAX_ROLE_PREFERENCES} role preferences are allowed, got {0}")]
    TooManyRoles(usize),
    /// 同じロールを2回書いた。
    #[error("duplicate role preference: {0}")]
    DuplicateRole(String),
    /// 優先度が 1..=n として詰まっていない（重複・欠番・範囲外）。
    #[error("priorities must be 1..={0} without gaps or duplicates")]
    MalformedPriorities(usize),
    /// ロール識別子が空。
    #[error("role identifier must not be empty")]
    EmptyRole,
}

/// 投入リクエストを検証する。**純粋関数**（DB を見ない）。
///
/// ロール識別子が実在するかは見ない。`roles` の一覧を Rust 側に複製すると、
/// マイグレーションと二重管理になる。実在しない値は DB の FK が 23503 で弾き、
/// エンドポイントがそれを 400 に写す。
///
/// 優先度は 1..=n が欠番なく詰まっていることを要求する。「第2希望だけ」のような
/// 入力は UI の不具合であり、黙って通すとマッチメイカの費用計算が意味を失う。
pub fn validate_request(
    down_range: i32,
    roles: &[RolePreference],
) -> Result<QueueRequest, QueueRequestError> {
    if !(MIN_DOWN_RANGE..=MAX_DOWN_RANGE).contains(&down_range) {
        return Err(QueueRequestError::DownRangeOutOfBounds(down_range));
    }
    if roles.is_empty() {
        return Err(QueueRequestError::NoRoles);
    }
    if roles.len() > MAX_ROLE_PREFERENCES {
        return Err(QueueRequestError::TooManyRoles(roles.len()));
    }

    let mut sorted = roles.to_vec();
    sorted.sort_by_key(|preference| preference.priority);

    let mut seen: Vec<&str> = Vec::with_capacity(sorted.len());
    for (index, preference) in sorted.iter().enumerate() {
        let role = preference.role.trim();
        if role.is_empty() {
            return Err(QueueRequestError::EmptyRole);
        }
        if seen.contains(&role) {
            return Err(QueueRequestError::DuplicateRole(role.to_owned()));
        }
        seen.push(role);
        // 1 起点で欠番なく詰まっていること（重複も欠番もここで落ちる）
        if preference.priority != (index as i16) + 1 {
            return Err(QueueRequestError::MalformedPriorities(sorted.len()));
        }
    }

    Ok(QueueRequest {
        down_range,
        roles: sorted
            .into_iter()
            .map(|preference| RolePreference {
                role: preference.role.trim().to_owned(),
                priority: preference.priority,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wish(role: &str, priority: i16) -> RolePreference {
        RolePreference {
            role: role.to_owned(),
            priority,
        }
    }

    #[test]
    fn the_down_range_is_bounded_by_the_matching_spec() {
        assert!(validate_request(MIN_DOWN_RANGE, &[wish("jungle", 1)]).is_ok());
        assert!(validate_request(MAX_DOWN_RANGE, &[wish("jungle", 1)]).is_ok());
        assert_eq!(
            validate_request(MIN_DOWN_RANGE - 1, &[wish("jungle", 1)]),
            Err(QueueRequestError::DownRangeOutOfBounds(94))
        );
        assert_eq!(
            validate_request(MAX_DOWN_RANGE + 1, &[wish("jungle", 1)]),
            Err(QueueRequestError::DownRangeOutOfBounds(201))
        );
    }

    #[test]
    fn role_preferences_are_capped_at_three_and_ordered_by_priority() {
        let request = validate_request(
            150,
            &[wish("bot_tank", 3), wish("jungle", 1), wish("top_carry", 2)],
        )
        .unwrap();
        let order: Vec<&str> = request.roles.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(order, ["jungle", "top_carry", "bot_tank"]);

        assert_eq!(
            validate_request(150, &[]),
            Err(QueueRequestError::NoRoles),
            "queueing with no role is not a queue entry"
        );
        assert_eq!(
            validate_request(
                150,
                &[wish("a", 1), wish("b", 2), wish("c", 3), wish("d", 4),],
            ),
            Err(QueueRequestError::TooManyRoles(4))
        );
    }

    #[test]
    fn priorities_must_be_one_through_n_without_gaps_or_duplicates() {
        assert_eq!(
            validate_request(150, &[wish("jungle", 2)]),
            Err(QueueRequestError::MalformedPriorities(1)),
            "a lone second choice is a UI bug"
        );
        assert_eq!(
            validate_request(150, &[wish("jungle", 1), wish("bot_tank", 3)]),
            Err(QueueRequestError::MalformedPriorities(2)),
            "gaps are rejected"
        );
        assert_eq!(
            validate_request(150, &[wish("jungle", 1), wish("bot_tank", 1)]),
            Err(QueueRequestError::MalformedPriorities(2)),
            "duplicate priorities are rejected"
        );
        assert_eq!(
            validate_request(150, &[wish("jungle", 0)]),
            Err(QueueRequestError::MalformedPriorities(1))
        );
    }

    #[test]
    fn the_same_role_cannot_be_wished_for_twice() {
        assert_eq!(
            validate_request(150, &[wish("jungle", 1), wish(" jungle ", 2)]),
            Err(QueueRequestError::DuplicateRole("jungle".into())),
            "trimmed identifiers still collide"
        );
        assert_eq!(
            validate_request(150, &[wish("   ", 1)]),
            Err(QueueRequestError::EmptyRole)
        );
    }
}
