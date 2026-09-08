//! 管理パネルの HTML。外枠は登録画面（[`crate::registration::html::page`]）をそのまま使う。
//!
//! 表示する値はトレーナー名・Discord ID・数値・日時だけ。いずれも [`escape`] を通す。
//! 打ち消しは行ごとの POST フォーム（`/admin/revert`）で、ボタンは**未打ち消しの
//! 通常行にだけ**出す（権威は DB の unique index とトリガー。ここは表示の面倒だけ）。

use chrono::{DateTime, Utc};

use crate::queue::html::role_label;
use crate::registration::Viewer;
use crate::registration::html::{escape, page};
use crate::status::html::{display_time, format_delta};

use super::actions::{AdminActionRow, MAX_BLOCK_ALLOWANCE, PauseState};
use super::repository::{AdjustmentRow, FoundPlayer, QueuedEntry};
use super::season::{MAX_SEASON_LABEL_LENGTH, SeasonPreview};

/// 検索画面に渡すデータ。
#[derive(Debug, Clone)]
pub struct SearchView<'a> {
    /// 検索ボックスに戻す文字列。
    pub query: &'a str,
    /// 検索結果（クエリが空なら空）。
    pub players: &'a [FoundPlayer],
    /// 直近の調整履歴（新しい順）。
    pub recent: &'a [AdjustmentRow],
    /// マッチメイカの停止・再開。**当日いちばん先に触る操作なので入口の画面に置く。**
    pub matchmaker: MatchmakerView<'a>,
    /// 操作後の通知。
    pub notice: Option<Notice>,
}

/// プレイヤー 1 件の調整画面に渡すデータ。
#[derive(Debug, Clone)]
pub struct PlayerView<'a> {
    pub player: &'a FoundPlayer,
    /// そのプレイヤーの調整履歴（新しい順）。
    pub adjustments: &'a [AdjustmentRow],
    /// 調整フォームの冪等キー（フォーム描画時にサーバ発行。同一フォームの
    /// 二重送信は同じキーを持つので、2 回目の送信は行を増やさない）。
    pub form_key: &'a str,
    /// 現在のブロック枠（`admin_actions` の最新行からの導出値。既定は 2）。
    pub block_allowance: i32,
    pub notice: Option<Notice>,
}

/// 通知メッセージ。
#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: NoticeKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub enum NoticeKind {
    Info,
    Error,
}

impl NoticeKind {
    fn class(self) -> &'static str {
        match self {
            Self::Info => "notice",
            Self::Error => "notice notice-error",
        }
    }
}

const ADMIN_STYLES: &str = r#"
<style>
.notice { border: 1px solid var(--border); border-left: 4px solid var(--accent);
          border-radius: 8px; background: var(--surface); padding: 12px 16px;
          margin: 16px 0; }
.notice-error { border-left-color: var(--danger); }
.table-wrap { overflow-x: auto; margin-top: 16px; }
table.log { width: 100%; border-collapse: collapse; font-size: 0.9rem; }
table.log th { text-align: left; color: var(--muted); font-weight: 650;
               border-bottom: 1px solid var(--border); padding: 8px 10px;
               white-space: nowrap; }
table.log td { border-bottom: 1px solid var(--border); padding: 8px 10px;
               vertical-align: top; }
table.log td.num { text-align: right; font-variant-numeric: tabular-nums;
                   white-space: nowrap; }
.delta-pos { color: var(--accent); font-weight: 750; }
.delta-neg { color: var(--danger); font-weight: 750; }
.delta-zero { color: var(--muted); }
.tag { display: inline-block; border-radius: 999px; padding: 1px 10px;
       font-size: 0.78rem; font-weight: 700; border: 1px solid var(--border);
       color: var(--muted); white-space: nowrap; }
form.inline { display: inline; }
button.revert { background: none; border: 1px solid var(--border); color: var(--text);
                border-radius: 999px; padding: 2px 12px; font: inherit; font-size: 0.8rem;
                cursor: pointer; }
button.revert:hover { border-color: var(--danger); color: var(--danger); }
dl.fields { display: grid; grid-template-columns: max-content 1fr; gap: 4px 16px; }
dl.fields dt { color: var(--muted); font-weight: 650; }
dl.fields dd { margin: 0; }
.admin-nav { margin: -8px 0 18px; color: var(--muted); font-size: 0.9rem; }
.admin-nav a { color: var(--muted); }
.ops { margin: 20px 0 28px; padding: 18px 20px; border: 1px solid var(--border);
      border-radius: 8px; background: var(--surface); }
.ops h2 { margin-top: 0; }
.ops form { margin-top: 12px; }
.state { display: inline-block; margin-left: 6px; border-radius: 999px; padding: 1px 12px;
         font-size: 0.82rem; font-weight: 700; vertical-align: middle; }
.state-running { border: 1px solid var(--border); color: var(--accent); }
.state-paused { border: 1px solid var(--danger); color: var(--danger); }
table.log td form { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; margin: 0; }
table.log td form input[type="text"] { width: 12rem; margin: 0; }
.sr-only { position: absolute; width: 1px; height: 1px; overflow: hidden;
           clip: rect(0 0 0 0); white-space: nowrap; }
</style>
"#;

fn notice_html(notice: Option<&Notice>) -> String {
    match notice {
        Some(notice) => format!(
            "<p class=\"{}\">{}</p>\n",
            notice.kind.class(),
            escape(&notice.message)
        ),
        None => String::new(),
    }
}

fn delta_cell(delta: f64) -> String {
    let rounded = delta.round();
    let class = if rounded > 0.0 {
        "delta-pos"
    } else if rounded < 0.0 {
        "delta-neg"
    } else {
        "delta-zero"
    };
    format!("<td class=\"num {class}\">{}</td>", format_delta(delta))
}

/// 行が打ち消し行（他行の取消し）かどうか。
fn is_reversal(row: &AdjustmentRow) -> bool {
    row.reverts_id.is_some()
}

/// 調整ログの表。`show_player` はプレイヤー列を出す（検索画面の一覧用）。
fn adjustments_table(rows: &[AdjustmentRow], show_player: bool) -> String {
    if rows.is_empty() {
        return String::from("<p class=\"muted\">調整はまだありません。</p>\n");
    }
    let player_head = if show_player {
        "<th>プレイヤー</th>"
    } else {
        ""
    };
    let mut body = String::new();
    for row in rows {
        let player_cell = if show_player {
            format!(
                "<td><a href=\"/admin/player?player={}\">{}</a></td>",
                escape(&row.player_id.to_string()),
                escape(row.trainer_name.as_deref().unwrap_or("(未設定)"))
            )
        } else {
            String::new()
        };
        let state = if is_reversal(row) {
            format!(
                "<span class=\"tag\">打ち消し (#{})</span>",
                row.reverts_id.unwrap_or_default()
            )
        } else if row.reverted {
            String::from("<span class=\"tag\">打ち消済み</span>")
        } else {
            // 未打ち消しの通常行だけ、打ち消しフォームを出す
            format!(
                "<form class=\"inline\" method=\"post\" action=\"/admin/revert\">\
                 <input type=\"hidden\" name=\"adjustment_id\" value=\"{id}\">\
                 <input type=\"hidden\" name=\"key\" value=\"revert:{id}\">\
                 <button class=\"revert\" type=\"submit\">打ち消す</button></form>",
                id = row.id,
            )
        };
        body.push_str(&format!(
            "<tr><td class=\"num\">{id}</td>{player_cell}\
             <td>{played_at}</td>{delta_mu}{delta_sigma}\
             <td>{reason}</td><td>{created_by}</td><td>{state}</td></tr>\n",
            id = row.id,
            player_cell = player_cell,
            played_at = display_time(row.created_at),
            delta_mu = delta_cell(row.delta_mu),
            delta_sigma = delta_cell(row.delta_sigma),
            reason = escape(&row.reason),
            created_by = escape(&row.created_by),
            state = state,
        ));
    }
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>#</th>{player_head}<th>日時</th><th>Δmu</th><th>Δsigma</th>\
         <th>理由</th><th>who</th><th>状態</th></tr></thead>\n<tbody>\n{body}</tbody>\n</table></div>\n"
    )
}

/// 検索画面（`/admin`）。
pub fn search_page(view: &SearchView<'_>) -> String {
    let mut results = String::new();
    for player in view.players {
        results.push_str(&format!(
            "<tr><td><a href=\"/admin/player?player={}\">{}</a></td>\
             <td>{}</td><td>{}</td></tr>\n",
            escape(&player.player_id.to_string()),
            escape(player.trainer_name.as_deref().unwrap_or("(未設定)")),
            player.discord_user_id,
            escape(&player.player_id.to_string()),
        ));
    }
    let found = if view.query.is_empty() {
        String::from(
            "<p class=\"fine-print\">トレーナー名（部分一致）または Discord ID（完全一致）で検索します。</p>\n",
        )
    } else if view.players.is_empty() {
        String::from("<p class=\"muted\">該当するプレイヤーが見つかりませんでした。</p>\n")
    } else {
        format!(
            "<div class=\"table-wrap\"><table class=\"log\">\n\
             <thead><tr><th>トレーナー名</th><th>Discord ID</th><th>player id</th></tr></thead>\n\
             <tbody>\n{results}</tbody>\n</table></div>\n"
        )
    };

    // この画面は管理者しか到達しない（非管理者は 404）。ヘッダーも管理者のもの
    page(
        Viewer::ADMIN,
        "管理 / レート調整",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-title\">レート調整パネル</h1>\n\
             {nav}\
             <p class=\"lead\">調整は <strong>追記のみ</strong> です。既存の行は変更も削除もできず、\
             取り消しは「打ち消し行」の追記で行います。</p>\n\
             {notice}\
             {matchmaker}\
             <form method=\"get\" action=\"/admin\">\
             <label for=\"q\">プレイヤー検索</label>\
             <input id=\"q\" name=\"q\" type=\"search\" value=\"{q}\" placeholder=\"トレーナー名または Discord ID\">\
             <div class=\"actions\"><button class=\"button button-primary\" type=\"submit\">検索</button></div>\
             </form>\n\
             {found}\
             <h2>直近の調整履歴</h2>\n\
             {recent}\
             </section>\n",
            nav = admin_nav("/admin"),
            notice = notice_html(view.notice.as_ref()),
            matchmaker = matchmaker_section(view.matchmaker),
            q = escape(view.query),
            found = found,
            recent = adjustments_table(view.recent, true),
        ),
    )
}

/// プレイヤー 1 件の調整画面（`/admin/player`）。
pub fn player_page(view: &PlayerView<'_>) -> String {
    // この画面は管理者しか到達しない（非管理者は 404）。ヘッダーも管理者のもの
    page(
        Viewer::ADMIN,
        "管理 / レート調整",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-player-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-player-title\">{name}</h1>\n\
             {nav}\
             <dl class=\"fields\">\n\
             <dt>Discord ID</dt><dd>{discord}</dd>\n\
             <dt>player id</dt><dd>{player_id}</dd>\n\
             <dt>ブロック枠</dt><dd>{allowance} 枠</dd>\n\
             </dl>\n\
             {notice}\
             <h2>ブロック枠</h2>\n\
             <p class=\"fine-print\">既定は 2 枠です。変更は追記として記録され、\
             ブロック機能が入ったときにこの値が参照されます。0〜{max} の整数で指定します。</p>\n\
             <form method=\"post\" action=\"/admin/blocks\">\
             <input type=\"hidden\" name=\"player\" value=\"{player_id}\">\
             <input type=\"hidden\" name=\"key\" value=\"blocks:{form_key}\">\
             <label for=\"allowance\">ブロック枠</label>\
             <input id=\"allowance\" name=\"allowance\" type=\"number\" min=\"0\" max=\"{max}\" step=\"1\" value=\"{allowance}\" required>\
             <label for=\"allowance_reason\">理由（任意・操作ログに残ります）</label>\
             <input id=\"allowance_reason\" name=\"reason\" type=\"text\" maxlength=\"200\">\
             <div class=\"actions\"><button class=\"button button-primary\" type=\"submit\">ブロック枠を変更</button></div>\
             </form>\n\
             <h2>調整の追記</h2>\n\
             <p class=\"fine-print\">入力は表示スケール（レートの表示と同じ単位）です。\
             内部的な μ / σ への変換は既存の経路で行われます。理由は必須です。</p>\n\
             <form method=\"post\" action=\"/admin/adjustments\">\
             <input type=\"hidden\" name=\"player\" value=\"{player_id}\">\
             <input type=\"hidden\" name=\"key\" value=\"{form_key}\">\
             <label for=\"delta_mu\">レート増減（例: +10 / -25）</label>\
             <input id=\"delta_mu\" name=\"delta_mu\" type=\"number\" step=\"any\" required>\
             <label for=\"delta_sigma\">σ 増減（表示スケール。通常は 0）</label>\
             <input id=\"delta_sigma\" name=\"delta_sigma\" type=\"number\" step=\"any\" value=\"0\" required>\
             <label for=\"reason\">理由（必須・監査ログに残ります）</label>\
             <textarea id=\"reason\" name=\"reason\" rows=\"2\" required></textarea>\
             <div class=\"actions\">\
             <button class=\"button button-primary\" type=\"submit\">調整を追記</button>\
             <a class=\"button button-secondary\" href=\"/admin\">検索に戻る</a>\
             </div>\
             </form>\n\
             <h2>このプレイヤーの調整履歴</h2>\n\
             {history}\
             </section>\n",
            name = escape(view.player.trainer_name.as_deref().unwrap_or("(未設定)")),
            nav = admin_nav("/admin/player"),
            discord = view.player.discord_user_id,
            player_id = escape(&view.player.player_id.to_string()),
            allowance = view.block_allowance,
            max = MAX_BLOCK_ALLOWANCE,
            notice = notice_html(view.notice.as_ref()),
            form_key = escape(view.form_key),
            history = adjustments_table(view.adjustments, false),
        ),
    )
}

// ----------------------------------------------------------------------
// 当日の運用（マッチメイカの停止・再開 / キュー中の一覧と強制離脱 / 操作履歴）
// ----------------------------------------------------------------------

/// マッチメイカの停止・再開ブロックに渡すデータ。
#[derive(Debug, Clone, Copy)]
pub struct MatchmakerView<'a> {
    /// 停止中なら誰がいつ止めたか。`None` は稼働中。
    pub paused: Option<&'a PauseState>,
    /// 停止 / 再開フォームの冪等キー（フォーム描画時にサーバ発行）。
    pub form_key: &'a str,
}

/// キュー中の一覧画面（`/admin/queue`）に渡すデータ。
#[derive(Debug, Clone)]
pub struct QueueView<'a> {
    /// 同じ画面からマッチメイカも止められるようにする（事故対応で画面を跨がせない）。
    pub matchmaker: MatchmakerView<'a>,
    /// キュー中の一覧（待ち時間の長い順）。
    pub entries: &'a [QueuedEntry],
    /// 直近の運営操作。誰がいつ何をしたかをこの画面で読めるようにする。
    pub actions: &'a [AdminActionRow],
    /// 強制離脱フォームの冪等キー（行ごとに player id を足して使う）。
    pub form_key: &'a str,
    /// 待ち時間の基準時刻。
    pub now: DateTime<Utc>,
    pub notice: Option<Notice>,
}

/// 管理パネル内の行き来。画面を跨いでも当日の導線を見失わないようにする。
fn admin_nav(current: &str) -> String {
    let link = |href: &str, label: &str| {
        if href == current {
            format!("<strong>{label}</strong>")
        } else {
            format!("<a href=\"{href}\">{label}</a>")
        }
    };
    format!(
        "<nav class=\"admin-nav\" aria-label=\"管理メニュー\">{} / {} / {}</nav>\n",
        link("/admin", "レート調整"),
        link("/admin/queue", "キューと運用"),
        link("/admin/season", "シーズン"),
    )
}

/// マッチメイカの状態と、停止 / 再開のフォーム。
///
/// **状態は DB から導出した値**（`admin_actions` の最新行）で、プロセス内のフラグではない。
/// 止めたまま再起動しても停止のままになる。
fn matchmaker_section(view: MatchmakerView<'_>) -> String {
    let (state, detail, action, button, help) = match view.paused {
        Some(pause) => (
            "<span class=\"state state-paused\">停止中</span>",
            format!(
                "<p class=\"fine-print\">{} に discord:{} が停止しました。理由: {}</p>\n",
                escape(&display_time(pause.since)),
                pause.actor_discord_user_id,
                escape(pause.reason.as_deref().unwrap_or("（記載なし）")),
            ),
            "resume",
            "マッチメイカを再開",
            "再開すると、次の実行から通常どおり試合が成立します。",
        ),
        None => (
            "<span class=\"state state-running\">稼働中</span>",
            String::new(),
            "pause",
            "マッチメイカを停止",
            "停止しても、進行中の試合とキューはそのままです。新しい試合が成立しなくなるだけで、\
             サービス自体は動き続けます。",
        ),
    };
    format!(
        "<section class=\"ops\" aria-labelledby=\"matchmaker-title\">\n\
         <h2 id=\"matchmaker-title\">マッチメイカ {state}</h2>\n\
         {detail}\
         <p class=\"fine-print\">{help}</p>\n\
         <form method=\"post\" action=\"/admin/matchmaker\">\
         <input type=\"hidden\" name=\"action\" value=\"{action}\">\
         <input type=\"hidden\" name=\"key\" value=\"matchmaker:{key}\">\
         <label for=\"matchmaker-reason\">理由（任意・操作ログに残ります）</label>\
         <input id=\"matchmaker-reason\" name=\"reason\" type=\"text\" maxlength=\"200\">\
         <div class=\"actions\"><button class=\"button button-primary\" type=\"submit\">{button}</button></div>\
         </form>\n</section>\n",
        key = escape(view.form_key),
        help = escape(help),
    )
}

/// 待ち時間の表示。秒は出さない（当日の判断に要らない）。
fn wait_label(now: DateTime<Utc>, enqueued_at: DateTime<Utc>) -> String {
    let seconds = now
        .signed_duration_since(enqueued_at)
        .num_seconds()
        .max(0)
        .unsigned_abs();
    let minutes = seconds / 60;
    if minutes >= 60 {
        format!("{}時間{}分", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}分")
    }
}

/// 希望ロールを表示名で並べる。**内部識別子を画面に出さない。**
/// 表記はキュー画面と同じ表を引く（選ぶときと運営が見るときで名前が食い違わない）。
fn role_wishes(entry: &QueuedEntry) -> String {
    if entry.roles.is_empty() {
        return String::from("（なし）");
    }
    entry
        .roles
        .iter()
        .map(|wish| format!("{}. {}", wish.priority, escape(role_label(&wish.role))))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// キュー中の一覧と、行ごとの強制離脱フォーム。
fn queue_table(view: &QueueView<'_>) -> String {
    if view.entries.is_empty() {
        return String::from("<p class=\"muted\">キュー中のプレイヤーはいません。</p>\n");
    }
    let mut body = String::new();
    for entry in view.entries {
        body.push_str(&format!(
            "<tr><td><a href=\"/admin/player?player={id}\">{name}</a>\
             <span class=\"fine-print\">{discord}</span></td>\
             <td class=\"num\">{wait}</td><td>{roles}</td>\
             <td class=\"num\">-{down_range}</td><td class=\"num\">{rating}</td>\
             <td><form method=\"post\" action=\"/admin/queue/remove\">\
             <input type=\"hidden\" name=\"player\" value=\"{id}\">\
             <input type=\"hidden\" name=\"key\" value=\"queue_removed:{id}:{key}\">\
             <label class=\"sr-only\" for=\"reason-{id}\">理由</label>\
             <input id=\"reason-{id}\" name=\"reason\" type=\"text\" maxlength=\"200\" placeholder=\"理由（任意・本人に表示）\">\
             <button class=\"revert\" type=\"submit\">キューから外す</button>\
             </form></td></tr>\n",
            id = escape(&entry.player_id.to_string()),
            name = escape(entry.trainer_name.as_deref().unwrap_or("(未設定)")),
            discord = entry.discord_user_id,
            wait = wait_label(view.now, entry.enqueued_at),
            roles = role_wishes(entry),
            down_range = entry.down_range,
            rating = format_rating(entry.rating),
            key = escape(view.form_key),
        ));
    }
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>トレーナー名</th><th>待ち時間</th><th>ロール希望</th>\
         <th>下方向</th><th>レート</th><th>操作</th></tr></thead>\n\
         <tbody>\n{body}</tbody>\n</table></div>\n"
    )
}

/// 表示スケールのレート。小数は落とす（一覧の読みやすさ優先）。
fn format_rating(rating: f64) -> String {
    format!("{:.0}", rating.round())
}

/// 運営操作の履歴。**誰がいつ何をしたか**をこの1つの表で読めるようにする。
fn actions_table(rows: &[AdminActionRow]) -> String {
    if rows.is_empty() {
        return String::from("<p class=\"muted\">運営操作の記録はまだありません。</p>\n");
    }
    let mut body = String::new();
    for row in rows {
        let target = match (row.target_player_id, row.target_trainer_name.as_deref()) {
            (Some(id), name) => format!(
                "<a href=\"/admin/player?player={}\">{}</a>",
                escape(&id.to_string()),
                escape(name.unwrap_or("(未設定)")),
            ),
            (None, _) => String::from("<span class=\"muted\">—</span>"),
        };
        let detail = match row.allowance {
            Some(allowance) => format!("{allowance} 枠"),
            None => String::new(),
        };
        // 知らない種別でも一覧は止めない（生の値をそのまま出す）
        let kind_label: &str = match row.kind {
            Some(kind) => kind.label(),
            None => row.raw_kind.as_str(),
        };
        body.push_str(&format!(
            "<tr><td class=\"num\">{id}</td><td>{at}</td><td>{kind}</td><td>{target}</td>\
             <td>{detail}</td><td>{reason}</td><td>discord:{actor}</td></tr>\n",
            id = row.id,
            at = display_time(row.occurred_at),
            kind = escape(kind_label),
            target = target,
            detail = escape(&detail),
            reason = escape(row.reason.as_deref().unwrap_or("")),
            actor = row.actor_discord_user_id,
        ));
    }
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>#</th><th>日時</th><th>操作</th><th>対象</th><th>内容</th>\
         <th>理由</th><th>who</th></tr></thead>\n<tbody>\n{body}</tbody>\n</table></div>\n"
    )
}

/// キューと当日の運用（`/admin/queue`）。
pub fn queue_page(view: &QueueView<'_>) -> String {
    // この画面は管理者しか到達しない（非管理者は 404）。ヘッダーも管理者のもの
    page(
        Viewer::ADMIN,
        "管理 / キューと運用",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-queue-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-queue-title\">キューと運用</h1>\n\
             {nav}\
             {notice}\
             {matchmaker}\
             <h2>キュー中（{count}人）</h2>\n\
             <p class=\"fine-print\">キューから外すと、本人のキュー画面に「運営によってキューから\
             外されました」と表示されます。理由を書けば、それも本人に表示されます。\
             進行中の試合には影響しません。</p>\n\
             {queue}\
             <h2>運営操作の履歴</h2>\n\
             {actions}\
             </section>\n",
            nav = admin_nav("/admin/queue"),
            notice = notice_html(view.notice.as_ref()),
            matchmaker = matchmaker_section(view.matchmaker),
            count = view.entries.len(),
            queue = queue_table(view),
            actions = actions_table(view.actions),
        ),
    )
}

// ----------------------------------------------------------------------
// シーズン境界（導出の起点を追記する）
// ----------------------------------------------------------------------

/// シーズン画面（`/admin/season`）に渡すデータ。
#[derive(Debug, Clone)]
pub struct SeasonView<'a> {
    /// 実行前の件数。すべて導出値。
    pub preview: &'a SeasonPreview,
    /// 境界に入れる時刻（＝この操作を行う時刻）。
    pub effective_at: DateTime<Utc>,
    /// 冪等キー（フォーム描画時にサーバ発行）。
    pub form_key: &'a str,
    /// 2 段目なら、1 段目で入力されたラベル。1 段目では `None`。
    pub confirming: Option<&'a str>,
    pub notice: Option<Notice>,
}

/// いま効いている境界の説明。まだ無ければ「全履歴が対象」。
fn current_season_line(preview: &SeasonPreview) -> String {
    match &preview.current {
        Some(boundary) => format!(
            "<p class=\"fine-print\">現在のシーズン: <strong>{label}</strong>（{at} 以降。\
             discord:{by} が開始）</p>\n",
            label = escape(&boundary.label),
            at = escape(&display_time(boundary.effective_at)),
            by = escape(boundary.created_by.trim_start_matches("discord:")),
        ),
        None => String::from(
            "<p class=\"fine-print\">シーズン境界はまだ 1 本もありません。\
             現在は<strong>全履歴</strong>が集計対象です。</p>\n",
        ),
    }
}

/// 実行前に数える 4 つの値。
fn season_counts(preview: &SeasonPreview) -> String {
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>項目</th><th>件数</th></tr></thead>\n<tbody>\n\
         <tr><td>進行中の formation（matched / awaiting_host / awaiting_code / in_progress）</td>\
         <td class=\"num\">{active}</td></tr>\n\
         <tr><td>未報告の試合</td><td class=\"num\">{pending}</td></tr>\n\
         <tr><td>集計から外れる試合</td><td class=\"num\">{leaving}</td></tr>\n\
         <tr><td>影響を受けるプレイヤー</td><td class=\"num\">{players}</td></tr>\n\
         </tbody>\n</table></div>\n",
        active = preview.active_formations,
        pending = preview.pending_reports,
        leaving = preview.matches_leaving,
        players = preview.players_affected,
    )
}

/// 進行中があるときの停止表示。**件数を出して止める。**
fn blocked_by_active(preview: &SeasonPreview) -> String {
    format!(
        "<p class=\"notice notice-error\">進行中の formation が <strong>{}</strong> 件あります。\
         終わるまで開始できません。（進行中のまま境界を入れると、その試合は集計から外れるのに\
         進行だけ続き、報告しても何にも反映されません）</p>\n",
        preview.active_formations
    )
}

/// シーズン画面。1 段目は警告と件数、2 段目はラベルの打ち直し。
pub fn season_page(view: &SeasonView<'_>) -> String {
    let body = if let Some(label) = view.confirming {
        // 2 段目: 同じラベルを手で入力させる
        format!(
            "<h2>確認</h2>\n\
             <p class=\"notice notice-error\"><strong>この操作は取り消せません。</strong>\
             追記専用なので、間違えた場合は「別の境界をもう 1 本追記して戻す」ことになります。</p>\n\
             <p>開始するシーズン: <strong>{label}</strong><br>\
             起点: <strong>{at}</strong>（この時刻より前に作られた試合と、この時刻より前に\
             追記された調整が集計から外れます）</p>\n\
             {counts}\
             <form method=\"post\" action=\"/admin/season\">\
             <input type=\"hidden\" name=\"label\" value=\"{label}\">\
             <input type=\"hidden\" name=\"key\" value=\"season:{key}\">\
             <label for=\"confirm_label\">確認のため、シーズン名をもう一度入力してください</label>\
             <input id=\"confirm_label\" name=\"confirm_label\" type=\"text\" autocomplete=\"off\" required>\
             <div class=\"actions\">\
             <button class=\"button button-primary\" type=\"submit\">新しいシーズンを開始する</button>\
             <a class=\"button button-secondary\" href=\"/admin/season\">やめる</a>\
             </div></form>\n",
            label = escape(label),
            at = escape(&display_time(view.effective_at)),
            counts = season_counts(view.preview),
            key = escape(view.form_key),
        )
    } else if view.preview.can_start() {
        // 1 段目: 警告と件数、ラベルの入力
        format!(
            "<p class=\"lead\">新しいシーズンを開始すると、<strong>ここより前の試合が集計から\
             外れます</strong>。レートだけが戻るのではありません。試合数・勝率・ランキング・\
             キューのゲートも、すべて新しい起点から数え直しになります。</p>\n\
             <p class=\"notice\"><strong>試合のログは 1 行も消えません。</strong>\
             formations / formation_players / formation_events / match_reports / \
             rating_adjustments はそのまま残り、集計に入らなくなるだけです。\
             境界そのものも追記されるので、後から完全に再構築できます。</p>\n\
             {current}\
             <h2>この操作で外れるもの</h2>\n\
             {counts}\
             <form method=\"post\" action=\"/admin/season/confirm\">\
             <input type=\"hidden\" name=\"key\" value=\"{key}\">\
             <label for=\"label\">新しいシーズンの名前（最大 {max} 文字）</label>\
             <input id=\"label\" name=\"label\" type=\"text\" maxlength=\"{max}\" autocomplete=\"off\" required>\
             <div class=\"actions\">\
             <button class=\"button button-primary\" type=\"submit\">確認へ進む</button>\
             </div></form>\n",
            current = current_season_line(view.preview),
            counts = season_counts(view.preview),
            key = escape(view.form_key),
            max = MAX_SEASON_LABEL_LENGTH,
        )
    } else {
        format!(
            "{blocked}{current}<h2>現在の状況</h2>\n{counts}",
            blocked = blocked_by_active(view.preview),
            current = current_season_line(view.preview),
            counts = season_counts(view.preview),
        )
    };

    // この画面は管理者しか到達しない（非管理者は 404）。ヘッダーも管理者のもの
    page(
        Viewer::ADMIN,
        "管理 / シーズン",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-season-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-season-title\">新しいシーズンを開始する</h1>\n\
             {nav}\
             {notice}\
             {body}\
             </section>\n",
            nav = admin_nav("/admin/season"),
            notice = notice_html(view.notice.as_ref()),
        ),
    )
}
#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::admin::actions::{AdminActionKind, DEFAULT_BLOCK_ALLOWANCE};
    use crate::admin::repository::QueuedRole;

    /// 停止していないマッチメイカ。既存のテストはこの状態を前提にしている
    fn running() -> MatchmakerView<'static> {
        MatchmakerView {
            paused: None,
            form_key: "form-key",
        }
    }

    fn player() -> FoundPlayer {
        FoundPlayer {
            player_id: Uuid::from_u128(7),
            discord_user_id: 900000000000000001,
            trainer_name: Some("ハル".to_owned()),
        }
    }

    fn adjustment(
        id: i64,
        delta_mu: f64,
        reverts_id: Option<i64>,
        reverted: bool,
    ) -> AdjustmentRow {
        AdjustmentRow {
            id,
            player_id: Uuid::from_u128(7),
            delta_mu,
            delta_sigma: 0.0,
            reason: "誤った初期レートの補正".to_owned(),
            created_by: "discord:900000000000000001".to_owned(),
            created_at: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap(),
            reverts_id,
            reverted,
            trainer_name: Some("ハル".to_owned()),
        }
    }

    #[test]
    fn the_search_page_lists_players_and_recent_logs() {
        let players = vec![player()];
        let recent = vec![
            adjustment(2, -10.0, Some(1), false),
            adjustment(1, 10.0, None, true),
        ];
        let rendered = search_page(&SearchView {
            query: "ハル",
            players: &players,
            recent: &recent,
            matchmaker: running(),
            notice: None,
        });
        assert!(rendered.starts_with("<!doctype html>"));
        assert!(rendered.contains("/admin/player?player="));
        assert!(rendered.contains("900000000000000001"));
        assert!(rendered.contains("<span class=\"tag\">打ち消し (#1)</span>"));
        assert!(rendered.contains("-10"));
    }

    #[test]
    fn the_player_page_carries_the_form_with_hidden_idempotency_key() {
        let rendered = player_page(&PlayerView {
            player: &player(),
            adjustments: &[],
            form_key: "0f6f0b7a-6e31-7a2c-9b93-0a3c6f9d2a11",
            block_allowance: DEFAULT_BLOCK_ALLOWANCE,
            notice: None,
        });
        assert!(rendered.contains("action=\"/admin/adjustments\""));
        assert!(rendered.contains("name=\"key\" value=\"0f6f0b7a-6e31-7a2c-9b93-0a3c6f9d2a11\""));
        // 理由は必須属性。サーバ側でも必須を検証する
        assert!(rendered.contains("id=\"reason\""));
        assert!(rendered.contains("required"));
        assert!(rendered.contains("調整はまだありません"));
    }

    #[test]
    fn only_unreverted_regular_rows_offer_a_revert_button() {
        let rows = vec![
            adjustment(1, 10.0, None, false),     // 打ち消せる
            adjustment(2, 10.0, None, true),      // 済
            adjustment(3, -10.0, Some(1), false), // 打ち消し行自体
        ];
        let rendered = player_page(&PlayerView {
            player: &player(),
            adjustments: &rows,
            form_key: "k",
            block_allowance: DEFAULT_BLOCK_ALLOWANCE,
            notice: None,
        });
        assert_eq!(rendered.matches("action=\"/admin/revert\"").count(), 1);
        assert!(rendered.contains("name=\"adjustment_id\" value=\"1\""));
        assert!(rendered.contains("<span class=\"tag\">打ち消済み</span>"));
        assert!(rendered.contains("<span class=\"tag\">打ち消し (#1)</span>"));
    }

    #[test]
    fn user_supplied_text_is_escaped() {
        let mut player = player();
        player.trainer_name = Some("<script>alert(1)</script>".to_owned());
        let rendered = player_page(&PlayerView {
            player: &player,
            adjustments: &[],
            form_key: "k",
            block_allowance: DEFAULT_BLOCK_ALLOWANCE,
            notice: Some(Notice {
                kind: NoticeKind::Error,
                message: "<b>not html</b>".to_owned(),
            }),
        });
        assert!(!rendered.contains("<script>alert"));
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(!rendered.contains("<b>not html</b>"));
    }

    #[test]
    fn the_panel_keeps_the_shared_dark_theme_and_mobile_layout() {
        let rendered = search_page(&SearchView {
            query: "",
            players: &[],
            recent: &[],
            matchmaker: running(),
            notice: None,
        });
        assert!(rendered.contains("color-scheme: dark"));
        assert!(rendered.contains("@media (max-width: 480px)"));
        assert!(rendered.contains("overflow-x: auto"));
    }

    fn paused_state() -> PauseState {
        PauseState {
            actor_discord_user_id: 900000000000000001,
            since: Utc.with_ymd_and_hms(2026, 9, 12, 12, 30, 0).unwrap(),
            reason: Some("VC が落ちた".to_owned()),
        }
    }

    fn queued(n: u128, minutes_ago: i64) -> QueuedEntry {
        QueuedEntry {
            player_id: Uuid::from_u128(n),
            discord_user_id: 1000 + n as i64,
            trainer_name: Some(format!("トレーナー{n}")),
            enqueued_at: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap()
                - TimeDelta::minutes(minutes_ago),
            down_range: 95,
            rating: 1511.0,
            roles: vec![
                QueuedRole {
                    role: "jungle".to_owned(),
                    priority: 1,
                },
                QueuedRole {
                    role: "bot_tank".to_owned(),
                    priority: 2,
                },
            ],
        }
    }

    fn action(id: i64, kind: AdminActionKind, allowance: Option<i32>) -> AdminActionRow {
        AdminActionRow {
            id,
            kind: Some(kind),
            raw_kind: kind.as_str().to_owned(),
            actor_discord_user_id: 900000000000000001,
            target_player_id: allowance.map(|_| Uuid::from_u128(7)),
            target_trainer_name: allowance.map(|_| "ハル".to_owned()),
            reason: Some("荒らし対応".to_owned()),
            allowance,
            occurred_at: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap(),
        }
    }

    fn queue_view<'a>(
        entries: &'a [QueuedEntry],
        actions: &'a [AdminActionRow],
        paused: Option<&'a PauseState>,
    ) -> QueueView<'a> {
        QueueView {
            matchmaker: MatchmakerView {
                paused,
                form_key: "form-key",
            },
            entries,
            actions,
            form_key: "form-key",
            now: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap(),
            notice: None,
        }
    }

    #[test]
    fn a_running_matchmaker_offers_pause_and_a_paused_one_offers_resume() {
        let running_page = search_page(&SearchView {
            query: "",
            players: &[],
            recent: &[],
            matchmaker: running(),
            notice: None,
        });
        assert!(running_page.contains("<span class=\"state state-running\">稼働中</span>"));
        assert!(running_page.contains("name=\"action\" value=\"pause\""));
        assert!(!running_page.contains("value=\"resume\""));

        let pause = paused_state();
        let paused_page = search_page(&SearchView {
            query: "",
            players: &[],
            recent: &[],
            matchmaker: MatchmakerView {
                paused: Some(&pause),
                form_key: "form-key",
            },
            notice: None,
        });
        assert!(paused_page.contains("<span class=\"state state-paused\">停止中</span>"));
        assert!(paused_page.contains("name=\"action\" value=\"resume\""));
        assert!(!paused_page.contains("value=\"pause\""));
        // 誰がいつ止めたかを画面に出す（止めたまま忘れないための情報）
        assert!(paused_page.contains("discord:900000000000000001"));
        assert!(paused_page.contains("2026-09-12 21:30"), "JST で出す");
        assert!(paused_page.contains("VC が落ちた"));
    }

    #[test]
    fn the_queue_page_lists_the_five_columns_and_a_removal_form_per_row() {
        let entries = vec![queued(11, 73), queued(12, 4)];
        let rendered = queue_page(&queue_view(&entries, &[], None));
        assert!(rendered.starts_with("<!doctype html>"));
        assert!(rendered.contains("キュー中（2人）"));
        // トレーナー名 / 待ち時間 / ロール希望 / 下方向の範囲 / レート
        assert!(rendered.contains("トレーナー11"));
        assert!(rendered.contains("1時間13分"));
        assert!(rendered.contains("4分"));
        assert!(rendered.contains("1. 中央 / 2. タンク"), "表示名で出す");
        assert!(!rendered.contains("jungle"), "内部識別子を画面に出さない");
        assert!(rendered.contains(">-95</td>"));
        assert!(rendered.contains(">1511</td>"));
        assert_eq!(
            rendered.matches("action=\"/admin/queue/remove\"").count(),
            2,
            "行ごとに強制離脱フォームが出る"
        );
        // 理由は任意。本人に見せると画面にも書いてある
        assert!(rendered.contains("placeholder=\"理由（任意・本人に表示）\""));
        assert!(rendered.contains("「運営によってキューから外されました」"));
    }

    #[test]
    fn an_empty_queue_says_so_instead_of_rendering_an_empty_table() {
        let rendered = queue_page(&queue_view(&[], &[], None));
        assert!(rendered.contains("キュー中のプレイヤーはいません"));
        assert!(!rendered.contains("action=\"/admin/queue/remove\""));
        assert!(rendered.contains("運営操作の記録はまだありません"));
    }

    #[test]
    fn the_action_log_names_who_did_what() {
        let actions = vec![
            action(3, AdminActionKind::BlockAllowanceSet, Some(4)),
            action(2, AdminActionKind::MatchmakerPaused, None),
        ];
        let rendered = queue_page(&queue_view(&[], &actions, None));
        assert!(rendered.contains("ブロック枠の変更"));
        assert!(rendered.contains("4 枠"));
        assert!(rendered.contains("マッチメイカ停止"));
        assert!(rendered.contains("discord:900000000000000001"));
        assert!(rendered.contains("荒らし対応"));
    }

    #[test]
    fn an_unknown_action_kind_still_renders_as_a_row() {
        // 読み手は知らない種別で止まらない（追記ログは消せない）
        let mut row = action(9, AdminActionKind::QueueRemoved, None);
        row.kind = None;
        row.raw_kind = "matchmaker_exploded".to_owned();
        let rendered = queue_page(&queue_view(&[], &[row], None));
        assert!(rendered.contains("matchmaker_exploded"));
    }

    #[test]
    fn the_player_page_shows_the_block_allowance_and_its_form() {
        let rendered = player_page(&PlayerView {
            player: &player(),
            adjustments: &[],
            form_key: "k",
            block_allowance: DEFAULT_BLOCK_ALLOWANCE,
            notice: None,
        });
        assert!(rendered.contains("<dt>ブロック枠</dt><dd>2 枠</dd>"));
        assert!(rendered.contains("action=\"/admin/blocks\""));
        assert!(rendered.contains("name=\"key\" value=\"blocks:k\""));
        assert!(rendered.contains(&format!("max=\"{MAX_BLOCK_ALLOWANCE}\"")));
        // 調整フォームの冪等キーと衝突しない
        assert!(rendered.contains("name=\"key\" value=\"k\""));
    }

    #[test]
    fn the_queue_page_escapes_user_supplied_text() {
        let mut entry = queued(11, 1);
        entry.trainer_name = Some("<script>alert(1)</script>".to_owned());
        let mut row = action(1, AdminActionKind::QueueRemoved, None);
        row.reason = Some("<b>not html</b>".to_owned());
        let rendered = queue_page(&queue_view(&[entry], &[row], None));
        assert!(!rendered.contains("<script>alert"));
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(!rendered.contains("<b>not html</b>"));
    }

    #[test]
    fn the_ops_pages_keep_the_shared_dark_theme_and_mobile_layout() {
        let entries = vec![queued(11, 1)];
        let rendered = queue_page(&queue_view(&entries, &[], None));
        assert!(rendered.contains("color-scheme: dark"));
        assert!(rendered.contains("@media (max-width: 480px)"));
        assert!(
            rendered.contains("overflow-x: auto"),
            "表は横スクロールする"
        );
    }

    /// レビュー用に全画面を吐く。`cargo test -- --ignored dump_every_admin_page` で使う。
    #[test]
    #[ignore = "レビュー用の出力。通常のテスト実行では回さない"]
    fn dump_every_admin_page_for_review() {
        let pause = paused_state();
        let entries = vec![queued(11, 73), queued(12, 4)];
        let actions = vec![
            action(3, AdminActionKind::BlockAllowanceSet, Some(4)),
            action(2, AdminActionKind::MatchmakerPaused, None),
        ];
        for (name, html) in [
            (
                "admin-queue-running",
                queue_page(&queue_view(&entries, &actions, None)),
            ),
            (
                "admin-queue-paused",
                queue_page(&queue_view(&entries, &actions, Some(&pause))),
            ),
            (
                "admin-player",
                player_page(&PlayerView {
                    player: &player(),
                    adjustments: &[],
                    form_key: "k",
                    block_allowance: DEFAULT_BLOCK_ALLOWANCE,
                    notice: None,
                }),
            ),
        ] {
            println!("===== {name}\n{html}");
        }
    }
}
