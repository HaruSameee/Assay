//! ランキングの HTML。他の画面と同じシェルとダークテーマを使う。
//!
//! **並べ替えの操作を置かない。**列見出しをリンクにもしない — 勝率で並べ替える
//! 手段を画面に用意しないことが仕様（5 試合では勝率が 6 通りしか取れない）。

use crate::registration::Viewer;
use crate::registration::html::{escape, page_with_styles};

use super::repository::{MINIMUM_MATCHES, RankingPage};

const RANKING_STYLES: &str = r#"
.ranking-note { margin: 0 0 22px; color: var(--muted); font-size: 0.9rem; }
.table-wrap { overflow-x: auto; margin: 24px 0; }
table.ranking { width: 100%; border-collapse: collapse; }
table.ranking th, table.ranking td { padding: 12px 14px; border-bottom: 1px solid var(--border); text-align: left; }
table.ranking th { color: var(--muted); font-size: 0.82rem; font-weight: 700; }
table.ranking td.num { text-align: right; font-variant-numeric: tabular-nums; }
table.ranking tr:hover td { background: var(--surface); }
.who { overflow-wrap: anywhere; }
.who .trainer { display: block; color: var(--muted); font-size: 0.82rem; }
.pager { display: flex; gap: 12px; align-items: center; justify-content: space-between; margin-top: 20px; }
.pager .position { color: var(--muted); font-size: 0.85rem; }
.empty { padding: 28px 0; color: var(--muted); }
"#;

/// ランキングの 1 ページ。
pub fn ranking_page(viewer: Viewer, page: &RankingPage) -> String {
    let rows = page
        .players
        .iter()
        .map(|player| {
            // 表示名が取れていなければトレーナー名、それも無ければ「（名前未登録）」
            let name = player
                .display_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("（名前未登録）");
            let trainer = player
                .trainer_name
                .as_deref()
                .map_or_else(String::new, |trainer| {
                    format!("<span class=\"trainer\">{}</span>", escape(trainer))
                });
            format!(
                "<tr><td class=\"num\">{rank}</td>\
                 <td class=\"who\"><a href=\"/players/{id}\">{name}</a>{trainer}</td>\
                 <td class=\"num\">{rating}</td><td class=\"num\">{matches}</td></tr>\n",
                rank = player.rank,
                id = escape(&player.player_id.to_string()),
                name = escape(name),
                rating = player.rating.round() as i64,
                matches = player.matches,
            )
        })
        .collect::<String>();

    let body = if page.total == 0 {
        format!(
            "<p class=\"empty\">まだ掲載条件を満たしている人がいません\
             （確定した試合が{MINIMUM_MATCHES}試合以上）。</p>\n"
        )
    } else {
        format!(
            "<div class=\"table-wrap\"><table class=\"ranking\">\n\
             <thead><tr><th>順位</th><th>プレイヤー</th><th>レート</th><th>試合</th></tr></thead>\n\
             <tbody>\n{rows}</tbody></table></div>\n{pager}",
            pager = pager(page),
        )
    };

    page_with_styles(
        viewer,
        "ランキング",
        &format!(
            "<section aria-labelledby=\"ranking-title\">\n<p class=\"eyebrow\">Ranking</p>\n\
             <h1 id=\"ranking-title\">ランキング</h1>\n\
             <p class=\"ranking-note\">\
             <strong>確定した試合が{MINIMUM_MATCHES}試合以上</strong>の人を、レートの高い順に並べています。\
             係争中・無効・不成立の試合は数に入りません。現在のシーズンの試合だけを数えます。</p>\n\
             {body}</section>\n"
        ),
        RANKING_STYLES,
    )
}

/// ページ送り。**並べ替えの操作は置かない。**
fn pager(page: &RankingPage) -> String {
    let previous = if page.page > 1 {
        format!(
            "<a class=\"button button-secondary\" href=\"/ranking?page={}\">前へ</a>",
            page.page - 1
        )
    } else {
        String::from("<span></span>")
    };
    let next = if page.page < page.pages {
        format!(
            "<a class=\"button button-secondary\" href=\"/ranking?page={}\">次へ</a>",
            page.page + 1
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranking::repository::RankedPlayer;
    use uuid::Uuid;

    fn player(rank: i64, rating: f64, name: &str) -> RankedPlayer {
        RankedPlayer {
            rank,
            player_id: Uuid::from_u128(rank as u128),
            display_name: Some(name.to_owned()),
            trainer_name: Some("トレーナー".to_owned()),
            rating,
            matches: 7,
        }
    }

    fn page() -> RankingPage {
        RankingPage {
            players: vec![player(1, 1560.0, "いちい"), player(2, 1540.0, "にい")],
            page: 1,
            pages: 3,
            total: 120,
        }
    }

    /// **勝率で並べ替える手段を画面に置かない。**列そのものも出さない。
    #[test]
    fn the_table_offers_no_way_to_sort_by_win_rate() {
        let rendered = ranking_page(Viewer::MEMBER, &page());
        assert!(!rendered.contains("勝率"), "{rendered}");
        assert!(!rendered.contains("sort"), "{rendered}");
        // 見出しはリンクになっていない
        assert!(!rendered.contains("<th><a"), "{rendered}");
        // 出す列は 順位 / プレイヤー / レート / 試合 の 4 つだけ
        assert_eq!(rendered.matches("<th>").count(), 4, "{rendered}");
    }

    /// 各行からプロフィールへ行ける。
    #[test]
    fn every_row_links_to_the_public_profile() {
        let rendered = ranking_page(Viewer::MEMBER, &page());
        for row in &page().players {
            assert!(
                rendered.contains(&format!("href=\"/players/{}\"", row.player_id)),
                "{rendered}"
            );
        }
    }

    /// 掲載条件を画面に書く。
    #[test]
    fn the_listing_condition_is_stated_on_the_page() {
        let rendered = ranking_page(Viewer::MEMBER, &page());
        assert!(rendered.contains("確定した試合が5試合以上"), "{rendered}");
        assert!(
            rendered.contains("係争中・無効・不成立の試合は数に入りません"),
            "{rendered}"
        );
    }

    /// 持ち込まれた表示名が HTML として解釈されない。
    #[test]
    fn hostile_names_never_reach_the_markup() {
        let hostile = "<img src=x onerror=alert(1)>";
        let rendered = ranking_page(
            Viewer::MEMBER,
            &RankingPage {
                players: vec![RankedPlayer {
                    display_name: Some(hostile.to_owned()),
                    trainer_name: Some(hostile.to_owned()),
                    ..player(1, 1500.0, "x")
                }],
                ..page()
            },
        );
        assert!(!rendered.contains("<img src=x"), "{rendered}");
        assert!(rendered.contains("&lt;img src=x"), "{rendered}");
    }

    /// 0 人でも画面が壊れず、条件が読める。
    #[test]
    fn an_empty_ranking_still_explains_the_condition() {
        let rendered = ranking_page(
            Viewer::ANONYMOUS,
            &RankingPage {
                players: Vec::new(),
                page: 1,
                pages: 1,
                total: 0,
            },
        );
        assert!(rendered.contains("まだ掲載条件を満たしている人がいません"));
        assert!(!rendered.contains("<table"), "{rendered}");
    }

    /// ページ送りは端で潰れる（1 ページ目に「前へ」は出ない）。
    #[test]
    fn the_pager_collapses_at_both_ends() {
        let first = ranking_page(Viewer::MEMBER, &page());
        assert!(!first.contains("前へ"), "{first}");
        assert!(first.contains("/ranking?page=2"), "{first}");

        let last = ranking_page(Viewer::MEMBER, &RankingPage { page: 3, ..page() });
        assert!(last.contains("/ranking?page=2"), "{last}");
        assert!(!last.contains("次へ"), "{last}");
    }
}
