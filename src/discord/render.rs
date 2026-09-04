//! スレッドに出す文言。純粋関数。
//!
//! スレッドに出してよいのは、割り当て表・集合場所（VC）・部屋設定・ルーム ID・ホスト打診だけ。
//! 勝敗に関する語は一切出さない（`tests` で検査する）。

use serenity::model::id::UserId;

use crate::lifecycle::{FormationId, PlayerId};
use crate::persistence::{RosterEntry, Team};

use super::custom_id::{Action, CustomId, ROOM_CODE_INPUT_ID};
use super::gateway::{Button, ButtonStyle, Modal, OutgoingMessage};
use super::vc_pool::VcPair;

/// ルーム ID の最大長（モーダルの入力制限）。
pub const ROOM_CODE_MAX_LENGTH: u16 = 32;

/// チームの表示名（`formation_players.team` は 0 / 1）。
pub fn team_name(team: Team) -> &'static str {
    match team {
        Team::Zero => "red",
        Team::One => "blue",
    }
}

/// 表示名からチームへ。`red` / `blue` 以外は `None`。
pub fn team_from_name(name: &str) -> Option<Team> {
    match name {
        "red" => Some(Team::Zero),
        "blue" => Some(Team::One),
        _ => None,
    }
}

fn team_emoji(team: Team) -> &'static str {
    match team {
        Team::Zero => "🔴",
        Team::One => "🔵",
    }
}

/// VC の名前（`VC-<label>-<team>`）。
pub fn vc_channel_name(pair: &VcPair, team: Team) -> String {
    format!("VC-{}-{}", pair.label, team_name(team))
}

fn vc_channel_of(pair: &VcPair, team: Team) -> u64 {
    match team {
        Team::Zero => pair.red.get(),
        Team::One => pair.blue.get(),
    }
}

fn vc_line(pair: &VcPair, team: Team) -> String {
    format!(
        "{} は {}（<#{}>）に集合",
        team_name(team),
        vc_channel_name(pair, team),
        vc_channel_of(pair, team)
    )
}

/// スレッド名。
pub fn thread_name(formation: FormationId) -> String {
    let simple = formation.0.simple().to_string();
    format!("match-{}", &simple[..8])
}

/// Discord ユーザーのメンション。
fn mention(user: UserId) -> String {
    format!("<@{user}>")
}

/// 割り当て表 + 集合場所 + 部屋設定のチェックリスト。
pub fn assignment(roster: &[RosterEntry], vc: &VcPair, checklist: &[String]) -> OutgoingMessage {
    let mut content = String::from("## 割り当て表\n");
    let mut mention_users = Vec::with_capacity(roster.len());
    for team in [Team::Zero, Team::One] {
        content.push_str(&format!(
            "{} **{}** — {}\n",
            team_emoji(team),
            team_name(team),
            vc_line(vc, team)
        ));
        let mut members: Vec<&RosterEntry> =
            roster.iter().filter(|entry| entry.team == team).collect();
        members.sort_by(|a, b| {
            a.role_sort_order
                .cmp(&b.role_sort_order)
                .then_with(|| a.player_id.cmp(&b.player_id))
        });
        for entry in members {
            if let Some(user) = user_of(entry) {
                mention_users.push(user);
                content.push_str(&format!(
                    "- {} — {}\n",
                    mention(user),
                    entry.role_display_name
                ));
            } else {
                content.push_str(&format!(
                    "- （不明なユーザー） — {}\n",
                    entry.role_display_name
                ));
            }
        }
    }
    if !checklist.is_empty() {
        content.push_str("\n## 部屋設定\n");
        for item in checklist {
            content.push_str(&format!("- [ ] {item}\n"));
        }
    }
    OutgoingMessage {
        content,
        buttons: Vec::new(),
        mention_users,
    }
}

/// ホスト候補への打診（[ 部屋を作る ] [ 今回はパスする ]）。
///
/// 候補が変わったらこのメッセージを編集する。新しいメッセージを積まない。
pub fn host_prompt(
    candidate: UserId,
    formation: FormationId,
    player: PlayerId,
    index: usize,
    total: usize,
) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{} さん、部屋の作成をお願いできますか？（候補 {}/{}）\n\
             部屋を作ったら「部屋を作る」を押して、ルーム ID を入力してください。",
            mention(candidate),
            index,
            total
        ),
        buttons: vec![
            Button {
                custom_id: CustomId::new(Action::HostAccept, formation, player).encode(),
                label: "部屋を作る".into(),
                style: ButtonStyle::Primary,
            },
            Button {
                custom_id: CustomId::new(Action::HostDecline, formation, player).encode(),
                label: "今回はパスする".into(),
                style: ButtonStyle::Secondary,
            },
        ],
        mention_users: vec![candidate],
    }
}

/// ホストが承諾した後の状態表示（[ ルーム ID を入力 ]）。他の 9 人にも承諾が分かる。
pub fn host_accepted(host: UserId, formation: FormationId, player: PlayerId) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{} さんが部屋を作成中です。作成できたら「ルーム ID を入力」からルーム ID を送ってください。",
            mention(host)
        ),
        buttons: vec![Button {
            custom_id: CustomId::new(Action::CodeOpen, formation, player).encode(),
            label: "ルーム ID を入力".into(),
            style: ButtonStyle::Primary,
        }],
        mention_users: Vec::new(),
    }
}

/// ルーム ID 入力後の状態表示（[ ルーム ID を修正 ]）。
pub fn code_status(
    host: UserId,
    room_code: &str,
    formation: FormationId,
    player: PlayerId,
) -> OutgoingMessage {
    OutgoingMessage {
        content: format!("ルーム ID: **{}**（ホスト: {}）", room_code, mention(host)),
        buttons: vec![Button {
            custom_id: CustomId::new(Action::CodeOpen, formation, player).encode(),
            label: "ルーム ID を修正".into(),
            style: ButtonStyle::Secondary,
        }],
        mention_users: Vec::new(),
    }
}

/// 残り 9 人へのルーム ID の開示。貼り直しはこのメッセージを編集する。
pub fn code_reveal(
    host: UserId,
    others: &[UserId],
    room_code: &str,
    vc: Option<&VcPair>,
) -> OutgoingMessage {
    let mut content = String::new();
    if !others.is_empty() {
        let mentions: Vec<String> = others.iter().map(|user| mention(*user)).collect();
        content.push_str(&mentions.join(" "));
        content.push('\n');
    }
    content.push_str(&format!(
        "ルーム ID は **{}** です（ホスト: {}）。\n",
        room_code,
        mention(host)
    ));
    if let Some(vc) = vc {
        content.push_str(&format!(
            "{}、{}してください。",
            vc_line(vc, Team::Zero),
            vc_line(vc, Team::One)
        ));
    }
    OutgoingMessage {
        content,
        buttons: Vec::new(),
        mention_users: others.to_vec(),
    }
}

/// ホスト候補が尽きた。
pub fn host_exhausted() -> OutgoingMessage {
    OutgoingMessage {
        content: "ホストが決まらなかったため、この試合は解散になりました。".into(),
        buttons: Vec::new(),
        mention_users: Vec::new(),
    }
}

/// ルーム ID を受け取るモーダル。
pub fn code_modal(formation: FormationId, player: PlayerId, current: Option<&str>) -> Modal {
    Modal {
        custom_id: CustomId::new(Action::CodeSubmit, formation, player).encode(),
        title: if current.is_some() {
            "ルーム ID の修正".into()
        } else {
            "ルーム ID の入力".into()
        },
        input_custom_id: ROOM_CODE_INPUT_ID.into(),
        input_label: "ルーム ID".into(),
        placeholder: Some("ゲーム内で表示されるルーム ID".into()),
        max_length: ROOM_CODE_MAX_LENGTH,
        prefill: current.map(str::to_owned),
    }
}

/// 打診されていない人がボタンを押した（本人にだけ表示）。
pub fn not_addressed_to_you(addressee: Option<UserId>) -> String {
    match addressee {
        Some(user) => format!("このボタンは {} さん宛てです。", mention(user)),
        None => "このボタンはあなた宛てではありません。".into(),
    }
}

/// 古い、または無効になった操作（本人にだけ表示）。
pub fn stale() -> String {
    "この操作はもう有効ではありません。最新のメッセージを確認してください。".into()
}

/// ホスト以外がルーム ID を入力しようとした（本人にだけ表示）。
pub fn not_the_host() -> String {
    "ルーム ID を入力できるのはホストだけです。".into()
}

/// ルーム ID が空だった（本人にだけ表示）。
pub fn code_invalid() -> String {
    "ルーム ID が空です。もう一度入力してください。".into()
}

/// 編成の Discord ユーザー。0 以下は無効（`None`）。
pub fn user_of(entry: &RosterEntry) -> Option<UserId> {
    u64::try_from(entry.discord_user_id)
        .ok()
        .filter(|id| *id != 0)
        .map(UserId::new)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    /// 勝敗を示す語。スレッドに出る文言にこれらが含まれてはならない。
    const FORBIDDEN: [&str; 14] = [
        "勝",
        "負",
        "敗",
        "引き分け",
        "結果",
        "報告",
        "win",
        "lose",
        "loss",
        "victory",
        "defeat",
        "draw",
        "result",
        "report",
    ];

    fn roster() -> Vec<RosterEntry> {
        (1..=10)
            .map(|n| RosterEntry {
                player_id: PlayerId(Uuid::from_u128(n)),
                discord_user_id: 1000 + n as i64,
                team: if n <= 5 { Team::Zero } else { Team::One },
                role: format!("role{}", n % 5),
                role_display_name: format!("ロール{}", n % 5),
                role_sort_order: (n % 5) as i32,
                mu_at_match: 25.0,
            })
            .collect()
    }

    fn vc() -> VcPair {
        VcPair {
            label: "01".into(),
            red: serenity::model::id::ChannelId::new(11),
            blue: serenity::model::id::ChannelId::new(12),
        }
    }

    fn all_texts() -> Vec<String> {
        let formation = FormationId(Uuid::from_u128(0xf0));
        let player = PlayerId(Uuid::from_u128(1));
        let host = UserId::new(1001);
        let others: Vec<UserId> = (1002..=1010).map(UserId::new).collect();
        let checklist = vec!["設定 A".to_owned(), "設定 B".to_owned()];
        let messages = [
            assignment(&roster(), &vc(), &checklist),
            host_prompt(host, formation, player, 1, 3),
            host_accepted(host, formation, player),
            code_status(host, "1234", formation, player),
            code_reveal(host, &others, "1234", Some(&vc())),
            code_reveal(host, &others, "1234", None),
            host_exhausted(),
        ];
        let mut texts: Vec<String> = messages
            .iter()
            .flat_map(|message| {
                std::iter::once(message.content.clone())
                    .chain(message.buttons.iter().map(|button| button.label.clone()))
            })
            .collect();
        let modal = code_modal(formation, player, Some("1234"));
        texts.push(modal.title);
        texts.push(modal.input_label);
        texts.extend(modal.placeholder);
        texts.push(not_addressed_to_you(Some(host)));
        texts.push(stale());
        texts.push(not_the_host());
        texts.push(code_invalid());
        texts.push(thread_name(formation));
        texts
    }

    #[test]
    fn thread_texts_never_mention_the_outcome() {
        for text in all_texts() {
            let lower = text.to_lowercase();
            for word in FORBIDDEN {
                assert!(
                    !lower.contains(word),
                    "{text:?} contains the forbidden word {word:?}"
                );
            }
        }
    }

    #[test]
    fn assignment_lists_all_ten_players_by_team_and_role_order() {
        let message = assignment(&roster(), &vc(), &["設定 A".to_owned()]);
        assert_eq!(message.mention_users.len(), 10);
        assert!(message.content.contains("VC-01-red"));
        assert!(message.content.contains("VC-01-blue"));
        assert!(message.content.contains("<#11>"));
        assert!(message.content.contains("<#12>"));
        assert!(message.content.contains("- [ ] 設定 A"));
        let red = message.content.find("**red**").unwrap();
        let blue = message.content.find("**blue**").unwrap();
        assert!(red < blue);
        // red チーム内はロールの表示順（ロール0 が先頭）
        let first_red = message.content[red..blue].find("<@1005>").unwrap();
        let second_red = message.content[red..blue].find("<@1001>").unwrap();
        assert!(first_red < second_red);
    }

    #[test]
    fn checklist_section_is_omitted_when_empty() {
        let message = assignment(&roster(), &vc(), &[]);
        assert!(!message.content.contains("部屋設定"));
    }

    #[test]
    fn host_prompt_has_accept_and_decline_buttons_for_the_candidate() {
        let formation = FormationId(Uuid::from_u128(0xf0));
        let player = PlayerId(Uuid::from_u128(1));
        let message = host_prompt(UserId::new(1001), formation, player, 2, 3);
        assert_eq!(message.mention_users, vec![UserId::new(1001)]);
        assert!(message.content.contains("2/3"));
        let ids: Vec<CustomId> = message
            .buttons
            .iter()
            .map(|button| CustomId::parse(&button.custom_id).unwrap())
            .collect();
        assert_eq!(ids[0].action, Action::HostAccept);
        assert_eq!(ids[1].action, Action::HostDecline);
        assert!(
            ids.iter()
                .all(|id| id.formation == formation && id.player == player)
        );
    }

    #[test]
    fn code_reveal_pings_the_other_nine_only() {
        let host = UserId::new(1001);
        let others: Vec<UserId> = (1002..=1010).map(UserId::new).collect();
        let message = code_reveal(host, &others, "8888", Some(&vc()));
        assert_eq!(message.mention_users, others);
        assert!(message.content.contains("**8888**"));
        assert!(message.content.contains("VC-01-red"));
    }

    #[test]
    fn modal_prefills_the_current_code_when_correcting() {
        let formation = FormationId(Uuid::from_u128(0xf0));
        let player = PlayerId(Uuid::from_u128(1));
        assert_eq!(code_modal(formation, player, None).prefill, None);
        assert_eq!(
            code_modal(formation, player, Some("1234"))
                .prefill
                .as_deref(),
            Some("1234")
        );
        let parsed = CustomId::parse(&code_modal(formation, player, None).custom_id).unwrap();
        assert_eq!(parsed.action, Action::CodeSubmit);
    }

    #[test]
    fn team_names_round_trip() {
        for team in [Team::Zero, Team::One] {
            assert_eq!(team_from_name(team_name(team)), Some(team));
        }
        assert_eq!(team_from_name("green"), None);
    }
}
