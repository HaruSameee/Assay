//! VC のテキストチャットに出す文言。純粋関数。
//!
//! 出してよいのは、割り当て表・集合場所（VC）・部屋設定・ルーム ID・ホスト打診・解散の通知だけ。
//! 勝敗に関する語は一切出さない（`tests` で検査する）。
//!
//! VC は使い回すので、1 本のチャットに何試合分ものメッセージが溜まる（消さない）。
//! どの試合のものか分かるよう、すべての投稿の先頭に見出し（VC ラベル・時刻・試合 ID）を付ける。

use chrono::FixedOffset;
use serenity::model::id::UserId;

use crate::lifecycle::{AbandonReason, FormationId, PlayerId, Timestamp};
use crate::persistence::{Outcome, ReportVote, RosterEntry, Team};

use super::custom_id::{Action, CustomId, ROOM_CODE_INPUT_ID};
use super::gateway::{Button, ButtonStyle, Modal, OutgoingMessage};
use super::vc_pool::VcPair;

/// ルーム ID の最大長（モーダルの入力制限）。
pub const ROOM_CODE_MAX_LENGTH: u16 = 32;

/// 見出しの時刻は日本時間で出す（コミュニティの時間帯）。
const JST_OFFSET_SECONDS: i32 = 9 * 3600;

fn team_emoji(team: Team) -> &'static str {
    match team {
        Team::Red => "🔴",
        Team::Blue => "🔵",
    }
}

fn vc_line(pair: &VcPair, team: Team) -> String {
    format!(
        "{} は {}（<#{}>）に集合",
        team,
        pair.channel_name(team),
        pair.channel(team).get()
    )
}

/// 試合の短い識別子（見出し用）。
pub fn match_label(formation: FormationId) -> String {
    let simple = formation.0.simple().to_string();
    format!("match-{}", &simple[..8])
}

/// すべての投稿の先頭に付ける見出し: VC ラベル・時刻（JST）・試合 ID。
pub fn heading(vc: &VcPair, at: Timestamp, formation: FormationId) -> String {
    let jst = FixedOffset::east_opt(JST_OFFSET_SECONDS).expect("valid offset");
    format!(
        "**[vc{}] {} JST · {}**\n",
        vc.label,
        at.with_timezone(&jst).format("%Y-%m-%d %H:%M"),
        match_label(formation)
    )
}

/// Discord ユーザーのメンション。
fn mention(user: UserId) -> String {
    format!("<@{user}>")
}

/// 割り当て表 + 集合場所 + 部屋設定のチェックリスト（マップを先頭に差し込む）。
///
/// red と blue の両方の VC チャットに投稿する（全員に届ける必要があるので）。
pub fn assignment(
    heading: &str,
    roster: &[RosterEntry],
    vc: &VcPair,
    room_map: &str,
    checklist: &[String],
) -> OutgoingMessage {
    let mut content = String::from(heading);
    content.push_str("## 割り当て表\n");
    let mut mention_users = Vec::with_capacity(roster.len());
    for team in Team::ALL {
        content.push_str(&format!(
            "{} **{}** — {}\n",
            team_emoji(team),
            team,
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
    content.push_str("\n## 部屋設定\n");
    content.push_str(&format!("- [ ] マップ: {room_map}\n"));
    for item in checklist {
        content.push_str(&format!("- [ ] {item}\n"));
    }
    OutgoingMessage {
        content,
        buttons: Vec::new(),
        mention_users,
    }
}

/// 公開招集チャンネルに出す、成立通知と行き先だけのメッセージ。
///
/// レート、ロール、報告内容は全員が見る場所へ出さない。本文のメンションと
/// `mention_users` は同じ10人に揃える。
pub fn summon(roster: &[RosterEntry], vc: &VcPair) -> OutgoingMessage {
    let mut users: Vec<UserId> = roster.iter().filter_map(user_of).collect();
    users.sort_unstable();
    users.dedup();
    let mentions = users
        .iter()
        .map(|user| mention(*user))
        .collect::<Vec<_>>()
        .join(" ");
    OutgoingMessage {
        content: format!(
            "{mentions}\nマッチが成立しました → <#{}> / <#{}>\n\
             割り当てとルーム設定は各VCのテキストチャットにあります",
            vc.red.get(),
            vc.blue.get(),
        ),
        buttons: Vec::new(),
        mention_users: users,
    }
}

/// ホスト候補への打診（[ 部屋を作る ] [ 今回はパスする ]）。候補の側の VC チャットにだけ投稿する。
///
/// 同じ側の候補に変わったらこのメッセージを編集する。新しいメッセージを積まない。
pub fn host_prompt(
    heading: &str,
    candidate: UserId,
    formation: FormationId,
    player: PlayerId,
    index: usize,
    total: usize,
) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{}{} さん、部屋の作成をお願いできますか？（候補 {}/{}）\n\
             部屋を作ったら「部屋を作る」を押して、ルーム ID を入力してください。",
            heading,
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

/// 候補が反対側のチームに移ったとき、古い打診メッセージを書き換える（ボタンを外す）。
pub fn host_prompt_moved(heading: &str, to: Team) -> OutgoingMessage {
    OutgoingMessage {
        content: format!("{heading}ホストの打診は {to} 側の VC に移りました。"),
        buttons: Vec::new(),
        mention_users: Vec::new(),
    }
}

/// ホストが承諾した後の状態表示（[ ルーム ID を入力 ]）。
pub fn host_accepted(
    heading: &str,
    host: UserId,
    formation: FormationId,
    player: PlayerId,
) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{}{} さんが部屋を作成中です。作成できたら「ルーム ID を入力」からルーム ID を送ってください。",
            heading,
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
    heading: &str,
    host: UserId,
    room_code: &str,
    formation: FormationId,
    player: PlayerId,
) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{}ルーム ID: **{}**（ホスト: {}）",
            heading,
            room_code,
            mention(host)
        ),
        buttons: vec![Button {
            custom_id: CustomId::new(Action::CodeOpen, formation, player).encode(),
            label: "ルーム ID を修正".into(),
            style: ButtonStyle::Secondary,
        }],
        mention_users: Vec::new(),
    }
}

/// ルーム ID の開示。red と blue の両方の VC チャットに投稿し、貼り直しは両方を編集する。
pub fn code_reveal(
    heading: &str,
    host: UserId,
    others: &[UserId],
    room_code: &str,
    vc: &VcPair,
) -> OutgoingMessage {
    let mut content = String::from(heading);
    if !others.is_empty() {
        let mentions: Vec<String> = others.iter().map(|user| mention(*user)).collect();
        content.push_str(&mentions.join(" "));
        content.push('\n');
    }
    content.push_str(&format!(
        "ルーム ID は **{}** です（ホスト: {}）。\n{}、{}してください。",
        room_code,
        mention(host),
        vc_line(vc, Team::Red),
        vc_line(vc, Team::Blue)
    ));
    OutgoingMessage {
        content,
        buttons: Vec::new(),
        mention_users: others.to_vec(),
    }
}

/// 全参加者向けの報告入口。集計・人数・報告者名は一切載せない。
pub fn report_prompt(heading: &str, formation: FormationId) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{heading}試合の内容を選んで報告してください。報告内容は他の参加者には表示されません。"
        ),
        buttons: vec![
            Button {
                custom_id: CustomId::report(Action::ReportRed, formation).encode(),
                label: "赤の勝ち".into(),
                style: ButtonStyle::Primary,
            },
            Button {
                custom_id: CustomId::report(Action::ReportBlue, formation).encode(),
                label: "青の勝ち".into(),
                style: ButtonStyle::Primary,
            },
            Button {
                custom_id: CustomId::report(Action::ReportNullified, formation).encode(),
                label: "無効試合".into(),
                style: ButtonStyle::Secondary,
            },
        ],
        mention_users: Vec::new(),
    }
}

/// 報告を受理した本人だけに返す。判定状態や他人の情報は含めない。
pub fn report_received() -> String {
    "受け付けました".into()
}

/// 編成外のユーザーにだけ返す。
pub fn report_not_participant() -> String {
    "この試合の参加者だけが報告できます。".into()
}

/// コード開示前や報告不能状態の操作にだけ返す。内部状態は明かさない。
pub fn report_unavailable() -> String {
    "この報告は受け付けられませんでした。".into()
}

/// 係争を運営専用チャンネルへ通知する。最新報告だけの内訳を出す。
pub(crate) fn dispute_notification(
    formation: FormationId,
    roster: &[RosterEntry],
    latest: &[ReportVote],
) -> OutgoingMessage {
    let participant = |player: PlayerId| {
        roster
            .iter()
            .find(|entry| entry.player_id == player)
            .and_then(user_of)
            .map(mention)
            .unwrap_or_else(|| player.to_string())
    };
    let voters = |outcome: Outcome| {
        let names: Vec<String> = latest
            .iter()
            .filter(|vote| vote.outcome == outcome)
            .map(|vote| participant(vote.player_id))
            .collect();
        if names.is_empty() {
            "なし".into()
        } else {
            names.join(" ")
        }
    };
    let participants = roster
        .iter()
        .map(|entry| participant(entry.player_id))
        .collect::<Vec<_>>()
        .join(" ");
    OutgoingMessage {
        content: format!(
            "## disputed: {}\nformation: `{formation}`\n参加者: {participants}\n赤の勝ち: {}\n青の勝ち: {}\n引き分け: {}\n無効試合: {}",
            match_label(formation),
            voters(Outcome::Win(Team::Red)),
            voters(Outcome::Win(Team::Blue)),
            voters(Outcome::Draw),
            voters(Outcome::Nullified),
        ),
        buttons: Vec::new(),
        mention_users: Vec::new(),
    }
}

/// 解散の通知。理由は運用上の事実だけを書く（人の推測はしない）。両方の VC チャットに出す。
pub fn abandoned_notice(heading: &str, reason: AbandonReason) -> OutgoingMessage {
    let why = match reason {
        AbandonReason::HostCandidatesExhausted => "ホストが決まらなかったため",
        AbandonReason::NotEnoughPlayers => "部屋に人が揃わなかったため",
        AbandonReason::VcPoolExhausted => "空いている VC が無かったため",
        AbandonReason::DiscordCallFailed => "Discord への投稿ができなかったため",
    };
    OutgoingMessage {
        content: format!("{heading}{why}、この試合は解散になりました。キューに戻ります。"),
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

/// ルーム ID の開示に失敗した（本人にだけ表示）。
pub fn reveal_failed() -> String {
    "ルーム ID を VC のチャットに投稿できませんでした。この試合は解散になります。".into()
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
    use chrono::{TimeZone, Utc};
    use serenity::model::id::ChannelId;
    use uuid::Uuid;

    use super::*;

    /// ロビー運用の文言には勝敗を混ぜない。Phase 6 の専用報告入口はこの対象外。
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
                team: if n <= 5 { Team::Red } else { Team::Blue },
                role: format!("role{}", n % 5),
                role_display_name: format!("ロール{}", n % 5),
                role_sort_order: (n % 5) as i32,
                mu_at_match: 25.0,
            })
            .collect()
    }

    fn vc() -> VcPair {
        VcPair {
            label: "3".into(),
            red: ChannelId::new(31),
            blue: ChannelId::new(32),
        }
    }

    fn formation() -> FormationId {
        FormationId(Uuid::from_u128(0xf0))
    }

    fn head() -> String {
        heading(
            &vc(),
            Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap(),
            formation(),
        )
    }

    /// VC チャットに投稿・編集されるメッセージを全部作る。
    fn all_messages() -> Vec<OutgoingMessage> {
        let formation = formation();
        let player = PlayerId(Uuid::from_u128(1));
        let host = UserId::new(1001);
        let others: Vec<UserId> = (1002..=1010).map(UserId::new).collect();
        let checklist = vec!["設定 A".to_owned(), "設定 B".to_owned()];
        let heading = head();
        let mut messages = vec![
            assignment(&heading, &roster(), &vc(), "kyogre", &checklist),
            host_prompt(&heading, host, formation, player, 1, 3),
            host_prompt_moved(&heading, Team::Blue),
            host_accepted(&heading, host, formation, player),
            code_status(&heading, host, "1234", formation, player),
            code_reveal(&heading, host, &others, "1234", &vc()),
        ];
        messages.extend(
            AbandonReason::ALL
                .into_iter()
                .map(|reason| abandoned_notice(&heading, reason)),
        );
        messages
    }

    /// メッセージに加えて、モーダルと本人向けの通知の文言も全部集める。
    fn all_texts() -> Vec<String> {
        let formation = formation();
        let player = PlayerId(Uuid::from_u128(1));
        let host = UserId::new(1001);
        let mut texts: Vec<String> = all_messages()
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
        texts.push(reveal_failed());
        texts
    }

    #[test]
    fn chat_texts_never_mention_the_outcome() {
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
    fn every_post_starts_with_the_heading() {
        let heading = head();
        // formation は 0xf0 なので simple 表記の先頭 8 桁は 0（実運用の v4 UUID ではランダム）
        assert_eq!(heading, "**[vc3] 2026-09-12 21:00 JST · match-00000000**\n");
        for message in all_messages() {
            assert!(message.content.starts_with(&heading), "{message:?}");
        }
    }

    #[test]
    fn assignment_lists_all_ten_players_and_inserts_the_map_first() {
        let message = assignment(&head(), &roster(), &vc(), "kyogre", &["設定 A".to_owned()]);
        assert_eq!(message.mention_users.len(), 10);
        assert!(message.content.contains("**red**"));
        assert!(message.content.contains("**blue**"));
        assert!(message.content.contains("vc3-red"));
        assert!(message.content.contains("vc3-blue"));
        assert!(message.content.contains("<#31>"));
        assert!(message.content.contains("<#32>"));
        let map = message.content.find("- [ ] マップ: kyogre").unwrap();
        let item = message.content.find("- [ ] 設定 A").unwrap();
        assert!(map < item, "the map comes first");
        let red = message.content.find("**red**").unwrap();
        let blue = message.content.find("**blue**").unwrap();
        assert!(red < blue);
        // red チーム内はロールの表示順（ロール0 が先頭）
        let first_red = message.content[red..blue].find("<@1005>").unwrap();
        let second_red = message.content[red..blue].find("<@1001>").unwrap();
        assert!(first_red < second_red);
    }

    #[test]
    fn summon_mentions_only_the_participants_and_destinations() {
        let message = summon(&roster(), &vc());
        assert_eq!(
            message.content,
            "<@1001> <@1002> <@1003> <@1004> <@1005> <@1006> <@1007> <@1008> <@1009> <@1010>\n\
             マッチが成立しました → <#31> / <#32>\n\
             割り当てとルーム設定は各VCのテキストチャットにあります"
        );
        assert_eq!(
            message.mention_users,
            (1001..=1010).map(UserId::new).collect::<Vec<_>>()
        );
        assert!(message.buttons.is_empty());
        for forbidden in ["1500", "レート", "top_carry", "上キャリー", "報告"] {
            assert!(!message.content.contains(forbidden), "{}", message.content);
        }
    }

    #[test]
    fn host_prompt_has_accept_and_decline_buttons_for_the_candidate() {
        let formation = formation();
        let player = PlayerId(Uuid::from_u128(1));
        let message = host_prompt(&head(), UserId::new(1001), formation, player, 2, 3);
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
        assert!(host_prompt_moved(&head(), Team::Blue).buttons.is_empty());
    }

    #[test]
    fn code_reveal_pings_the_other_nine_only() {
        let host = UserId::new(1001);
        let others: Vec<UserId> = (1002..=1010).map(UserId::new).collect();
        let message = code_reveal(&head(), host, &others, "8888", &vc());
        assert_eq!(message.mention_users, others);
        assert!(message.content.contains("**8888**"));
        assert!(message.content.contains("vc3-red"));
        assert!(message.content.contains("vc3-blue"));
    }

    #[test]
    fn modal_prefills_the_current_code_when_correcting() {
        let formation = formation();
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
}
