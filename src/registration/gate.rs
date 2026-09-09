//! キューのゲート。純粋関数。
//!
//! ゲートは**3つに分ける**（05-登録トラック仕様 §4）。1つのフラグに畳むと、キュー画面で
//! 「なぜ入れないのか」を説明できなくなる。
//!
//! - **登録ゲート**: 未登録 / `status = 'suspended'` / 直近の申告がレジェンド未満
//! - **進行中ゲート**: マッチが成立していて、まだ試合が始まっていない
//! - **報告ゲート**: 未報告の formation を持つ
//!
//! 報告ゲートの解除条件は「**自分が報告したこと**」だけ（02 D-3）。「確定したこと」を条件に
//! すると、他 9 人に人質を取られる。`disputed` は係争の解決を待たずに解除済みとして扱う
//! ——「自分が報告したか」しか見ないので、係争入りは判定に影響しない。
//!
//! 未報告かどうかは**自分の報告の有無だけ**で決める。他人の報告の内容・件数は読まない
//! （報告のブラインドは API レベルの制約であり、ここもその内側にある）。
//!
//! **進行中ゲートと報告ゲートは重ならない。**境界は「報告する手段があるか」である。
//!
//! | 状態 | 塞ぐゲート |
//! |---|---|
//! | `matched` / `awaiting_host` / `awaiting_code` | 進行中ゲート（報告ボタンがまだ無い） |
//! | `in_progress` 以降 | 報告ゲート（プレイヤー単位。報告すれば解除） |
//! | `in_progress`（`REPORT_GATE_DISABLED` 有効時のみ） | 進行中ゲート（報告ゲートが切れているため） |
//!
//! **`in_progress` 以降に進行中ゲートを被せないこと。**報告を済ませた人が formation の確定を
//! 待たされることになり、00 §5 の「報告ゲートの解除条件を『確定』にしない —— 他 9 人に
//! 人質を取られる」がそのまま復活する。
//!
//! **例外は `REPORT_GATE_DISABLED` が有効なときだけ。**報告ゲートを切ると `in_progress` を
//! 塞ぐものが無くなり、試合中の人がキューに入って二重割当される。フラグを使うのは
//! 「報告フローが壊れて縮退運用している最中」なので、そこで一番分かりにくいバグを出さない
//! ために、そのときだけ進行中ゲートを `in_progress` まで広げる
//! （[`active_formation_blocks_queue`]）。人質問題は起きない——報告ゲート自体が切れており、
//! 塞いでいるのは「まだ試合が終わっていない」ことだけだからである。
//!
//! 進行中ゲートが要る理由: マッチメイカは成立時に 10 人をキューから外すが、`awaiting_host`
//! （候補 1 人あたり 90 秒 × 最大 3 人）と `awaiting_code`（180 秒）の間、本人は再投入できて
//! しまう。マッチメイカは 3 分ごとに走るので、この窓は複数回のティックをまたぐ。塞がないと
//! 同じ人が 2 つの formation に割り当てられ、VC の組も 2 つ占有する。

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

/// キューに入れるかどうかの判定。3つのゲートを**別々に**持つ。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueEligibility {
    /// 登録ゲート。通っていれば `None`。
    pub registration: Option<RegistrationBlock>,
    /// 進行中ゲート。成立済みでまだ始まっていない formation。無ければ通過。
    pub active_formation: Option<FormationId>,
    /// 報告ゲート。未報告の formation。空なら通過。
    pub pending_reports: Vec<FormationId>,
}

impl QueueEligibility {
    /// 3つとも通ったか。
    pub fn is_allowed(&self) -> bool {
        self.registration.is_none()
            && self.active_formation.is_none()
            && self.pending_reports.is_empty()
    }

    /// 進行中ゲートに引っかかっているか。
    pub fn is_active_formation_blocked(&self) -> bool {
        self.active_formation.is_some()
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
        if self.active_formation.is_some() {
            messages.push(
                "マッチが成立しています。Discord の案内に従ってください。この試合が終わるまでキューには入れません。"
                    .into(),
            );
        }
        if let Some(count) = std::num::NonZeroUsize::new(self.pending_reports.len()) {
            messages.push(format!(
                "未報告の試合が{count}件あります。結果を報告するとキューに入れます。"
            ));
        }
        messages
    }
}

/// その formation が、このプレイヤーのキューを**報告ゲート**で塞ぐか。
///
/// 塞ぐのは「報告義務のある状態」かつ「本人がまだ報告していない」とき。
/// `player_reported` は**本人の報告の有無**だけで、他人の報告は見ない。
pub fn formation_blocks_queue(state: &FormationState, player_reported: bool) -> bool {
    state.requires_reports() && !player_reported
}

/// その formation が、このプレイヤーのキューを**進行中ゲート**で塞ぐか。
///
/// 通常は「成立済みで、まだ試合が始まっていない」3 状態だけ。報告する手段がまだ無いので、
/// 報告ゲートでは塞げない。`in_progress` 以降は**含めない**（そこからは報告ゲートが
/// プレイヤー単位で解除する。モジュール冒頭の表を読むこと）。
///
/// **`report_gate_disabled` が真のときだけ `in_progress` まで広げる。**報告ゲートを切ると
/// `in_progress` を塞ぐものが無くなり、試合中の人がキューに入って二重割当される。縮退運用は
/// 「報告フローが壊れている最中」に使うものなので、そこで一番分かりにくいバグを出さない。
/// 逆に、通常時に `in_progress` を被せてはならない（00 §5 の人質問題が復活する）。
///
/// 広げるのは `in_progress` までで、`confirmed` / `disputed` / `nullified` は含めない。
/// 試合は終わっているので二重割当は起きず、そこまで塞ぐとフラグの目的（報告フローが壊れても
/// キューを回す）を潰してしまう。
///
/// 本人の報告の有無を見ないので、他人の報告にも依存しない。
pub fn active_formation_blocks_queue(state: &FormationState, report_gate_disabled: bool) -> bool {
    let before_start = matches!(
        state,
        FormationState::Matched
            | FormationState::AwaitingHost { .. }
            | FormationState::AwaitingCode { .. }
    );
    let unreportable_in_progress =
        report_gate_disabled && matches!(state, FormationState::InProgress { .. });
    before_start || unreportable_in_progress
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

/// 3つのゲートをまとめて評価する。**畳まない**——それぞれの理由を保ったまま返す。
///
/// - `active_formation` は [`active_formation_blocks_queue`] が真になった formation。
/// - `pending_reports` は [`formation_blocks_queue`] が真になった formation の一覧。
///
/// 2つは重ならないので、同じ formation が両方に現れることはない
/// （`REPORT_GATE_DISABLED` で重なりうる `in_progress` は、そのとき報告ゲート側が空になる）。
pub fn evaluate(
    profile: Option<&PlayerProfile>,
    active_formation: Option<FormationId>,
    pending_reports: Vec<FormationId>,
) -> QueueEligibility {
    QueueEligibility {
        registration: registration_gate(profile).or_else(|| {
            // プロフィールが無い＝ログインすらしていない
            profile
                .is_none()
                .then_some(RegistrationBlock::NotRegistered)
        }),
        active_formation,
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

    // ---- 進行中ゲート ----

    /// 成立済みでまだ始まっていない 3 状態だけを塞ぐ。
    #[test]
    fn only_states_before_the_match_starts_raise_the_active_gate() {
        for state in [
            FormationState::Matched,
            FormationState::AwaitingHost {
                pending: None,
                tried: 0,
            },
            FormationState::AwaitingHost {
                pending: Some(host()),
                tried: 2,
            },
            FormationState::AwaitingCode {
                host: host(),
                tried: 1,
            },
        ] {
            assert!(
                active_formation_blocks_queue(&state, false),
                "{state} must block: the player has no way to report yet"
            );
        }
        // 始まった後と、終わった後は塞がない
        for state in [
            FormationState::InProgress {
                host: host(),
                room_code: "1234".into(),
            },
            FormationState::Confirmed,
            FormationState::Disputed,
            FormationState::Nullified,
            FormationState::Voided,
            FormationState::Abandoned,
        ] {
            assert!(!active_formation_blocks_queue(&state, false), "{state}");
        }
    }

    /// **2つのゲートは重ならない。**どの状態も、塞ぐとしてもどちらか一方だけ。
    ///
    /// `in_progress` 以降に進行中ゲートが被ると、報告を済ませた人が formation の確定を
    /// 待たされる（00 §5 の「解除条件を『確定』にしない」が壊れる）。
    #[test]
    fn the_active_and_report_gates_never_overlap() {
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
            FormationState::InProgress {
                host: host(),
                room_code: "1234".into(),
            },
            FormationState::Confirmed,
            FormationState::Disputed,
            FormationState::Nullified,
            FormationState::Voided,
            FormationState::Abandoned,
        ] {
            assert!(
                !(active_formation_blocks_queue(&state, false)
                    && formation_blocks_queue(&state, false)),
                "{state} is claimed by both gates"
            );
        }
    }

    /// 報告しても進行中ゲートは変わらない（本人の報告を見ないゲートなので）。
    #[test]
    fn reporting_does_not_affect_the_active_gate() {
        let state = FormationState::AwaitingCode {
            host: host(),
            tried: 1,
        };
        assert!(active_formation_blocks_queue(&state, false));
        // 始まる前は報告そのものができないので、報告ゲートは両方の値で通す
        assert!(!formation_blocks_queue(&state, false));
        assert!(!formation_blocks_queue(&state, true));
    }

    /// **報告ゲートを切ったときだけ、進行中ゲートが `in_progress` まで広がる。**
    ///
    /// 広げないと、縮退運用の最中に試合中の人がキューへ入って二重割当される。
    #[test]
    fn disabling_the_report_gate_extends_the_active_gate_to_in_progress() {
        let in_progress = FormationState::InProgress {
            host: host(),
            room_code: "1234".into(),
        };
        assert!(
            !active_formation_blocks_queue(&in_progress, false),
            "normally the report gate owns in_progress"
        );
        assert!(
            active_formation_blocks_queue(&in_progress, true),
            "with the report gate off, nothing else would block a live match"
        );

        // 始まる前の 3 状態はフラグに関係なく塞ぐ
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
        ] {
            assert!(active_formation_blocks_queue(&state, false), "{state}");
            assert!(active_formation_blocks_queue(&state, true), "{state}");
        }

        // 試合が終わった後まで広げない。ここまで塞ぐとフラグの目的を潰す
        for state in [
            FormationState::Confirmed,
            FormationState::Disputed,
            FormationState::Nullified,
            FormationState::Voided,
            FormationState::Abandoned,
        ] {
            assert!(
                !active_formation_blocks_queue(&state, true),
                "{state}: the match is over, so there is nothing to double-book"
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

    // ---- 3つを畳まないこと ----

    #[test]
    fn the_three_gates_stay_separate() {
        let profile = registered(Some(Rank::Master));
        let eligibility = evaluate(Some(&profile), Some(fid(3)), vec![fid(1), fid(2)]);
        assert!(!eligibility.is_allowed());
        // 3つとも理由が残っている
        assert_eq!(
            eligibility.registration,
            Some(RegistrationBlock::RankBelowEntry {
                declared: Some(Rank::Master)
            })
        );
        assert_eq!(eligibility.active_formation, Some(fid(3)));
        assert_eq!(eligibility.pending_reports, vec![fid(1), fid(2)]);
        assert!(eligibility.is_active_formation_blocked());
        assert!(eligibility.is_report_blocked());
        let messages = eligibility.messages();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(messages[0].contains("レジェンド"), "{messages:?}");
        assert!(messages[1].contains("マッチが成立"), "{messages:?}");
        assert!(messages[2].contains('2'), "{messages:?}");
    }

    #[test]
    fn a_registered_legend_with_no_pending_reports_may_queue() {
        let profile = registered(Some(Rank::Legend));
        let eligibility = evaluate(Some(&profile), None, Vec::new());
        assert!(eligibility.is_allowed());
        assert!(eligibility.messages().is_empty());
        assert_eq!(eligibility, QueueEligibility::default());
    }

    #[test]
    fn each_gate_can_block_on_its_own() {
        // 報告ゲートだけ
        let profile = registered(Some(Rank::Legend));
        let report_only = evaluate(Some(&profile), None, vec![fid(1)]);
        assert!(!report_only.is_allowed());
        assert_eq!(report_only.registration, None);
        assert!(!report_only.is_active_formation_blocked());
        assert_eq!(report_only.messages().len(), 1);

        // 進行中ゲートだけ
        let active_only = evaluate(Some(&profile), Some(fid(5)), Vec::new());
        assert!(!active_only.is_allowed());
        assert_eq!(active_only.registration, None);
        assert!(!active_only.is_report_blocked());
        assert_eq!(active_only.messages().len(), 1);
        assert!(active_only.messages()[0].contains("マッチが成立"));

        // 登録ゲートだけ
        let mut profile = registered(Some(Rank::Legend));
        profile.registered_at = None;
        let registration_only = evaluate(Some(&profile), None, Vec::new());
        assert!(!registration_only.is_allowed());
        assert!(!registration_only.is_active_formation_blocked());
        assert!(!registration_only.is_report_blocked());
        assert_eq!(registration_only.messages().len(), 1);
    }

    #[test]
    fn a_player_who_never_logged_in_is_not_registered() {
        let eligibility = evaluate(None, None, Vec::new());
        assert!(!eligibility.is_allowed());
        assert_eq!(
            eligibility.registration,
            Some(RegistrationBlock::NotRegistered)
        );
    }

    /// **確定後に未報告のまま残ると、その人のキューは開かない。**
    ///
    /// 早期確定（閾値 7 人）を入れると、確定の時点で 3 人が未報告のまま残るのが
    /// 通常になる。その 3 人の報告を 409 で断ると、ここが恒久的に真のままになり、
    /// キューへ戻れなくなる（00 §5「報告ゲートの解除条件を『確定』にしない」）。
    #[test]
    fn probe_a_confirmed_match_still_gates_an_unreported_player() {
        for state in [
            FormationState::Confirmed,
            FormationState::Nullified,
            FormationState::Disputed,
        ] {
            assert!(
                formation_blocks_queue(&state, false),
                "{state:?}: 未報告のまま塞がれない想定ではない"
            );
            assert!(
                !formation_blocks_queue(&state, true),
                "{state:?}: 報告すれば開く"
            );
            println!(
                "{state:?}: 未報告 -> 塞ぐ={} / 報告済み -> 塞ぐ={}",
                formation_blocks_queue(&state, false),
                formation_blocks_queue(&state, true)
            );
        }
        // 不成立・解散は報告義務が無いので塞がない
        for state in [FormationState::Voided, FormationState::Abandoned] {
            assert!(!formation_blocks_queue(&state, false), "{state:?}");
            println!("{state:?}: 未報告でも塞がない");
        }
    }
}
