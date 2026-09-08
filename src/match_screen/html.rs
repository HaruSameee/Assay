//! マッチ画面の HTML。登録・キュー画面と同じシェルとダークテーマを使い、新しいデザインを起こさない。
//!
//! **1枚で最初から最後まで。**成立から確定まで同じ URL で、10 秒ごとのポーリングで
//! 中身だけが切り替わる（キュー画面と同じ間隔・同じ作り。外部の JS ライブラリは使わない）。
//!
//! ここが描けるのは [`MatchView`] にある値だけ。相手チームのロールも他人の報告状況も
//! 型に存在しないので、この描画からは出しようがない。

use crate::registration::Viewer;
use crate::registration::html::{escape, page_with_styles};

use super::model::{AffordanceView, MatchView, OpponentView, StageView, TeammateView, VoiceView};

const MATCH_STYLES: &str = r#"
.teams { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1px; margin: 28px 0; background: var(--border); border: 1px solid var(--border); }
.team { min-width: 0; padding: 18px; background: var(--surface); }
.team-head { display: flex; align-items: baseline; justify-content: space-between; gap: 8px; margin-bottom: 4px; }
.team-name { font-size: 1.05rem; font-weight: 750; }
.team-red .team-name { color: #ff9aa2; }
.team-blue .team-name { color: #9db4ff; }
.team-vc { color: var(--muted); font-size: 0.85rem; overflow-wrap: anywhere; }
.roster { margin: 12px 0 0; padding: 0; list-style: none; }
.roster li { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 4px 10px; padding: 9px 0; border-top: 1px solid var(--border); }
.roster .who { min-width: 0; overflow-wrap: anywhere; font-weight: 650; }
.roster .role { grid-column: 1; color: var(--muted); font-size: 0.85rem; }
.roster .rating { grid-row: 1 / span 2; align-self: center; font-variant-numeric: tabular-nums; font-weight: 750; }
.roster li.me { margin: 0 -18px; padding-right: 18px; padding-left: 14px; border-left: 4px solid var(--accent); background: var(--surface-strong); }
.roster li.me .who::after { content: "あなた"; margin-left: 8px; border-radius: 999px; padding: 1px 8px; background: var(--accent); color: #07150f; font-size: 0.72rem; font-weight: 700; }
.host-tag { margin-left: 8px; border: 1px solid var(--border); border-radius: 999px; padding: 1px 8px; color: var(--muted); font-size: 0.72rem; font-weight: 700; }
.hidden-roles { margin: 10px 0 0; color: var(--muted); font-size: 0.8rem; }
.room-code { margin: 24px 0; padding: 24px; border: 1px solid var(--accent); border-left-width: 4px; background: var(--surface-strong); }
.room-code strong { display: block; margin: 6px 0; font-size: 2.4rem; line-height: 1.15; overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }
.checklist { margin: 8px 0 0; padding-left: 1.25rem; color: var(--muted); }
.checklist li { padding: 3px 0; }
.result { margin: 24px 0; padding: 20px; border-left: 3px solid var(--accent); background: var(--surface); }
.result .delta { font-size: 2rem; font-weight: 800; line-height: 1.2; }
.result .delta.up { color: var(--accent); }
.result .delta.down { color: var(--danger); }
.result .delta.flat { color: var(--muted); }
.choices { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px; margin-top: 16px; }
.choices button { width: 100%; }
.button-red { border-color: #b3555e; background: transparent; color: #ff9aa2; }
.button-red:hover { background: #2a191d; }
.button-blue { border-color: #4a63b0; background: transparent; color: #9db4ff; }
.button-blue:hover { background: #191d2a; }
.code-form { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 12px; align-items: start; margin-top: 16px; }
.code-form input { min-width: 0; }
.poll-status { min-height: 1.5em; margin-top: 14px; color: var(--danger); font-size: 0.875rem; }
@media (max-width: 600px) {
  .teams { grid-template-columns: 1fr; }
  .choices { grid-template-columns: 1fr; }
  .code-form { grid-template-columns: 1fr; }
  .room-code { padding: 18px; }
  .room-code strong { font-size: 1.9rem; }
}
"#;

/// 画面全体。`data-stage` は状態機械の状態名そのもの（テストと運用の照合用）。
pub(super) fn match_page(viewer: Viewer, view: &MatchView) -> String {
    let title = stage_title(&view.stage);
    let body = format!(
        "<section id=\"match-app\" data-stage=\"{stage}\" data-formation=\"{formation}\" \
         data-reported=\"{reported}\" aria-labelledby=\"match-title\">\n\
         <p class=\"eyebrow\">{eyebrow}</p>\n<h1 id=\"match-title\">{title}</h1>\n\
         <p class=\"lead\">{lead}</p>\n\
         {stage_block}\
         {teams}\
         {room}\
         <p id=\"poll-status\" class=\"poll-status\" role=\"status\"></p>\n\
         </section>\n<script>{script}</script>\n",
        stage = view.stage.name(),
        formation = escape(&view.formation_id.to_string()),
        reported = view.i_reported,
        eyebrow = stage_eyebrow(&view.stage),
        title = escape(title),
        lead = stage_lead(&view.stage, &view.actions),
        stage_block = stage_block(view),
        teams = teams(view),
        room = room_block(view),
        script = POLL_SCRIPT,
    );
    page_with_styles(viewer, title, &body, MATCH_STYLES)
}

fn stage_title(stage: &StageView) -> &'static str {
    match stage {
        StageView::Matched => "マッチが成立しました",
        StageView::AwaitingHost { .. } => "ホストを決めています",
        StageView::AwaitingCode { .. } => "部屋の作成を待っています",
        StageView::InProgress { .. } => "試合中",
        StageView::Confirmed { .. } => "結果が確定しました",
        StageView::Disputed => "運営が確認しています",
        StageView::Nullified => "無効試合になりました",
        StageView::Voided => "試合が成立しませんでした",
        StageView::Abandoned => "解散しました",
    }
}

fn stage_eyebrow(stage: &StageView) -> &'static str {
    match stage {
        StageView::Confirmed { .. } | StageView::Nullified => "Result",
        StageView::Disputed => "Under review",
        StageView::Voided | StageView::Abandoned => "Closed",
        _ => "Match",
    }
}

fn stage_lead(stage: &StageView, actions: &AffordanceView) -> String {
    let text = match stage {
        StageView::Matched => "VC の割り当てを待っています。この画面は自動で更新されます。",
        StageView::AwaitingHost { i_am_candidate, .. } if *i_am_candidate => {
            "あなたにホストをお願いしています。下から選んでください。"
        }
        StageView::AwaitingHost { .. } => "ホストの返事を待っています。",
        StageView::AwaitingCode { i_am_host, .. } if *i_am_host => {
            "部屋を作って、ルーム ID を入力してください。"
        }
        StageView::AwaitingCode { .. } => "ホストが部屋を作成中です。",
        StageView::InProgress { .. } if actions.report => {
            "試合が終わったら、下から結果を報告してください。"
        }
        StageView::InProgress { .. } => "ルーム ID で部屋に入ってください。",
        StageView::Confirmed { .. } => "この試合の結果とレートの増減です。",
        StageView::Disputed => {
            "報告が分かれたため、運営が確認しています。結果が出るまでお待ちください。"
        }
        StageView::Nullified => "参加者全員が無効試合として報告しました。レートは動きません。",
        StageView::Voided => "この試合はレートに影響しません。",
        StageView::Abandoned => "この試合は解散しました。キューに戻れます。",
    };
    escape(text)
}

/// 段階ごとの操作と説明。**自分に出せるものだけを描く。**
fn stage_block(view: &MatchView) -> String {
    let mut block = String::new();
    match &view.stage {
        StageView::AwaitingHost {
            pending,
            i_am_candidate,
        } => {
            if *i_am_candidate {
                block.push_str(
                    "<div class=\"notice\"><p><strong>あなたがホスト候補です。</strong></p>\n\
                     <p>部屋を作れる場合は「部屋を作る」を押し、作ったらルーム ID を入力してください。</p>\n\
                     <div class=\"actions\">\n\
                     <button id=\"host-accept\" type=\"button\">部屋を作る</button>\n\
                     <button id=\"host-decline\" class=\"button-secondary\" type=\"button\">今回はパスする</button>\n\
                     </div></div>\n",
                );
            } else {
                let who = pending.as_deref().unwrap_or("次のホスト候補");
                block.push_str(&format!(
                    "<div class=\"notice\"><p><strong>{}</strong> さんに部屋の作成をお願いしています。</p></div>\n",
                    escape(who)
                ));
            }
        }
        StageView::AwaitingCode { host, i_am_host } => {
            if *i_am_host {
                block.push_str(&code_form(None));
            } else {
                block.push_str(&format!(
                    "<div class=\"notice\"><p><strong>{}</strong> さんが部屋を作成中です。ルーム ID が届くまでお待ちください。</p></div>\n",
                    escape(host)
                ));
            }
        }
        StageView::InProgress {
            room_code,
            host,
            i_am_host,
        } => {
            block.push_str(&format!(
                "<div class=\"room-code\"><span class=\"muted\">ルーム ID</span>\
                 <strong>{code}</strong>\
                 <span class=\"fine-print\">ホスト: {host}</span></div>\n",
                code = escape(room_code),
                host = escape(host),
            ));
            if *i_am_host {
                block.push_str(&code_form(Some(room_code)));
            }
        }
        StageView::Confirmed {
            result, my_delta, ..
        } => {
            let label = match result.as_str() {
                "won" => "勝ち",
                "lost" => "負け",
                "drawn" => "引き分け",
                _ => "確定",
            };
            let delta = match my_delta {
                Some(delta) => {
                    let rounded = delta.round();
                    let class = if rounded > 0.0 {
                        "up"
                    } else if rounded < 0.0 {
                        "down"
                    } else {
                        "flat"
                    };
                    let sign = if rounded > 0.0 { "+" } else { "" };
                    format!("<div class=\"delta {class}\">{sign}{rounded:.0}</div>")
                }
                None => String::from(
                    "<div class=\"delta flat\">—</div><p class=\"fine-print\">増減の反映を待っています。</p>",
                ),
            };
            block.push_str(&format!(
                "<div class=\"result\"><p class=\"muted\">あなたの結果</p>\
                 <p><strong>{label}</strong></p>{delta}\
                 <p class=\"fine-print\">増減はこの試合の開始時点のレートから決まります（07 §2-2）。</p></div>\n",
            ));
        }
        StageView::Disputed => {
            // 他人の報告内容も件数もここには無い（型に存在しない）
            block.push_str(
                "<div class=\"notice\"><p>運営が報告を確認しています。追加の操作は要りません。</p></div>\n",
            );
        }
        _ => {}
    }
    if view.actions.report {
        block.push_str(REPORT_BLOCK);
    } else if view.i_reported && !view.stage.is_closed() {
        block.push_str(
            "<div class=\"notice\"><p><strong>受け付けました。</strong>確定を待っています。</p>\n\
             <p class=\"fine-print\">他の参加者の報告状況は表示されません。</p></div>\n",
        );
    }
    if view.stage.is_closed() || matches!(view.stage, StageView::Confirmed { .. }) {
        block.push_str(
            "<div class=\"actions\"><a class=\"button button-primary\" href=\"/queue\">キューへ戻る</a></div>\n",
        );
    }
    block
}

impl StageView {
    /// 試合が終わっている段階か（キューへ戻る導線を出す）。
    fn is_closed(&self) -> bool {
        matches!(
            self,
            Self::Nullified | Self::Voided | Self::Abandoned | Self::Confirmed { .. }
        )
    }
}

const REPORT_BLOCK: &str = "<section class=\"section\" aria-labelledby=\"report-title\">\n\
     <h2 id=\"report-title\">結果を報告</h2>\n\
     <p class=\"field-help\">報告内容は他の参加者には表示されません。他の人が何人報告したかも表示されません。</p>\n\
     <div class=\"choices\">\n\
     <button id=\"report-red\" class=\"button-red\" type=\"button\">赤の勝ち</button>\n\
     <button id=\"report-blue\" class=\"button-blue\" type=\"button\">青の勝ち</button>\n\
     <button id=\"report-nullified\" class=\"button-secondary\" type=\"button\">無効試合</button>\n\
     </div>\n<div id=\"match-error\" class=\"error-summary\" role=\"alert\" hidden></div>\n</section>\n";

fn code_form(current: Option<&str>) -> String {
    let (label, button) = match current {
        Some(_) => ("ルーム ID を打ち直す", "打ち直す"),
        None => ("ルーム ID", "送信"),
    };
    format!(
        "<section class=\"section\" aria-labelledby=\"code-title\">\n\
         <h2 id=\"code-title\">{label}</h2>\n\
         <p class=\"field-help\">部屋を作ったら、表示されているルーム ID をそのまま入力してください。\
         入力すると全員の画面と各VCのテキストチャットに表示されます。</p>\n\
         <div class=\"code-form\">\n\
         <input id=\"room-code\" name=\"room_code\" maxlength=\"32\" autocomplete=\"off\" \
         inputmode=\"numeric\" placeholder=\"例: 12345678\" value=\"{value}\" aria-label=\"{label}\">\n\
         <button id=\"room-code-submit\" type=\"button\">{button}</button>\n</div>\n\
         <div id=\"match-error\" class=\"error-summary\" role=\"alert\" hidden></div>\n</section>\n",
        label = escape(label),
        button = escape(button),
        value = escape(current.unwrap_or("")),
    )
}

fn teams(view: &MatchView) -> String {
    let my_class = if view.my_team == super::model::RED {
        "team-red"
    } else {
        "team-blue"
    };
    let opponent_class = if view.my_team == super::model::RED {
        "team-blue"
    } else {
        "team-red"
    };
    format!(
        "<div class=\"teams\">\n\
         <section class=\"team {my_class}\" aria-labelledby=\"my-team-title\">\n\
         <div class=\"team-head\"><span class=\"team-name\" id=\"my-team-title\">{my_team}（自分）</span>\
         <span class=\"team-vc\">{my_vc}</span></div>\n\
         <ul class=\"roster\">\n{mine}</ul>\n</section>\n\
         <section class=\"team {opponent_class}\" aria-labelledby=\"opponent-title\">\n\
         <div class=\"team-head\"><span class=\"team-name\" id=\"opponent-title\">{opponent_team}</span>\
         <span class=\"team-vc\">{opponent_vc}</span></div>\n\
         <ul class=\"roster\">\n{theirs}</ul>\n\
         <p class=\"hidden-roles\">相手のロールは部屋に入るまで表示されません。</p>\n</section>\n</div>\n",
        my_team = escape(&view.my_team),
        opponent_team = escape(view.opponent_team()),
        my_vc = voice(view.my_voice.as_ref()),
        opponent_vc = voice(view.opponent_voice.as_ref()),
        mine = view.teammates.iter().map(teammate_row).collect::<String>(),
        theirs = view.opponents.iter().map(opponent_row).collect::<String>(),
    )
}

fn voice(voice: Option<&VoiceView>) -> String {
    match voice {
        Some(voice) => format!(
            "<a href=\"https://discord.com/channels/@me/{id}\">{name}</a>",
            id = escape(&voice.channel_id),
            name = escape(&voice.channel_name),
        ),
        None => String::from("VC 割り当て中"),
    }
}

fn teammate_row(entry: &TeammateView) -> String {
    format!(
        "<li{me}><span class=\"who\">{name}{host}</span>\
         <span class=\"rating\">{rating:.0}</span>\
         <span class=\"role\">{role}</span></li>\n",
        me = if entry.is_me { " class=\"me\"" } else { "" },
        name = escape(&entry.trainer_name),
        host = host_tag(entry.is_host),
        rating = entry.rating,
        role = escape(&entry.role),
    )
}

/// 相手チームの行。**ロールを書く場所が無い**（[`OpponentView`] が持っていない）。
fn opponent_row(entry: &OpponentView) -> String {
    format!(
        "<li><span class=\"who\">{name}{host}</span>\
         <span class=\"rating\">{rating:.0}</span></li>\n",
        name = escape(&entry.trainer_name),
        host = host_tag(entry.is_host),
        rating = entry.rating,
    )
}

fn host_tag(is_host: bool) -> &'static str {
    if is_host {
        "<span class=\"host-tag\">ホスト</span>"
    } else {
        ""
    }
}

fn room_block(view: &MatchView) -> String {
    if view.room_checklist.is_empty() || view.stage.is_closed() {
        return String::new();
    }
    let items: String = view
        .room_checklist
        .iter()
        .map(|item| format!("<li>{}</li>\n", escape(item)))
        .collect();
    format!(
        "<section class=\"section\" aria-labelledby=\"room-title\">\n\
         <h2 id=\"room-title\">部屋設定</h2>\n<ul class=\"checklist\">\n{items}</ul>\n</section>\n"
    )
}

/// 10 秒ごとに自分の状態を取り直す。キュー画面と同じ作り・同じ間隔。
///
/// 操作は同じ画面の POST。応答の中身は使わず、成功したら即座に描き直す
/// （判定結果を JS に持たせない = 他人の報告が画面に届く経路を作らない）。
const POLL_SCRIPT: &str = r#"
(() => {
  const app = document.getElementById('match-app');
  if (!app) return;
  const formation = app.dataset.formation;
  const pollStatus = document.getElementById('poll-status');
  const errorBox = document.getElementById('match-error');
  const base = '/match/' + encodeURIComponent(formation);
  let stopped = false;

  const showError = (message) => {
    if (!errorBox) return;
    errorBox.textContent = message;
    errorBox.hidden = false;
  };

  const send = async (path, body) => {
    if (errorBox) errorBox.hidden = true;
    try {
      const response = await fetch(base + path, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (response.ok) {
        // 行き先はサーバが決める（報告を受け付けたらキュー画面へ戻す）。
        // クライアント側に遷移条件もタイマーも持たない
        let next = null;
        try { const payload = await response.json(); next = payload && payload.next; } catch (_) {}
        if (next) { window.location.assign(next); return; }
        window.location.reload();
        return;
      }
      let message = '受け付けられませんでした。画面を更新して確認してください。';
      try {
        const payload = await response.json();
        if (payload && payload.message) message = payload.message;
      } catch (_) {}
      showError(message);
    } catch (_) {
      showError('通信できませんでした。もう一度お試しください。');
    }
  };

  const click = (id, handler) => {
    const element = document.getElementById(id);
    if (element) element.addEventListener('click', handler);
  };

  click('host-accept', () => send('/host', { action: 'accept' }));
  click('host-decline', () => send('/host', { action: 'decline' }));
  click('report-red', () => send('/report', { outcome: 'red' }));
  click('report-blue', () => send('/report', { outcome: 'blue' }));
  click('report-nullified', () => send('/report', { outcome: 'nullified' }));
  click('room-code-submit', () => {
    const input = document.getElementById('room-code');
    const value = input ? input.value.trim() : '';
    if (!value) { showError('ルーム ID を入力してください。'); return; }
    send('/room-code', { room_code: value });
  });

  // 状態が変わったら描き直す。判定や他人の状況を JS 側で組み立てない
  const poll = async () => {
    if (stopped) return;
    try {
      const response = await fetch(base, { headers: { accept: 'application/json' } });
      if (response.status === 401 || response.status === 404) { stopped = true; return; }
      if (!response.ok) throw new Error('poll failed');
      const status = await response.json();
      pollStatus.textContent = '';
      if (status.stage !== app.dataset.stage
          || String(status.i_reported) !== app.dataset.reported) {
        window.location.reload();
      }
    } catch (_) {
      pollStatus.textContent = '状態を更新できませんでした。次の更新を待っています。';
    }
  };
  document.addEventListener('visibilitychange', () => { if (!document.hidden) poll(); });
  setInterval(poll, 10000);
})();
"#;

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::match_screen::model::{BLUE, RED};

    fn teammate(name: &str, role: &str, is_me: bool, is_host: bool) -> TeammateView {
        TeammateView {
            player_id: Uuid::from_u128(1),
            trainer_name: name.into(),
            role: role.into(),
            rating: 1511.0,
            is_me,
            is_host,
        }
    }

    fn opponent(name: &str, is_host: bool) -> OpponentView {
        OpponentView {
            player_id: Uuid::from_u128(2),
            trainer_name: name.into(),
            rating: 1489.0,
            is_host,
        }
    }

    fn view(stage: StageView) -> MatchView {
        MatchView {
            formation_id: Uuid::from_u128(0xabc),
            stage,
            my_team: RED.into(),
            teammates: vec![
                teammate("自分", "中央", true, false),
                teammate("味方", "上キャリー", false, false),
            ],
            opponents: vec![opponent("相手A", false), opponent("相手B", true)],
            my_voice: Some(VoiceView {
                channel_name: "vc3-red".into(),
                channel_id: "101".into(),
            }),
            opponent_voice: Some(VoiceView {
                channel_name: "vc3-blue".into(),
                channel_id: "102".into(),
            }),
            room_checklist: vec!["マップ: kyogre".into(), "大会モード".into()],
            i_reported: false,
            actions: AffordanceView::default(),
        }
    }

    fn in_progress(i_am_host: bool) -> StageView {
        StageView::InProgress {
            room_code: "12345678".into(),
            host: "味方".into(),
            i_am_host,
        }
    }

    #[test]
    fn the_opponent_roles_are_nowhere_in_the_html() {
        let mut state = view(in_progress(false));
        state.teammates[0].role = "中央".into();
        state.teammates[1].role = "上キャリー".into();
        let rendered = match_page(Viewer::MEMBER, &state);
        // 自チームのロールは出る
        assert!(rendered.contains("中央"), "{rendered}");
        assert!(rendered.contains("上キャリー"), "{rendered}");
        // 相手の名前とレートは出るが、行にロールの入る場所が無い
        assert!(rendered.contains("相手A"), "{rendered}");
        assert!(rendered.contains("1489"), "{rendered}");
        assert!(
            rendered.contains("相手のロールは部屋に入るまで表示されません"),
            "{rendered}"
        );
        // 相手の行に role のクラスが付いていないこと
        let opponent_section = rendered
            .split("id=\"opponent-title\"")
            .nth(1)
            .expect("opponent section");
        let opponent_section = opponent_section.split("</section>").next().unwrap();
        assert!(
            !opponent_section.contains("class=\"role\""),
            "{opponent_section}"
        );
    }

    #[test]
    fn nobody_elses_report_status_reaches_the_page() {
        for stage in [
            in_progress(false),
            StageView::Disputed,
            StageView::Confirmed {
                outcome: "red".into(),
                result: "won".into(),
                my_delta: Some(11.0),
            },
        ] {
            let mut state = view(stage);
            state.i_reported = true;
            let rendered = match_page(Viewer::MEMBER, &state);
            for forbidden in ["人が報告", "報告済み:", "9人", "件の報告", "reported_count"]
            {
                assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
            }
        }
    }

    #[test]
    fn only_the_candidate_sees_the_host_buttons() {
        let mine = match_page(
            Viewer::MEMBER,
            &view(StageView::AwaitingHost {
                pending: Some("自分".into()),
                i_am_candidate: true,
            }),
        );
        assert!(mine.contains("id=\"host-accept\""), "{mine}");
        assert!(mine.contains("id=\"host-decline\""), "{mine}");

        let theirs = match_page(
            Viewer::MEMBER,
            &view(StageView::AwaitingHost {
                pending: Some("味方".into()),
                i_am_candidate: false,
            }),
        );
        assert!(!theirs.contains("id=\"host-accept\""), "{theirs}");
        assert!(theirs.contains("味方"), "{theirs}");
    }

    #[test]
    fn only_the_host_sees_the_room_code_input() {
        let host = match_page(Viewer::MEMBER, &view(in_progress(true)));
        assert!(host.contains("id=\"room-code\""), "{host}");
        assert!(host.contains("value=\"12345678\""), "打ち直せること");

        let other = match_page(Viewer::MEMBER, &view(in_progress(false)));
        assert!(!other.contains("id=\"room-code\""), "{other}");
        // ルーム ID そのものは全員に見える
        assert!(other.contains("12345678"), "{other}");
    }

    #[test]
    fn the_report_choices_never_offer_a_draw() {
        let mut state = view(in_progress(false));
        state.actions.report = true;
        let rendered = match_page(Viewer::MEMBER, &state);
        assert!(rendered.contains("赤の勝ち"), "{rendered}");
        assert!(rendered.contains("青の勝ち"), "{rendered}");
        assert!(rendered.contains("無効試合"), "{rendered}");
        assert!(!rendered.contains("引き分け"), "{rendered}");
    }

    #[test]
    fn a_reported_player_sees_only_their_own_acknowledgement() {
        let mut state = view(in_progress(false));
        state.i_reported = true;
        state.actions.report = false;
        let rendered = match_page(Viewer::MEMBER, &state);
        assert!(rendered.contains("受け付けました"), "{rendered}");
        assert!(rendered.contains("確定を待っています"), "{rendered}");
        assert!(!rendered.contains("id=\"report-red\""), "{rendered}");
    }

    #[test]
    fn the_confirmed_page_shows_my_own_delta_and_a_way_back() {
        let rendered = match_page(
            Viewer::MEMBER,
            &view(StageView::Confirmed {
                outcome: "red".into(),
                result: "won".into(),
                my_delta: Some(11.0),
            }),
        );
        assert!(rendered.contains("勝ち"), "{rendered}");
        assert!(rendered.contains(">+11<"), "{rendered}");
        assert!(rendered.contains("href=\"/queue\""), "{rendered}");

        let lost = match_page(
            Viewer::MEMBER,
            &view(StageView::Confirmed {
                outcome: "blue".into(),
                result: "lost".into(),
                my_delta: Some(-12.0),
            }),
        );
        assert!(lost.contains(">-12<"), "{lost}");
    }

    #[test]
    fn a_hostile_trainer_name_cannot_break_out() {
        let mut state = view(in_progress(false));
        state.opponents[0].trainer_name = r#"<script>alert(1)</script>"#.into();
        let rendered = match_page(Viewer::MEMBER, &state);
        assert!(!rendered.contains("<script>alert(1)"), "{rendered}");
        assert!(rendered.contains("&lt;script&gt;alert(1)"), "{rendered}");
    }

    #[test]
    fn the_page_keeps_the_shared_shell_and_mobile_layout() {
        let rendered = match_page(Viewer::MEMBER, &view(in_progress(false)));
        assert!(rendered.starts_with("<!doctype html>"), "{rendered}");
        assert!(
            rendered.contains("<header class=\"site-header\">"),
            "共通ヘッダー"
        );
        assert!(rendered.contains("color-scheme: dark"));
        assert!(rendered.contains("@media (max-width: 600px)"));
        assert!(rendered.contains(".teams { grid-template-columns: 1fr; }"));
        // 外部のCSS/JSを引かない
        assert!(!rendered.contains("http://"), "{rendered}");
        assert!(!rendered.contains("cdn"), "{rendered}");
    }

    #[test]
    fn the_page_never_shows_sigma_or_rating_adjustments() {
        for stage in [
            StageView::Matched,
            StageView::AwaitingHost {
                pending: Some("味方".into()),
                i_am_candidate: false,
            },
            in_progress(false),
            StageView::Confirmed {
                outcome: "red".into(),
                result: "won".into(),
                my_delta: Some(11.0),
            },
            StageView::Disputed,
        ] {
            let rendered = match_page(Viewer::MEMBER, &view(stage));
            for forbidden in [
                "sigma",
                "σ",
                "delta_mu",
                "delta_sigma",
                "rating_adjust",
                "最高レート",
            ] {
                assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
            }
        }
    }

    #[test]
    fn every_stage_renders_a_complete_document() {
        let stages = [
            StageView::Matched,
            StageView::AwaitingHost {
                pending: None,
                i_am_candidate: false,
            },
            StageView::AwaitingCode {
                host: "味方".into(),
                i_am_host: true,
            },
            in_progress(true),
            StageView::Confirmed {
                outcome: "draw".into(),
                result: "drawn".into(),
                my_delta: Some(0.0),
            },
            StageView::Disputed,
            StageView::Nullified,
            StageView::Voided,
            StageView::Abandoned,
        ];
        for stage in stages {
            let name = stage.name();
            let rendered = match_page(Viewer::MEMBER, &view(stage));
            assert!(rendered.starts_with("<!doctype html>"), "{name}");
            assert!(rendered.trim_end().ends_with("</html>"), "{name}");
            assert!(
                rendered.contains(&format!("data-stage=\"{name}\"")),
                "{name}: {rendered}"
            );
        }
    }

    #[test]
    fn blue_side_sees_its_own_team_first() {
        let mut state = view(in_progress(false));
        state.my_team = BLUE.into();
        let rendered = match_page(Viewer::MEMBER, &state);
        let my_title = rendered.find("id=\"my-team-title\"").expect("my team");
        let opponent_title = rendered.find("id=\"opponent-title\"").expect("opponent");
        assert!(my_title < opponent_title, "自分のチームが先");
        assert!(rendered.contains("blue（自分）"), "{rendered}");
    }

    /// レビュー用に各段階の HTML を丸ごと出す（`queue::html` の同名テストと対）。
    ///
    /// `cargo test --lib match_screen::html -- --ignored --nocapture` で取り出す。
    #[ignore = "レビュー用の出力。--ignored で明示的に走らせる"]
    #[test]
    fn dump_every_stage_for_review() {
        let mut candidate = view(StageView::AwaitingHost {
            pending: Some("自分".into()),
            i_am_candidate: true,
        });
        candidate.my_voice = None;
        candidate.opponent_voice = None;

        let mut reporting = view(in_progress(false));
        reporting.actions.report = true;

        let mut reported = view(in_progress(false));
        reported.i_reported = true;

        let mut host_typing = view(StageView::AwaitingCode {
            host: "自分".into(),
            i_am_host: true,
        });
        host_typing.actions.room_code = true;

        let stages: [(&str, MatchView); 8] = [
            ("1. 成立直後（ホスト未確定）", view(StageView::Matched)),
            ("2a. ホスト打診中（候補本人）", candidate),
            (
                "2b. ホスト打診中（他の9人）",
                view(StageView::AwaitingHost {
                    pending: Some("味方".into()),
                    i_am_candidate: false,
                }),
            ),
            ("3. 部屋ID待ち（ホスト本人）", host_typing),
            ("4. 部屋ID開示済み（報告可能）", reporting),
            ("5. 自分が報告済み", reported),
            (
                "6. 確定",
                view(StageView::Confirmed {
                    outcome: "red".into(),
                    result: "won".into(),
                    my_delta: Some(11.0),
                }),
            ),
            ("7. disputed", view(StageView::Disputed)),
        ];
        for (label, state) in stages {
            println!("========== {label} ==========");
            println!("{}", match_page(Viewer::MEMBER, &state));
        }
    }
}
