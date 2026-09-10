//! キュー画面の HTML。登録画面と同じシェルとダークテーマを使う。
//!
//! ヘッダーは共有のシェル（[`page_with_styles`]）が出す。ここでは書かない。

use crate::lifecycle::{FormationId, Timestamp};
use crate::registration::Viewer;
use crate::registration::html::{escape, page_with_styles};

use super::model::RolePreference;
use super::routes::{EntryView, RegistrationBlockView, StatusView};

const QUEUE_STYLES: &str = r#"
.queue-form { margin-top: 30px; }
fieldset { min-width: 0; margin: 0; padding: 0; border: 0; }
legend { padding: 0; font-weight: 700; }
.role-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 14px; }
.role-grid .field { min-width: 0; margin-bottom: 18px; }
.metrics { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1px; margin: 28px 0; background: var(--border); border: 1px solid var(--border); }
.metric { min-width: 0; padding: 18px; background: var(--surface); }
.metric-label { display: block; margin-bottom: 3px; color: var(--muted); font-size: 0.82rem; }
.metric-value { display: block; overflow-wrap: anywhere; font-size: 1.35rem; font-weight: 750; }
.role-preferences { margin: 8px 0 0; padding-left: 1.4rem; }
.role-preferences li { padding: 4px 0; }
.gate-list { display: grid; gap: 0; margin-top: 28px; border-top: 1px solid var(--border); }
.gate-block { padding: 22px 0; border-bottom: 1px solid var(--border); }
.gate-block h2 { margin-bottom: 7px; }
.destination { margin-top: 16px; padding: 16px 18px; border-left: 3px solid var(--accent); background: var(--surface); }
.destination strong { display: block; font-size: 1.25rem; }
/* 成立はブロックではなく朗報。止められている見た目のままにしない（ゲートの判定は同じ） */
.matched .eyebrow { color: var(--accent); }
.matched .gate-list { margin-top: 24px; border-top: 0; }
.matched .gate-block { padding: 0 0 22px; border-bottom: 0; }
.matched .gate-block h2 { color: var(--muted); font-size: 0.82rem; font-weight: 700; text-transform: uppercase; }
.matched .destination { margin-top: 20px; padding: 24px; border: 1px solid var(--accent); border-left-width: 4px; background: var(--surface-strong); }
.matched .destination strong { margin: 6px 0; font-size: 2rem; line-height: 1.2; overflow-wrap: anywhere; }
@media (max-width: 600px) {
  .matched .destination { padding: 18px; }
  .matched .destination strong { font-size: 1.6rem; }
}
.notice-removed { border-left: 3px solid var(--danger); }
.first-choice { margin-top: 14px; padding-top: 12px; border-top: 1px solid var(--border); }
.first-choice-label { margin: 0 0 8px; color: var(--muted); font-size: 0.82rem; }
.first-choice-list { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 8px; margin: 0; padding: 0; list-style: none; }
.first-choice-list li { min-width: 0; }
.first-choice-list .role-name { display: block; color: var(--muted); font-size: 0.78rem; overflow-wrap: anywhere; }
.first-choice-list .role-count { font-variant-numeric: tabular-nums; }
@media (max-width: 600px) { .first-choice-list { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
.poll-status { min-height: 1.5em; margin-top: 14px; color: var(--danger); font-size: 0.875rem; }
.button-danger { border-color: #7e3d43; background: transparent; color: var(--danger); }
.button-danger:hover { border-color: var(--danger); background: var(--danger-bg); }
@media (max-width: 600px) {
  .role-grid { grid-template-columns: 1fr; gap: 0; }
  .metrics { grid-template-columns: 1fr; }
}
"#;

/// 画面に出すロールの表記。**内部識別子を画面に出さない。**
///
/// 変換はここ 1 か所だけ。`<select>`（[`role_options`]）とキュー中の一覧（[`role_label`]）が
/// 同じ表を引くので、選ぶときと選んだ後で表記が食い違わない。
///
/// 並びと表記は Phase 1 マイグレーションの `roles` シードに揃えてある
/// （`the_role_labels_match_the_migration_seed` が両者の一致を検査する）。
const ROLE_OPTIONS: [(&str, &str); 5] = [
    ("top_carry", "上キャリー"),
    ("top_support", "サポート"),
    ("jungle", "中央"),
    ("bot_carry", "下キャリー"),
    ("bot_tank", "タンク"),
];

pub(super) fn queue_page(
    viewer: Viewer,
    status: &StatusView,
    now: Timestamp,
    reported: Option<FormationId>,
    prefill: &[RolePreference],
) -> String {
    if status.eligibility.registration.is_some()
        || status.eligibility.active_formation.is_some()
        || !status.eligibility.pending_reports.is_empty()
    {
        blocked_page(viewer, status, reported)
    } else if let Some(entry) = &status.entry {
        queued_page(viewer, status, entry, now, reported)
    } else {
        unqueued_page(viewer, status, reported, prefill)
    }
}

/// 直前に報告した試合への導線。
///
/// 報告が済んだらキュー画面へ戻す（報告ゲートは本人の報告で解除されるので、確定を
/// 待たせない）。ただし戻したきりでは、**自分の報告後に確定した結果・disputed に
/// なったこと・運営の裁定**を知る手段が無くなる。`/status` は
/// `player_match_rating_deltas` を読むので**確定した試合しか出ない**（disputed も
/// 未確定も出てこない）。そこでマッチ画面へ戻る導線をここに残す。
fn reported_notice(reported: Option<FormationId>) -> String {
    reported.map_or_else(String::new, |formation| {
        format!(
            "<div class=\"notice\" data-notice=\"reported\"><p><strong>報告を受け付けました。</strong>\
             確定を待たずにキューへ戻れます。</p>\n\
             <p class=\"fine-print\">結果・係争・運営の裁定は\
             <a href=\"/match/{formation}\">この試合のマッチ画面</a>で確認できます。</p></div>\n",
            formation = escape(&formation.to_string()),
        )
    })
}

/// 前回選んだ `priority` 番目のロール（1 始まり）。無ければ `None`。
fn prefilled(prefill: &[RolePreference], priority: i16) -> Option<&str> {
    prefill
        .iter()
        .find(|preference| preference.priority == priority)
        .map(|preference| preference.role.as_str())
}

/// ロール別の待機人数。**第1希望がそのロールの人の数**であることを画面に明記する。
///
/// 「タンク 4人」だけだと「タンクを希望に入れている人」と読まれる。5 ロールすべてを
/// 出す（0 人でも 0。出ないと「集計されていない」と読まれる）。
fn first_choice_breakdown(status: &StatusView) -> String {
    let mut items = String::new();
    for (role, label) in ROLE_OPTIONS {
        // 0 人でも必ず出す。行ごと消えると「集計されていない」と読まれる
        let people = status
            .in_range_by_first_choice
            .iter()
            .find(|entry| entry.role == role)
            .map_or(0, |entry| entry.count);
        items.push_str(&format!(
            "<li><span class=\"role-name\">{label}</span>\
             <strong class=\"role-count\" data-count=\"{role}\">{people}人</strong></li>\n",
            label = escape(label),
        ));
    }
    format!(
        "<div class=\"first-choice\">\n\
         <p class=\"first-choice-label\"><strong>第1希望</strong>の内訳（この範囲内）</p>\n\
         <ul class=\"first-choice-list\">\n{items}</ul></div>\n"
    )
}

fn role_options(selected: Option<&str>) -> String {
    let mut output = String::from("<option value=\"\">選択しない</option>\n");
    for (value, label) in ROLE_OPTIONS {
        let selected = if selected == Some(value) {
            " selected"
        } else {
            ""
        };
        output.push_str(&format!(
            "<option value=\"{value}\"{selected}>{label}</option>\n"
        ));
    }
    output
}

fn unqueued_page(
    viewer: Viewer,
    status: &StatusView,
    reported: Option<FormationId>,
    prefill: &[RolePreference],
) -> String {
    let in_range = status.in_range.unwrap_or(0);
    // 前回の希望で埋める。第3希望は第2希望が入っていないと選べない（JS と同じ条件）
    let first = role_options(prefilled(prefill, 1));
    let second = role_options(prefilled(prefill, 2));
    let third = role_options(prefilled(prefill, 3));
    let third_disabled = if prefilled(prefill, 2).is_some() {
        ""
    } else {
        " disabled"
    };
    let body = format!(
        "<section id=\"queue-app\" data-view=\"unqueued\" aria-labelledby=\"queue-title\">\n\
         <div class=\"form-header\">\n<p class=\"eyebrow\">Match queue</p>\n\
         <h1 id=\"queue-title\">キューに参加</h1>\n\
         <p class=\"lead\">希望ロールを選びます。</p>\n</div>\n\
         {reported}{removal}\
         <form id=\"queue-form\" class=\"queue-form\" novalidate>\n\
         <fieldset class=\"field\"><legend>ロール希望</legend>\n\
         <p class=\"field-help\">第1希望は必須です。第2・第3希望は必要な分だけ選べます。</p>\n\
         <div class=\"role-grid\">\n\
         <div class=\"field\"><label for=\"role-1\">第1希望</label>\n<select id=\"role-1\" required>{first}</select></div>\n\
         <div class=\"field\"><label for=\"role-2\">第2希望</label>\n<select id=\"role-2\">{second}</select></div>\n\
         <div class=\"field\"><label for=\"role-3\">第3希望</label>\n<select id=\"role-3\"{third_disabled}>{third}</select></div>\n\
         </div>\n</fieldset>\n\
         <div class=\"notice\" aria-live=\"polite\"><p>マッチング範囲は上 +{up} / 下 -{down} です。\
         現在この範囲内でキュー中: <strong id=\"in-range\">{in_range}人</strong></p>\n\
         {breakdown}</div>\n\
         <div id=\"queue-error\" class=\"error-summary\" role=\"alert\" hidden></div>\n\
         <div class=\"actions\"><button type=\"submit\">キューに入る</button></div>\n\
         </form>\n<p id=\"poll-status\" class=\"poll-status\" role=\"status\"></p>\n</section>\n\
         <script>{script}</script>\n",
        up = status.up_range,
        down = status.down_range,
        reported = reported_notice(reported),
        breakdown = first_choice_breakdown(status),
        removal = removal_notice(status),
        script = UNQUEUED_SCRIPT,
    );
    page_with_styles(viewer, "キュー", &body, QUEUE_STYLES)
}

/// 「運営によってキューから外されました」。**黙って消さない**（そうすると
/// 「バグでキューから外れた」と受け取られる）。運営が理由を書いていればそれも出す。
///
/// キュー中の人にはこの通知が来ない（[`crate::queue::routes`] が引かない）ので、
/// 外された後に入り直せば消える。
fn removal_notice(status: &StatusView) -> String {
    let Some(removal) = &status.removed_by_staff else {
        return String::new();
    };
    let reason = removal.reason.as_deref().map_or_else(
        || {
            String::from(
                "<p class=\"fine-print\">理由は記載されていません。運営にお問い合わせください。</p>\n",
            )
        },
        |reason| format!("<p>理由: {}</p>\n", escape(reason)),
    );
    format!(
        "<div class=\"notice notice-removed\" data-removed=\"1\" role=\"status\">\n\
         <p><strong>運営によってキューから外されました。</strong></p>\n\
         {reason}\
         <p class=\"fine-print\">再度キューに入ることはできます。</p>\n</div>\n"
    )
}

fn queued_page(
    viewer: Viewer,
    status: &StatusView,
    entry: &EntryView,
    now: Timestamp,
    reported: Option<FormationId>,
) -> String {
    let roles = entry
        .roles
        .iter()
        .map(|preference| {
            format!(
                "<li>第{}希望: {}</li>",
                preference.priority,
                escape(role_label(&preference.role))
            )
        })
        .collect::<String>();
    let in_range = status.in_range.unwrap_or(0);
    let queue_total = status.queue_total.unwrap_or(0);
    let waiting = format_wait(now, &entry.enqueued_at);
    let body = format!(
        "<section id=\"queue-app\" data-view=\"queued\" data-enqueued-at=\"{enqueued}\" aria-labelledby=\"queue-title\">\n\
         <p class=\"eyebrow\">Waiting for match</p>\n<h1 id=\"queue-title\">キュー参加中</h1>\n\
         <p class=\"lead\">マッチ成立までこの画面を開いたまま待機してください。</p>\n\
         {reported}\
         <div class=\"metrics\">\n<div class=\"metric\"><span class=\"metric-label\">待ち時間</span><strong id=\"wait-time\" class=\"metric-value\">{waiting}</strong></div>\n\
         <div class=\"metric\"><span class=\"metric-label\">全体でキュー中</span><strong id=\"queue-total\" class=\"metric-value\">{queue_total}人</strong></div>\n\
         <div class=\"metric\"><span class=\"metric-label\">範囲内でキュー中</span><strong id=\"in-range\" class=\"metric-value\">{in_range}人</strong></div>\n</div>\n\
         {breakdown}\
         <section class=\"section\" aria-labelledby=\"settings-title\"><h2 id=\"settings-title\">現在の設定</h2>\n\
         <dl class=\"details\"><div><dt>ロール希望</dt><dd><ol class=\"role-preferences\">{roles}</ol></dd></div>\n\
         <div><dt>レート範囲</dt><dd>上下 ±{up_range}（固定）</dd></div></dl></section>\n\
         <div id=\"queue-error\" class=\"error-summary\" role=\"alert\" hidden></div>\n\
         <div class=\"actions\"><button id=\"leave-queue\" class=\"button-danger\" type=\"button\">キューを離脱</button></div>\n\
         <p id=\"poll-status\" class=\"poll-status\" role=\"status\"></p>\n</section>\n\
         <script>{script}</script>\n",
        enqueued = escape(&entry.enqueued_at),
        breakdown = first_choice_breakdown(status),
        up_range = entry.up_range,
        reported = reported_notice(reported),
        script = QUEUED_SCRIPT,
    );
    page_with_styles(viewer, "キュー参加中", &body, QUEUE_STYLES)
}

fn blocked_page(viewer: Viewer, status: &StatusView, reported: Option<FormationId>) -> String {
    let matched = status.eligibility.active_formation.is_some();
    let title = if matched {
        "マッチが成立しました"
    } else {
        "キューに参加できません"
    };
    let mut gates = String::new();
    if let Some(block) = &status.eligibility.registration {
        gates.push_str(&registration_gate(block));
    }
    if let Some(formation) = status.eligibility.active_formation {
        let destination = status.destination.as_ref().map_or_else(
            || {
                "<div class=\"destination\"><span class=\"muted\">集合先</span><strong>VC割り当て処理中</strong><span class=\"fine-print\">この画面は自動で更新されます。</span></div>\n".to_owned()
            },
            |destination| {
                format!(
                    "<div class=\"destination\"><span class=\"muted\">あなたの集合先</span><strong>{}</strong><span class=\"fine-print\">{} チームのVCです。Discordへ移動してください。</span></div>\n",
                    escape(&destination.channel_name),
                    escape(&destination.team),
                )
            },
        );
        gates.push_str(&format!(
            "<section class=\"gate-block\" data-gate=\"active_formation\"><h2>進行中のマッチ</h2>\n\
             <p>割り当てられたVCへ向かい、<strong>マッチ画面</strong>で進行してください。\
             ホストの返事・ルーム ID・結果の報告はすべてマッチ画面から行います。</p>\n\
             {destination}\
             <div class=\"actions\"><a class=\"button button-primary\" href=\"/match/{formation}\">マッチ画面へ</a></div>\n\
             <p class=\"fine-print\">formation: {formation}</p></section>\n",
            formation = escape(&formation.to_string()),
        ));
    }
    if !status.eligibility.pending_reports.is_empty() {
        // **マッチ画面への導線を必ず出す。**報告は Discord のボタンでもできるが、
        // 押し忘れたまま VC を出た人には、この画面から辿れる経路しか残らない。
        // 未報告のあいだキューへは入れないので、ここで行き止まりにしない
        let links = status
            .eligibility
            .pending_reports
            .iter()
            .map(|formation| {
                format!(
                    "<a class=\"button button-primary\" href=\"/match/{formation}\">\
                     マッチ画面で報告する</a>",
                    formation = escape(&formation.to_string()),
                )
            })
            .collect::<Vec<_>>()
            .join("");
        gates.push_str(&format!(
            "<section class=\"gate-block\" data-gate=\"pending_reports\"><h2>未報告の試合</h2>\n\
             <p>未報告の試合が {count} 件あります。下のボタンからマッチ画面を開いて報告してください\
             （DiscordのVCテキストチャットにある報告ボタンからでも同じです）。</p>\n\
             <div class=\"actions\">{links}</div></section>\n",
            count = status.eligibility.pending_reports.len(),
        ));
    }

    let registration_code = status
        .eligibility
        .registration
        .as_ref()
        .map(|block| block.code)
        .unwrap_or("");
    let active = status
        .eligibility
        .active_formation
        .map(|formation| formation.to_string())
        .unwrap_or_default();
    let destination = status
        .destination
        .as_ref()
        .map(|destination| destination.channel_name.as_str())
        .unwrap_or("");
    // 成立だけ見た目を分ける。`data-view` とゲートの判定はそのまま
    let (view_class, eyebrow, lead) = if matched {
        (
            " class=\"matched\"",
            "Match found",
            "下のVCへ移動してください。試合が終わるまで次のキューには入れません。",
        )
    } else {
        (
            "",
            "Queue status",
            "現在止まっている条件をそれぞれ確認してください。",
        )
    };
    let body = format!(
        "<section id=\"queue-app\"{view_class} data-view=\"blocked\" data-registration=\"{registration_code}\" data-active=\"{active}\" data-pending=\"{pending}\" data-destination=\"{destination_name}\" aria-labelledby=\"queue-title\">\n\
         <p class=\"eyebrow\">{eyebrow}</p>\n<h1 id=\"queue-title\">{title}</h1>\n\
         <p class=\"lead\">{lead}</p>\n\
         {reported}\
         <div class=\"gate-list\">{gates}</div>\n\
         <p id=\"poll-status\" class=\"poll-status\" role=\"status\"></p>\n</section>\n\
         <script>{script}</script>\n",
        registration_code = escape(registration_code),
        active = escape(&active),
        pending = status.eligibility.pending_reports.len(),
        destination_name = escape(destination),
        title = escape(title),
        reported = reported_notice(reported),
        script = BLOCKED_SCRIPT,
    );
    page_with_styles(viewer, title, &body, QUEUE_STYLES)
}

fn registration_gate(block: &RegistrationBlockView) -> String {
    format!(
        "<section class=\"gate-block\" data-gate=\"registration\"><h2>参加登録</h2>\n\
         <p>{}</p><div class=\"actions\"><a class=\"button button-primary\" href=\"/register\">登録画面へ</a></div></section>\n",
        escape(&block.message)
    )
}

/// 内部識別子から画面の表記へ。管理パネルのキュー一覧もこの表を引く
/// （選ぶときと運営が見るときで名前が食い違わない）。
pub(crate) fn role_label(role: &str) -> &str {
    ROLE_OPTIONS
        .iter()
        .find_map(|(value, label)| (*value == role).then_some(*label))
        .unwrap_or(role)
}

fn format_wait(now: Timestamp, enqueued_at: &str) -> String {
    let Ok(enqueued_at) = enqueued_at.parse::<Timestamp>() else {
        return "--:--".to_owned();
    };
    let seconds = (now - enqueued_at).num_seconds().max(0);
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

const UNQUEUED_SCRIPT: &str = r#"
(() => {
  const form = document.getElementById('queue-form');
  const inRange = document.getElementById('in-range');
  const pollStatus = document.getElementById('poll-status');
  const errorBox = document.getElementById('queue-error');
  const roles = [1, 2, 3].map((n) => document.getElementById(`role-${n}`));

  const blocked = (state) => state.eligibility.registration !== null ||
    state.eligibility.active_formation !== null || state.eligibility.pending_reports.length > 0;
  const showError = (message) => {
    errorBox.textContent = message;
    errorBox.hidden = false;
  };
  const clearError = () => { errorBox.hidden = true; errorBox.textContent = ''; };
  const refreshRoles = (changed) => {
    if (changed === roles[1] && !roles[1].value) roles[2].value = '';
    roles[2].disabled = !roles[1].value;
    const selected = new Set(roles.map((select) => select.value).filter(Boolean));
    for (const select of roles) {
      for (const option of select.options) {
        option.disabled = Boolean(option.value && selected.has(option.value) && option.value !== select.value);
      }
    }
  };
  const preview = async () => {
    if (document.hidden) return;
    try {
      const response = await fetch('/queue', { headers: { Accept: 'application/json' } });
      if (!response.ok) throw new Error('preview failed');
      const state = await response.json();
      if (blocked(state) || state.queued) return window.location.reload();
      inRange.textContent = `${state.in_range ?? 0}人`;
      for (const entry of state.in_range_by_first_choice ?? []) {
        const cell = document.querySelector(`[data-count="${entry.role}"]`);
        if (cell) cell.textContent = `${entry.count}人`;
      }
      pollStatus.textContent = '';
    } catch (_) {
      pollStatus.textContent = '人数を更新できませんでした。次の更新を待っています。';
    }
  };
  for (const select of roles) select.addEventListener('change', () => refreshRoles(select));
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    clearError();
    const selected = roles.map((select) => select.value).filter(Boolean);
    if (!roles[0].value) return showError('第1希望のロールを選んでください。');
    if (new Set(selected).size !== selected.length) return showError('同じロールは重複して選べません。');
    const preferences = selected.map((role, index) => ({ role, priority: index + 1 }));
    try {
      const response = await fetch('/queue', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
        body: JSON.stringify({ roles: preferences })
      });
      const result = await response.json();
      if (response.status === 403) return window.location.reload();
      if (!response.ok) return showError(result.message || 'キューに参加できませんでした。');
      window.location.reload();
    } catch (_) {
      showError('通信に失敗しました。時間をおいてもう一度お試しください。');
    }
  });
  document.addEventListener('visibilitychange', () => { if (!document.hidden) preview(); });
  refreshRoles();
  setInterval(preview, 10000);
})();
"#;

const QUEUED_SCRIPT: &str = r#"
(() => {
  const root = document.getElementById('queue-app');
  const waitTime = document.getElementById('wait-time');
  const inRange = document.getElementById('in-range');
  const queueTotal = document.getElementById('queue-total');
  const pollStatus = document.getElementById('poll-status');
  const errorBox = document.getElementById('queue-error');
  const blocked = (state) => state.eligibility.registration !== null ||
    state.eligibility.active_formation !== null || state.eligibility.pending_reports.length > 0;
  const updateWait = () => {
    const start = Date.parse(root.dataset.enqueuedAt);
    if (Number.isNaN(start)) return;
    const seconds = Math.max(0, Math.floor((Date.now() - start) / 1000));
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    const rest = seconds % 60;
    waitTime.textContent = hours > 0
      ? `${String(hours).padStart(2, '0')}:${String(minutes).padStart(2, '0')}:${String(rest).padStart(2, '0')}`
      : `${String(minutes).padStart(2, '0')}:${String(rest).padStart(2, '0')}`;
  };
  const poll = async () => {
    if (document.hidden) return;
    try {
      const response = await fetch('/queue', { headers: { Accept: 'application/json' } });
      if (!response.ok) throw new Error('status failed');
      const state = await response.json();
      // 成立したらマッチ画面へ。行き先はサーバの状態（active_formation）から来る
      const matched = state.eligibility.active_formation;
      if (matched) return window.location.assign('/match/' + encodeURIComponent(matched));
      if (!state.queued || blocked(state)) return window.location.reload();
      if (queueTotal) queueTotal.textContent = `${state.queue_total ?? 0}人`;
      inRange.textContent = `${state.in_range ?? 0}人`;
      for (const entry of state.in_range_by_first_choice ?? []) {
        const cell = document.querySelector(`[data-count="${entry.role}"]`);
        if (cell) cell.textContent = `${entry.count}人`;
      }
      pollStatus.textContent = '';
    } catch (_) {
      pollStatus.textContent = '状態を更新できませんでした。次の更新を待っています。';
    }
  };
  document.getElementById('leave-queue').addEventListener('click', async () => {
    errorBox.hidden = true;
    try {
      const response = await fetch('/queue', { method: 'DELETE', headers: { Accept: 'application/json' } });
      if (!response.ok) throw new Error('leave failed');
      window.location.reload();
    } catch (_) {
      errorBox.textContent = 'キューから離脱できませんでした。もう一度お試しください。';
      errorBox.hidden = false;
    }
  });
  document.addEventListener('visibilitychange', () => { if (!document.hidden) poll(); });
  updateWait();
  setInterval(updateWait, 1000);
  setInterval(poll, 10000);
})();
"#;

const BLOCKED_SCRIPT: &str = r#"
(() => {
  const root = document.getElementById('queue-app');
  const pollStatus = document.getElementById('poll-status');
  const poll = async () => {
    if (document.hidden) return;
    try {
      const response = await fetch('/queue', { headers: { Accept: 'application/json' } });
      if (!response.ok) throw new Error('status failed');
      const state = await response.json();
      const registration = state.eligibility.registration?.code || '';
      const active = state.eligibility.active_formation || '';
      // 成立していればマッチ画面へ。この画面に留めない
      if (active) return window.location.assign('/match/' + encodeURIComponent(active));
      const pending = String(state.eligibility.pending_reports.length);
      const destination = state.destination?.channel_name || '';
      if (state.queued || registration !== root.dataset.registration || active !== root.dataset.active ||
          pending !== root.dataset.pending || destination !== root.dataset.destination ||
          (!registration && !active && pending === '0')) {
        return window.location.reload();
      }
      pollStatus.textContent = '';
    } catch (_) {
      pollStatus.textContent = '状態を更新できませんでした。次の更新を待っています。';
    }
  };
  document.addEventListener('visibilitychange', () => { if (!document.hidden) poll(); });
  // 直接この画面を開いた場合に、10秒待たずにマッチ画面へ移る
  poll();
  setInterval(poll, 10000);
})();
"#;

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::queue::model::RolePreference;
    use crate::queue::routes::{DestinationView, EligibilityView, RoleCountView};

    fn status() -> StatusView {
        StatusView {
            queued: false,
            up_range: 50,
            down_range: 50,
            entry: None,
            queue_total: Some(9),
            in_range: Some(4),
            // 4 人ぶんの内訳。タンクは 0 人（0 も出ることを画面のテストで見る）
            in_range_by_first_choice: vec![
                RoleCountView {
                    role: "top_carry".to_owned(),
                    count: 2,
                },
                RoleCountView {
                    role: "jungle".to_owned(),
                    count: 2,
                },
            ],
            eligibility: EligibilityView {
                registration: None,
                active_formation: None,
                pending_reports: Vec::new(),
            },
            destination: None,
            removed_by_staff: None,
        }
    }

    #[test]
    fn the_unqueued_page_has_all_roles_range_and_resilient_polling() {
        let rendered = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &[]);
        for (role, label) in ROLE_OPTIONS {
            assert!(rendered.contains(&format!("value=\"{role}\"")));
            assert!(rendered.contains(label));
        }
        // 調整UIは無い。プレイヤーは幅を変えられない（運営だけが管理パネルから変える）
        assert!(!rendered.contains("type=\"range\""), "スライダーを出さない");
        assert!(!rendered.contains("down-range"), "{rendered}");
        assert!(rendered.contains("マッチング範囲は上 +50 / 下 -50 です"));
        assert!(rendered.contains("現在この範囲内でキュー中"));
        assert!(rendered.contains("document.hidden"));
        assert!(rendered.contains("setInterval(preview, 10000)"));
    }

    #[test]
    fn the_queued_page_shows_wait_settings_and_leave() {
        let mut queued = status();
        queued.queued = true;
        queued.entry = Some(EntryView {
            id: Uuid::from_u128(1),
            player_id: Uuid::from_u128(2),
            enqueued_at: "2026-09-12T12:00:00+00:00".into(),
            updated_at: "2026-09-12T12:00:00+00:00".into(),
            down_range: 50,
            up_range: 50,
            roles: vec![RolePreference {
                role: "jungle".into(),
                priority: 1,
            }],
        });
        let rendered = queue_page(
            Viewer::MEMBER,
            &queued,
            Utc.with_ymd_and_hms(2026, 9, 12, 12, 2, 3).unwrap(),
            None,
            &[],
        );
        assert!(rendered.contains("02:03"));
        assert!(rendered.contains("第1希望: 中央"));
        assert!(rendered.contains("上下 ±50（固定）"));
        assert!(rendered.contains("キューを離脱"));
        assert!(rendered.contains("setInterval(poll, 10000)"));
        // **全体と範囲内を 2 つ並べる。**「範囲内 4 人」だけだと、
        // 待たされているのが人数不足なのかレート帯の偏りなのか分からない
        assert!(rendered.contains("全体でキュー中"), "{rendered}");
        assert!(
            rendered.contains("id=\"queue-total\" class=\"metric-value\">9人"),
            "{rendered}"
        );
        assert!(rendered.contains("範囲内でキュー中"), "{rendered}");
        assert!(
            rendered.contains("id=\"in-range\" class=\"metric-value\">4人"),
            "{rendered}"
        );
        // 全体が先、範囲内が後（狭い方へ絞り込む順に読ませる）
        let total_at = rendered.find("全体でキュー中").expect("全体");
        let range_at = rendered.find("範囲内でキュー中").expect("範囲内");
        assert!(total_at < range_at, "{rendered}");
        // ポーリングでも両方が更新される
        assert!(rendered.contains("state.queue_total"), "{rendered}");
    }

    #[test]
    fn all_three_gates_are_rendered_as_separate_sections() {
        let mut blocked = status();
        blocked.eligibility = EligibilityView {
            registration: Some(RegistrationBlockView {
                code: "not_registered",
                message: "登録が済んでいません。".into(),
                declared: None,
            }),
            active_formation: Some(Uuid::from_u128(8)),
            pending_reports: vec![Uuid::from_u128(9)],
        };
        blocked.destination = Some(DestinationView {
            formation_id: Uuid::from_u128(8),
            team: "red".into(),
            channel_name: "vc3-red".into(),
            channel_id: "123".into(),
        });
        let rendered = queue_page(Viewer::MEMBER, &blocked, Utc::now(), None, &[]);
        for gate in ["registration", "active_formation", "pending_reports"] {
            assert!(rendered.contains(&format!("data-gate=\"{gate}\"")));
        }
        assert!(rendered.contains("vc3-red"));
        assert!(rendered.contains("登録画面へ"));
        assert!(rendered.contains("報告ボタン"));
        // **未報告のゲートで行き止まりにしない。**Discord のボタンを押し忘れたまま
        // VC を出た人には、この画面から辿れる経路しか残らない
        assert!(rendered.contains("マッチ画面で報告する"), "{rendered}");
        assert!(
            rendered.contains("href=\"/match/00000000-0000-0000-0000-000000000009\""),
            "未報告の試合へのリンクが無い: {rendered}"
        );
    }

    /// 成立は**朗報として見せる**。ゲートの判定も `data-view` も変えず、見た目だけ分ける。
    #[test]
    fn a_formed_match_looks_different_from_being_blocked() {
        let mut matched = status();
        matched.eligibility.active_formation = Some(Uuid::from_u128(8));
        matched.destination = Some(DestinationView {
            formation_id: Uuid::from_u128(8),
            team: "red".into(),
            channel_name: "vc3-red".into(),
            channel_id: "123".into(),
        });
        let rendered = queue_page(Viewer::MEMBER, &matched, Utc::now(), None, &[]);
        assert!(rendered.contains("class=\"matched\""), "{rendered}");
        // ゲートの側は一切変えていない
        assert!(rendered.contains("data-view=\"blocked\""));
        assert!(rendered.contains("data-gate=\"active_formation\""));
        assert!(rendered.contains("data-active=\"00000000-0000-0000-0000-000000000008\""));
        // 行き先が目立つ指定になっている
        assert!(rendered.contains(".matched .destination strong"));
        assert!(rendered.contains("vc3-red"));
        // 「止められている」文言を出さない
        assert!(!rendered.contains("現在止まっている条件"), "{rendered}");

        // 成立していないブロックは従来どおり
        let mut plain = status();
        plain.eligibility.pending_reports = vec![Uuid::from_u128(9)];
        let rendered = queue_page(Viewer::MEMBER, &plain, Utc::now(), None, &[]);
        assert!(!rendered.contains("class=\"matched\""));
        assert!(rendered.contains("現在止まっている条件"));
        assert!(rendered.contains("data-view=\"blocked\""));
    }

    /// 表記の対応は 1 か所だけ。`<select>` とキュー中の一覧が同じ表を引くこと。
    ///
    /// 選ぶときと選んだ後で表記が違うと、同じものを指していると分からない。
    #[test]
    fn the_select_and_the_queued_list_use_the_same_labels() {
        let mut queued = status();
        queued.queued = true;
        queued.entry = Some(EntryView {
            id: Uuid::from_u128(1),
            player_id: Uuid::from_u128(2),
            enqueued_at: "2026-09-12T12:00:00+00:00".into(),
            updated_at: "2026-09-12T12:00:00+00:00".into(),
            down_range: 50,
            up_range: 50,
            roles: ROLE_OPTIONS
                .iter()
                .enumerate()
                .map(|(index, (role, _))| RolePreference {
                    role: (*role).to_owned(),
                    priority: index as i16 + 1,
                })
                .collect(),
        });
        let selecting = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &[]);
        let selected = queue_page(Viewer::MEMBER, &queued, Utc::now(), None, &[]);
        for (role, label) in ROLE_OPTIONS {
            // 選ぶ側は日本語のラベル。識別子は value 属性の中だけ
            assert!(
                selecting.contains(&format!("<option value=\"{role}\">{label}</option>")),
                "{role}"
            );
            // 選んだ後も同じ表記
            assert_eq!(role_label(role), label);
            assert!(selected.contains(label), "{label}");
        }
        // どちらの画面にも内部識別子が地の文として出ていない
        for page in [&selecting, &selected] {
            for (role, _) in ROLE_OPTIONS {
                assert!(
                    !page.contains(&format!(">{role}<")),
                    "internal identifier {role} is visible"
                );
            }
        }
    }

    /// 表記の正規値はマイグレーションの `roles` シード。片方だけ変わる事故を防ぐ。
    #[test]
    fn the_role_labels_match_the_migration_seed() {
        const SEED: &str =
            include_str!("../../migrations/20260904141514_formation_lifecycle.up.sql");
        // `('top_carry',   '上キャリー', 1),` の形の行から、引用符の中だけを取る
        let seeded: Vec<(String, String)> = SEED
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix('(')?;
                let quoted: Vec<&str> = rest.split('\'').skip(1).step_by(2).collect();
                match quoted.as_slice() {
                    [id, display] => Some(((*id).to_owned(), (*display).to_owned())),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(seeded.len(), ROLE_OPTIONS.len(), "seed rows: {seeded:?}");
        let expected: Vec<(String, String)> = ROLE_OPTIONS
            .iter()
            .map(|(role, label)| ((*role).to_owned(), (*label).to_owned()))
            .collect();
        assert_eq!(seeded, expected, "UI の表記と roles シードがずれている");
    }

    #[test]
    fn mobile_layout_is_explicit() {
        let rendered = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &[]);
        assert!(rendered.contains("@media (max-width: 600px)"));
        assert!(rendered.contains(".role-grid { grid-template-columns: 1fr"));
    }

    /// キュー画面も共通ヘッダーをそのまま出す（自前の `<header>` を持たない）。
    ///
    /// 一般参加者の 3 つの表示（未投入・キュー中・ブロック / 成立）では同じもの。
    /// 管理者には管理パネルへの導線だけを追加する。
    #[test]
    fn every_queue_view_renders_the_header_for_its_viewer() {
        let now = Utc.with_ymd_and_hms(2026, 9, 12, 12, 2, 3).unwrap();
        let mut queued = status();
        queued.queued = true;
        queued.entry = Some(EntryView {
            id: Uuid::from_u128(1),
            player_id: Uuid::from_u128(2),
            enqueued_at: "2026-09-12T12:00:00+00:00".into(),
            updated_at: "2026-09-12T12:00:00+00:00".into(),
            down_range: 50,
            up_range: 50,
            roles: vec![RolePreference {
                role: "jungle".into(),
                priority: 1,
            }],
        });
        let mut blocked = status();
        blocked.eligibility.pending_reports = vec![Uuid::from_u128(9)];

        let expected = "<header class=\"site-header\">\n\
             <a class=\"brand\" href=\"/queue\">Assay</a>\n\
             <nav class=\"site-nav\" aria-label=\"メニュー\">\n\
             <a href=\"/status\">戦績</a>\n\
             <a href=\"/profile\">プロフィール</a>\n\
             <form method=\"post\" action=\"/auth/logout\">\
             <button type=\"submit\">ログアウト</button></form>\n\
             </nav>\n</header>\n";
        for rendered in [
            queue_page(Viewer::MEMBER, &status(), now, None, &[]),
            queue_page(Viewer::MEMBER, &queued, now, None, &[]),
            queue_page(Viewer::MEMBER, &blocked, now, None, &[]),
        ] {
            assert!(rendered.contains(expected), "{rendered}");
            // 非管理者にパネルの存在を知らせない
            assert!(!rendered.contains("admin"), "{rendered}");
        }
        // 管理者にはサーバーが管理パネルへの導線を含むヘッダーを送る。
        let admin = queue_page(Viewer::ADMIN, &status(), now, None, &[]);
        let admin_expected = "<header class=\"site-header\">\n\
             <a class=\"brand\" href=\"/queue\">Assay</a>\n\
             <nav class=\"site-nav\" aria-label=\"メニュー\">\n\
             <a href=\"/status\">戦績</a>\n\
             <a href=\"/profile\">プロフィール</a>\n\
             <a href=\"/admin\">管理</a>\n\
             <form method=\"post\" action=\"/auth/logout\">\
             <button type=\"submit\">ログアウト</button></form>\n\
             </nav>\n</header>\n";
        assert!(admin.contains(admin_expected), "{admin}");
    }

    /// レビュー用に各画面の HTML を丸ごと出す。スクリーンショットが撮れないため。
    ///
    /// `cargo test --lib queue::html -- --ignored --nocapture` で取り出す。
    #[ignore = "レビュー用の出力。--ignored で明示的に走らせる"]
    #[test]
    fn dump_every_page_for_review() {
        let now = Utc.with_ymd_and_hms(2026, 9, 12, 12, 2, 3).unwrap();

        println!("========== 1. 未投入 (data-view=unqueued) ==========");
        println!("{}", queue_page(Viewer::MEMBER, &status(), now, None, &[]));

        let mut queued = status();
        queued.queued = true;
        queued.entry = Some(EntryView {
            id: Uuid::from_u128(1),
            player_id: Uuid::from_u128(2),
            enqueued_at: "2026-09-12T12:00:00+00:00".into(),
            updated_at: "2026-09-12T12:00:00+00:00".into(),
            down_range: 50,
            up_range: 50,
            roles: vec![
                RolePreference {
                    role: "jungle".into(),
                    priority: 1,
                },
                RolePreference {
                    role: "bot_tank".into(),
                    priority: 2,
                },
            ],
        });
        println!("========== 2. キュー中 (data-view=queued) ==========");
        println!("{}", queue_page(Viewer::MEMBER, &queued, now, None, &[]));

        let mut matched = status();
        matched.eligibility.active_formation = Some(Uuid::from_u128(8));
        matched.destination = Some(DestinationView {
            formation_id: Uuid::from_u128(8),
            team: "red".into(),
            channel_name: "vc3-red".into(),
            channel_id: "123".into(),
        });
        println!("========== 3. マッチ成立 ==========");
        println!("{}", queue_page(Viewer::MEMBER, &matched, now, None, &[]));

        let mut blocked = status();
        blocked.eligibility = EligibilityView {
            registration: Some(RegistrationBlockView {
                code: "not_registered",
                message: "登録が済んでいません。".into(),
                declared: None,
            }),
            active_formation: None,
            pending_reports: vec![Uuid::from_u128(9)],
        };
        println!("========== 4. ゲートで停止 ==========");
        println!("{}", queue_page(Viewer::MEMBER, &blocked, now, None, &[]));
    }

    /// **成立したらマッチ画面へ移す。**行き先はサーバの状態（`active_formation`）から来る。
    ///
    /// クライアントにタイマーも遷移条件も持たせない。ポーリングは既存の10秒のまま。
    #[test]
    fn the_queue_page_moves_to_the_match_screen_once_one_forms() {
        // キュー中の画面
        let mut queued = status();
        queued.queued = true;
        queued.entry = Some(EntryView {
            id: Uuid::nil(),
            player_id: Uuid::nil(),
            enqueued_at: "2026-09-12T12:00:00+00:00".to_owned(),
            updated_at: "2026-09-12T12:00:00+00:00".to_owned(),
            down_range: 50,
            up_range: 50,
            roles: vec![RolePreference {
                role: "jungle".to_owned(),
                priority: 1,
            }],
        });
        let rendered = queue_page(Viewer::MEMBER, &queued, Utc::now(), None, &[]);
        assert!(
            rendered.contains("state.eligibility.active_formation"),
            "サーバの状態を見ていない: {rendered}"
        );
        assert!(
            rendered.contains("window.location.assign('/match/'"),
            "マッチ画面へ移していない: {rendered}"
        );
        // クライアント側のタイマーで遷移していない（間隔は既存のポーリングだけ）
        assert_eq!(rendered.matches("setInterval(poll, 10000)").count(), 1);

        // ゲートで止まっている画面（直接開いた場合）も同じ経路で移る
        let mut blocked = status();
        blocked.eligibility.active_formation = Some(Uuid::from_u128(7));
        let rendered = queue_page(Viewer::MEMBER, &blocked, Utc::now(), None, &[]);
        assert!(
            rendered.contains("window.location.assign('/match/'"),
            "{rendered}"
        );
        // 10 秒待たずに、開いた直後に一度だけ評価する
        assert!(rendered.contains("\n  poll();\n"), "{rendered}");
    }

    /// 報告後に戻ったキュー画面から、その試合の結果を追える導線が残ること。
    ///
    /// `/status` は確定した試合しか出さないので、disputed と未確定はここからしか辿れない。
    #[test]
    fn the_reported_notice_links_back_to_the_match_screen() {
        let formation = FormationId(Uuid::from_u128(0xabc));
        for view in [status(), {
            let mut blocked = status();
            blocked.eligibility.pending_reports = vec![Uuid::from_u128(1)];
            blocked
        }] {
            let rendered = queue_page(Viewer::MEMBER, &view, Utc::now(), Some(formation), &[]);
            assert!(
                rendered.contains(&format!("/match/{}", formation.0)),
                "{rendered}"
            );
            assert!(rendered.contains("報告を受け付けました"), "{rendered}");
            assert!(rendered.contains("運営の裁定"), "{rendered}");
        }
        // 付いていなければ何も出さない
        let plain = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &[]);
        assert!(!plain.contains("報告を受け付けました"), "{plain}");
    }

    /// **前回のロール希望でフォームが埋まる。**
    #[test]
    fn the_form_is_prefilled_with_the_previous_choices() {
        let prefill = vec![
            RolePreference {
                role: "jungle".to_owned(),
                priority: 1,
            },
            RolePreference {
                role: "bot_tank".to_owned(),
                priority: 2,
            },
        ];
        let rendered = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &prefill);
        assert!(
            rendered.contains("<option value=\"jungle\" selected>中央</option>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("<option value=\"bot_tank\" selected>タンク</option>"),
            "{rendered}"
        );
        // 第2希望が入っているので第3希望も選べる
        assert!(rendered.contains("<select id=\"role-3\">"), "{rendered}");

        // 何も無ければ従来どおり。第3希望は塞いだまま
        let plain = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &[]);
        assert!(!plain.contains(" selected>"), "{plain}");
        assert!(plain.contains("<select id=\"role-3\" disabled>"), "{plain}");
    }

    /// 第1希望だけなら、第3希望は塞がったまま（JS の条件と揃える）。
    #[test]
    fn a_single_prefilled_choice_leaves_the_third_slot_closed() {
        let prefill = vec![RolePreference {
            role: "top_carry".to_owned(),
            priority: 1,
        }];
        let rendered = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &prefill);
        assert!(
            rendered.contains("<option value=\"top_carry\" selected>上キャリー</option>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("<select id=\"role-3\" disabled>"),
            "{rendered}"
        );
    }

    /// **5 ロールすべてを出し、0 人のロールも 0 と表示する。**
    ///
    /// 出ないと「集計されていない」と読まれる。数え方が第1希望であることも明記する。
    #[test]
    fn the_role_breakdown_shows_every_role_including_zero() {
        let rendered = queue_page(Viewer::MEMBER, &status(), Utc::now(), None, &[]);
        // 数え方の明示
        assert!(
            rendered.contains("<strong>第1希望</strong>の内訳"),
            "{rendered}"
        );
        // 5 ロールぶんの枠がある
        assert_eq!(rendered.matches("class=\"role-count\"").count(), 5);
        for (role, label) in ROLE_OPTIONS {
            assert!(
                rendered.contains(&format!("data-count=\"{role}\"")),
                "{role} の枠が無い: {rendered}"
            );
            assert!(rendered.contains(label), "{label} が無い: {rendered}");
        }
        // 内訳のある2つは実数、残る3つは 0 人
        assert!(
            rendered.contains("data-count=\"top_carry\">2人"),
            "{rendered}"
        );
        assert!(rendered.contains("data-count=\"jungle\">2人"), "{rendered}");
        for zero in ["top_support", "bot_carry", "bot_tank"] {
            assert!(
                rendered.contains(&format!("data-count=\"{zero}\">0人")),
                "{zero} が 0 人として出ていない: {rendered}"
            );
        }
    }

    /// キュー中の画面にも同じ内訳が出る。ポーリングでも書き換わる。
    #[test]
    fn the_role_breakdown_is_on_the_queued_page_too() {
        let mut queued = status();
        queued.queued = true;
        queued.entry = Some(EntryView {
            id: Uuid::nil(),
            player_id: Uuid::nil(),
            enqueued_at: "2026-09-12T12:00:00+00:00".to_owned(),
            updated_at: "2026-09-12T12:00:00+00:00".to_owned(),
            down_range: 50,
            up_range: 50,
            roles: vec![RolePreference {
                role: "jungle".to_owned(),
                priority: 1,
            }],
        });
        let rendered = queue_page(Viewer::MEMBER, &queued, Utc::now(), None, &[]);
        assert!(
            rendered.contains("<strong>第1希望</strong>の内訳"),
            "{rendered}"
        );
        assert!(
            rendered.contains("data-count=\"bot_tank\">0人"),
            "{rendered}"
        );
        // ポーリングが内訳も書き換える
        assert!(
            rendered.contains("state.in_range_by_first_choice"),
            "{rendered}"
        );
    }
}
