//! キューのゲート。純粋関数。
//!
//! ゲートは**2つに分ける**（05-登録トラック仕様 §4）。1つのフラグに畳むと、キュー画面で
//! 「なぜ入れないのか」を説明できなくなる。
//!
//! - **登録ゲート**: 未登録 / `status = 'suspended'` / 直近の申告がレジェンド未満
//! - **報告ゲート**: 未報告の formation を持つ
//!
//! 報告ゲートの解除条件は「**自分が報告したこと**」だけ（02 D-3）。「確定したこと」を条件に
//! すると、他 9 人に人質を取られる。`disputed` は係争の解決を待たずに解除済みとして扱う
//! ——「自分が報告したか」しか見ないので、係争入りは判定に影響しない。
//!
//! 未報告かどうかは**自分の報告の有無だけ**で決める。他人の報告の内容・件数は読まない
//! （報告のブラインドは API レベルの制約であり、ここもその内側にある）。

use crate::lifecycle::{FormationId, FormationState};

use super::model::{PlayerProfile, PlayerStatus, Rank};

/// 登録ゲートに引っかかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationBlock {
    /// ログインはしたが登録フォームを出していない。
    NotRegistered,
    /// 停止処分。
    Suspended,
    /// `players.status` が読めない値だった。安全側に倒して弾く。
    UnknownStatus,
    /// 直近の申告が参加資格（レジェンド）未満。申告そのものは記録済み。
    RankBelowEntry {
        /// 直近の申告ランク。申告が無い / 読めない場合は `None`。
        declared: Option<Rank>,
    },
}

impl RegistrationBlock {
    /// キュー画面に出す説明。
    pub fn message(&self) -> String {
        match self {
            Self::NotRegistered => "登録がまだです。登録フォームを送信してください。".into(),
            Self::Suspended => "アカウントが停止されています。運営に連絡してください。".into(),
            Self::UnknownStatus => {
                "アカウントの状態を確認できません。運営に連絡してください。".into()
            }
            Self::RankBelowEntry {
                declared: Some(rank),
            } => format!(
                "参加資格は{}以上です（現在の申告: {}）。上がったら再申告してください。",
                Rank::ENTRY_REQUIREMENT.display_name(),
                rank.display_name()
            ),
            Self::RankBelowEntry { declared: None } => format!(
                "参加資格は{}以上です。ランクを申告してください。",
                Rank::ENTRY_REQUIREMENT.display_name()
            ),
        }
    }
}

/// キューに入れるかどうかの判定。2つのゲートを**別々に**持つ。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueEligibility {
    /// 登録ゲート。通っていれば `None`。
    pub registration: Option<RegistrationBlock>,
    /// 報告ゲート。未報告の formation。空なら通過。
    pub pending_reports: Vec<FormationId>,
}

impl QueueEligibility {
    /// 両方のゲートを通ったか。
    pub fn is_allowed(&self) -> bool {
        self.registration.is_none() && self.pending_reports.is_empty()
    }

    /// 報告ゲートに引っかかっているか。
    pub fn is_report_blocked(&self) -> bool {
        !self.pending_reports.is_empty()
    }

    /// キュー画面に出す説明。引っかかっている分だけ、畳まずに並べる。
    pub fn messages(&self) -> Vec<String> {
        let mut messages = Vec::new();
        if let Some(block) = &self.registration {
            messages.push(block.message());
        }
        if let Some(count) = std::num::NonZeroUsize::new(self.pending_reports.len()) {
            messages.push(format!(
                "未報告の試合が{count}件あります。結果を報告するとキューに入れます。"
            ));
        }
        messages
    }
}

/// その formation が、このプレイヤーのキューを塞ぐか。
///
/// 塞ぐのは「報告義務のある状態」かつ「本人がまだ報告していない」とき。
/// `player_reported` は**本人の報告の有無**だけで、他人の報告は見ない。
pub fn formation_blocks_queue(state: &FormationState, player_reported: bool) -> bool {
    state.requires_reports() && !player_reported
}

/// 登録ゲート。プロフィールが無い（一度もログインしていない）場合も未登録として扱う。
pub fn registration_gate(profile: Option<&PlayerProfile>) -> Option<RegistrationBlock> {
    let profile = profile?;
    match profile.status {
        Some(PlayerStatus::Suspended) => return Some(RegistrationBlock::Suspended),
        None => return Some(RegistrationBlock::UnknownStatus),
        Some(PlayerStatus::Active) => {}
    }
    if !profile.is_registered() {
        return Some(RegistrationBlock::NotRegistered);
    }
    let declared = profile
        .latest_declaration
        .as_ref()
        .and_then(|declaration| declaration.rank);
    match declared {
        Some(rank) if rank.meets_entry_requirement() => None,
        other => Some(RegistrationBlock::RankBelowEntry { declared: other }),
    }
}

/// 2つのゲートをまとめて評価する。**畳まない**——それぞれの理由を保ったまま返す。
///
/// `pending_reports` は [`formation_blocks_queue`] が真になった formation の一覧。
pub fn evaluate(
    profile: Option<&PlayerProfile>,
    pending_reports: Vec<FormationId>,
) -> QueueEligibility {
    QueueEligibility {
        registration: registration_gate(profile).or_else(|| {
            // プロフィールが無い＝ログインすらしていない
            profile
                .is_none()
                .then_some(RegistrationBlock::NotRegistered)
        }),
        pending_reports,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::lifecycle::{PlayerId, Timestamp};
    use crate::registration::model::Declaration;

    fn at(seconds: i64) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + chrono::TimeDelta::seconds(seconds)
    }

    fn fid(n: u128) -> FormationId {
        FormationId(Uuid::from_u128(n))
    }

    fn host() -> PlayerId {
        PlayerId(Uuid::from_u128(1))
    }

    /// 登録済み・active・レジェンド申告のプロフィール。
    fn registered(rank: Option<Rank>) -> PlayerProfile {
        PlayerProfile {
            player_id: host(),
            discord_user_id: "1001".into(),
            trainer_name: Some("トレーナー".into()),
            status: Some(PlayerStatus::Active),
            registered_at: Some(at(0)),
            latest_declaration: rank.map(|rank| Declaration {
                declared_at: at(0),
                rank: Some(rank),
                rating: Some(1500),
            }),
        }
    }

    // ---- 報告ゲート ----

    #[test]
    fn only_states_that_require_reports_block_the_queue() {
        let in_progress = FormationState::InProgress {
            host: host(),
            room_code: "1234".into(),
        };
        // 報告義務のある状態で、本人が未報告なら塞ぐ
        for state in [
            in_progress.clone(),
            FormationState::Confirmed,
            FormationState::Disputed,
            FormationState::Nullified,
        ] {
            assert!(
                formation_blocks_queue(&state, false),
                "{state} must block an unreported player"
            );
        }
        // 試合が成立していない状態は、報告していなくても塞がない
        for state in [
            FormationState::Matched,
            FormationState::AwaitingHost {
                pending: None,
                tried: 0,
            },
            FormationState::AwaitingCode {
                host: host(),
                tried: 1,
            },
            FormationState::Voided,
            FormationState::Abandoned,
        ] {
            assert!(!formation_blocks_queue(&state, false), "{state}");
        }
    }

    #[test]
    fn reporting_yourself_releases_the_gate_even_while_disputed() {
        // 解除条件は「自分が報告したこと」だけ。確定を待たない（他 9 人に人質を取られない）
        for state in [
            FormationState::InProgress {
                host: host(),
                room_code: "1234".into(),
            },
            FormationState::Confirmed,
            FormationState::Disputed,
            FormationState::Nullified,
        ] {
            assert!(
                !formation_blocks_queue(&state, true),
                "{state} must not block a player who reported"
            );
        }
    }

    // ---- 登録ゲート ----

    #[test]
    fn the_registration_gate_lets_a_registered_legend_through() {
        let profile = registered(Some(Rank::Legend));
        assert_eq!(registration_gate(Some(&profile)), None);
    }

    #[test]
    fn the_registration_gate_blocks_each_reason_separately() {
        // 一度もログインしていない
        assert_eq!(
            registration_gate(None),
            None,
            "no profile is handled by evaluate"
        );

        // ログイン済みだが未登録
        let mut profile = registered(Some(Rank::Legend));
        profile.registered_at = None;
        assert_eq!(
            registration_gate(Some(&profile)),
            Some(RegistrationBlock::NotRegistered)
        );

        // 停止処分。登録済みでも弾く
        let mut profile = registered(Some(Rank::Legend));
        profile.status = Some(PlayerStatus::Suspended);
        assert_eq!(
            registration_gate(Some(&profile)),
            Some(RegistrationBlock::Suspended)
        );

        // 読めない status は安全側に倒す
        let mut profile = registered(Some(Rank::Legend));
        profile.status = None;
        assert_eq!(
            registration_gate(Some(&profile)),
            Some(RegistrationBlock::UnknownStatus)
        );

        // 直近の申告がレジェンド未満
        let profile = registered(Some(Rank::Master));
        assert_eq!(
            registration_gate(Some(&profile)),
            Some(RegistrationBlock::RankBelowEntry {
                declared: Some(Rank::Master)
            })
        );

        // 申告が無い / 読めない
        let profile = registered(None);
        assert_eq!(
            registration_gate(Some(&profile)),
            Some(RegistrationBlock::RankBelowEntry { declared: None })
        );
    }

    #[test]
    fn suspension_takes_precedence_over_being_unregistered() {
        let mut profile = registered(Some(Rank::Legend));
        profile.status = Some(PlayerStatus::Suspended);
        profile.registered_at = None;
        assert_eq!(
            registration_gate(Some(&profile)),
            Some(RegistrationBlock::Suspended)
        );
    }

    // ---- 2つを畳まないこと ----

    #[test]
    fn the_two_gates_stay_separate() {
        let profile = registered(Some(Rank::Master));
        let eligibility = evaluate(Some(&profile), vec![fid(1), fid(2)]);
        assert!(!eligibility.is_allowed());
        // どちらの理由も残っている
        assert_eq!(
            eligibility.registration,
            Some(RegistrationBlock::RankBelowEntry {
                declared: Some(Rank::Master)
            })
        );
        assert_eq!(eligibility.pending_reports, vec![fid(1), fid(2)]);
        assert!(eligibility.is_report_blocked());
        let messages = eligibility.messages();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[0].contains("レジェンド"), "{messages:?}");
        assert!(messages[1].contains('2'), "{messages:?}");
    }

    #[test]
    fn a_registered_legend_with_no_pending_reports_may_queue() {
        let profile = registered(Some(Rank::Legend));
        let eligibility = evaluate(Some(&profile), Vec::new());
        assert!(eligibility.is_allowed());
        assert!(eligibility.messages().is_empty());
        assert_eq!(eligibility, QueueEligibility::default());
    }

    #[test]
    fn each_gate_can_block_on_its_own() {
        // 報告ゲートだけ
        let profile = registered(Some(Rank::Legend));
        let report_only = evaluate(Some(&profile), vec![fid(1)]);
        assert!(!report_only.is_allowed());
        assert_eq!(report_only.registration, None);
        assert_eq!(report_only.messages().len(), 1);

        // 登録ゲートだけ
        let mut profile = registered(Some(Rank::Legend));
        profile.registered_at = None;
        let registration_only = evaluate(Some(&profile), Vec::new());
        assert!(!registration_only.is_allowed());
        assert!(!registration_only.is_report_blocked());
        assert_eq!(registration_only.messages().len(), 1);
    }

    #[test]
    fn a_player_who_never_logged_in_is_not_registered() {
        let eligibility = evaluate(None, Vec::new());
        assert!(!eligibility.is_allowed());
        assert_eq!(
            eligibility.registration,
            Some(RegistrationBlock::NotRegistered)
        );
    }
}
