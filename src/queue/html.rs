//! キュー画面の HTML。登録画面と同じシェルとダークテーマを使う。

use crate::lifecycle::Timestamp;
use crate::registration::html::{escape, page_with_styles};

use super::model::{MAX_DOWN_RANGE, MIN_DOWN_RANGE};
use super::routes::{EntryView, RegistrationBlockView, StatusView};

const QUEUE_STYLES: &str = r#"
.queue-form { margin-top: 30px; }
fieldset { min-width: 0; margin: 0; padding: 0; border: 0; }
legend { padding: 0; font-weight: 700; }
.role-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 14px; }
.role-grid .field { min-width: 0; margin-bottom: 18px; }
.range-row { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 18px; align-items: center; }
.range-value { min-width: 5.5rem; color: var(--text); font-size: 1.15rem; font-weight: 750; text-align: right; }
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
.poll-status { min-height: 1.5em; margin-top: 14px; color: var(--danger); font-size: 0.875rem; }
.button-danger { border-color: #7e3d43; background: transparent; color: var(--danger); }
.button-danger:hover { border-color: var(--danger); background: var(--danger-bg); }
input[type="range"] { min-height: 40px; padding: 0; accent-color: var(--accent); }
@media (max-width: 600px) {
  .role-grid { grid-template-columns: 1fr; gap: 0; }
  .metrics { grid-template-columns: 1fr; }
  .range-row { grid-template-columns: 1fr; gap: 4px; }
  .range-value { text-align: left; }
}
"#;

const ROLE_OPTIONS: [(&str, &str); 5] = [
    ("top_carry", "上キャリー"),
    ("top_support", "サポート"),
    ("jungle", "中央"),
    ("bot_carry", "下キャリー"),
    ("bot_tank", "タンク"),
];

pub(super) fn queue_page(status: &StatusView, preview_down_range: i32, now: Timestamp) -> String {
    if status.eligibility.registration.is_some()
        || status.eligibility.active_formation.is_some()
        || !status.eligibility.pending_reports.is_empty()
    {
        blocked_page(status)
    } else if let Some(entry) = &status.entry {
        queued_page(status, entry, now)
    } else {
        unqueued_page(status, preview_down_range)
    }
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

fn unqueued_page(status: &StatusView, down_range: i32) -> String {
    let in_range = status.in_range.unwrap_or(0);
    let first = role_options(None);
    let second = role_options(None);
    let third = role_options(None);
    let body = format!(
        "<section id=\"queue-app\" data-view=\"unqueued\" aria-labelledby=\"queue-title\">\n\
         <div class=\"form-header\">\n<p class=\"eyebrow\">Match queue</p>\n\
         <h1 id=\"queue-title\">キューに参加</h1>\n\
         <p class=\"lead\">希望ロールと、受け入れる下方向のレート範囲を選びます。</p>\n</div>\n\
         <form id=\"queue-form\" class=\"queue-form\" novalidate>\n\
         <fieldset class=\"field\"><legend>ロール希望</legend>\n\
         <p class=\"field-help\">第1希望は必須です。第2・第3希望は必要な分だけ選べます。</p>\n\
         <div class=\"role-grid\">\n\
         <div class=\"field\"><label for=\"role-1\">第1希望</label>\n<select id=\"role-1\" required>{first}</select></div>\n\
         <div class=\"field\"><label for=\"role-2\">第2希望</label>\n<select id=\"role-2\">{second}</select></div>\n\
         <div class=\"field\"><label for=\"role-3\">第3希望</label>\n<select id=\"role-3\" disabled>{third}</select></div>\n\
         </div>\n</fieldset>\n\
         <div class=\"field\">\n<label for=\"down-range\">下方向のレート範囲</label>\n\
         <div class=\"range-row\"><input id=\"down-range\" type=\"range\" min=\"{min}\" max=\"{max}\" value=\"{down_range}\">\n\
         <output id=\"down-range-value\" class=\"range-value\" for=\"down-range\">-{down_range}</output></div>\n\
         <p class=\"field-help\">上方向は +95 固定で、変更できません。下方向のみ -{min} から -{max} まで広げられます。</p>\n</div>\n\
         <div class=\"notice\" aria-live=\"polite\"><p>現在この範囲内でキュー中: <strong id=\"in-range\">{in_range}人</strong></p></div>\n\
         <div id=\"queue-error\" class=\"error-summary\" role=\"alert\" hidden></div>\n\
         <div class=\"actions\"><button type=\"submit\">キューに入る</button></div>\n\
         </form>\n<p id=\"poll-status\" class=\"poll-status\" role=\"status\"></p>\n</section>\n\
         <script>{script}</script>\n",
        min = MIN_DOWN_RANGE,
        max = MAX_DOWN_RANGE,
        script = UNQUEUED_SCRIPT,
    );
    page_with_styles("キュー", &body, QUEUE_STYLES)
}

fn queued_page(status: &StatusView, entry: &EntryView, now: Timestamp) -> String {
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
    let waiting = format_wait(now, &entry.enqueued_at);
    let body = format!(
        "<section id=\"queue-app\" data-view=\"queued\" data-enqueued-at=\"{enqueued}\" aria-labelledby=\"queue-title\">\n\
         <p class=\"eyebrow\">Waiting for match</p>\n<h1 id=\"queue-title\">キュー参加中</h1>\n\
         <p class=\"lead\">マッチ成立までこの画面を開いたまま待機してください。</p>\n\
         <div class=\"metrics\">\n<div class=\"metric\"><span class=\"metric-label\">待ち時間</span><strong id=\"wait-time\" class=\"metric-value\">{waiting}</strong></div>\n\
         <div class=\"metric\"><span class=\"metric-label\">範囲内でキュー中</span><strong id=\"in-range\" class=\"metric-value\">{in_range}人</strong></div>\n</div>\n\
         <section class=\"section\" aria-labelledby=\"settings-title\"><h2 id=\"settings-title\">現在の設定</h2>\n\
         <dl class=\"details\"><div><dt>ロール希望</dt><dd><ol class=\"role-preferences\">{roles}</ol></dd></div>\n\
         <div><dt>レート範囲</dt><dd>上方向 +{up_range} / 下方向 -{down_range}</dd></div></dl></section>\n\
         <div id=\"queue-error\" class=\"error-summary\" role=\"alert\" hidden></div>\n\
         <div class=\"actions\"><button id=\"leave-queue\" class=\"button-danger\" type=\"button\">キューを離脱</button></div>\n\
         <p id=\"poll-status\" class=\"poll-status\" role=\"status\"></p>\n</section>\n\
         <script>{script}</script>\n",
        enqueued = escape(&entry.enqueued_at),
        up_range = entry.up_range,
        down_range = entry.down_range,
        script = QUEUED_SCRIPT,
    );
    page_with_styles("キュー参加中", &body, QUEUE_STYLES)
}

fn blocked_page(status: &StatusView) -> String {
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
             <p>試合がまだ始まっていません。Discordの案内を確認し、割り当てられたVCへ向かってください。</p>\n\
             {destination}<p class=\"fine-print\">formation: {formation}</p></section>\n",
            formation = escape(&formation.to_string()),
        ));
    }
    if !status.eligibility.pending_reports.is_empty() {
        gates.push_str(&format!(
            "<section class=\"gate-block\" data-gate=\"pending_reports\"><h2>未報告の試合</h2>\n\
             <p>未報告の試合が {} 件あります。DiscordのVCテキストチャットにある報告ボタンを押してください。</p></section>\n",
            status.eligibility.pending_reports.len()
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
    let body = format!(
        "<section id=\"queue-app\" data-view=\"blocked\" data-registration=\"{registration_code}\" data-active=\"{active}\" data-pending=\"{pending}\" data-destination=\"{destination_name}\" aria-labelledby=\"queue-title\">\n\
         <p class=\"eyebrow\">Queue status</p>\n<h1 id=\"queue-title\">{title}</h1>\n\
         <p class=\"lead\">現在止まっている条件をそれぞれ確認してください。</p>\n\
         <div class=\"gate-list\">{gates}</div>\n\
         <p id=\"poll-status\" class=\"poll-status\" role=\"status\"></p>\n</section>\n\
         <script>{script}</script>\n",
        registration_code = escape(registration_code),
        active = escape(&active),
        pending = status.eligibility.pending_reports.len(),
        destination_name = escape(destination),
        title = escape(title),
        script = BLOCKED_SCRIPT,
    );
    page_with_styles(title, &body, QUEUE_STYLES)
}

fn registration_gate(block: &RegistrationBlockView) -> String {
    format!(
        "<section class=\"gate-block\" data-gate=\"registration\"><h2>参加登録</h2>\n\
         <p>{}</p><div class=\"actions\"><a class=\"button button-primary\" href=\"/register\">登録画面へ</a></div></section>\n",
        escape(&block.message)
    )
}

fn role_label(role: &str) -> &str {
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
  const range = document.getElementById('down-range');
  const rangeValue = document.getElementById('down-range-value');
  const inRange = document.getElementById('in-range');
  const pollStatus = document.getElementById('poll-status');
  const errorBox = document.getElementById('queue-error');
  const roles = [1, 2, 3].map((n) => document.getElementById(`role-${n}`));
  let previewTimer;

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
      const response = await fetch(`/queue?down_range=${encodeURIComponent(range.value)}`, {
        headers: { Accept: 'application/json' }
      });
      if (!response.ok) throw new Error('preview failed');
      const state = await response.json();
      if (blocked(state) || state.queued) return window.location.reload();
      inRange.textContent = `${state.in_range ?? 0}人`;
      pollStatus.textContent = '';
    } catch (_) {
      pollStatus.textContent = '人数を更新できませんでした。次の更新を待っています。';
    }
  };
  range.addEventListener('input', () => {
    rangeValue.textContent = `-${range.value}`;
    clearTimeout(previewTimer);
    previewTimer = setTimeout(preview, 250);
  });
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
        body: JSON.stringify({ down_range: Number(range.value), roles: preferences })
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
      if (!state.queued || blocked(state)) return window.location.reload();
      inRange.textContent = `${state.in_range ?? 0}人`;
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
  setInterval(poll, 10000);
})();
"#;

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::queue::model::RolePreference;
    use crate::queue::routes::{DestinationView, EligibilityView};

    fn status() -> StatusView {
        StatusView {
            queued: false,
            entry: None,
            in_range: Some(4),
            eligibility: EligibilityView {
                registration: None,
                active_formation: None,
                pending_reports: Vec::new(),
            },
            destination: None,
        }
    }

    #[test]
    fn the_unqueued_page_has_all_roles_range_and_resilient_polling() {
        let rendered = queue_page(&status(), 95, Utc::now());
        for (role, label) in ROLE_OPTIONS {
            assert!(rendered.contains(&format!("value=\"{role}\"")));
            assert!(rendered.contains(label));
        }
        assert!(rendered.contains("min=\"95\" max=\"200\""));
        assert!(rendered.contains("上方向は +95 固定"));
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
            down_range: 150,
            up_range: 95,
            roles: vec![RolePreference {
                role: "jungle".into(),
                priority: 1,
            }],
        });
        let rendered = queue_page(
            &queued,
            95,
            Utc.with_ymd_and_hms(2026, 9, 12, 12, 2, 3).unwrap(),
        );
        assert!(rendered.contains("02:03"));
        assert!(rendered.contains("第1希望: 中央"));
        assert!(rendered.contains("上方向 +95 / 下方向 -150"));
        assert!(rendered.contains("キューを離脱"));
        assert!(rendered.contains("setInterval(poll, 10000)"));
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
        let rendered = queue_page(&blocked, 95, Utc::now());
        for gate in ["registration", "active_formation", "pending_reports"] {
            assert!(rendered.contains(&format!("data-gate=\"{gate}\"")));
        }
        assert!(rendered.contains("vc3-red"));
        assert!(rendered.contains("登録画面へ"));
        assert!(rendered.contains("報告ボタン"));
    }

    #[test]
    fn mobile_layout_is_explicit() {
        let rendered = queue_page(&status(), 95, Utc::now());
        assert!(rendered.contains("@media (max-width: 600px)"));
        assert!(rendered.contains(".role-grid { grid-template-columns: 1fr"));
    }
}
