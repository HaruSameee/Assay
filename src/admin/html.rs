//! 管理パネルの HTML。外枠は登録画面（[`crate::registration::html::page`]）をそのまま使う。
//!
//! 表示する値はトレーナー名・Discord ID・数値・日時だけ。いずれも [`escape`] を通す。
//! 打ち消しは行ごとの POST フォーム（`/admin/revert`）で、ボタンは**未打ち消しの
//! 通常行にだけ**出す（権威は DB の unique index とトリガー。ここは表示の面倒だけ）。

use crate::registration::Viewer;
use crate::registration::html::{escape, page};
use crate::status::html::{display_time, format_delta};

use super::repository::{AdjustmentRow, FoundPlayer};

/// 検索画面に渡すデータ。
#[derive(Debug, Clone)]
pub struct SearchView<'a> {
    /// 検索ボックスに戻す文字列。
    pub query: &'a str,
    /// 検索結果（クエリが空なら空）。
    pub players: &'a [FoundPlayer],
    /// 直近の調整履歴（新しい順）。
    pub recent: &'a [AdjustmentRow],
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
             <p class=\"lead\">調整は <strong>追記のみ</strong> です。既存の行は変更も削除もできず、\
             取り消しは「打ち消し行」の追記で行います。</p>\n\
             {notice}\
             <form method=\"get\" action=\"/admin\">\
             <label for=\"q\">プレイヤー検索</label>\
             <input id=\"q\" name=\"q\" type=\"search\" value=\"{q}\" placeholder=\"トレーナー名または Discord ID\">\
             <div class=\"actions\"><button class=\"button button-primary\" type=\"submit\">検索</button></div>\
             </form>\n\
             {found}\
             <h2>直近の調整履歴</h2>\n\
             {recent}\
             </section>\n",
            notice = notice_html(view.notice.as_ref()),
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
             <dl class=\"fields\">\n\
             <dt>Discord ID</dt><dd>{discord}</dd>\n\
             <dt>player id</dt><dd>{player_id}</dd>\n\
             </dl>\n\
             {notice}\
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
            discord = view.player.discord_user_id,
            player_id = escape(&view.player.player_id.to_string()),
            notice = notice_html(view.notice.as_ref()),
            form_key = escape(view.form_key),
            history = adjustments_table(view.adjustments, false),
        ),
    )
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;

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
            notice: None,
        });
        assert!(rendered.contains("color-scheme: dark"));
        assert!(rendered.contains("@media (max-width: 480px)"));
        assert!(rendered.contains("overflow-x: auto"));
    }
}
