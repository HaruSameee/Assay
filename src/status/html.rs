//! 戦績画面の HTML。登録画面と同じ外枠（[`crate::registration::html::page`]）を使い、
//! 新しいデザインを起こさない。表だけ既存の CSS 変数で最小限のスタイルを足す。
//!
//! ユーザー由来の値は出さない（日付・チーム・数値のみ）が、チーム名等の文字列も
//! 念のため [`escape`] を通す。

use chrono::{FixedOffset, Utc};

use crate::rating::MatchResult;
use crate::registration::html::{escape, page};

use super::repository::{MatchHistoryEntry, StatusSummary};

/// 履歴に並べる最大件数（「直近 50 件程度」）。
pub const HISTORY_LIMIT: i64 = 50;

/// 表示に使う標準時（JST）。DB は UTC で持つので、見せるときだけ寄せる。
const JST: FixedOffset = match FixedOffset::east_opt(9 * 3600) {
    Some(offset) => offset,
    None => panic!("JST offset is valid"),
};

/// 増減の符号付き表示。`+11` / `-12` / `0`。
pub fn format_delta(delta: f64) -> String {
    let rounded = delta.round();
    if rounded > 0.0 {
        format!("+{rounded:.0}")
    } else {
        format!("{rounded:.0}")
    }
}

/// 増減の色分けクラス。
fn delta_class(delta: f64) -> &'static str {
    let rounded = delta.round();
    if rounded > 0.0 {
        "delta-win"
    } else if rounded < 0.0 {
        "delta-loss"
    } else {
        "delta-draw"
    }
}

fn result_label(result: MatchResult) -> &'static str {
    match result {
        MatchResult::Won => "勝ち",
        MatchResult::Lost => "負け",
        MatchResult::Drawn => "引き分け",
    }
}

fn result_class(result: MatchResult) -> &'static str {
    match result {
        MatchResult::Won => "delta-win",
        MatchResult::Lost => "delta-loss",
        MatchResult::Drawn => "delta-draw",
    }
}

/// 画面が描画するデータ。レートは既存の導出経路（`display_rating`）から渡す。
#[derive(Debug, Clone)]
pub struct StatusView {
    /// 現在のレート（表示スケール）。
    pub rating: f64,
    pub summary: StatusSummary,
    /// 直近の履歴（新しい順）。
    pub history: Vec<MatchHistoryEntry>,
}

/// 表用の追加スタイル。色はすべて登録画面と同じ変数から取る。
const TABLE_STYLES: &str = r#"
<style>
.stats { display: flex; flex-wrap: wrap; gap: 12px; margin: 24px 0 8px; }
.stat { flex: 1 1 140px; border: 1px solid var(--border); border-radius: 8px;
        background: var(--surface); padding: 14px 18px; }
.stat .value { font-size: 1.6rem; font-weight: 800; line-height: 1.2; }
.stat .label { color: var(--muted); font-size: 0.78rem; font-weight: 700;
               text-transform: uppercase; letter-spacing: 0.04em; }
.table-wrap { overflow-x: auto; margin-top: 16px; }
table.history { width: 100%; border-collapse: collapse; font-size: 0.92rem; }
table.history th { text-align: left; color: var(--muted); font-weight: 650;
                   border-bottom: 1px solid var(--border); padding: 8px 10px;
                   white-space: nowrap; }
table.history td { border-bottom: 1px solid var(--border); padding: 8px 10px;
                   white-space: nowrap; }
.delta-win { color: var(--accent); font-weight: 750; }
.delta-loss { color: var(--danger); font-weight: 750; }
.delta-draw { color: var(--muted); font-weight: 750; }
.badge { display: inline-block; border-radius: 999px; padding: 1px 10px;
         font-size: 0.8rem; font-weight: 700; border: 1px solid var(--border); }
.badge-red { border-color: #b3555e; color: #ff9aa2; }
.badge-blue { border-color: #4a63b0; color: #9db4ff; }
</style>
"#;

/// 戦績画面。
pub fn status_page(view: &StatusView) -> String {
    let mut rows = String::new();
    for entry in &view.history {
        let played_at = entry.played_at.with_timezone(&JST).format("%Y-%m-%d %H:%M");
        let badge = if entry.team == "red" {
            "badge-red"
        } else {
            "badge-blue"
        };
        rows.push_str(&format!(
            "<tr><td>{played_at}</td>\
             <td><span class=\"badge {badge}\">{team}</span></td>\
             <td class=\"{result_class}\">{result}</td>\
             <td class=\"{delta_class}\">{delta}</td>\
             <td>{rating_at_start:.0}</td></tr>\n",
            played_at = played_at,
            badge = badge,
            team = escape(&entry.team),
            result_class = result_class(entry.result),
            result = result_label(entry.result),
            delta_class = delta_class(entry.delta),
            delta = format_delta(entry.delta),
            rating_at_start = entry.rating_at_start,
        ));
    }
    let history = if rows.is_empty() {
        String::from(
            "<p class=\"muted\">まだ確定した試合がありません。試合が確定すると、ここに現れます。</p>\n",
        )
    } else {
        format!(
            "<div class=\"table-wrap\"><table class=\"history\">\n\
             <thead><tr><th>日時</th><th>チーム</th><th>結果</th><th>増減</th>\
             <th>試合時点のレート</th></tr></thead>\n<tbody>\n{rows}</tbody>\n</table></div>\n"
        )
    };

    page(
        "自分の戦績",
        &format!(
            "{TABLE_STYLES}\
             <section aria-labelledby=\"status-title\">\n\
             <p class=\"eyebrow\">Player status</p>\n\
             <h1 id=\"status-title\">自分の戦績</h1>\n\
             <p class=\"lead\">表示されているのは <strong>あなたの</strong> レートと試合履歴だけです。\
             他の参加者のレートや戦績はここには出ません。</p>\n\
             <div class=\"stats\">\n\
             <div class=\"stat\"><div class=\"value\">{rating:.0}</div><div class=\"label\">現在のレート</div></div>\n\
             <div class=\"stat\"><div class=\"value\">{matches}</div><div class=\"label\">試合数</div></div>\n\
             <div class=\"stat\"><div class=\"value\">{wins}<span class=\"muted\"> / {losses}</span></div>\
             <div class=\"label\">勝ち / 負け</div></div>\n\
             </div>\n\
             <p class=\"fine-print\">引き分け {draws} 件を含む一覧は確定した試合のみ（直近 {limit} 件）です。\
             増減はその試合の開始時点のレートから決まります（07 §2-2）。</p>\n\
             {history}\
             <div class=\"actions\">\n\
             <a class=\"button button-primary\" href=\"/queue\">キュー画面へ</a>\n\
             <a class=\"button button-secondary\" href=\"/register\">登録内容を変更</a>\n\
             </div>\n\
             </section>\n",
            rating = view.rating,
            matches = view.summary.matches,
            wins = view.summary.wins,
            losses = view.summary.losses,
            draws = view.summary.draws,
            limit = HISTORY_LIMIT,
            history = history,
        ),
    )
}

/// UTC のタイムスタンプを JST の表示文字列へ寄せる（テストからフォーマットを検証するため公開）。
pub fn display_time(when: chrono::DateTime<Utc>) -> String {
    when.with_timezone(&JST)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::lifecycle::FormationId;

    fn entry(minutes: i64, team: &str, result: MatchResult, delta: f64) -> MatchHistoryEntry {
        MatchHistoryEntry {
            played_at: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap()
                + chrono::TimeDelta::minutes(minutes),
            team: team.to_owned(),
            result,
            delta,
            rating_at_start: 1500.0,
            formation_id: FormationId(Uuid::from_u128(1)),
        }
    }

    fn view(history: Vec<MatchHistoryEntry>) -> StatusView {
        StatusView {
            rating: 1522.0,
            summary: StatusSummary {
                matches: 3,
                wins: 2,
                losses: 1,
                draws: 0,
            },
            history,
        }
    }

    #[test]
    fn deltas_carry_their_sign() {
        assert_eq!(format_delta(11.0), "+11");
        assert_eq!(format_delta(-12.0), "-12");
        assert_eq!(format_delta(0.0), "0");
        assert_eq!(format_delta(11.6), "+12");
    }

    #[test]
    fn the_page_shows_rating_counts_and_rows() {
        let rendered = status_page(&view(vec![
            entry(120, "red", MatchResult::Won, 11.0),
            entry(60, "blue", MatchResult::Lost, -12.0),
            entry(0, "red", MatchResult::Drawn, 0.0),
        ]));
        assert!(rendered.starts_with("<!doctype html>"));
        assert!(rendered.contains("1522"));
        assert!(rendered.contains(">+11<"));
        assert!(rendered.contains(">-12<"));
        assert!(rendered.contains("勝ち"));
        assert!(rendered.contains("負け"));
        assert!(rendered.contains("引き分け"));
        // JST 表示: 12:00 UTC -> 21:00 JST
        assert!(rendered.contains("2026-09-12 21:00"), "{rendered}");
        assert!(rendered.contains("badge-red"));
        assert!(rendered.contains("badge-blue"));
    }

    #[test]
    fn an_empty_history_says_so_without_a_table() {
        let rendered = status_page(&view(vec![]));
        assert!(rendered.contains("まだ確定した試合がありません"));
        assert!(!rendered.contains("<table"));
    }

    #[test]
    fn the_page_keeps_the_shared_dark_theme_and_mobile_layout() {
        let rendered = status_page(&view(vec![]));
        // 登録画面と同じ外枠・配色
        assert!(rendered.contains("color-scheme: dark"));
        assert!(rendered.contains("var(--accent)"));
        assert!(rendered.contains("@media (max-width: 480px)"));
        // 横スクロールで 375px でも崩さない
        assert!(rendered.contains("overflow-x: auto"));
    }

    #[test]
    fn the_page_never_shows_other_players() {
        // 画面に入るのは数値と日付とチームだけ。プレイヤー名の差し込み口が無いこと
        // （`--discord` は共有 CSS の変数名なので文字列検査からは除く）
        let rendered = status_page(&view(vec![entry(0, "red", MatchResult::Won, 11.0)]));
        assert!(!rendered.contains("trainer"), "{rendered}");
        assert!(!rendered.contains("trainer_name"), "{rendered}");
        assert!(!rendered.contains("global_name"), "{rendered}");
    }

    #[test]
    fn display_time_shifts_utc_to_jst() {
        let utc = Utc.with_ymd_and_hms(2026, 9, 12, 16, 30, 0).unwrap();
        assert_eq!(display_time(utc), "2026-09-13 01:30");
    }
}
