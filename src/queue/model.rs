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

/// 上方向のマッチング範囲の**既定値**（07 §3）。
///
/// ユーザーは変更できない。運営だけが管理パネルから変えられる
/// （`matching_range_changes` の最新行が正。1 行も無ければこの値）。
/// 高レート側が低レートだけを狙う設定を防ぐため、本人には触らせない。
pub const DEFAULT_UP_RANGE: i32 = 50;
/// 下方向のマッチング範囲の**既定値**。上方向と同じ幅。
///
/// `queue_entries.down_range` には投入時点の現在値を書く。判定は
/// [`MatchingRanges::clamp_down`] を通した値だけを使う。
pub const DEFAULT_DOWN_RANGE: i32 = 50;
/// マッチング範囲として受け付ける下限。これより狭いと 10 人が集まらない。
pub const MIN_RANGE: i32 = 10;
/// マッチング範囲として受け付ける上限。これより広いと事実上の無制限になる。
pub const MAX_RANGE: i32 = 500;
/// 希望ロールの最大数（07 §4-5）。
pub const MAX_ROLE_PREFERENCES: usize = 3;

/// 現在のマッチング範囲。**`current_matching_ranges` の 1 行に対応する。**
///
/// マッチメイカは 1 スキャンにつき 1 回だけ読み、その 1 個をスキャン全体で使い回す。
/// 途中で読み直さないので、運営が同時に幅を変えても 1 回の選抜の中では変わらない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchingRanges {
    /// 上方向の幅。アンカー（ウィンドウの下端）から上に何点までを同じ試合に入れるか。
    pub up: i32,
    /// 下方向の幅。投入時に `queue_entries.down_range` へ書く値でもある。
    pub down: i32,
}

impl Default for MatchingRanges {
    fn default() -> Self {
        Self {
            up: DEFAULT_UP_RANGE,
            down: DEFAULT_DOWN_RANGE,
        }
    }
}

impl MatchingRanges {
    /// 保存済みの下方向範囲を、現在の幅に丸める。**読み出し側で必ず通すこと。**
    ///
    /// 旧仕様（95〜200、のちに 30〜100、のちに 50 固定）で入った行が
    /// `queue_entries` に残りうる。既存行を一括 UPDATE すると、本人が設定した覚えの
    /// ない値に書き換わるうえ、追記でない書き込みが増える。DB の CHECK も
    /// `not valid` で足してあるので、**古い行は古い値のまま残る**のが正しい状態で、
    /// 丸めるのは読み出し側の責任。
    ///
    /// 上下は同じ幅とは限らないので、丸め先は `down` だけを見る。
    pub fn clamp_down(&self, stored: i32) -> i32 {
        stored.clamp(self.down, self.down)
    }
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
    /// 上方向のマッチング範囲。**運営が変えられる**（本人は変えられない）。
    pub up_range: i32,
    /// 下方向のマッチング範囲。運営が変えた後は、読み出し時に現在の幅へ丸めた値。
    pub down_range: i32,
    /// 希望ロール。`priority` の昇順。
    pub roles: Vec<RolePreference>,
    /// 配信中として申告しているか。
    pub streaming: bool,
}

/// 検証済みの投入リクエスト。DB へ渡せるのはこの型だけ。
///
/// **マッチング範囲は持たない。**ユーザーが送る値は無く、投入時点の
/// `current_matching_ranges` を DB 側で書く（既定は [`DEFAULT_UP_RANGE`] /
/// [`DEFAULT_DOWN_RANGE`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRequest {
    /// 希望ロール。`priority` の昇順で 1..=n が詰まっている。
    pub roles: Vec<RolePreference>,
    /// 配信中として申告するか。**表示のためだけの値**で、マッチングには関与しない。
    /// 成立時に `formation_players.streaming` へ写され、マッチ画面で全員に見える。
    pub streaming: bool,
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
        // 配信中の申告は検証の対象ではない（呼ぶ側が本人の入力をそのまま入れる）
        streaming: false,
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

    /// 既定は上下とも 50。**ユーザーが送る値は無い**（運営だけが変えられる）。
    #[test]
    fn the_user_never_sends_a_range() {
        assert_eq!(
            (DEFAULT_UP_RANGE, DEFAULT_DOWN_RANGE),
            (50, 50),
            "07 §3 の既定値"
        );
        assert_eq!(
            MatchingRanges::default(),
            MatchingRanges { up: 50, down: 50 }
        );
        // 投入リクエストに範囲が無いこと（型がそれを保証する）
        let request = validate_request(&[wish("jungle", 1)]).expect("valid");
        assert_eq!(request.roles.len(), 1);
    }

    /// 旧仕様（95〜200 / 30〜100 / 50 固定）で入った行は一括 UPDATE せず、読み出し時に丸める。
    ///
    /// **丸め先は現在の幅**なので、運営が変えれば既存のキュー行もその幅で判定される。
    #[test]
    fn stored_values_from_the_old_ranges_are_clamped_to_the_current_range() {
        let defaults = MatchingRanges::default();
        for stored in [200, 101, 100, 95, 30, 0, -1] {
            assert_eq!(defaults.clamp_down(stored), DEFAULT_DOWN_RANGE, "{stored}");
        }
        assert_eq!(defaults.clamp_down(DEFAULT_DOWN_RANGE), DEFAULT_DOWN_RANGE);

        // 運営が広げた後は、同じ古い行が新しい幅に丸まる
        let widened = MatchingRanges { up: 120, down: 120 };
        for stored in [200, 50, 30, 0] {
            assert_eq!(widened.clamp_down(stored), 120, "{stored}");
        }
        // 上方向は丸めに関係しない（列が無いので保存値も無い）
        assert_eq!(MatchingRanges { up: 500, down: 25 }.clamp_down(200), 25);
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
