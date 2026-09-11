//! VC のテキストチャットと招集チャンネルに出す文言。純粋関数。
//!
//! **すべて通知であり、操作は無い。**ボタンもモーダルも作らない（Discord に
//! インタラクションを残さない）。行き先としてマッチ画面の URL を添える。
//!
//! 出してよいのは、集合場所（VC）・ルーム ID・ホスト打診・解散の通知だけ。
//! 勝敗に関する語は一切出さない（`tests` で検査する）。
//!
//! **参加者の構成は載せない。**名前もロールも VC には出さない（割り当てはマッチ画面）。
//! サイト側だけ隠しても、VC の投稿に相手のロールが載っていれば意味が無い。
//!
//! VC は使い回すので、1 本のチャットに何試合分ものメッセージが溜まる（消さない）。
//! どの試合のものか分かるよう、すべての投稿の先頭に見出し（VC ラベル・時刻・試合 ID）を付ける。

use chrono::FixedOffset;
use serenity::model::id::UserId;

use crate::host::HostResources;
use crate::lifecycle::{AbandonReason, FormationId, PlayerId, Timestamp};
use crate::matchmaker::{MatchmakerSummary, RunOutcome};
use crate::persistence::{Outcome, ReportVote, RosterEntry, Team};

use super::gateway::OutgoingMessage;
use super::vc_pool::VcPair;

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

/// マッチ画面の URL。**通知には必ずこれを添える**（操作はサイトにしか無いので、
/// 行き先が書いていない通知は「あなたの番です」を伝えきれない）。
pub fn match_url(site_base_url: &str, formation: FormationId) -> String {
    format!(
        "{}/match/{}",
        site_base_url.trim_end_matches('/'),
        formation.0
    )
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

/// 集合の案内。**参加者名もロールも部屋設定も載せない。**
///
/// 以前はここに割り当て表（名前 — ロール）と部屋設定のチェックリストを出していた。
/// チームごとに本文を分けてはいたが、**その分離は VC テキストチャットの閲覧権限が
/// 手で絞られていることに依存する**。bot は権限を触らない設計（04）なので、権限が
/// 開いていれば相手の VC を開くだけで構成が読める。分離が効いているかどうかを
/// コード側から保証できない以上、VC には最初から載せない。
///
/// 割り当てと部屋設定は**マッチ画面**にある（相手のロールは型に存在しないので出ない）。
/// VC に残すのは「どこへ集合するか」と、ホストによるルーム ID の開示だけ。
pub fn assignment(heading: &str, vc: &VcPair, team: Team, match_url: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{heading}## 集合\n{emoji} **{team}** — {line}してください。\n\
             割り当てと部屋設定はマッチ画面にあります: {match_url}\n",
            emoji = team_emoji(team),
            line = vc_line(vc, team),
        ),
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
///
/// **走査の方向（上から / 下から）は出さない。**周期が読めると「下からの回」を
/// 狙ってキューに入れてしまう。サーバのログ（journal）にだけ残る。
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

/// 停止中に 15 分サマリの先頭へ差し込む見出し。
///
/// **止めたまま忘れることを防ぐのが目的**なので、キューが空でもサマリごと投稿する
/// （`MatchmakerSummary::should_publish`）。
///
/// 止めた人はメンションではなく `discord:<id>` の平文で書く。運用ログのメッセージに
/// メンション構文を混ぜない方針（管理パネルの表示とも揃う）で、15 分ごとに通知が飛ぶ
/// こともない。
fn paused_banner(summary: &MatchmakerSummary, jst: FixedOffset) -> String {
    summary.paused.as_ref().map_or_else(String::new, |pause| {
        format!(
            "**⏸ マッチメイカは停止中です。新しい試合は成立しません**\n\
             停止: {} JST / 操作: discord:{}\n\
             理由: {}\n\
             再開するまで新しい試合は組まれません。`/admin` から再開してください。\n\n",
            pause.since.with_timezone(&jst).format("%Y-%m-%d %H:%M"),
            pause.actor_discord_user_id,
            plain_discord_text(pause.reason.as_deref().unwrap_or("記載なし")),
        )
    })
}

/// サマリ末尾に足すホストのリソース行（0〜2 行）。
///
/// **読めなかった項目は黙って省く。**`/proc` が読めなくてもサマリ本体は落とさない
/// （00 §3-6 の方針）。1 項目も読めなければ 1 行も足さない。
///
/// ロードアベレージはスナップショットなので、プロセス内に前回値を持たない。
/// 15 分平均がそのまま 15 分サマリに対応する（[`crate::host`] の冒頭）。
fn host_lines(host: &HostResources) -> String {
    if host.is_empty() {
        return String::new();
    }
    let mut lines = String::new();
    if let Some(load) = host.load {
        let per_core =
            load.per_core_percent(host.cores)
                .map_or_else(String::new, |(one, five, fifteen)| {
                    format!(
                        "、{}コア換算 {one}% / {five}% / {fifteen}%",
                        host.cores.unwrap_or_default()
                    )
                });
        lines.push_str(&format!(
            "\nホスト負荷: {:.2} / {:.2} / {:.2}（1分 / 5分 / 15分{per_core}）",
            load.one, load.five, load.fifteen
        ));
    }
    let mut memory = Vec::new();
    if let Some(usage) = host.memory {
        memory.push(format!(
            "使用 {}%（{} / {}）",
            usage.used_percent(),
            gibibytes(usage.used_kib()),
            gibibytes(usage.total_kib)
        ));
    }
    if let Some(rss_kib) = host.process_rss_kib {
        memory.push(format!("このプロセス {}", mebibytes(rss_kib)));
    }
    if !memory.is_empty() {
        lines.push_str(&format!("\nホストメモリ: {}", memory.join(" / ")));
    }
    lines
}

fn gibibytes(kib: u64) -> String {
    format!("{:.1}GiB", kib as f64 / 1024.0 / 1024.0)
}

fn mebibytes(kib: u64) -> String {
    format!("{}MiB", (kib as f64 / 1024.0).round() as u64)
}

/// 15分ごとのマッチメイカ運用サマリ。
pub fn matchmaker_summary(summary: &MatchmakerSummary, at: Timestamp) -> OutgoingMessage {
    let jst = FixedOffset::east_opt(JST_OFFSET_SECONDS).expect("valid offset");
    OutgoingMessage {
        content: format!(
            "{paused}\
             **マッチメイカ 15分サマリ {} JST**\n\
             キュー中: {}人\n\
             第1希望: 上キャリー {} / サポート {} / 中央 {} / 下キャリー {} / タンク {}\n\
             待ち時間: 中央値 {} / 最長 {}\n\
             VC: 使用中 {} / {}組\n\
             進行中: {}試合（ホスト待ち {} / コード待ち {} / 報告待ち {} / 係争対応待ち {}）\n\
             直近15分の成立: {}組\n\
             直近15分の終了: 確定 {} / 無効試合 {} / 係争 {} / 放棄 {}\n\
             未報告で停止: {}人\n\
             本日累計: 成立 {}組 / 確定 {}組\
             {host}",
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
            paused = paused_banner(summary, jst),
            host = host_lines(&summary.host),
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

/// 公開招集チャンネルに出す成立通知。**10 人ぶんを 1 通にまとめる。**
///
/// 以前はチームごとに 2 通に分けていた。「同じメッセージに両チームを入れると相手の
/// 構成が読めてしまう」という理由だったが、**2 通とも同じ招集チャンネルに並ぶ**ので
/// 隠せていなかった。実際に相手から隠れているのは各チームの VC に出す割り当て表の方で、
/// そちらは分けたままである。
///
/// 分けない方が読みやすい。1 通にまとめれば URL が 1 つで済み、
/// 自分がメンションされたかどうかも 1 か所を見れば分かる。
///
/// レート、ロール割り当て、報告に関わる情報は全員が見る場所へ出さない（00 §3-5）。
/// 出してよいのは**参加者名・チーム・行き先 VC と、サイトへの導線だけ**。
/// 本文のメンションと `mention_users` は同じ 10 人に揃える。
pub fn summon(roster: &[RosterEntry], vc: &VcPair, match_url: &str) -> OutgoingMessage {
    // チーム内はユーザー ID 順で安定させる（同じ編成なら毎回同じ本文になる）
    let team_users = |team: Team| -> Vec<UserId> {
        let mut users: Vec<UserId> = roster
            .iter()
            .filter(|entry| entry.team == team)
            .filter_map(user_of)
            .collect();
        users.sort_unstable();
        users.dedup();
        users
    };
    let line = |team: Team, label: &str| -> String {
        let mentions = team_users(team)
            .iter()
            .map(|user| mention(*user))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{label}: {mentions} → <#{channel}>",
            channel = vc.channel(team).get(),
        )
    };
    // 赤が先。マッチ画面もチーム表記も赤 → 青の順で揃えてある
    let mut users = team_users(Team::Red);
    users.extend(team_users(Team::Blue));
    OutgoingMessage {
        content: format!(
            "マッチが成立しました。**操作はすべてマッチ画面から**行います: {match_url}\n\
             {red}\n{blue}\n\
             割り当てと部屋設定もマッチ画面にあります",
            red = line(Team::Red, "赤"),
            blue = line(Team::Blue, "青"),
        ),
        buttons: Vec::new(),
        mention_users: users,
    }
}

/// 招集チャンネルに出すホストの通知。**ホストは相手チームの人にも伝える**
/// （誰が部屋を建てたか分からないと、入れなかったときに聞く相手が分からない）。
///
/// ホストのロールは出さない。メンションはホスト1人だけなので、招集の通知と重複しない。
///
/// **URL は載せない。**同じチャンネルの直前に出る成立通知（[`summon`]）に
/// マッチ画面へのリンクがあり、同じ URL を 2 行あとにもう一度出すと
/// 「別の場所へ行くのか」と読ませてしまう。
pub fn host_announcement(
    formation: FormationId,
    host: UserId,
    index: usize,
    total: usize,
) -> OutgoingMessage {
    let changed = if index > 1 {
        format!("（候補 {index}/{total} に交代しました）")
    } else {
        String::new()
    };
    OutgoingMessage {
        content: format!(
            "**{label}** ホストは {host} さんです{changed}。\n\
             ホストの方はマッチ画面で部屋を作って、ルーム ID を入力してください",
            label = match_label(formation),
            host = mention(host),
        ),
        buttons: Vec::new(),
        mention_users: vec![host],
    }
}

/// ホスト候補への打診の**通知**。候補の側の VC チャットにだけ投稿する。
///
/// **ボタンは無い。**操作はマッチ画面で行う（Discord にインタラクションを残さない）。
/// ここに残すのは「あなたの番です」を伝える役目だけで、これが無いと画面を開いている人
/// しか進行できなくなる。同じ側の候補に変わったらこのメッセージを編集する。
pub fn host_prompt(
    heading: &str,
    candidate: UserId,
    index: usize,
    total: usize,
    match_url: &str,
) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{}{} さん、部屋の作成をお願いできますか？（候補 {}/{}）\n\
             マッチ画面で「部屋を作る」か「今回はパスする」を選んでください: {}",
            heading,
            mention(candidate),
            index,
            total,
            match_url
        ),
        buttons: Vec::new(),
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

/// ホストが承諾した後の状態表示。**ボタンは無い**（入力はマッチ画面）。
pub fn host_accepted(heading: &str, host: UserId, match_url: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{}{} さんが部屋を作成中です。作成できたら、マッチ画面でルーム ID を入力してください: {}",
            heading,
            mention(host),
            match_url
        ),
        buttons: Vec::new(),
        mention_users: Vec::new(),
    }
}

/// ルーム ID の開示。**そのVCのチーム側に投稿する**（貼り直しは編集する）。
///
/// **参加者10人のメンションは付けない。**招集チャンネルで既に通知しており、重複する。
/// ホストのメンションだけ残す — 1人なので通知の重複にならず、部屋に入れなかったときに
/// 誰へ聞けばよいかが分かる。表示名は [`plain_discord_text`] を通し、名前に紛れ込んだ
/// mention 構文を本文へ持ち込まない。
/// ルーム ID は公開 VC に出してよい。参加者以外が入室したときは、ゲーム内のルームオーナーが
/// 退出させられる。bot は VC の権限を変更しない。
///
/// **通知が飛ぶのはホスト自身のチーム側の1通だけ。**開示は red / blue 両方へ出すので、
/// 2通とも `mention_users` にホストを載せると、ホスト1人が同じ知らせで2回叩かれる
/// （04 §「ホストのメンションだけ残す — 1人なので通知の重複にならず」が崩れる）。
/// 相手チーム側は `<@id>` の表示だけ残す。`allowed_mentions` に載らなければ名前は
/// 見えても通知は飛ばず、「誰に聞けばよいか」は相手チームにも伝わる。
pub fn code_reveal(
    heading: &str,
    host: UserId,
    host_team: Team,
    room_code: &str,
    vc: &VcPair,
    team: Team,
) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{}ルーム ID は **{}** です（ホスト: {}）。\n{}してください。\n\
             ホストの方へ: 参加者以外が入室していた場合は、ゲーム内で退出させてください。",
            heading,
            plain_discord_text(room_code),
            mention(host),
            vc_line(vc, team),
        ),
        buttons: Vec::new(),
        // ホスト自身のチーム側だけ通知する。相手側は表示のみ
        mention_users: if team == host_team {
            vec![host]
        } else {
            Vec::new()
        },
    }
}

/// 報告の**催促**。集計・人数・報告者名は一切載せない。
///
/// **ボタンは無く、メンションも付けない。**報告はマッチ画面で受ける。ここに残すのは
/// 「終わったら報告してください」を伝える役目だけ（報告には 15 分の期限がある。REPORT_WINDOW_MINUTES）。
pub fn report_prompt(heading: &str, match_url: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: format!(
            "{heading}試合が終わったら、マッチ画面で結果を報告してください: {match_url}\n\
             報告内容は他の参加者には表示されません。"
        ),
        buttons: Vec::new(),
        mention_users: Vec::new(),
    }
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

/// 過去に投稿したボタンを押した人にだけ返す（本人にしか見えない）。
///
/// 新しいボタンはもう作らないが、**チャット履歴に残った古いメッセージは押せる**。
/// 黙って無視すると「押したのに何も起きない」になるので、行き先を返す。
/// このメッセージは状態を一切変えない。
pub fn moved_to_the_site(site_base_url: &str) -> String {
    format!(
        "この操作はサイトへ移りました。マッチ画面から操作してください: {}",
        site_base_url.trim_end_matches('/')
    )
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
    /// VC チャットに出すメッセージ（見出し付き）。
    ///
    /// **報告の催促は含めない。**あれは「報告してください」と書くのが仕事なので、
    /// 禁止語（`報告`）の検査からは外す。代わりに
    /// `the_report_prompt_only_points_at_the_site` が勝敗を名指ししないことを見る。
    fn all_messages() -> Vec<OutgoingMessage> {
        let host = UserId::new(1001);
        let heading = head();
        let url = SITE;
        let mut messages = vec![
            host_prompt(&heading, host, 1, 3, url),
            host_prompt_moved(&heading, Team::Blue),
            host_accepted(&heading, host, url),
        ];
        for team in Team::ALL {
            messages.push(assignment(&heading, &vc(), team, SITE));
            messages.push(code_reveal(&heading, host, Team::Red, "1234", &vc(), team));
        }
        messages.extend(
            AbandonReason::ALL
                .into_iter()
                .map(|reason| abandoned_notice(&heading, reason)),
        );
        messages
    }

    /// メッセージに加えて、本人向けの通知の文言も全部集める。
    fn all_texts() -> Vec<String> {
        let mut texts: Vec<String> = all_messages()
            .iter()
            .flat_map(|message| {
                std::iter::once(message.content.clone())
                    .chain(message.buttons.iter().map(|button| button.label.clone()))
            })
            .collect();
        texts.push(moved_to_the_site(SITE));
        texts
    }

    /// テスト用のサイト起点。
    const SITE: &str = "https://match.example.test";

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

    /// **VC の投稿に参加者名もロールも載せない。**
    ///
    /// チームごとに本文を分ける方式は、VC テキストチャットの閲覧権限が手で絞られて
    /// いることに依存していた。bot は権限を触らない（04）ので、そこが開いていれば
    /// 相手の VC を開くだけで構成が読める。コード側で保証できないものは載せない。
    #[test]
    fn the_voice_chat_post_carries_no_names_or_roles() {
        for team in Team::ALL {
            let message = assignment(&head(), &vc(), team, SITE);
            assert!(message.mention_users.is_empty());
            assert!(!message.content.contains("<@"), "{}", message.content);
            // 参加者名が 1 つも出ない
            for n in 1..=10 {
                assert!(
                    !message.content.contains(&format!("トレーナー{n}")),
                    "{}",
                    message.content
                );
            }
            // ロールの表示名も出ない
            for n in 0..5 {
                assert!(
                    !message.content.contains(&format!("ロール{n}")),
                    "{}",
                    message.content
                );
            }
            // 部屋設定のチェックリストも出さない（マッチ画面にある）
            assert!(!message.content.contains("- [ ]"), "{}", message.content);
            assert!(
                !message.content.contains("割り当て表"),
                "{}",
                message.content
            );
            // 出すのは集合先と、マッチ画面への導線だけ
            assert!(message.content.contains(&vc().channel_name(team)));
            assert!(message.content.contains(SITE), "{}", message.content);
        }
    }

    /// 片方の VC の投稿に相手チームの行き先が混ざらないこと。
    #[test]
    fn the_voice_chat_post_only_names_its_own_channel() {
        let red = assignment(&head(), &vc(), Team::Red, SITE);
        let blue = assignment(&head(), &vc(), Team::Blue, SITE);
        assert!(red.content.contains("**red**"));
        assert!(!red.content.contains("**blue**"), "{}", red.content);
        assert!(red.content.contains("vc3-red"));
        assert!(!red.content.contains("vc3-blue"), "{}", red.content);
        assert!(blue.content.contains("vc3-blue"));
        assert!(!blue.content.contains("vc3-red"), "{}", blue.content);
    }

    /// **招集は 1 通。**10 人ぶんのメンションと URL 1 つで、行き先だけチームごとに書く。
    ///
    /// 以前はチームごとに 2 通に分けていた。相手の構成を隠す意図だったが、
    /// 2 通とも同じ招集チャンネルに並ぶので隠せていなかった。
    #[test]
    fn the_summon_is_one_message_for_all_ten() {
        let message = summon(&roster(), &vc(), SITE);
        assert_eq!(
            message.content,
            format!(
                "マッチが成立しました。**操作はすべてマッチ画面から**行います: {SITE}\n\
                 赤: <@1001> <@1002> <@1003> <@1004> <@1005> → <#31>\n\
                 青: <@1006> <@1007> <@1008> <@1009> <@1010> → <#32>\n\
                 割り当てと部屋設定もマッチ画面にあります"
            )
        );
        // **URL は 1 つだけ。**2 通に分かれていた頃は同じ URL が 2 回出ていた
        assert_eq!(
            message.content.matches(SITE).count(),
            1,
            "{}",
            message.content
        );
        // 10 人全員がメンションされる（本文と mention_users が一致すること）
        assert_eq!(
            message.mention_users,
            (1001..=1010).map(UserId::new).collect::<Vec<_>>()
        );
        for user in 1001..=1010 {
            assert!(
                message.content.contains(&format!("<@{user}>")),
                "{}",
                message.content
            );
        }
        // 行き先は両方書く（どちらの VC に行くかが 1 通で分かる）
        assert!(message.content.contains("<#31>"), "{}", message.content);
        assert!(message.content.contains("<#32>"), "{}", message.content);
        assert!(message.buttons.is_empty());
    }

    /// ホストは**相手チームの人にも**伝える。メンションはホスト1人だけ。
    #[test]
    fn the_host_announcement_names_the_host_once() {
        let first = host_announcement(formation(), UserId::new(1001), 1, 3);
        assert!(first.content.contains("<@1001>"), "{}", first.content);
        assert_eq!(first.content.matches("<@").count(), 1, "{}", first.content);
        assert_eq!(first.mention_users, vec![UserId::new(1001)]);
        // **URL は載せない。**直前の成立通知に同じリンクがある
        assert!(!first.content.contains(SITE), "{}", first.content);
        assert!(
            first.content.contains("ルーム ID を入力してください"),
            "{}",
            first.content
        );
        // 1人目は「交代」と書かない
        assert!(!first.content.contains("交代"), "{}", first.content);
        assert!(first.buttons.is_empty());

        // 候補が移ったら、その旨も招集チャンネルに出る
        let second = host_announcement(formation(), UserId::new(1002), 2, 3);
        assert!(second.content.contains("交代"), "{}", second.content);
        assert!(second.content.contains("2/3"), "{}", second.content);
        // ロールは出さない
        for role in ["ロール0", "ロール1", "上キャリー", "中央"] {
            assert!(!second.content.contains(role), "{}", second.content);
        }
    }

    #[test]
    fn matchmaker_logs_use_aggregate_selection_metrics_without_player_details() {
        use crate::matchmaker::{
            ActiveFormationCounts, AssignedFormation, FirstChoiceRoleCounts, MatchmakerSummary,
            RecentCompletionCounts, RunOutcome, ScanDirection, SelectionMetrics,
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
            paused: false,
            direction: ScanDirection::BottomUp,
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
                paused: None,
                host: HostResources::default(),
            },
            at,
        );
        assert!(run.content.contains("vc3 (<#31> / <#32>)"));
        assert!(run.content.contains("レート帯"));
        // **走査の方向は出さない。**周期が読めると「下からの回」を狙って
        // キューに入れてしまう（select.rs の ScanDirection を見よ）
        for hidden in ["走査", "上から", "下から", "top_down", "bottom_up"] {
            assert!(!run.content.contains(hidden), "{hidden}: {}", run.content);
        }
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
            RecentCompletionCounts, RunOutcome, ScanDirection, SelectionMetrics,
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
            paused: false,
            direction: ScanDirection::BottomUp,
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
            paused: None,
            host: production_host(),
        };
        println!(
            "========== 割り当て表（red） ==========\n{}",
            assignment(&head(), &vc(), Team::Red, SITE).content
        );
        println!(
            "========== 招集通知（red） ==========\n{}",
            summon(&roster(), &vc(), SITE).content
        );
        println!(
            "========== 成立時ログ ==========\n{}",
            matchmaker_run_log(&outcome, at).content
        );
        println!(
            "========== 15分サマリ ==========\n{}",
            matchmaker_summary(&summary, at).content
        );
        println!(
            "========== 15分サマリ（マッチメイカ停止中） ==========\n{}",
            matchmaker_summary(
                &MatchmakerSummary {
                    paused: Some(paused_by_staff()),
                    ..summary
                },
                at
            )
            .content
        );
    }

    #[ignore = "レビュー用の出力。--ignored で明示的に走らせる"]
    #[test]
    fn dump_notifications_for_review() {
        let host = UserId::new(1001);
        println!("========== 招集チャンネル: 成立（10 人ぶん 1 通） ==========");
        println!("{}", summon(&roster(), &vc(), SITE).content);
        println!("\n========== 招集チャンネル: ホスト決定 ==========");
        println!("{}", host_announcement(formation(), host, 1, 3).content);
        println!("\n========== 招集チャンネル: ホスト交代 ==========");
        println!(
            "{}",
            host_announcement(formation(), UserId::new(1006), 2, 3).content
        );
        println!("\n========== red の VC: 割り当て表 ==========");
        println!("{}", assignment(&head(), &vc(), Team::Red, SITE).content);
        println!("\n========== blue の VC: 割り当て表 ==========");
        println!("{}", assignment(&head(), &vc(), Team::Blue, SITE).content);
        println!("\n========== ホスト候補の VC: 打診 ==========");
        println!("{}", host_prompt(&head(), host, 1, 3, SITE).content);
        println!("\n========== ホスト候補の VC: 承諾後 ==========");
        println!("{}", host_accepted(&head(), host, SITE).content);
        // ホストは red の人。通知が飛ぶのは red 側の 1 通だけ
        for team in Team::ALL {
            let reveal = code_reveal(&head(), host, Team::Red, "12345678", &vc(), team);
            println!("\n========== {team} の VC: ルーム ID 開示 ==========");
            println!("{}", reveal.content);
            println!("通知が飛ぶ相手: {:?}", reveal.mention_users);
        }
        println!("\n========== 両 VC: 報告の催促 ==========");
        println!("{}", report_prompt(&head(), SITE).content);
        println!("\n========== 古いボタンを押した人にだけ ==========");
        println!("{}", moved_to_the_site(SITE));
    }

    /// 招集チャンネルは全員が見る。**レート・ロール割り当て・報告の情報を出さない**（00 §3-5）。
    #[test]
    fn summon_never_leaks_ratings_roles_or_reports() {
        {
            let message = summon(&roster(), &vc(), SITE);
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
            // roster が持っているロール表示名も識別子も出ていない
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
    }

    /// **打診にボタンは無い。**操作はマッチ画面。通知としての役目だけ残す。
    #[test]
    fn the_host_prompt_is_a_notification_without_buttons() {
        let message = host_prompt(&head(), UserId::new(1001), 2, 3, SITE);
        assert_eq!(message.mention_users, vec![UserId::new(1001)]);
        assert!(message.content.contains("2/3"));
        assert!(message.content.contains(SITE), "{}", message.content);
        assert!(message.buttons.is_empty(), "{message:?}");
        assert!(host_prompt_moved(&head(), Team::Blue).buttons.is_empty());
        assert!(
            host_accepted(&head(), UserId::new(1001), SITE)
                .buttons
                .is_empty()
        );
    }

    /// **開示のメンションはホスト1人だけ。**参加者10人分は招集チャンネルと重複する。
    #[test]
    fn the_room_code_mentions_only_the_host() {
        let host = UserId::new(1001);
        for team in Team::ALL {
            let message = code_reveal(&head(), host, Team::Red, "8888", &vc(), team);
            // ホスト以外の `<@` が本文に無い
            assert_eq!(
                message.content.matches("<@").count(),
                1,
                "{}",
                message.content
            );
            assert!(message.content.contains("<@1001>"), "{}", message.content);
            for user in 1002..=1010 {
                assert!(
                    !message.content.contains(&format!("<@{user}>")),
                    "{}",
                    message.content
                );
            }
            assert!(message.content.contains("**8888**"));
            assert!(
                message.content.contains(
                    "ホストの方へ: 参加者以外が入室していた場合は、ゲーム内で退出させてください。"
                ),
                "{}",
                message.content
            );
            // 集合先はそのチームの側だけ
            assert!(message.content.contains(&vc().channel_name(team)));
            assert!(message.buttons.is_empty());
        }
        // red の投稿に blue の VC が出てこない
        let red = code_reveal(&head(), host, Team::Red, "8888", &vc(), Team::Red);
        assert!(!red.content.contains("vc3-blue"), "{}", red.content);
    }

    /// **通知が飛ぶのはホスト自身のチーム側の1通だけ。**
    ///
    /// 開示は red / blue の両方へ出すので、2 通とも `mention_users` に載せると
    /// ホスト1人が同じ知らせで 2 回叩かれる。相手チーム側では `<@id>` の表示だけ残し、
    /// 「誰に聞けばよいか」は伝わったまま通知は飛ばさない。
    #[test]
    fn only_the_hosts_own_team_notifies_the_host() {
        let host = UserId::new(1001);
        for host_team in Team::ALL {
            let mut notified = 0;
            for team in Team::ALL {
                let message = code_reveal(&head(), host, host_team, "8888", &vc(), team);
                // 表示は両方に出る（部屋に入れなかったときに聞く相手が分かる）
                assert!(message.content.contains("<@1001>"), "{}", message.content);
                if team == host_team {
                    assert_eq!(message.mention_users, vec![host], "{team} / {host_team}");
                    notified += 1;
                } else {
                    assert!(
                        message.mention_users.is_empty(),
                        "相手チーム側で通知が飛んでいる: {team} / {host_team}"
                    );
                }
            }
            // 1 試合につきホストへの通知はちょうど 1 回
            assert_eq!(notified, 1, "host_team = {host_team}");
        }
    }

    /// ルーム ID に紛れ込んだ mention 構文を本文へ持ち込まない。
    #[test]
    fn a_hostile_room_code_cannot_mention_anyone() {
        let message = code_reveal(
            &head(),
            UserId::new(1001),
            Team::Red,
            "<@&12345>",
            &vc(),
            Team::Red,
        );
        assert_eq!(
            message.content.matches("<@").count(),
            1,
            "{}",
            message.content
        );
        assert!(
            message.content.contains("＜@&12345＞"),
            "{}",
            message.content
        );
    }

    /// **報告の催促にボタンは無く、メンションも付けない。**
    ///
    /// 「報告してください」とは書くが、**勝敗を名指ししない**。選択肢はサイトにある。
    #[test]
    fn the_report_prompt_only_points_at_the_site() {
        let message = report_prompt(&head(), SITE);
        assert!(message.buttons.is_empty(), "{message:?}");
        assert!(message.mention_users.is_empty(), "{message:?}");
        assert!(message.content.contains(SITE), "{}", message.content);
        assert!(!message.content.contains("<@"), "{}", message.content);
        // 勝敗の語も、他人の状況も、件数も出さない
        for forbidden in [
            "勝",
            "負",
            "敗",
            "赤",
            "青",
            "無効",
            "人が報告",
            "件",
            "済み",
        ] {
            assert!(
                !message.content.contains(forbidden),
                "{forbidden}: {}",
                message.content
            );
        }
    }

    /// 通知にはすべてマッチ画面の行き先が入る。**操作はサイトにしか無い。**
    #[test]
    fn every_actionable_notification_carries_the_match_url() {
        let url = match_url(SITE, formation());
        assert_eq!(
            url,
            format!("{SITE}/match/000000000000000000000000000000f0").replace(
                "000000000000000000000000000000f0",
                &formation().0.to_string()
            )
        );
        for message in [
            summon(&roster(), &vc(), &url),
            host_prompt(&head(), UserId::new(1001), 1, 3, &url),
            host_accepted(&head(), UserId::new(1001), &url),
            report_prompt(&head(), &url),
        ] {
            assert!(message.content.contains(&url), "{}", message.content);
        }
        // 末尾の / を重ねない
        assert_eq!(
            match_url("https://match.example.test/", formation()),
            match_url("https://match.example.test", formation())
        );
    }

    /// **どのメッセージにもボタンが無い**（Discord にインタラクションを残さない）。
    #[test]
    fn no_message_carries_an_interaction() {
        let mut messages = all_messages();
        messages.push(report_prompt(&head(), SITE));
        messages.push(summon(&roster(), &vc(), SITE));
        messages.push(host_announcement(formation(), UserId::new(1001), 1, 3));
        for message in messages {
            assert!(message.buttons.is_empty(), "{message:?}");
        }
    }

    /// 停止中のサマリは、先頭に停止の見出しを付ける（止めたまま忘れない）。
    #[test]
    fn a_paused_summary_leads_with_the_pause_notice() {
        let at = Utc.with_ymd_and_hms(2026, 9, 12, 13, 0, 0).unwrap();
        let summary = MatchmakerSummary {
            paused: Some(paused_by_staff()),
            ..MatchmakerSummary::default()
        };
        let message = matchmaker_summary(&summary, at);
        assert!(
            message.content.starts_with("**⏸ マッチメイカは停止中です"),
            "{}",
            message.content
        );
        // 誰がいつ止めたか。JST で出す
        assert!(message.content.contains("2026-09-12 21:30 JST"));
        assert!(message.content.contains("discord:900000000000000001"));
        assert!(message.content.contains("理由: VC が落ちた"));
        // 通常のサマリ本文はそのまま続く
        assert!(message.content.contains("**マッチメイカ 15分サマリ"));
        // 運用ログにメンション構文を混ぜない（15 分ごとに通知を飛ばさない）
        assert!(!message.content.contains("<@"));
        assert!(message.mention_users.is_empty());
    }

    /// 停止していなければ見出しは出ない。
    #[test]
    fn a_running_summary_has_no_pause_notice() {
        let at = Utc.with_ymd_and_hms(2026, 9, 12, 13, 0, 0).unwrap();
        let message = matchmaker_summary(&MatchmakerSummary::default(), at);
        assert!(message.content.starts_with("**マッチメイカ 15分サマリ"));
        assert!(!message.content.contains("停止中"));
    }

    fn paused_by_staff() -> crate::admin::PauseState {
        crate::admin::PauseState {
            actor_discord_user_id: 900000000000000001,
            since: Utc.with_ymd_and_hms(2026, 9, 12, 12, 30, 0).unwrap(),
            reason: Some("VC が落ちた".to_owned()),
        }
    }

    // ------------------------------------------------------------------
    // ホストのリソース（15分サマリの末尾）
    // ------------------------------------------------------------------

    /// 本番（`match.aynm.dev`、2 コア）で実際に読んだ `/proc` の文字列。
    fn production_host() -> crate::host::HostResources {
        crate::host::HostResources {
            load: crate::host::parse_loadavg("0.13 0.09 0.09 1/178 111858\n"),
            memory: crate::host::parse_meminfo(
                "MemTotal:        5549640 kB\n\
                 MemFree:         2629588 kB\n\
                 MemAvailable:    4717120 kB\n",
            ),
            process_rss_kib: crate::host::parse_vm_rss_kib("VmRSS:\t   20912 kB\n"),
            cores: Some(2),
        }
    }

    fn summary_with_host(host: crate::host::HostResources) -> MatchmakerSummary {
        MatchmakerSummary {
            queued: 3,
            host,
            ..MatchmakerSummary::default()
        }
    }

    #[test]
    fn the_host_lines_come_last_and_use_the_real_production_numbers() {
        let at = Utc.with_ymd_and_hms(2026, 9, 12, 13, 0, 0).unwrap();
        let content = matchmaker_summary(&summary_with_host(production_host()), at).content;

        assert!(
            content.contains(
                "\nホスト負荷: 0.13 / 0.09 / 0.09（1分 / 5分 / 15分、2コア換算 7% / 5% / 5%）"
            ),
            "{content}"
        );
        assert!(
            content.contains("\nホストメモリ: 使用 15%（0.8GiB / 5.3GiB） / このプロセス 20MiB"),
            "{content}"
        );
        // 既存の運用項目より前に出さない
        let today = content.find("本日累計").expect("本日累計");
        assert!(
            content.find("ホスト負荷").expect("ホスト負荷") > today,
            "{content}"
        );
        assert!(
            content.find("ホストメモリ").expect("ホストメモリ") > today,
            "{content}"
        );
        // 既存の方針は変えない（個人名・レート・ロール割り当ては出さない）
        for forbidden in ["レート", "トレーナー", "top_carry", "<@"] {
            assert!(!content.contains(forbidden), "{forbidden}: {content}");
        }
    }

    /// 読めなかった項目だけを省く。**サマリ全体は落とさない。**
    #[test]
    fn an_unreadable_proc_only_drops_its_own_line() {
        let at = Utc.with_ymd_and_hms(2026, 9, 12, 13, 0, 0).unwrap();
        let body = "**マッチメイカ 15分サマリ";

        // 全滅（/proc が無い環境）でも本体は出る
        let none = matchmaker_summary(&summary_with_host(Default::default()), at).content;
        assert!(none.contains(body), "{none}");
        assert!(none.contains("本日累計"), "{none}");
        assert!(!none.contains("ホスト負荷"), "{none}");
        assert!(!none.contains("ホストメモリ"), "{none}");

        // loadavg だけ読めた
        let load_only = matchmaker_summary(
            &summary_with_host(crate::host::HostResources {
                load: production_host().load,
                cores: Some(2),
                ..Default::default()
            }),
            at,
        )
        .content;
        assert!(load_only.contains("ホスト負荷: 0.13"), "{load_only}");
        assert!(!load_only.contains("ホストメモリ"), "{load_only}");

        // meminfo が読めず RSS だけ読めた
        let rss_only = matchmaker_summary(
            &summary_with_host(crate::host::HostResources {
                process_rss_kib: Some(20_912),
                ..Default::default()
            }),
            at,
        )
        .content;
        assert!(!rss_only.contains("ホスト負荷"), "{rss_only}");
        assert!(
            rss_only.contains("ホストメモリ: このプロセス 20MiB"),
            "{rss_only}"
        );
        assert!(
            !rss_only.contains("ホストメモリ: 使用"),
            "総量が無ければ使用率は出さない: {rss_only}"
        );

        // コア数が分からなければ換算だけ落とす
        let no_cores = matchmaker_summary(
            &summary_with_host(crate::host::HostResources {
                cores: None,
                ..production_host()
            }),
            at,
        )
        .content;
        assert!(
            no_cores.contains("ホスト負荷: 0.13 / 0.09 / 0.09（1分 / 5分 / 15分）"),
            "{no_cores}"
        );
        assert!(!no_cores.contains("コア換算"), "{no_cores}");
    }

    /// 停止中の見出しは先頭のまま、ホストの行は末尾のまま。両方が共存する。
    #[test]
    fn the_pause_banner_stays_first_and_the_host_lines_stay_last() {
        let at = Utc.with_ymd_and_hms(2026, 9, 12, 13, 0, 0).unwrap();
        let content = matchmaker_summary(
            &MatchmakerSummary {
                paused: Some(paused_by_staff()),
                host: production_host(),
                ..MatchmakerSummary::default()
            },
            at,
        )
        .content;
        assert!(
            content.starts_with("**⏸ マッチメイカは停止中です"),
            "{content}"
        );
        assert!(content.ends_with("このプロセス 20MiB"), "{content}");
    }

    /// キューが空でも投稿する挙動は変えない（停止中の判定にホストは関与しない）。
    #[test]
    fn the_host_reading_does_not_change_when_a_summary_is_published() {
        let quiet = summary_with_host(production_host());
        assert!(quiet.should_publish(), "キュー 3 人なので投稿する");
        let empty = MatchmakerSummary {
            host: production_host(),
            ..MatchmakerSummary::default()
        };
        assert!(
            !empty.should_publish(),
            "ホストが読めても、キューも進行中も無ければ投稿しない"
        );
        let paused = MatchmakerSummary {
            paused: Some(paused_by_staff()),
            ..MatchmakerSummary::default()
        };
        assert!(paused.should_publish(), "停止中は空でも投稿する");
    }
}
