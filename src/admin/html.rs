//! 管理パネルの HTML。外枠は登録画面（[`crate::registration::html::page`]）をそのまま使う。
//!
//! 表示する値はトレーナー名・Discord ID・数値・日時だけ。いずれも [`escape`] を通す。
//! 打ち消しは行ごとの POST フォーム（`/admin/revert`）で、ボタンは**未打ち消しの
//! 通常行にだけ**出す（権威は DB の unique index とトリガー。ここは表示の面倒だけ）。

use chrono::{DateTime, Utc};

use crate::blocks::{CurrentBlock, IncomingBlock, ReceivedBlockCount};
use crate::queue::html::role_label;
use crate::queue::{MAX_RANGE, MIN_RANGE};
use crate::registration::Viewer;
use crate::registration::html::{escape, page};
use crate::status::html::{display_time, format_delta};

use super::actions::{AdminActionRow, MAX_BLOCK_ALLOWANCE, PauseState};
use super::disputes::DisputeDecision;
use super::matching_range::MatchingRangePreview;
use super::rating_params::{MAX_BASE_DELTA, RatingParamsPreview};
use super::repository::RosterPage;
use super::repository::{
    AdjustmentRow, AwaitingReports, DisputedFormation, DisputedVote, FinishedFormation,
    FoundPlayer, QueuedEntry,
};
use super::schedule::MatchmakerSchedule;
use super::season::{MAX_SEASON_LABEL_LENGTH, SeasonPreview};
use crate::nullified::{NullifiedReportRow, NullifiedTally};
use crate::quiet::{QuietReportRow, QuietReporterTally};

/// 検索画面に渡すデータ。
#[derive(Debug, Clone)]
pub struct SearchView<'a> {
    /// 検索ボックスに戻す文字列。
    pub query: &'a str,
    /// 検索結果（クエリが空なら空）。
    pub players: &'a [FoundPlayer],
    /// 直近の調整履歴（新しい順）。
    pub recent: &'a [AdjustmentRow],
    /// 現在受信しているブロック数。件数の多い順で、運営だけに表示する。
    pub received_blocks: &'a [ReceivedBlockCount],
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
    /// 現在のブロック枠（`admin_actions` の最新行からの導出値。既定は 1）。
    pub block_allowance: i32,
    pub outgoing_blocks: &'a [CurrentBlock],
    pub incoming_blocks: &'a [IncomingBlock],
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
.tag-danger { border-color: var(--danger); color: var(--danger); }
/* 登録者一覧: 名前の下に Discord 表示名を小さく重ねる */
td.who { overflow-wrap: anywhere; }
td.who .trainer { display: block; color: var(--muted); font-size: 0.82rem; }
td.who .tag { margin-left: 6px; }
.pager { display: flex; gap: 12px; align-items: center; justify-content: space-between; margin-top: 20px; }
.pager .position { color: var(--muted); font-size: 0.85rem; }
form.search { margin: 0 0 20px; }
/* 稼働スケジュール: チェックボックスは共通の input 指定（width:100% /
   min-height:48px）に引っ張られて巨大な箱になるので、そこだけ打ち消す。
   label も共通で display:block なので、行に並べるために inline-flex に戻す */
.days { display: flex; flex-wrap: wrap; gap: 6px 14px; margin: 0 0 6px; }
label.day { display: inline-flex; align-items: center; gap: 7px; margin: 0 0 8px;
            font-weight: 650; white-space: nowrap; }
label.day input[type="checkbox"] { flex: 0 0 auto; width: 18px; min-height: 0; height: 18px;
                                   margin: 0; padding: 0; accent-color: var(--accent); }
.ops .field-label { display: block; margin-bottom: 7px; font-weight: 700; }
.ops input[type="time"] { width: auto; min-width: 9rem; }
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
    let received_blocks = received_block_counts_table(view.received_blocks);

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
             <h2>受信ブロック件数</h2>\n\
             <p class=\"fine-print\">現在有効な味方ブロックのみを、件数の多い順に表示します。</p>\n\
             {received_blocks}\
             <h2>直近の調整履歴</h2>\n\
             {recent}\
             </section>\n",
            nav = admin_nav("/admin"),
            notice = notice_html(view.notice.as_ref()),
            matchmaker = matchmaker_section(view.matchmaker),
            q = escape(view.query),
            found = found,
            received_blocks = received_blocks,
            recent = adjustments_table(view.recent, true),
        ),
    )
}

fn received_block_counts_table(rows: &[ReceivedBlockCount]) -> String {
    if rows.is_empty() {
        return String::from("<p class=\"muted\">現在有効な受信ブロックはありません。</p>\n");
    }
    let body = rows
        .iter()
        .map(|row| {
            let name = row
                .display_name
                .as_deref()
                .or(row.trainer_name.as_deref())
                .unwrap_or("(未設定)");
            format!(
                "<tr><td><a href=\"/admin/player?player={id}\">{name}</a></td>\
                 <td class=\"num\">{count}</td></tr>\n",
                id = escape(&row.player_id.to_string()),
                name = escape(name),
                count = row.count,
            )
        })
        .collect::<String>();
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>プレイヤー</th><th>受信件数</th></tr></thead>\n\
         <tbody>\n{body}</tbody>\n</table></div>\n"
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
             <p class=\"fine-print\">既定は 1 枠です。変更は追記として記録され、\
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
             {blocks}\
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
            blocks = block_section(view),
        ),
    )
}

fn block_section(view: &PlayerView<'_>) -> String {
    let outgoing = if view.outgoing_blocks.is_empty() {
        "<p class=\"fine-print\">現在ブロックしているプレイヤーはいません。</p>".to_owned()
    } else {
        let rows = view
            .outgoing_blocks
            .iter()
            .map(|block| {
                let name = block
                    .display_name
                    .as_deref()
                    .or(block.trainer_name.as_deref())
                    .unwrap_or("表示名を取得できませんでした");
                format!(
                    "<li><strong>{name}</strong> <span class=\"fine-print\">登録: {blocked_at}</span>\
                     <form class=\"inline\" method=\"post\" action=\"/admin/blocks/override\">\
                     <input type=\"hidden\" name=\"player\" value=\"{player_id}\">\
                     <input type=\"hidden\" name=\"target\" value=\"{target}\">\
                     <input type=\"hidden\" name=\"key\" value=\"block-override:{form_key}:{target}\">\
                     <label class=\"sr-only\" for=\"override-reason-{target}\">理由</label>\
                     <input id=\"override-reason-{target}\" name=\"reason\" type=\"text\" maxlength=\"200\" placeholder=\"理由（任意）\">\
                     <button class=\"revert\" type=\"submit\">管理解除</button></form></li>",
                    name = escape(name),
                    blocked_at = escape(&display_time(block.blocked_at)),
                    player_id = escape(&view.player.player_id.to_string()),
                    target = escape(&block.blocked_player_id.to_string()),
                    form_key = escape(view.form_key),
                )
            })
            .collect::<String>();
        format!("<ul class=\"block-list\">{rows}</ul>")
    };
    let incoming = if view.incoming_blocks.is_empty() {
        "<p class=\"fine-print\">このプレイヤーをブロックしているプレイヤーはいません。</p>"
            .to_owned()
    } else {
        let rows = view
            .incoming_blocks
            .iter()
            .map(|block| {
                let name = block
                    .display_name
                    .as_deref()
                    .or(block.trainer_name.as_deref())
                    .unwrap_or("表示名を取得できませんでした");
                format!(
                    "<li><strong>{}</strong> <span class=\"fine-print\">登録: {}</span></li>",
                    escape(name),
                    escape(&display_time(block.blocked_at)),
                )
            })
            .collect::<String>();
        format!(
            "<p class=\"fine-print\">ブロックされている件数: {}</p><ul class=\"block-list\">{rows}</ul>",
            view.incoming_blocks.len()
        )
    };
    format!(
        "<section class=\"ops\" aria-labelledby=\"block-relationships-title\">\
         <h2 id=\"block-relationships-title\">味方ブロック</h2>\
         <h3>このプレイヤーがブロックしている相手</h3>{outgoing}\
         <h3>このプレイヤーをブロックしている相手</h3>{incoming}\
         </section>"
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
/// 稼働スケジュールの現在値と、いま枠の内側かどうか。
pub struct ScheduleView<'a> {
    pub schedule: &'a MatchmakerSchedule,
    /// いま稼働時間内か。**手動停止とは別**（画面で取り違えさせない）。
    pub open: bool,
    pub form_key: &'a str,
}

pub struct QueueView<'a> {
    /// 同じ画面からマッチメイカも止められるようにする（事故対応で画面を跨がせない）。
    pub matchmaker: MatchmakerView<'a>,
    /// 稼働スケジュール。手動の停止と同じ画面に置く。
    pub schedule: ScheduleView<'a>,
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
        "<nav class=\"admin-nav\" aria-label=\"管理メニュー\">{} / {} / {} / {} / {} / {} / {} / {}</nav>\n",
        link("/admin", "レート調整"),
        link("/admin/players", "登録者一覧"),
        link("/admin/queue", "キューと運用"),
        link("/admin/reports", "報告の入力"),
        link("/admin/season", "シーズン"),
        link("/admin/rating-params", "レート増減幅"),
        link("/admin/matching-range", "マッチング範囲"),
        link("/admin/quiet-reports", "聞き専報告"),
    )
}

/// マッチメイカの稼働スケジュール。**手動の停止が常に勝つ**ことを画面で言い切る。
///
/// 「21 時になったら勝手に再開する」と思われると、障害対応中に緊急停止した人が
/// 画面から離れられなくなる。効き方をそのまま書く。
fn schedule_section(view: &ScheduleView<'_>) -> String {
    const DAYS: [(&str, &str); 7] = [
        ("mon", "月"),
        ("tue", "火"),
        ("wed", "水"),
        ("thu", "木"),
        ("fri", "金"),
        ("sat", "土"),
        ("sun", "日"),
    ];
    let schedule = view.schedule;
    let checkboxes = DAYS
        .iter()
        .enumerate()
        .map(|(index, (key, label))| {
            let checked = if schedule.days_mask & (1 << index) != 0 {
                " checked"
            } else {
                ""
            };
            format!(
                "<label class=\"day\"><input type=\"checkbox\" name=\"day_{key}\" \
                 value=\"on\"{checked}> {label}</label>"
            )
        })
        .collect::<String>();

    // いまの状態を 1 行で。**手動とスケジュールを取り違えさせない**
    let state = if schedule.enabled {
        let days = schedule.day_labels().join("");
        let wraps = if schedule.crosses_midnight() {
            "（翌日）"
        } else {
            ""
        };
        format!(
            "<p class=\"fine-print\">現在の枠: <strong>{days}</strong> の \
             <strong>{start}</strong> 〜 <strong>{wraps}{end}</strong>（JST）、\
             キュー受付は <strong>{queue_open}</strong> から。\
             いまは<strong>{open}</strong>です。</p>\n",
            days = escape(&if days.is_empty() {
                String::from("（曜日なし）")
            } else {
                days
            }),
            start = escape(&schedule.start_text()),
            end = escape(&schedule.end_text()),
            queue_open = escape(&schedule.queue_open_text()),
            open = if view.open {
                "稼働時間内"
            } else {
                "稼働時間外"
            },
        )
    } else {
        String::from(
            "<p class=\"fine-print\">スケジュールは<strong>使っていません</strong>。\
             手動の停止・再開だけが効きます。</p>\n",
        )
    };

    format!(
        "<section class=\"ops\" aria-labelledby=\"schedule-title\">\n\
         <h2 id=\"schedule-title\">稼働スケジュール</h2>\n\
         {state}\
         <p class=\"fine-print\"><strong>手動の停止が常に勝ちます。</strong>\
         手で止めているあいだは、稼働時間になっても再開しません\
         （障害対応中に勝手に動き出さないため）。手動の再開は「手動停止の解除」で、\
         稼働時間外ならそのまま止まったままです。</p>\n\
         <form method=\"post\" action=\"/admin/matchmaker/schedule\">\
         <input type=\"hidden\" name=\"key\" value=\"schedule:{key}\">\
         <div class=\"field\"><label class=\"day\">\
         <input type=\"checkbox\" name=\"enabled\" value=\"on\"{enabled}> \
         スケジュールを使う</label></div>\
         <div class=\"field\"><span class=\"field-label\">稼働する曜日</span>\
         <div class=\"days\">{checkboxes}</div></div>\
         <div class=\"field\"><label for=\"start\">開始（JST）</label>\
         <input id=\"start\" name=\"start\" type=\"time\" value=\"{start}\" required></div>\
         <div class=\"field\"><label for=\"end\">終了（JST。開始より早ければ翌日）</label>\
         <input id=\"end\" name=\"end\" type=\"time\" value=\"{end}\" required></div>\
         <div class=\"field\"><label for=\"queue-open\">キュー受付開始（JST。開始と同じか、その前 12 時間以内）</label>\
         <input id=\"queue-open\" name=\"queue_open\" type=\"time\" value=\"{queue_open}\" required>\
         <p class=\"fine-print\">受付は稼働の終了と同時に閉じます。稼働時間外でも、\
         受付が開いていればキューには入れます（組まれるのは稼働開始から）。</p></div>\
         <label for=\"schedule-reason\">理由（記録に残ります）</label>\
         <input id=\"schedule-reason\" name=\"reason\" type=\"text\" autocomplete=\"off\" required>\
         <div class=\"actions\">\
         <button class=\"button button-primary\" type=\"submit\">スケジュールを保存</button>\
         </div></form>\n</section>\n",
        key = escape(view.form_key),
        enabled = if schedule.enabled { " checked" } else { "" },
        start = escape(&schedule.start_text()),
        end = escape(&schedule.end_text()),
        queue_open = escape(&schedule.queue_open_text()),
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
             {schedule}\
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
            schedule = schedule_section(&view.schedule),
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
         <tr><td>係争中の試合</td><td class=\"num\">{disputed}</td></tr>\n\
         <tr><td>集計から外れる試合</td><td class=\"num\">{leaving}</td></tr>\n\
         <tr><td>影響を受けるプレイヤー</td><td class=\"num\">{players}</td></tr>\n\
         </tbody>\n</table></div>\n",
        active = preview.active_formations,
        pending = preview.pending_reports,
        disputed = preview.disputed,
        leaving = preview.matches_leaving,
        players = preview.players_affected,
    )
}

/// **係争中のまま境界を引くと、裁定しても新シーズンに入らない。**
///
/// 境界の判定は `formations.created_at` なので、境界より前に作られた試合は
/// 境界より後に確定させても集計に入らない。裁定した運営には結果が見えないため、
/// 件数だけでなく**何が起きるか**を書く。
fn season_disputed_warning(preview: &SeasonPreview) -> String {
    if preview.disputed == 0 {
        return String::new();
    }
    format!(
        "<p class=\"notice notice-error\">\
         <strong>係争中の試合が {disputed} 件あります。先に裁定してください。</strong>\
         これらの試合は、境界を引いた後に裁定して確定させても\
         <strong>新しいシーズンのレートには反映されません</strong>。\
         集計に入るかどうかは試合が<strong>作られた</strong>時刻で決まるためです\
         （確定した時刻ではありません）。裁定した運営の画面にも「反映されなかった」ことは\
         出ないので、境界を引く前に係争をゼロにしてください。</p>\n",
        disputed = preview.disputed,
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
             {disputed}{counts}\
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
            disputed = season_disputed_warning(view.preview),
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
             {disputed}{counts}\
             <form method=\"post\" action=\"/admin/season/confirm\">\
             <input type=\"hidden\" name=\"key\" value=\"{key}\">\
             <label for=\"label\">新しいシーズンの名前（最大 {max} 文字）</label>\
             <input id=\"label\" name=\"label\" type=\"text\" maxlength=\"{max}\" autocomplete=\"off\" required>\
             <div class=\"actions\">\
             <button class=\"button button-primary\" type=\"submit\">確認へ進む</button>\
             </div></form>\n",
            disputed = season_disputed_warning(view.preview),
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

// ----------------------------------------------------------------------
// レート増減幅
// ----------------------------------------------------------------------
//
// **この操作は過去の全試合を引き直す。**docs/02 の不変条件 3（常に現行 params を
// 全履歴に適用する）そのもので、意図した挙動である。だからこそ画面で必ず言い切る。

pub struct RatingParamsView<'a> {
    /// 実行前の件数と履歴。すべて導出値。
    pub preview: &'a RatingParamsPreview,
    /// 冪等キー（フォーム描画時にサーバ発行）。
    pub form_key: &'a str,
    /// 2 段目なら、1 段目で入力された値。1 段目では `None`。
    pub confirming: Option<f64>,
    pub notice: Option<Notice>,
}

/// 増減幅を人が読める形にする。整数なら小数点を出さない。
fn delta_text(value: f64) -> String {
    if (value - value.round()).abs() < f64::EPSILON {
        format!("{}", value.round() as i64)
    } else {
        format!("{value}")
    }
}

/// 引き直しの規模。**実行前に必ず出す。**
fn rating_params_counts(preview: &RatingParamsPreview) -> String {
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>項目</th><th>件数</th></tr></thead>\n<tbody>\n\
         <tr><td><strong>引き直される試合</strong></td><td class=\"num\">{matches}</td></tr>\n\
         <tr><td><strong>レートが動くプレイヤー</strong></td><td class=\"num\">{players}</td></tr>\n\
         <tr><td>進行中の formation（1 件でもあれば実行できません）</td>\
         <td class=\"num\">{active}</td></tr>\n\
         <tr><td>うち未報告の試合</td><td class=\"num\">{pending}</td></tr>\n\
         <tr><td>係争中の試合（裁定時に新しい値で計算されます）</td>\
         <td class=\"num\">{disputed}</td></tr>\n\
         </tbody>\n</table></div>\n",
        matches = preview.matches_affected,
        players = preview.players_affected,
        active = preview.active_formations,
        pending = preview.pending_reports,
        disputed = preview.disputed,
    )
}

/// 進行中の formation があるので実行できない、という説明。
fn rating_params_blocked(preview: &RatingParamsPreview) -> String {
    format!(
        "<p class=\"notice notice-error\"><strong>進行中の試合が {active} 件あるため、\
         いまは増減幅を変更できません。</strong>増減幅は 1 行積んだ瞬間に全員のレートを\
         変えるので、進行中の試合があると、その試合が始まったときのレートと確定時の計算が\
         食い違って見えます。キュー中の人のマッチング範囲（±50）も同時に動きます。\
         終わってからやり直してください。</p>\n",
        active = preview.active_formations,
    )
}

/// 係争中の試合があるときの警告。**拒否はしない。**
fn rating_params_disputed_warning(preview: &RatingParamsPreview) -> String {
    if preview.disputed == 0 {
        return String::new();
    }
    format!(
        "<p class=\"notice\">係争中の試合が {disputed} 件あります。\
         裁定した時点で<strong>新しい</strong>増減幅が適用されるので、いま変えても\
         「揉めていた試合だけ別の幅で計算される」ことはありません\
         （現在のシーズンに入っている試合に限ります。シーズン境界より前に作られた試合は、\
         裁定してもそもそも集計に入りません）。\
         ただし裁定の結果が変更後の値になることは把握しておいてください。</p>\n",
        disputed = preview.disputed,
    )
}

/// いま効いている値。
fn current_rating_params_line(preview: &RatingParamsPreview) -> String {
    format!(
        "<p class=\"notice\">現在の増減幅: <strong>勝ち +{delta} / 負け −{delta}</strong>\
         （params v{version}）</p>\n",
        delta = escape(&delta_text(preview.current_base_delta)),
        version = preview.current_params_version,
    )
}

/// 変更履歴。追記専用なので、過去の行はそのまま残る。
fn rating_params_history(preview: &RatingParamsPreview) -> String {
    if preview.history.is_empty() {
        return "<p class=\"muted\">まだ一度も変更していません（既定値のまま）。</p>\n".to_owned();
    }
    let rows = preview
        .history
        .iter()
        .map(|change| {
            format!(
                "<tr><td>{at}</td><td class=\"num\">v{version}</td>\
                 <td class=\"num\">±{delta}</td><td>{reason}</td><td>{by}</td></tr>\n",
                at = escape(&display_time(change.created_at)),
                version = change.params_version,
                delta = escape(&delta_text(change.base_delta)),
                reason = escape(&change.reason),
                by = escape(&change.created_by),
            )
        })
        .collect::<String>();
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>時刻</th><th>版</th><th>増減幅</th><th>理由</th><th>操作者</th></tr></thead>\n\
         <tbody>\n{rows}</tbody>\n</table></div>\n"
    )
}

/// **引き直しの警告。**1 段目と 2 段目の両方で出す。
const RECOMPUTE_WARNING: &str = "<p class=\"notice notice-error\">\
     <strong>変更すると過去の全試合のレートが引き直されます。</strong>\
     新しい値が過去にさかのぼって適用されるため、全プレイヤーの現在レートとランキングが\
     その場で変わります。「これから行う試合にだけ効く」設定ではありません。</p>\n";

pub fn rating_params_page(view: &RatingParamsView<'_>) -> String {
    let body = if let Some(base_delta) = view.confirming {
        // 2 段目: 同じ数値を手で入力させる
        format!(
            "<h2>確認</h2>\n\
             {warning}\
             <p class=\"notice notice-error\"><strong>この操作は取り消せません。</strong>\
             追記専用なので、間違えた場合は「元の値をもう 1 行追記して戻す」ことになります\
             （そのときも全履歴が引き直されます）。</p>\n\
             <p>新しい増減幅: <strong>勝ち +{delta} / 負け −{delta}</strong>\
             （現在は ±{current}）</p>\n\
             {counts}\
             <form method=\"post\" action=\"/admin/rating-params\">\
             <input type=\"hidden\" name=\"base_delta\" value=\"{delta}\">\
             <input type=\"hidden\" name=\"key\" value=\"rating-params:{key}\">\
             <label for=\"reason\">理由（記録に残ります）</label>\
             <input id=\"reason\" name=\"reason\" type=\"text\" autocomplete=\"off\" required>\
             <label for=\"confirm_delta\">確認のため、同じ数値をもう一度入力してください</label>\
             <input id=\"confirm_delta\" name=\"confirm_delta\" type=\"text\" autocomplete=\"off\" required>\
             <div class=\"actions\">\
             <button class=\"button button-primary\" type=\"submit\">増減幅を変更して全履歴を引き直す</button>\
             <a class=\"button button-secondary\" href=\"/admin/rating-params\">やめる</a>\
             </div></form>\n",
            warning = RECOMPUTE_WARNING,
            delta = escape(&delta_text(base_delta)),
            current = escape(&delta_text(view.preview.current_base_delta)),
            counts = rating_params_counts(view.preview),
            key = escape(view.form_key),
        )
    } else if !view.preview.can_change() {
        // 進行中の試合があるので、そもそもフォームを出さない
        format!(
            "{blocked}{disputed}{current}<h2>現在の状況</h2>\n{counts}\
             <h2>変更履歴</h2>\n{history}",
            blocked = rating_params_blocked(view.preview),
            disputed = rating_params_disputed_warning(view.preview),
            current = current_rating_params_line(view.preview),
            counts = rating_params_counts(view.preview),
            history = rating_params_history(view.preview),
        )
    } else {
        // 1 段目: 警告と件数、値の入力
        format!(
            "<p class=\"lead\">1 試合あたりのレート増減幅です。勝ちが +この値、負けが −この値、\
             引き分けは 0 になります（相手との差は増減に影響しません）。</p>\n\
             {disputed}{warning}\
             <p class=\"notice\"><strong>試合のログは 1 行も消えません。</strong>\
             レートは導出値で、増減幅も追記ログの最新行から導出されます。\
             値を変えると、同じ試合ログを新しい増減幅で計算し直すだけです。</p>\n\
             {current}\
             <h2>この操作で引き直されるもの</h2>\n\
             {counts}\
             <form method=\"post\" action=\"/admin/rating-params/confirm\">\
             <input type=\"hidden\" name=\"key\" value=\"{key}\">\
             <label for=\"base_delta\">新しい増減幅（0 より大きく {max} 以下）</label>\
             <input id=\"base_delta\" name=\"base_delta\" type=\"text\" inputmode=\"decimal\" autocomplete=\"off\" required>\
             <div class=\"actions\">\
             <button class=\"button button-primary\" type=\"submit\">確認へ進む</button>\
             </div></form>\n\
             <h2>変更履歴</h2>\n{history}",
            warning = RECOMPUTE_WARNING,
            disputed = rating_params_disputed_warning(view.preview),
            current = current_rating_params_line(view.preview),
            counts = rating_params_counts(view.preview),
            key = escape(view.form_key),
            max = delta_text(MAX_BASE_DELTA),
            history = rating_params_history(view.preview),
        )
    };

    // この画面は管理者しか到達しない（非管理者は 404）
    page(
        Viewer::ADMIN,
        "管理 / レート増減幅",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-rating-params-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-rating-params-title\">レートの増減幅を変える</h1>\n\
             {nav}\
             {notice}\
             {body}\
             </section>\n",
            nav = admin_nav("/admin/rating-params"),
            notice = notice_html(view.notice.as_ref()),
        ),
    )
}

// ----------------------------------------------------------------------
// マッチング範囲
// ----------------------------------------------------------------------
//
// **この操作は過去を 1 件も変えない。**増減幅（上）と対になる画面なので、
// そこだけは取り違えないように画面でも言い切る。効くのは次のスキャン以降の選抜だけ。

pub struct MatchingRangeView<'a> {
    /// 現在値・キュー人数・履歴。すべて導出値。
    pub preview: &'a MatchingRangePreview,
    /// 冪等キー（フォーム描画時にサーバ発行）。
    pub form_key: &'a str,
    pub notice: Option<Notice>,
}

fn matching_range_history(preview: &MatchingRangePreview) -> String {
    if preview.history.is_empty() {
        return "<p class=\"muted\">まだ一度も変更していません（既定値のまま）。</p>\n".to_owned();
    }
    let rows = preview
        .history
        .iter()
        .map(|change| {
            format!(
                "<tr><td>{at}</td><td class=\"num\">+{up}</td><td class=\"num\">-{down}</td>\
                 <td>{reason}</td><td>{by}</td></tr>\n",
                at = escape(&display_time(change.created_at)),
                up = change.up_range,
                down = change.down_range,
                reason = escape(&change.reason),
                by = escape(&change.created_by),
            )
        })
        .collect::<String>();
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>時刻</th><th>上</th><th>下</th><th>理由</th><th>操作者</th></tr></thead>\n\
         <tbody>\n{rows}</tbody>\n</table></div>\n"
    )
}

pub fn matching_range_page(view: &MatchingRangeView<'_>) -> String {
    let current = view.preview.current;
    let body = format!(
        "<p class=\"lead\">同じ試合に入れるレート差の幅です。ウィンドウの下端から\
         <strong>上に +{up}</strong>、そこから<strong>下に -{down}</strong> までを 1 つの\
         組み合わせとして探します。プレイヤーは変更できません。</p>\n\
         <p class=\"notice\"><strong>過去の試合は 1 件も変わりません。</strong>\
         幅が効くのは<strong>次のスキャン以降の選抜だけ</strong>です。確定済みの試合の\
         レートも、いま進行中の formation も、この操作では動きません\
         （レートの増減幅を変えたいときは<a href=\"/admin/rating-params\">レート増減幅</a>）。</p>\n\
         <p class=\"notice\">追記専用です。間違えたら<strong>元の値をもう 1 行追記すれば戻ります</strong>。\
         走っているスキャンとは同じロックで直列化されるので、1 回の選抜の途中で\
         幅が入れ替わることはありません。</p>\n\
         <div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>項目</th><th>値</th></tr></thead>\n<tbody>\n\
         <tr><td><strong>現在の上方向の幅</strong></td><td class=\"num\">+{up}</td></tr>\n\
         <tr><td><strong>現在の下方向の幅</strong></td><td class=\"num\">-{down}</td></tr>\n\
         <tr><td>いまキューにいる人数（次のポーリングで「範囲内で N 人」が変わります）</td>\
         <td class=\"num\">{queued}</td></tr>\n\
         </tbody>\n</table></div>\n\
         <form method=\"post\" action=\"/admin/matching-range\">\
         <input type=\"hidden\" name=\"key\" value=\"matching-range:{key}\">\
         <label for=\"up_range\">上方向の幅（{min}〜{max}）</label>\
         <input id=\"up_range\" name=\"up_range\" type=\"text\" inputmode=\"numeric\" \
         autocomplete=\"off\" value=\"{up}\" required>\
         <label for=\"down_range\">下方向の幅（{min}〜{max}）</label>\
         <input id=\"down_range\" name=\"down_range\" type=\"text\" inputmode=\"numeric\" \
         autocomplete=\"off\" value=\"{down}\" required>\
         <label for=\"reason\">理由（記録に残ります）</label>\
         <input id=\"reason\" name=\"reason\" type=\"text\" autocomplete=\"off\" required>\
         <div class=\"actions\">\
         <button class=\"button button-primary\" type=\"submit\">マッチング範囲を変更する</button>\
         </div></form>\n\
         <h2>変更履歴</h2>\n{history}",
        up = current.up,
        down = current.down,
        queued = view.preview.queued,
        key = escape(view.form_key),
        min = MIN_RANGE,
        max = MAX_RANGE,
        history = matching_range_history(view.preview),
    );

    // この画面は管理者しか到達しない（非管理者は 404）
    page(
        Viewer::ADMIN,
        "管理 / マッチング範囲",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-matching-range-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-matching-range-title\">マッチング範囲を変える</h1>\n\
             {nav}\
             {notice}\
             {body}\
             </section>\n",
            nav = admin_nav("/admin/matching-range"),
            notice = notice_html(view.notice.as_ref()),
        ),
    )
}

// ----------------------------------------------------------------------
// 登録者の一覧
// ----------------------------------------------------------------------
//
// 検索（`/admin`）は名前か Discord ID を知っている前提の入口だった。
// 「誰が居るのか」を端から見る場所として別に置く。

pub struct RosterView<'a> {
    pub page: &'a RosterPage,
}

/// 停止処分のタグ。**通常の人には何も出さない**（全員に緑のタグを付けると、
/// 目で拾いたい停止処分が埋もれる）。
fn status_tag(status: &str) -> &'static str {
    match status {
        "suspended" => "<span class=\"tag tag-danger\">停止</span>",
        _ => "",
    }
}

fn roster_rows(page: &RosterPage) -> String {
    page.players
        .iter()
        .map(|entry| {
            let trainer = entry
                .trainer_name
                .as_deref()
                .filter(|name| !name.trim().is_empty());
            // **登録が済んでいない人を見落とさない。**名前が無いのは未登録の印なので、
            // Discord 表示名で埋めずに「未登録」と出す
            let name = trainer.map_or_else(
                || String::from("<span class=\"muted\">（未登録）</span>"),
                escape,
            );
            let discord = entry
                .display_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map_or_else(String::new, |name| {
                    format!("<span class=\"trainer\">{}</span>", escape(name))
                });
            let registered = entry
                .registered_at
                .map_or_else(|| String::from("—"), |at| escape(&display_time(at)));
            format!(
                "<tr><td class=\"who\"><a href=\"/admin/player?player={id}\">{name}</a>\
                 {status}{queued}{discord}</td>\
                 <td class=\"num\">{rating}</td><td class=\"num\">{matches}</td>\
                 <td>{registered}</td><td class=\"num\">{discord_id}</td></tr>\n",
                id = escape(&entry.player_id.to_string()),
                status = status_tag(&entry.status),
                queued = if entry.queued {
                    "<span class=\"tag\">キュー中</span>"
                } else {
                    ""
                },
                rating = entry.rating.round() as i64,
                matches = entry.matches,
                discord_id = entry.discord_user_id,
            )
        })
        .collect::<String>()
}

fn roster_pager(page: &RosterPage) -> String {
    let link = |target: i64, label: &str| {
        // 絞り込みを持ち回る。**クエリ文字列なので percent エンコードしてから escape する**
        // （どちらか片方だけだと、空白や & を含む名前でリンクが壊れる）
        format!(
            "<a class=\"button button-secondary\" href=\"/admin/players?page={target}&q={q}\">{label}</a>",
            q = escape(&percent_encode(&page.query))
        )
    };
    let previous = if page.page > 1 {
        link(page.page - 1, "前へ")
    } else {
        String::from("<span></span>")
    };
    let next = if page.page < page.pages {
        link(page.page + 1, "次へ")
    } else {
        String::from("<span></span>")
    };
    format!(
        "<div class=\"pager\">{previous}\
         <span class=\"position\">{page} / {pages} ページ（{total}人）</span>\
         {next}</div>\n",
        page = page.page,
        pages = page.pages,
        total = page.total,
    )
}

pub fn roster_page(view: &RosterView<'_>) -> String {
    let body = if view.page.total == 0 {
        String::from("<p class=\"muted\">該当する登録者がいません。</p>\n")
    } else {
        format!(
            "<div class=\"table-wrap\"><table class=\"log\">\n\
             <thead><tr><th>プレイヤー</th><th>レート</th><th>試合</th>\
             <th>登録</th><th>Discord ID</th></tr></thead>\n\
             <tbody>\n{rows}</tbody></table></div>\n{pager}",
            rows = roster_rows(view.page),
            pager = roster_pager(view.page),
        )
    };

    // この画面は管理者しか到達しない（非管理者は 404）
    page(
        Viewer::ADMIN,
        "管理 / 登録者一覧",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-roster-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-roster-title\">登録者一覧</h1>\n\
             {nav}\
             <p class=\"lead\">登録の新しい順です。レート・試合数・キュー在席はすべて\
             追記ログからの導出値で、この画面では何も保存しません。</p>\n\
             <form method=\"get\" action=\"/admin/players\" class=\"search\">\
             <label for=\"roster-q\">名前 / Discord 表示名 / Discord ID で絞り込む</label>\
             <input id=\"roster-q\" name=\"q\" type=\"text\" autocomplete=\"off\" value=\"{q}\">\
             <div class=\"actions\"><button class=\"button button-primary\" type=\"submit\">絞り込む</button>\
             <a class=\"button button-secondary\" href=\"/admin/players\">解除</a></div></form>\n\
             {body}",
            nav = admin_nav("/admin/players"),
            q = escape(&view.page.query),
        ),
    )
}

/// クエリ文字列に載せるための percent エンコード。
///
/// 依存を増やさないための最小実装。**通すのは RFC 3986 の unreserved だけ**で、
/// それ以外は UTF-8 のバイト列として `%XX` にする。日本語のトレーナー名が
/// そのまま来るので、ASCII だけを見る実装にはしない。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ----------------------------------------------------------------------
// 聞き専報告（運営だけが見る）
// ----------------------------------------------------------------------
//
// **ここが票数を見られる唯一の画面。**プレイヤー向けの経路には票数も閾値も出ない。

pub struct QuietReportsView<'a> {
    /// プレイヤーごとの集計。**報告した数の多い順**（乱用の検出）。
    pub tallies: &'a [QuietReporterTally],
    /// 誰が誰を報告したかの履歴（新しい順）。
    pub history: &'a [QuietReportRow],
    /// いま効いている閾値。**運営にだけ出す。**
    pub winner_threshold: i32,
    pub loser_threshold: i32,
    /// 無効試合の通報の集計（票の多い順）。
    pub nullified_tallies: &'a [NullifiedTally],
    /// 無効試合の通報の履歴（新しい順）。「誰も通報しない」も含む。
    pub nullified_history: &'a [NullifiedReportRow],
    /// 無効試合の通報の閾値とペナルティ幅。**運営にだけ出す。**
    pub nullified_threshold: i32,
    pub nullified_penalty: f64,
    pub notice: Option<Notice>,
}

/// 勝ち負けの表記。確定していない・勝敗が無い試合は「—」。
fn reporter_side(won: Option<bool>) -> &'static str {
    match won {
        Some(true) => "勝ち",
        Some(false) => "<strong>負け</strong>",
        None => "—",
    }
}

/// 無効試合の通報の集計。**票数が見えるのはここだけ。**
///
/// 「まだ無効試合になっていない試合の票」も出す。通報は報告直後に受け付けるので、
/// 結局勝敗がついた試合の票も残る——それが**レートに効いていない**ことが
/// 運営から見えないと、「票が入っているのに下がっていない」が事故に見える。
fn nullified_tallies(tallies: &[NullifiedTally]) -> String {
    if tallies.is_empty() {
        return String::from("<p class=\"muted\">まだ無効試合の通報はありません。</p>\n");
    }
    let rows = tallies
        .iter()
        .map(|row| {
            let state = if row.penalised {
                "<span class=\"tag tag-danger\">成立</span>"
            } else if row.match_nullified {
                "<span class=\"tag\">未達</span>"
            } else {
                "<span class=\"tag\">無効試合でない</span>"
            };
            format!(
                "<tr><td>{name}</td><td class=\"num\">{votes}</td><td>{state}</td>\
                 <td class=\"num\">{formation}</td></tr>\n",
                name = escape(row.trainer_name.as_deref().unwrap_or("（未設定）")),
                votes = row.votes,
                formation = escape(&format!(
                    "match-{}",
                    &row.formation_id.simple().to_string()[..8]
                )),
            )
        })
        .collect::<String>();
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>通報された人</th><th>票</th><th>状態</th><th>試合</th></tr></thead>\n\
         <tbody>\n{rows}</tbody>\n</table></div>\n"
    )
}

/// 無効試合の通報の履歴。**「誰も通報しない」も 1 行として出す。**
fn nullified_history(history: &[NullifiedReportRow]) -> String {
    if history.is_empty() {
        return String::new();
    }
    let rows = history
        .iter()
        .map(|row| {
            let target = row.target_name.as_deref().map_or_else(
                || String::from("<span class=\"muted\">誰も通報しない</span>"),
                escape,
            );
            let side = match row.cross_team {
                Some(true) => "<span class=\"tag\">相手チーム</span>",
                Some(false) => "<span class=\"tag\">自チーム</span>",
                None => "",
            };
            format!(
                "<tr><td>{at}</td><td>{reporter}</td><td>{target} {side}</td>\
                 <td class=\"num\">{formation}</td></tr>\n",
                at = escape(&display_time(row.reported_at)),
                reporter = escape(row.reporter_name.as_deref().unwrap_or("（未設定）")),
                formation = escape(&format!(
                    "match-{}",
                    &row.formation_id.simple().to_string()[..8]
                )),
            )
        })
        .collect::<String>();
    format!(
        "<h2>無効試合の通報（履歴）</h2>\n\
         <div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>時刻</th><th>通報した人</th><th>通報された人</th>\
         <th>試合</th></tr></thead>\n\
         <tbody>\n{rows}</tbody>\n</table></div>\n"
    )
}

pub fn quiet_reports_page(view: &QuietReportsView<'_>) -> String {
    let tallies = if view.tallies.is_empty() {
        String::from("<p class=\"muted\">まだ聞き専報告はありません。</p>\n")
    } else {
        let rows = view
            .tallies
            .iter()
            .map(|row| {
                format!(
                    "<tr><td>{name}</td>\
                     <td class=\"num\">{made}</td><td class=\"num\">{losing}</td>\
                     <td class=\"num\">{received}</td><td class=\"num\">{penalties}</td></tr>\n",
                    name = escape(row.trainer_name.as_deref().unwrap_or("（未設定）")),
                    made = row.reports_made,
                    losing = row.made_while_losing,
                    received = row.reports_received,
                    penalties = row.penalties,
                )
            })
            .collect::<String>();
        format!(
            "<div class=\"table-wrap\"><table class=\"log\">\n\
             <thead><tr><th>プレイヤー</th><th>報告した数</th><th>うち負け試合</th>\
             <th>報告された数</th><th>ペナルティ成立</th></tr></thead>\n\
             <tbody>\n{rows}</tbody>\n</table></div>\n"
        )
    };

    let history = if view.history.is_empty() {
        String::new()
    } else {
        let rows = view
            .history
            .iter()
            .map(|row| {
                format!(
                    "<tr><td>{at}</td><td>{reporter}</td><td>{side}</td>\
                     <td>{target}</td><td>{penalised}</td><td class=\"mono\">{formation}</td></tr>\n",
                    at = escape(&display_time(row.reported_at)),
                    reporter = escape(row.reporter_name.as_deref().unwrap_or("（未設定）")),
                    side = reporter_side(row.reporter_won),
                    target = escape(row.target_name.as_deref().unwrap_or("（未設定）")),
                    penalised = if row.penalised { "成立" } else { "—" },
                    formation = escape(&row.formation_id.simple().to_string()[..8]),
                )
            })
            .collect::<String>();
        format!(
            "<h2>報告の履歴</h2>\n\
             <div class=\"table-wrap\"><table class=\"log\">\n\
             <thead><tr><th>時刻</th><th>報告した人</th><th>その試合</th>\
             <th>報告された人</th><th>ペナルティ</th><th>試合</th></tr></thead>\n\
             <tbody>\n{rows}</tbody>\n</table></div>\n"
        )
    };

    // この画面は管理者しか到達しない（非管理者は 404）
    page(
        Viewer::ADMIN,
        "管理 / 聞き専報告",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-quiet-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-quiet-title\">聞き専報告</h1>\n\
             {nav}\
             {notice}\
             <p class=\"lead\">誰が誰を報告したかは<strong>この画面からしか読めません</strong>。\
             プレイヤー側の画面にも API にも、票数・閾値・報告の内容は出ません。</p>\n\
             <p class=\"notice\">現在の閾値: 勝ちチームからの報告 <strong>{winner}</strong> 票 / \
             負けチームからの報告 <strong>{loser}</strong> 票。\
             報告できるのは対象者を除いた同チームの 4 人なので、3 票は「4 人中 3 人」です。\
             閾値は環境変数で変えられ、変更すると過去の試合も新しい閾値で再判定されます。</p>\n\
             <h2>プレイヤーごとの集計</h2>\n\
             <p class=\"field-help\"><strong>報告した数の多い順</strong>に並べています。\
             負け試合でしか報告していない人は「うち負け試合」が「報告した数」と同じになります。</p>\n\
             {tallies}\
             {history}\
             <h2>無効試合の通報</h2>\n\
             <p class=\"notice\">現在の閾値: <strong>{nullified_threshold}</strong> 票で\
             <strong>−{nullified_penalty}</strong>。<strong>相手チームも通報できます。</strong>\
             通報は「無効試合」を報告した直後に受け付けるので、\
             <strong>結局勝敗がついた試合の票も残ります</strong>——その票はレートに効きません\
             （状態が「無効試合でない」の行）。</p>\n\
             {nullified_tallies}\
             {nullified_history}\
             </section>\n",
            nav = admin_nav("/admin/quiet-reports"),
            notice = notice_html(view.notice.as_ref()),
            winner = view.winner_threshold,
            loser = view.loser_threshold,
            nullified_threshold = view.nullified_threshold,
            nullified_penalty = view.nullified_penalty.round() as i64,
            nullified_tallies = nullified_tallies(view.nullified_tallies),
            nullified_history = nullified_history(view.nullified_history),
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
            received_blocks: &[],
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
            outgoing_blocks: &[],
            incoming_blocks: &[],
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
            outgoing_blocks: &[],
            incoming_blocks: &[],
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
            outgoing_blocks: &[],
            incoming_blocks: &[],
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
            received_blocks: &[],
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

    /// テストのスケジュールは**使わない設定**。既存のテストは手動の停止だけを見ている。
    const NO_SCHEDULE: MatchmakerSchedule = MatchmakerSchedule {
        enabled: false,
        days_mask: 0,
        start_minute: 0,
        end_minute: 1,
        queue_open_minute: 0,
    };

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
            schedule: ScheduleView {
                schedule: &NO_SCHEDULE,
                open: true,
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
            received_blocks: &[],
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
            received_blocks: &[],
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
    fn received_block_counts_are_rendered_in_the_repository_order() {
        let received_blocks = vec![
            ReceivedBlockCount {
                player_id: Uuid::from_u128(8),
                display_name: Some("多い方".to_owned()),
                trainer_name: None,
                count: 3,
            },
            ReceivedBlockCount {
                player_id: Uuid::from_u128(9),
                display_name: Some("少ない方".to_owned()),
                trainer_name: None,
                count: 1,
            },
        ];
        let rendered = search_page(&SearchView {
            query: "",
            players: &[],
            recent: &[],
            received_blocks: &received_blocks,
            matchmaker: running(),
            notice: None,
        });
        assert!(rendered.contains("受信ブロック件数"));
        assert!(rendered.contains("現在有効な味方ブロックのみ"));
        assert!(
            rendered.find("多い方").unwrap() < rendered.find("少ない方").unwrap(),
            "集計済みの降順を表示順も保つ: {rendered}"
        );
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
            outgoing_blocks: &[],
            incoming_blocks: &[],
            notice: None,
        });
        assert!(rendered.contains("<dt>ブロック枠</dt><dd>1 枠</dd>"));
        assert!(rendered.contains("action=\"/admin/blocks\""));
        assert!(rendered.contains("name=\"key\" value=\"blocks:k\""));
        assert!(rendered.contains(&format!("max=\"{MAX_BLOCK_ALLOWANCE}\"")));
        // 調整フォームの冪等キーと衝突しない
        assert!(rendered.contains("name=\"key\" value=\"k\""));
    }

    #[test]
    fn the_player_page_lists_block_relationships_and_the_override_form() {
        let outgoing = [CurrentBlock {
            blocked_player_id: Uuid::from_u128(8),
            display_name: Some("Global Outgoing".to_owned()),
            trainer_name: None,
            blocked_at: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap(),
        }];
        let incoming = [IncomingBlock {
            player_id: Uuid::from_u128(9),
            display_name: Some("Global Incoming".to_owned()),
            trainer_name: None,
            blocked_at: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap(),
        }];
        let rendered = player_page(&PlayerView {
            player: &player(),
            adjustments: &[],
            form_key: "k",
            block_allowance: DEFAULT_BLOCK_ALLOWANCE,
            outgoing_blocks: &outgoing,
            incoming_blocks: &incoming,
            notice: None,
        });
        assert!(rendered.contains("Global Outgoing"), "{rendered}");
        assert!(rendered.contains("Global Incoming"), "{rendered}");
        assert!(
            rendered.contains("action=\"/admin/blocks/override\""),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("value=\"{}\"", Uuid::from_u128(8))),
            "{rendered}"
        );
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

    /// **既定の枠がフォームに反映される。**何も設定していなくても、
    /// 水金土日 21:00〜02:00 にチェックが入った状態で開く。
    #[test]
    fn the_schedule_form_is_prefilled_with_the_current_window() {
        let defaults = MatchmakerSchedule::default();
        let view = QueueView {
            schedule: ScheduleView {
                schedule: &defaults,
                open: false,
                form_key: "form-key",
            },
            ..queue_view(&[], &[], None)
        };
        let rendered = queue_page(&view);
        // 選ばれている曜日だけ checked が付く
        for day in ["wed", "fri", "sat", "sun"] {
            assert!(
                rendered.contains(&format!(
                    "<input type=\"checkbox\" name=\"day_{day}\" value=\"on\" checked>"
                )),
                "{day} に checked が無い"
            );
        }
        for day in ["mon", "tue", "thu"] {
            assert!(
                rendered.contains(&format!(
                    "<input type=\"checkbox\" name=\"day_{day}\" value=\"on\">"
                )),
                "{day} に checked が付いている"
            );
        }
        // 時刻も入っている
        assert!(rendered.contains("name=\"start\" type=\"time\" value=\"21:00\""));
        assert!(rendered.contains("name=\"end\" type=\"time\" value=\"02:00\""));
        assert!(rendered.contains("name=\"queue_open\" type=\"time\" value=\"20:45\""));
        assert!(rendered.contains("キュー受付は <strong>20:45</strong> から"));
        // 使う設定なので現在の枠が出る
        assert!(rendered.contains("現在の枠"), "{rendered}");
        assert!(rendered.contains("水金土日"), "{rendered}");
        assert!(rendered.contains("稼働時間外"), "open=false のはず");

        // **チェックボックスが巨大な箱にならないこと。**共通の input 指定
        // （width:100% / min-height:48px）を打ち消す CSS が入っている
        assert!(
            rendered.contains("label.day input[type=\"checkbox\"]"),
            "チェックボックスの打ち消しが無い"
        );
        assert!(
            rendered.contains("label.day { display: inline-flex"),
            "{rendered}"
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
                    outgoing_blocks: &[],
                    incoming_blocks: &[],
                    notice: None,
                }),
            ),
        ] {
            println!("===== {name}\n{html}");
        }
    }

    /// **係争中のまま境界を引こうとしたら、何が起きるかを書く。**
    ///
    /// 件数だけでは意味が伝わらない。境界の判定は `formations.created_at` なので、
    /// 境界より後に裁定して確定させても新シーズンには入らない。
    #[test]
    fn the_season_page_explains_what_happens_to_disputed_matches() {
        let with_disputes = SeasonPreview {
            active_formations: 0,
            pending_reports: 0,
            disputed: 3,
            matches_leaving: 12,
            players_affected: 20,
            current: None,
        };
        let rendered = season_page(&SeasonView {
            preview: &with_disputes,
            effective_at: Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap(),
            form_key: "k",
            confirming: None,
            notice: None,
        });
        assert!(
            rendered.contains("係争中の試合が 3 件あります"),
            "{rendered}"
        );
        assert!(
            rendered.contains("新しいシーズンのレートには反映されません"),
            "何が起きるかが書かれていない: {rendered}"
        );
        assert!(
            rendered.contains("作られた"),
            "どの時刻で切るかが書かれていない: {rendered}"
        );
        // 2 段目にも同じ警告を出す（1 段目だけだと読み飛ばされる）
        let confirming = season_page(&SeasonView {
            preview: &with_disputes,
            effective_at: Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap(),
            form_key: "k",
            confirming: Some("シーズン2"),
            notice: None,
        });
        assert!(
            confirming.contains("新しいシーズンのレートには反映されません"),
            "{confirming}"
        );

        // 係争がゼロなら出さない
        let clean = SeasonPreview {
            disputed: 0,
            ..with_disputes
        };
        let rendered = season_page(&SeasonView {
            preview: &clean,
            effective_at: Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap(),
            form_key: "k",
            confirming: None,
            notice: None,
        });
        assert!(!rendered.contains("先に裁定してください"), "{rendered}");
        // 件数の行そのものは常に出す（0 でも「集計されていない」と読まれないように）
        assert!(rendered.contains("係争中の試合"), "{rendered}");
    }
}

// ----------------------------------------------------------------------
// 運営による報告の入力
// ----------------------------------------------------------------------

/// 報告の入力画面（`/admin/reports`）に渡すデータ。
#[derive(Debug, Clone)]
pub struct ReportsView<'a> {
    /// 報告待ちの formation と未報告者。
    pub awaiting: &'a [AwaitingReports],
    /// 冪等キー（フォーム描画時にサーバ発行）。
    pub form_key: &'a str,
    /// 係争中の試合。**閲覧だけ**（入力欄は出さない）。
    pub disputed: &'a [DisputedFormation],
    /// 終わった試合（新しい順）。**裁定のやり直し**の材料。
    pub finished: &'a [FinishedFormation],
    pub notice: Option<Notice>,
}

/// 報告の入力画面。**未報告の人だけを並べる。**
///
/// 報告済みの人はそもそも一覧に出ない。運営が上書きできる形にしない
/// （裁定は disputed のフローで行う）。
pub fn reports_page(view: &ReportsView<'_>) -> String {
    let body = if view.awaiting.is_empty() {
        String::from("<p class=\"fine-print\">報告待ちの試合はありません。</p>\n")
    } else {
        view.awaiting
            .iter()
            .map(|formation| awaiting_block(formation, view.form_key))
            .collect::<String>()
    };
    page(
        Viewer::ADMIN,
        "管理 / 報告の入力",
        &format!(
            "{ADMIN_STYLES}\
             <section aria-labelledby=\"admin-reports-title\">\n\
             <p class=\"eyebrow\">Admin</p>\n\
             <h1 id=\"admin-reports-title\">未報告のプレイヤーに報告を入れる</h1>\n\
             {nav}\
             <p class=\"lead\">Discord 上で裁定した結果をここから入れます。\
             <strong>入るのは追記で、プレイヤーの報告は書き換わりません。</strong>\
             確定の条件も変わりません（全員一致 + 全員報告、または受付時間の経過）。</p>\n\
             <p class=\"fine-print\">既に報告している人はこの一覧に出ません。\
             その人の内容を運営が上書きする経路はありません。\
             <strong>係争中（disputed）の試合もこの一覧には出ません</strong> — \
             報告が割れたことが原因なので、未報告者ぶんを足しても状態は動きません。\
             下の一覧で内訳を見て、そこから裁定してください。</p>\n\
             {notice}\
             {body}\
             {disputed}\
             {finished}\
             </section>\n",
            nav = admin_nav("/admin/reports"),
            disputed = disputed_section(view.disputed, view.form_key),
            finished = finished_section(view.finished, view.form_key),
            notice = notice_html(view.notice.as_ref()),
        ),
    )
}

/// formation 1 件ぶんの入力欄。
fn awaiting_block(formation: &AwaitingReports, form_key: &str) -> String {
    let rows = formation
        .unreported
        .iter()
        .map(|player| {
            let name = player
                .trainer_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("（名前未登録）");
            format!(
                "<tr><td>{name}</td><td>{team}</td><td>\
                 <form method=\"post\" action=\"/admin/reports\" class=\"inline-form\">\
                 <input type=\"hidden\" name=\"formation\" value=\"{formation_id}\">\
                 <input type=\"hidden\" name=\"player\" value=\"{player_id}\">\
                 <input type=\"hidden\" name=\"key\" value=\"{key}:{player_id}\">\
                 <label class=\"sr-only\" for=\"reason-{player_id}\">理由</label>\
                 <input id=\"reason-{player_id}\" name=\"reason\" type=\"text\" \
                 placeholder=\"理由（任意）\" maxlength=\"200\">\
                 <button class=\"button-secondary\" type=\"submit\" name=\"outcome\" value=\"red\">赤の勝ち</button>\
                 <button class=\"button-secondary\" type=\"submit\" name=\"outcome\" value=\"blue\">青の勝ち</button>\
                 <button class=\"button-secondary\" type=\"submit\" name=\"outcome\" value=\"nullified\">無効試合</button>\
                 </form></td></tr>\n",
                name = escape(name),
                team = escape(&player.team),
                formation_id = escape(&formation.formation_id.to_string()),
                player_id = escape(&player.player_id.to_string()),
                key = escape(form_key),
            )
        })
        .collect::<String>();
    format!(
        "<section class=\"section\"><h2>{label}</h2>\n\
         <p class=\"fine-print\">状態: {state} / 成立: {at} / 未報告: {count}人</p>\n\
         <div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>プレイヤー</th><th>チーム</th><th>入れる結果</th></tr></thead>\n\
         <tbody>\n{rows}</tbody></table></div></section>\n",
        label = escape(&format!(
            "match-{}",
            &formation.formation_id.simple().to_string()[..8]
        )),
        state = escape(&formation.state),
        at = escape(&display_time(formation.created_at)),
        count = formation.unreported.len(),
    )
}

/// 係争中の試合の一覧と、**その場で裁定するフォーム**。
///
/// 未報告者の一覧から係争中を外してあるので、運営が係争を見る場所はここだけ。
/// 誰が何を報告したかを読んだうえで、同じ画面で裁定できる。
///
/// **報告は書き換えない。**裁定は終端イベントを 1 行積むだけで、割れた報告は
/// そのまま残る。だから「誰の報告を採用したか」ではなく「運営が何と決めたか」を
/// 記録することになる。
fn disputed_section(disputed: &[DisputedFormation], form_key: &str) -> String {
    let body = if disputed.is_empty() {
        String::from("<p class=\"fine-print\">係争中の試合はありません。</p>\n")
    } else {
        disputed
            .iter()
            .map(|formation| disputed_block(formation, form_key))
            .collect::<String>()
    };
    format!(
        "<section class=\"section\" aria-labelledby=\"disputed-title\">\n\
         <h2 id=\"disputed-title\">係争中の試合（{count}件）</h2>\n\
         <p class=\"fine-print\">報告が割れたまま期限を過ぎた試合です。\
         内訳を見て裁定してください。<strong>裁定しないと、その試合で未報告の人は\
         キューに入れないままになります。</strong>\
         また、シーズン境界を引いた後に裁定しても新シーズンのレートには入りません。</p>\n\
         <p class=\"fine-print\"><strong>報告は書き換わりません。</strong>\
         割れた報告はそのまま残り、裁定はその上に積む別の 1 行として記録されます。</p>\n\
         {body}</section>\n",
        count = disputed.len(),
    )
}

/// 係争中の試合 1 件の内訳と裁定フォーム。
fn disputed_block(formation: &DisputedFormation, form_key: &str) -> String {
    let id = formation.formation_id.to_string();
    format!(
        "<h3>{label}</h3>\n<p class=\"fine-print\">成立: {at}</p>\n\
         {votes}\
         <form method=\"post\" action=\"/admin/reports/dispute\">\
         <input type=\"hidden\" name=\"formation\" value=\"{id}\">\
         <input type=\"hidden\" name=\"key\" value=\"dispute:{id}:{form_key}\">\
         <label for=\"decision-{id}\">裁定</label>\
         <select id=\"decision-{id}\" name=\"decision\" required>{choices}</select>\
         <label for=\"reason-{id}\">理由（記録に残ります）</label>\
         <input id=\"reason-{id}\" name=\"reason\" type=\"text\" autocomplete=\"off\" required>\
         <div class=\"actions\">\
         <button class=\"button button-primary\" type=\"submit\">この裁定で確定する</button>\
         </div></form>\n",
        label = escape(&match_label(formation.formation_id)),
        at = escape(&display_time(formation.created_at)),
        votes = vote_table(&formation.votes),
        choices = decision_options(),
        id = escape(&id),
        form_key = escape(form_key),
    )
}

/// 管理画面での試合の呼び名（`match-xxxxxxxx`）。マッチ画面の URL の先頭 8 桁と同じ。
fn match_label(formation_id: uuid::Uuid) -> String {
    format!("match-{}", &formation_id.simple().to_string()[..8])
}

/// 参加者 10 人の最新報告の表。係争と裁定のやり直しで共通。
fn vote_table(votes: &[DisputedVote]) -> String {
    let rows = votes
        .iter()
        .map(|vote| {
            let name = vote
                .trainer_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("（名前未登録）");
            let outcome = vote.outcome.as_deref().map_or_else(
                || String::from("<span class=\"tag\">未報告</span>"),
                |outcome| escape(outcome_label(outcome)),
            );
            let source = if vote.from_staff {
                "<span class=\"tag\">運営が入力</span>"
            } else {
                ""
            };
            format!(
                "<tr><td>{name}</td><td>{team}</td><td>{outcome} {source}</td></tr>\n",
                name = escape(name),
                team = escape(&vote.team),
            )
        })
        .collect::<String>();
    format!(
        "<div class=\"table-wrap\"><table class=\"log\">\n\
         <thead><tr><th>プレイヤー</th><th>チーム</th><th>最新の報告</th></tr></thead>\n\
         <tbody>\n{rows}</tbody></table></div>\n"
    )
}

/// 裁定の選択肢。係争と裁定のやり直しで同じ 4 つ。
fn decision_options() -> String {
    DisputeDecision::ALL
        .iter()
        .map(|decision| {
            let note = if decision.changes_ratings() {
                "（レートが動きます）"
            } else {
                "（レートは動きません）"
            };
            format!(
                "<option value=\"{value}\">{label}{note}</option>\n",
                value = escape(decision.as_str()),
                label = escape(decision.label()),
            )
        })
        .collect()
}

/// 終わった試合の一覧と、**裁定をやり直すフォーム**。
///
/// 「無効試合にするか微妙な試合を各自の判断で報告し、確定した後で『やっぱり
/// 無効試合にしよう』となる」ときに使う。10 人ぶんのレートを 1 人ずつ調整で
/// 打ち消す代わりに、試合の結果そのものを変える。レートは導出値なので、
/// 結果が変われば 10 人ぶんが同時に引き直される。
///
/// 1 件ずつ折りたたんで出す。件数が多いので、開くまで内訳もフォームも見せない。
fn finished_section(finished: &[FinishedFormation], form_key: &str) -> String {
    let body = if finished.is_empty() {
        String::from("<p class=\"fine-print\">今シーズンに終わった試合はまだありません。</p>\n")
    } else {
        finished
            .iter()
            .map(|formation| finished_block(formation, form_key))
            .collect::<String>()
    };
    format!(
        "<section class=\"section\" aria-labelledby=\"finished-title\">\n\
         <h2 id=\"finished-title\">終わった試合の裁定をやり直す（直近 {count}件）</h2>\n\
         <p class=\"fine-print\">確定した後で「やっぱり無効試合にする」「勝敗が逆だった」\
         ときはここから。<strong>その試合の 10 人ぶんのレートがまとめて引き直されます</strong>\
         ——1 人ずつ調整を入れる必要はありません。\
         試合の呼び名（match-xxxxxxxx）はマッチ画面の URL の先頭 8 桁です。</p>\n\
         <p class=\"fine-print\"><strong>元の裁定もプレイヤーの報告も書き換わりません。</strong>\
         やり直しは、その上に積む別の 1 行として記録されます。\
         今シーズンの試合だけを出しています（境界より前の試合はやり直してもレートに入りません）。</p>\n\
         {body}</section>\n",
        count = finished.len(),
    )
}

/// 終わった試合 1 件。折りたたみの見出しに、いまの結果を出す。
fn finished_block(formation: &FinishedFormation, form_key: &str) -> String {
    let id = formation.formation_id.to_string();
    format!(
        "<details class=\"finished\" data-formation=\"{id}\">\
         <summary><strong>{label}</strong> — いまの結果: {result} / 成立: {at}</summary>\n\
         <p class=\"fine-print\">最後の裁定: {decided}</p>\n\
         {votes}\
         <form method=\"post\" action=\"/admin/reports/readjudicate\">\
         <input type=\"hidden\" name=\"formation\" value=\"{id}\">\
         <input type=\"hidden\" name=\"key\" value=\"readjudicate:{id}:{form_key}\">\
         <label for=\"readjudicate-{id}\">新しい裁定</label>\
         <select id=\"readjudicate-{id}\" name=\"decision\" required>{choices}</select>\
         <label for=\"readjudicate-reason-{id}\">理由（記録に残ります）</label>\
         <input id=\"readjudicate-reason-{id}\" name=\"reason\" type=\"text\" \
         autocomplete=\"off\" required>\
         <div class=\"actions\">\
         <button class=\"button button-primary\" type=\"submit\">この試合の裁定をやり直す</button>\
         </div></form>\n\
         </details>\n",
        label = escape(&match_label(formation.formation_id)),
        result = escape(formation.result.label()),
        at = escape(&display_time(formation.created_at)),
        decided = escape(&display_time(formation.decided_at)),
        votes = vote_table(&formation.votes),
        choices = decision_options(),
        id = escape(&id),
        form_key = escape(form_key),
    )
}

/// 報告の表記。**内部の識別子を画面に出さない。**
fn outcome_label(outcome: &str) -> &str {
    match outcome {
        "red" => "赤の勝ち",
        "blue" => "青の勝ち",
        "nullified" => "無効試合",
        "draw" => "引き分け",
        other => other,
    }
}
