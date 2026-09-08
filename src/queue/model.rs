//! キューの型と、投入リクエストの検証（純粋関数）。
//!
//! マッチング範囲の値は 07-マッチング仕様 §3 に従う。**上下とも 50 固定**で、
//! ユーザーは調整できない。したがって定数として持ち、判定に使う値は DB の列から読まない。
//!
//! **群の幅がちょうど 50 になる。**両立条件（07 §3-1）は `max − min ≤ 50` かつ
//! 各人 `rating_i − min ≤ d_i` で、`d_i` も 50 なので前者と一致する。seed の差 100 と
//! 組み合わせると、1600 群と 1500 群は初期状態で**分離する**（差 100 > 50）。
//! 境界の差ちょうど 50 は**組める側**（比較は両端とも `<=`）。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::lifecycle::{FormationId, PlayerId, Timestamp};

/// 上方向のマッチング範囲。**固定**。ユーザーは変更できない（07 §3）。
///
/// 高レート側が低レートだけを狙う設定を防ぐため、狭められない。列にしないのは、
/// 保存すると「保存値と定数のどちらが正か」が後から分からなくなるため。
pub const UP_RANGE: i32 = 50;
/// 下方向のマッチング範囲。**固定**。上方向と同じ幅で、ユーザーは変更できない。
///
/// `queue_entries.down_range` の列は残してあるが、新しい行には必ずこの値を書く。
/// 判定はこの定数を通した値（[`clamp_down_range`]）だけを使う。
pub const DOWN_RANGE: i32 = 50;
/// 希望ロールの最大数（07 §4-5）。
pub const MAX_ROLE_PREFERENCES: usize = 3;

/// 保存済みの下方向範囲を、現在の設定に丸める。**読み出し側で必ず通すこと。**
///
/// 旧仕様（95〜200、のちに 30〜100）で入った行が `queue_entries` に残りうる。既存行を
/// 一括 UPDATE すると、本人が設定した覚えのない値に書き換わるうえ、追記でない書き込みが
/// 増える。DB の CHECK も `not valid` で足してあるので、**古い行は古い値のまま残る**のが
/// 正しい状態で、丸めるのは読み出し側の責任。
///
/// いまは上下 50 固定なので、下限と上限が同じ値になる。範囲がまた可変になったときに
/// ここだけ直せば済むよう、[`i32::clamp`] の形は保つ。
pub fn clamp_down_range(stored: i32) -> i32 {
    stored.clamp(DOWN_RANGE, DOWN_RANGE)
}

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
///
/// **マッチング範囲は持たない。**上下とも 50 固定で、ユーザーが送る値は無い
/// （[`UP_RANGE`] / [`DOWN_RANGE`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRequest {
    /// 希望ロール。`priority` の昇順で 1..=n が詰まっている。
    pub roles: Vec<RolePreference>,
}

/// 成立済み formation で本人に割り当てられた VC。
///
/// `vc_assigned` の追記ログから読む表示用の導出値であり、Discord 側を検索しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceDestination {
    /// 対象 formation。
    pub formation_id: FormationId,
    /// `red` / `blue`。
    pub team: String,
    /// VC 組のラベル。
    pub label: String,
    /// Discord のチャンネル ID。snowflake は文字列のまま扱う。
    pub channel_id: String,
}

impl VoiceDestination {
    /// `vc3-red` 形式の表示名。
    pub fn channel_name(&self) -> String {
        format!("vc{}-{}", self.label, self.team)
    }
}

/// 「運営に外された」通知を出し続ける長さ（時間）。
///
/// キューは可変テーブルで出入りの履歴が無い（マイグレーション `20260906140000_queue`）ため、
/// 「外された後に自分で入り直して自分で抜けた」を区別できない。窓を切らないと、何日も前の
/// 通知が残り続ける。開催は 21:00〜25:00 なので、1 回の開催とその後の確認に足りる長さにする。
pub const REMOVAL_NOTICE_WINDOW_HOURS: i64 = 12;

/// 運営がキューから外したことを本人に伝える通知。`admin_actions` の追記から導出する。
///
/// **黙って消さない。**理由が分からないと「バグでキューから外れた」と受け取られる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaffRemoval {
    /// 外された時刻。
    pub removed_at: Timestamp,
    /// 運営が書いた理由（任意）。**本人にそのまま見せる。**
    pub reason: Option<String>,
}

/// 投入リクエストが受け付けられない理由。**値そのものはメッセージに載せてよい**
/// （秘密ではなく、本人が送った設定なので）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueRequestError {
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
pub fn validate_request(roles: &[RolePreference]) -> Result<QueueRequest, QueueRequestError> {
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

    /// マッチング範囲は上下とも 50 固定。**ユーザーが送る値は無い。**
    #[test]
    fn the_range_is_fixed_at_fifty_in_both_directions() {
        assert_eq!((UP_RANGE, DOWN_RANGE), (50, 50), "07 §3");
        // 投入リクエストに範囲が無いこと（型がそれを保証する）
        let request = validate_request(&[wish("jungle", 1)]).expect("valid");
        assert_eq!(request.roles.len(), 1);
    }

    /// 旧仕様（95〜200 / 30〜100）で入った行は一括 UPDATE せず、読み出し時に丸める。
    #[test]
    fn stored_values_from_the_old_ranges_are_clamped_on_read() {
        for stored in [200, 101, 100, 95, 30, 0, -1] {
            assert_eq!(clamp_down_range(stored), DOWN_RANGE, "{stored}");
        }
        assert_eq!(clamp_down_range(DOWN_RANGE), DOWN_RANGE);
    }

    #[test]
    fn role_preferences_are_capped_at_three_and_ordered_by_priority() {
        let request =
            validate_request(&[wish("bot_tank", 3), wish("jungle", 1), wish("top_carry", 2)])
                .unwrap();
        let order: Vec<&str> = request.roles.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(order, ["jungle", "top_carry", "bot_tank"]);

        assert_eq!(
            validate_request(&[]),
            Err(QueueRequestError::NoRoles),
            "queueing with no role is not a queue entry"
        );
        assert_eq!(
            validate_request(&[wish("a", 1), wish("b", 2), wish("c", 3), wish("d", 4),],),
            Err(QueueRequestError::TooManyRoles(4))
        );
    }

    #[test]
    fn priorities_must_be_one_through_n_without_gaps_or_duplicates() {
        assert_eq!(
            validate_request(&[wish("jungle", 2)]),
            Err(QueueRequestError::MalformedPriorities(1)),
            "a lone second choice is a UI bug"
        );
        assert_eq!(
            validate_request(&[wish("jungle", 1), wish("bot_tank", 3)]),
            Err(QueueRequestError::MalformedPriorities(2)),
            "gaps are rejected"
        );
        assert_eq!(
            validate_request(&[wish("jungle", 1), wish("bot_tank", 1)]),
            Err(QueueRequestError::MalformedPriorities(2)),
            "duplicate priorities are rejected"
        );
        assert_eq!(
            validate_request(&[wish("jungle", 0)]),
            Err(QueueRequestError::MalformedPriorities(1))
        );
    }

    #[test]
    fn the_same_role_cannot_be_wished_for_twice() {
        assert_eq!(
            validate_request(&[wish("jungle", 1), wish(" jungle ", 2)]),
            Err(QueueRequestError::DuplicateRole("jungle".into())),
            "trimmed identifiers still collide"
        );
        assert_eq!(
            validate_request(&[wish("   ", 1)]),
            Err(QueueRequestError::EmptyRole)
        );
    }
}
