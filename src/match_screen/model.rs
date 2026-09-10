//! マッチ画面が描く値。**外に出せないものは、そもそも型に場所が無い。**
//!
//! この方針が守るもの:
//!
//! - **他人の報告状況は入る場所が無い**（00 §3-5）。件数のフィールドも作らない。
//!   「9人報告済み、自分だけ未報告」が読めると圧力になり、00 §5 が却下した
//!   「他9人に人質を取られる」構造が別の形で戻る。ここにあるのは
//!   [`MatchView::i_reported`]（自分の分だけ）と、確定後の自分の増減だけ。
//! - **相手チームのロールは入る場所が無い**。自チームは [`TeammateView`]、相手は
//!   [`OpponentView`] と型を分けてあり、後者に `role` フィールドが存在しない。
//!   HTML にもJSON にも出しようがない（DOM を見れば分かる形にしない）。
//! - σ、レート調整の内訳、他人の戦績は読み込まない。**現在のレートだけ**を出す。
//!   レートは公開方針（00 §4「σは公開する」「ランキングは公開」）なので隠さない。
//!
//! ロールを隠すのは、部屋に入るまでの数分で相手の構成に合わせたドラフト準備を
//! されるのを防ぐため。ゲーム内のドラフトが始まれば分かるので、隠せるのはその数分だけ。

use serde::Serialize;
use uuid::Uuid;

/// チームの識別子。`red` / `blue`（DB は text）。
pub const RED: &str = "red";
/// 青チーム。
pub const BLUE: &str = "blue";

/// 自チームの1人。**ロールを出す。**
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TeammateView {
    /// `players.id`。画面には出さないが、自分の行を特定するために持つ。
    pub player_id: Uuid,
    /// 表示名。未登録なら `(名前未登録)`。
    pub trainer_name: String,
    /// 割り当てられたロールの表示名。
    pub role: String,
    /// 現在のレート（表示スケール）。追記ログから毎回導出した値。
    pub rating: f64,
    /// Discord の表示名。取れていなければ `None`（画面はトレーナー名で出す）。
    pub discord_name: Option<String>,
    /// Discord のアバター。**未設定でも既定アバターの URL が入る。**
    pub avatar_url: String,
    /// 自分自身の行か。画面で強調する。
    pub is_me: bool,
    /// この試合のホストか。
    pub is_host: bool,
}

/// 相手チームの1人。**`role` フィールドが無い。**
///
/// 「HTML から消す」ではなく「値を持たない」形にしてある。描画の分岐を間違えても、
/// レスポンスに載せる対象を増やしても、ロールは出てこない。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpponentView {
    /// `players.id`。
    pub player_id: Uuid,
    /// 表示名。
    pub trainer_name: String,
    /// 現在のレート（表示スケール）。
    pub rating: f64,
    /// Discord の表示名。取れていなければ `None`。
    pub discord_name: Option<String>,
    /// Discord のアバター。
    pub avatar_url: String,
    /// この試合のホストか。**ホストは相手チームでも伝える**
    /// （誰が部屋を建てたか分からないと、入れなかったときに聞く相手が分からない）。
    pub is_host: bool,
}

/// 片チームの集合先 VC。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VoiceView {
    /// `vc3-red` 形式の表示名。
    pub channel_name: String,
    /// Discord のチャンネル ID（文字列）。リンクに使う。
    pub channel_id: String,
}

/// 進行状況。マッチ画面はこの1値で表示を切り替える。
///
/// 状態機械（`lifecycle`）の状態を**そのまま写したもの**で、判定を足していない。
/// 進行に関わる分岐は必ず `FormationState` の側で決まる。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum StageView {
    /// 成立直後。VC 割り当て待ち。**JSON の値は状態機械の名前と同じ**
    /// （画面のポーリングが `data-stage` と突き合わせる）。
    Matched,
    /// ホスト打診中。誰に打診しているかは全員に見せる（進行状況なので隠さない）。
    AwaitingHost {
        /// 打診中の候補の表示名。指名前なら `None`。
        pending: Option<String>,
        /// 自分が打診されている候補か。
        i_am_candidate: bool,
    },
    /// ホストが承諾し、ルーム ID の入力待ち。
    AwaitingCode {
        /// ホストの表示名。
        host: String,
        /// 自分がホストか。
        i_am_host: bool,
    },
    /// ルーム ID 開示済み。試合中および報告受付中。
    InProgress {
        /// 現在有効なルーム ID。
        room_code: String,
        /// ホストの表示名。
        host: String,
        /// 自分がホストか（打ち直しの入力欄を出す）。
        i_am_host: bool,
    },
    /// 勝敗確定。
    Confirmed {
        /// `red` / `blue` / `draw`。
        outcome: String,
        /// 自分から見た勝敗（`won` / `lost` / `drawn`）。
        result: String,
        /// **自分の**増減。確定していて自分の行がビューに出るときだけ入る。
        my_delta: Option<f64>,
    },
    /// 報告が割れた。**他人の報告内容は一切入れない。**
    Disputed,
    /// 報告者全員が無効試合で一致した。増減なし。
    Nullified,
    /// 試合が成立しなかった。
    Voided,
    /// 解散。
    Abandoned,
}

impl StageView {
    /// `formation_events` 由来の状態名（ログとテストの照合用）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::AwaitingHost { .. } => "awaiting_host",
            Self::AwaitingCode { .. } => "awaiting_code",
            Self::InProgress { .. } => "in_progress",
            Self::Confirmed { .. } => "confirmed",
            Self::Disputed => "disputed",
            Self::Nullified => "nullified",
            Self::Voided => "voided",
            Self::Abandoned => "abandoned",
        }
    }
}

/// この閲覧者が**いま押せる操作**。押せない理由は返さない（他人の状況が漏れる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct AffordanceView {
    /// 「部屋を作る」「今回はパスする」。打診中の候補本人にだけ true。
    pub host_response: bool,
    /// ルーム ID の入力欄。ホスト本人にだけ true（開示後の打ち直しも同じ入口）。
    pub room_code: bool,
    /// 3択の報告。参加者で、報告を受け付ける状態で、まだ自分が報告していないとき true。
    pub report: bool,
    /// 聞き専報告の入口。**確定していて、勝敗がついているときだけ true。**
    ///
    /// 確定前に出さないのは、勝敗で閾値が変わるため。**票数も閾値もここには入らない**
    /// ——出せるのは「入口を出すかどうか」だけ。
    pub quiet_report: bool,
    /// **自分が**聞き専報告を出したか。二度押しさせないためだけに返す。
    /// 誰を報告したかも、何票入っているかも返さない。
    pub quiet_reported: bool,
    /// 無効試合の通報の入口。**自分が「無効試合」を報告していて、まだ通報して
    /// いないときだけ true。**確定は待たない（報告直後に推移させるのが目的）。
    ///
    /// 票数も閾値もここには入らない——出せるのは「入口を出すかどうか」だけ。
    pub nullified_report: bool,
    /// **自分が**無効試合の通報を出したか（「誰も通報しない」も含む）。
    /// 二度押しさせないためだけに返す。誰を通報したかは返さない。
    pub nullified_reported: bool,
}

/// マッチ画面1枚ぶん。成立から確定まで、この型の中身だけが変わる。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchView {
    /// `formations.id`。
    pub formation_id: Uuid,
    /// 進行状況。
    #[serde(flatten)]
    pub stage: StageView,
    /// 自分のチーム（`red` / `blue`）。
    pub my_team: String,
    /// 自チームの5人。ロール順。
    pub teammates: Vec<TeammateView>,
    /// 相手チームの5人。**ロールは持たない。**
    pub opponents: Vec<OpponentView>,
    /// 自チームの集合先。割り当て前は `None`。
    pub my_voice: Option<VoiceView>,
    /// 相手チームの集合先（どこにいるかは公開情報）。
    pub opponent_voice: Option<VoiceView>,
    /// 部屋設定のチェックリスト（先頭がマップ）。
    pub room_checklist: Vec<String>,
    /// **自分が**報告済みか。他人の分は数えない。
    pub i_reported: bool,
    /// いま押せる操作。
    pub actions: AffordanceView,
}

impl MatchView {
    /// 相手チームの識別子。
    pub fn opponent_team(&self) -> &str {
        if self.my_team == RED { BLUE } else { RED }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opponent() -> OpponentView {
        OpponentView {
            player_id: Uuid::from_u128(1),
            trainer_name: "相手".into(),
            rating: 1511.0,
            discord_name: Some("ディスコ名".into()),
            avatar_url: "https://cdn.discordapp.com/embed/avatars/1.png".into(),
            is_host: true,
        }
    }

    #[test]
    fn an_opponent_row_has_nowhere_to_put_a_role() {
        // 相手チームの行を JSON にしても、ロールに相当するキーが出てこないこと。
        // フィールドを足したらこのテストが落ちる（意図的に足すなら、この理由を否定してから）
        let json = serde_json::to_value(opponent()).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "avatar_url",
                "discord_name",
                "is_host",
                "player_id",
                "rating",
                "trainer_name"
            ]
        );
        assert!(!json.to_string().contains("role"), "{json}");
    }

    #[test]
    fn the_view_never_carries_other_players_reports() {
        let view = MatchView {
            formation_id: Uuid::from_u128(9),
            stage: StageView::Disputed,
            my_team: RED.into(),
            teammates: vec![TeammateView {
                player_id: Uuid::from_u128(2),
                trainer_name: "自分".into(),
                role: "中央".into(),
                rating: 1500.0,
                discord_name: None,
                avatar_url: "https://cdn.discordapp.com/embed/avatars/0.png".into(),
                is_me: true,
                is_host: false,
            }],
            opponents: vec![opponent()],
            my_voice: None,
            opponent_voice: None,
            room_checklist: vec!["マップ: kyogre".into()],
            i_reported: true,
            actions: AffordanceView::default(),
        };
        let json = serde_json::to_string(&view).unwrap();
        // 件数・内訳・他人の報告に相当するキーがどこにも無い
        for forbidden in [
            "reports",
            "reported_count",
            "pending",
            "agreement",
            "outcome",
            "sigma",
            "delta_mu",
            "adjust",
            "role\":\"", // 相手側の role が紛れ込んでいないこと（自チームは role キーを持つ）
        ] {
            if forbidden == "role\":\"" {
                assert_eq!(
                    json.matches(forbidden).count(),
                    1,
                    "role は自チームの1人ぶんだけ: {json}"
                );
                continue;
            }
            assert!(
                !json.contains(forbidden),
                "{forbidden} が漏れている: {json}"
            );
        }
    }

    #[test]
    fn the_opposing_team_is_derived_not_stored_twice() {
        let mut view = MatchView {
            formation_id: Uuid::from_u128(9),
            stage: StageView::Matched,
            my_team: RED.into(),
            teammates: Vec::new(),
            opponents: Vec::new(),
            my_voice: None,
            opponent_voice: None,
            room_checklist: Vec::new(),
            i_reported: false,
            actions: AffordanceView::default(),
        };
        assert_eq!(view.opponent_team(), BLUE);
        view.my_team = BLUE.into();
        assert_eq!(view.opponent_team(), RED);
    }

    #[test]
    fn stage_names_match_the_state_machine() {
        use crate::lifecycle::{FormationState, PlayerId};
        let player = PlayerId(Uuid::from_u128(1));
        let pairs: [(StageView, FormationState); 8] = [
            (StageView::Matched, FormationState::Matched),
            (
                StageView::AwaitingHost {
                    pending: None,
                    i_am_candidate: false,
                },
                FormationState::AwaitingHost {
                    pending: None,
                    tried: 0,
                },
            ),
            (
                StageView::AwaitingCode {
                    host: "h".into(),
                    i_am_host: false,
                },
                FormationState::AwaitingCode {
                    host: player,
                    tried: 1,
                },
            ),
            (
                StageView::InProgress {
                    room_code: "1234".into(),
                    host: "h".into(),
                    i_am_host: false,
                },
                FormationState::InProgress {
                    host: player,
                    room_code: "1234".into(),
                },
            ),
            (StageView::Disputed, FormationState::Disputed),
            (StageView::Nullified, FormationState::Nullified),
            (StageView::Voided, FormationState::Voided),
            (StageView::Abandoned, FormationState::Abandoned),
        ];
        for (stage, state) in pairs {
            assert_eq!(stage.name(), state.name(), "{stage:?}");
        }
    }
}
