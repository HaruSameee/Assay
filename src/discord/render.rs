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
use crate::matchmaker::{MatchmakerSummary, RunOutcome};
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
            let name = entry
                .trainer_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("（名前未登録）");
            content.push_str(&format!(
                "- {} — {}\n",
                plain_discord_text(name),
                entry.role_display_name
            ));
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
        mention_users: Vec::new(),
    }
}

/// 表示名を1行の平文にする。Discord の mention 構文を本文へ持ち込まない。
fn plain_discord_text(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\r' | '\n' => ' ',
            '<' => '＜',
            '>' => '＞',
            character => character,
        })
        .collect()
}

/// 成立があった1回分のマッチメイカ運用ログ。
pub fn matchmaker_run_log(outcome: &RunOutcome, at: Timestamp) -> OutgoingMessage {
    let jst = FixedOffset::east_opt(JST_OFFSET_SECONDS).expect("valid offset");
    let mut content = format!(
        "**マッチメイカ実行 {} JST**\nキュー中: {}人\n成立: {}組\n",
        at.with_timezone(&jst).format("%Y-%m-%d %H:%M"),
        outcome.candidates,
        outcome.assignments.len()
    );
    for assigned in &outcome.assignments {
        let selection = &assigned.selection;
        content.push_str(&format!(
            "- {} → vc{} (<#{}> / <#{}>)\n  レート帯: {}〜{}（幅 {}） / チーム平均差: {}\n  待ち時間: 合計 {} / 最長 {} / 最短 {}\n  希望順位: 第1 {}人 / 第2 {}人 / 第3 {}人\n  フレックス補償: {}人\n",
            match_label(assigned.formation_id),
            assigned.vc.label,
            assigned.vc.red_channel_id,
            assigned.vc.blue_channel_id,
            rating(selection.minimum_rating),
            rating(selection.maximum_rating),
            rating(selection.maximum_rating - selection.minimum_rating),
            rating(selection.team_average_difference),
            duration_minutes(selection.total_wait_seconds),
            duration_minutes(selection.longest_wait_seconds),
            duration_minutes(selection.shortest_wait_seconds),
            selection.first_choice_count,
            selection.second_choice_count,
            selection.third_choice_count,
            selection.flex_compensation_count,
        ));
    }
    content.push_str(&format!(
        "VC枯渇: {}\n未成立: {}人\n最長待ち: {}",
        if outcome.vc_exhausted {
            "あり"
        } else {
            "なし"
        },
        outcome.unmatched,
        wait_minutes(outcome.longest_unmatched_wait_seconds)
    ));
    OutgoingMessage {
        content,
        buttons: Vec::new(),
        mention_users: Vec::new(),
    }
}

/// 15分ごとのマッチメイカ運用サマリ。
pub fn matchmaker_summary(summary: &MatchmakerSummary, at: Timestamp) -> OutgoingMessage {
    let jst = FixedOffset::east_opt(JST_OFFSET_SECONDS).expect("valid offset");
    OutgoingMessage {
        content: format!(
            "**マッチメイカ 15分サマリ {} JST**\n\
             キュー中: {}人\n\
             第1希望: 上キャリー {} / サポート {} / 中央 {} / 下キャリー {} / タンク {}\n\
             待ち時間: 中央値 {} / 最長 {}\n\
             VC: 使用中 {} / {}組\n\
             進行中: {}試合（ホスト待ち {} / コード待ち {} / 報告待ち {} / 係争対応待ち {}）\n\
             直近15分の成立: {}組\n\
             直近15分の終了: 確定 {} / 無効試合 {} / 係争 {} / 放棄 {}\n\
             未報告で停止: {}人\n\
             本日累計: 成立 {}組 / 確定 {}組",
            at.with_timezone(&jst).format("%Y-%m-%d %H:%M"),
            summary.queued,
            summary.first_choice_roles.top_carry,
            summary.first_choice_roles.top_support,
            summary.first_choice_roles.jungle,
            summary.first_choice_roles.bot_carry,
            summary.first_choice_roles.bot_tank,
            wait_minutes(summary.median_wait_seconds),
            wait_minutes(summary.longest_wait_seconds),
            summary.vc_in_use,
            summary.vc_total,
            summary.active.total(),
            summary.active.awaiting_host,
            summary.active.awaiting_code,
            summary.active.awaiting_reports,
            summary.active.awaiting_resolution,
            summary.formed_last_15_minutes,
            summary.recent_completions.confirmed,
            summary.recent_completions.nullified,
            summary.recent_completions.disputed,
            summary.recent_completions.abandoned,
            summary.pending_reporters,
            summary.formed_today,
            summary.confirmed_today,
        ),
        buttons: Vec::new(),
        mention_users: Vec::new(),
    }
}

fn wait_minutes(seconds: Option<u64>) -> String {
    seconds.map_or_else(|| "なし".to_owned(), |value| format!("{}分", value / 60))
}

fn duration_minutes(seconds: u64) -> String {
    format!("{}分", seconds / 60)
}

fn rating(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}", value)
    } else {
        format!("{value:.1}")
    }
}

/// 公開招集チャンネルに出す、成立通知と行き先だけのメッセージ。
///
/// **チームごとに行を分ける。**10 人が同じメッセージを見るので、赤と青を 1 行に並べると
/// 自分がどちらへ行くのか分からない。チーム分けは formation 確定時に決まっているので、
/// ここで新しく判定するものは無い。
///
/// レート、ロール割り当て、報告に関わる情報は全員が見る場所へ出さない（00 §3-5）。
/// 出してよいのは**参加者名・チーム・行き先 VC だけ**。本文のメンションと `mention_users` は
/// 同じ10人に揃える。
pub fn summon(roster: &[RosterEntry], vc: &VcPair) -> OutgoingMessage {
    let mut lines = String::new();
    let mut everyone: Vec<UserId> = Vec::new();
    // red が先。並びは Team::ALL に従い、チーム内はユーザー ID 順で安定させる
    for team in Team::ALL {
        let mut users: Vec<UserId> = roster
            .iter()
            .filter(|entry| entry.team == team)
            .filter_map(user_of)
            .collect();
        users.sort_unstable();
        users.dedup();
        if users.is_empty() {
            continue;
        }
        let mentions = users
            .iter()
            .map(|user| mention(*user))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push_str(&format!("{mentions} → <#{}>\n", vc.channel(team).get()));
        everyone.extend(users);
    }
    everyone.sort_unstable();
    everyone.dedup();
    OutgoingMessage {
        content: format!(
            "{lines}マッチが成立しました。割り当てとルーム設定は各VCのテキストチャットにあります"
        ),
        buttons: Vec::new(),
        mention_users: everyone,
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
                trainer_name: Some(format!("トレーナー{n}")),
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
        assert!(message.mention_users.is_empty());
        assert!(!message.content.contains("<@"), "{}", message.content);
        for n in 1..=10 {
            assert!(message.content.contains(&format!("トレーナー{n}")));
        }
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
        let first_red = message.content[red..blue].find("トレーナー5").unwrap();
        let second_red = message.content[red..blue].find("トレーナー1").unwrap();
        assert!(first_red < second_red);
    }

    #[test]
    fn assignment_neutralizes_mention_syntax_in_trainer_names() {
        let mut roster = roster();
        roster[0].trainer_name = Some("<@123>\n@everyone".into());
        let message = assignment(&head(), &roster, &vc(), "kyogre", &[]);
        assert!(!message.content.contains("<@"), "{}", message.content);
        assert!(message.content.contains("＜@123＞ @everyone"));
        assert!(message.mention_users.is_empty());
    }

    /// **チーム別に 1 行ずつ。**自分がどちらの VC へ行くのかが一目で分かること。
    #[test]
    fn summon_splits_the_two_teams_onto_their_own_lines() {
        let message = summon(&roster(), &vc());
        assert_eq!(
            message.content,
            "<@1001> <@1002> <@1003> <@1004> <@1005> → <#31>\n\
             <@1006> <@1007> <@1008> <@1009> <@1010> → <#32>\n\
             マッチが成立しました。割り当てとルーム設定は各VCのテキストチャットにあります"
        );
        // red が先、blue が後
        let red = message.content.find("<#31>").unwrap();
        let blue = message.content.find("<#32>").unwrap();
        assert!(red < blue, "{}", message.content);
        // 5 人ずつに分かれ、どちらの行にも相手チームの誰かが混ざらない
        let lines: Vec<&str> = message.content.lines().collect();
        assert_eq!(lines.len(), 3, "{}", message.content);
        for (line, members) in [(lines[0], 1001..=1005), (lines[1], 1006..=1010)] {
            assert_eq!(line.matches("<@").count(), 5, "{line}");
            for user in members {
                assert!(line.contains(&format!("<@{user}>")), "{line}");
            }
        }
        assert_eq!(
            message.mention_users,
            (1001..=1010).map(UserId::new).collect::<Vec<_>>()
        );
        assert!(message.buttons.is_empty());
    }

    #[test]
    fn matchmaker_logs_use_aggregate_selection_metrics_without_player_details() {
        use crate::matchmaker::{
            ActiveFormationCounts, AssignedFormation, FirstChoiceRoleCounts, MatchmakerSummary,
            RecentCompletionCounts, RunOutcome, SelectionMetrics,
        };
        use crate::persistence::VcAssignment;

        let outcome = RunOutcome {
            candidates: 23,
            created: vec![formation()],
            assignments: vec![AssignedFormation {
                formation_id: formation(),
                vc: VcAssignment {
                    label: "3".into(),
                    red_channel_id: 31,
                    blue_channel_id: 32,
                },
                selection: SelectionMetrics {
                    minimum_rating: 1450.0,
                    maximum_rating: 1520.0,
                    team_average_difference: 3.2,
                    total_wait_seconds: 4_200,
                    longest_wait_seconds: 900,
                    shortest_wait_seconds: 180,
                    first_choice_count: 7,
                    second_choice_count: 2,
                    third_choice_count: 1,
                    flex_compensation_count: 1,
                },
            }],
            vc_exhausted: false,
            unmatched: 13,
            longest_unmatched_wait_seconds: Some(12 * 60),
        };
        let at = Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap();
        let run = matchmaker_run_log(&outcome, at);
        let summary = matchmaker_summary(
            &MatchmakerSummary {
                queued: 13,
                first_choice_roles: FirstChoiceRoleCounts {
                    top_carry: 3,
                    top_support: 2,
                    jungle: 4,
                    bot_carry: 2,
                    bot_tank: 2,
                },
                formed_last_15_minutes: 1,
                median_wait_seconds: Some(6 * 60),
                longest_wait_seconds: Some(18 * 60),
                vc_in_use: 2,
                vc_total: 30,
                active: ActiveFormationCounts {
                    awaiting_host: 1,
                    awaiting_code: 1,
                    awaiting_reports: 1,
                    awaiting_resolution: 1,
                },
                recent_completions: RecentCompletionCounts {
                    confirmed: 1,
                    nullified: 2,
                    disputed: 1,
                    abandoned: 0,
                },
                pending_reporters: 5,
                formed_today: 8,
                confirmed_today: 4,
            },
            at,
        );
        assert!(run.content.contains("vc3 (<#31> / <#32>)"));
        assert!(run.content.contains("レート帯"));
        assert!(
            run.content
                .contains("希望順位: 第1 7人 / 第2 2人 / 第3 1人")
        );
        assert!(summary.content.contains("中央値 6分 / 最長 18分"));
        assert!(summary.content.contains("VC: 使用中 2 / 30組"));
        assert!(summary.content.contains("未報告で停止: 5人"));
        assert!(summary.content.contains("本日累計: 成立 8組 / 確定 4組"));
        for forbidden in [
            "トレーナー1",
            "top_carry",
            "top_support",
            "jungle",
            "bot_carry",
            "bot_tank",
            "<@",
        ] {
            assert!(!run.content.contains(forbidden), "{}", run.content);
        }
        for forbidden in ["レート", "match-", "トレーナー1", "<@"] {
            assert!(!summary.content.contains(forbidden), "{}", summary.content);
        }
        for message in [run, summary] {
            assert!(message.mention_users.is_empty());
        }
    }

    /// レビュー用に、今回変更した4種類の文面をそのまま出す。
    #[ignore = "レビュー用の出力。--ignored で明示的に走らせる"]
    #[test]
    fn dump_discord_tuning_messages_for_review() {
        use crate::matchmaker::{
            ActiveFormationCounts, AssignedFormation, FirstChoiceRoleCounts, MatchmakerSummary,
            RecentCompletionCounts, RunOutcome, SelectionMetrics,
        };
        use crate::persistence::VcAssignment;

        let at = Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap();
        let outcome = RunOutcome {
            candidates: 23,
            created: vec![formation()],
            assignments: vec![AssignedFormation {
                formation_id: formation(),
                vc: VcAssignment {
                    label: "3".into(),
                    red_channel_id: 31,
                    blue_channel_id: 32,
                },
                selection: SelectionMetrics {
                    minimum_rating: 1450.0,
                    maximum_rating: 1520.0,
                    team_average_difference: 3.2,
                    total_wait_seconds: 4_200,
                    longest_wait_seconds: 900,
                    shortest_wait_seconds: 180,
                    first_choice_count: 7,
                    second_choice_count: 2,
                    third_choice_count: 1,
                    flex_compensation_count: 1,
                },
            }],
            vc_exhausted: false,
            unmatched: 13,
            longest_unmatched_wait_seconds: Some(12 * 60),
        };
        let summary = MatchmakerSummary {
            queued: 13,
            first_choice_roles: FirstChoiceRoleCounts {
                top_carry: 3,
                top_support: 2,
                jungle: 4,
                bot_carry: 2,
                bot_tank: 2,
            },
            formed_last_15_minutes: 1,
            median_wait_seconds: Some(6 * 60),
            longest_wait_seconds: Some(18 * 60),
            vc_in_use: 2,
            vc_total: 30,
            active: ActiveFormationCounts {
                awaiting_host: 1,
                awaiting_code: 1,
                awaiting_reports: 1,
                awaiting_resolution: 1,
            },
            recent_completions: RecentCompletionCounts {
                confirmed: 1,
                nullified: 2,
                disputed: 1,
                abandoned: 0,
            },
            pending_reporters: 5,
            formed_today: 8,
            confirmed_today: 4,
        };
        println!(
            "========== 割り当て表 ==========\n{}",
            assignment(&head(), &roster(), &vc(), "kyogre", &["設定 A".into()]).content
        );
        println!(
            "========== 招集通知 ==========\n{}",
            summon(&roster(), &vc()).content
        );
        println!(
            "========== 成立時ログ ==========\n{}",
            matchmaker_run_log(&outcome, at).content
        );
        println!(
            "========== 15分サマリ ==========\n{}",
            matchmaker_summary(&summary, at).content
        );
    }

    /// レビュー用に招集の文面をそのまま出す。
    ///
    /// `cargo test --lib discord::render -- --ignored --nocapture` で取り出す。
    #[ignore = "レビュー用の出力。--ignored で明示的に走らせる"]
    #[test]
    fn dump_summon_for_review() {
        println!("========== 招集チャンネルの文面 ==========");
        println!("{}", summon(&roster(), &vc()).content);
    }

    /// 招集チャンネルは全員が見る。**レート・ロール割り当て・報告の情報を出さない**（00 §3-5）。
    #[test]
    fn summon_never_leaks_ratings_roles_or_reports() {
        let message = summon(&roster(), &vc());
        for forbidden in [
            "1500",
            "レート",
            "25",
            "top_carry",
            "上キャリー",
            "ロール",
            "報告",
            "勝",
        ] {
            assert!(
                !message.content.contains(forbidden),
                "{forbidden} leaked: {}",
                message.content
            );
        }
        // roster が持っているロール表示名も出ていない
        for entry in roster() {
            assert!(
                !message.content.contains(&entry.role_display_name),
                "{}",
                message.content
            );
            assert!(
                !message.content.contains(&entry.role),
                "{}",
                message.content
            );
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
