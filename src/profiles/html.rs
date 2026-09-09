//! プロフィールの HTML。登録・キュー画面と同じシェルとダークテーマを使う。
//!
//! **公開ビューが描けるのは [`PublicProfile`] にある値だけ。**担当ロールを非公開に
//! している人の値は型に入っていないので、ここからは出しようがない。

use crate::registration::Viewer;
use crate::registration::html::{escape, page_with_styles};
use crate::status::html::display_time;

use super::model::{MAX_BIO_CHARS, PublicProfile};
use super::repository::OwnProfile;
use crate::blocks::{BlockSearchResult, CurrentBlock};

const PROFILE_STYLES: &str = r#"
.profile-head { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 18px; align-items: center; margin-bottom: 26px; }
.avatar { width: 88px; height: 88px; border: 1px solid var(--border); border-radius: 50%; background: var(--surface); object-fit: cover; }
.profile-names { min-width: 0; }
.profile-names h1 { margin: 0 0 4px; overflow-wrap: anywhere; font-size: 1.6rem; }
.profile-names .trainer { color: var(--muted); overflow-wrap: anywhere; }
.profile-metrics { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1px; margin: 24px 0; background: var(--border); border: 1px solid var(--border); }
.profile-metrics .metric { min-width: 0; padding: 18px; background: var(--surface); }
.metric-label { display: block; margin-bottom: 3px; color: var(--muted); font-size: 0.82rem; }
.metric-value { display: block; overflow-wrap: anywhere; font-size: 1.35rem; font-weight: 750; font-variant-numeric: tabular-nums; }
.bio { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.role-tags { display: flex; flex-wrap: wrap; gap: 8px; margin: 10px 0 0; padding: 0; list-style: none; }
.role-tags li { border: 1px solid var(--border); border-radius: 999px; padding: 3px 12px; color: var(--muted); font-size: 0.85rem; }
.links { margin-top: 18px; }
.links a { overflow-wrap: anywhere; }
.role-choice { display: flex; flex-wrap: wrap; gap: 14px; margin: 8px 0 0; }
.role-choice label { display: inline-flex; gap: 6px; align-items: center; }
.char-count { color: var(--muted); font-size: 0.82rem; }
.block-list, .block-search-results { margin: 0; padding: 0; list-style: none; }
.block-list li, .block-search-results li { display: flex; flex-wrap: wrap; align-items: center;
  justify-content: space-between; gap: 12px; padding: 12px 0; border-bottom: 1px solid var(--border); }
.block-person { min-width: 0; overflow-wrap: anywhere; }
.block-meta { color: var(--muted); font-size: 0.875rem; }
.block-list form, .block-search-results form { margin: 0; }
.block-list button, .block-search-results button { min-height: 38px; padding: 7px 12px; }
@media (max-width: 600px) {
  .profile-head { grid-template-columns: 1fr; }
  .profile-metrics { grid-template-columns: 1fr; }
}
"#;

pub struct EditPage<'a> {
    pub viewer: Viewer,
    pub player_id: uuid::Uuid,
    pub own: &'a OwnProfile,
    pub known_roles: &'a [String],
    pub current_blocks: &'a [CurrentBlock],
    pub block_query: &'a str,
    pub search_results: &'a [BlockSearchResult],
    pub error: Option<&'a str>,
    pub saved: bool,
}

/// 公開プロフィール。**担当ロールは型に入っているときだけ描かれる。**
pub fn public_page(viewer: Viewer, profile: &PublicProfile) -> String {
    let trainer = profile.trainer_name.as_deref().map_or_else(
        || String::from("<p class=\"trainer\">トレーナー名は未登録です</p>\n"),
        |name| format!("<p class=\"trainer\">{}</p>\n", escape(name)),
    );
    let bio = if profile.bio.trim().is_empty() {
        String::new()
    } else {
        format!(
            "<section class=\"section\"><h2>自己紹介</h2>\n<p class=\"bio\">{}</p></section>\n",
            escape(&profile.bio)
        )
    };
    // **公開を選んでいるときだけ値がある。**非公開・未設定はここに来ない
    let roles = profile.main_roles.as_ref().map_or_else(String::new, |roles| {
        let tags = roles
            .iter()
            .map(|role| format!("<li>{}</li>", escape(role_label(role))))
            .collect::<String>();
        format!(
            "<section class=\"section\"><h2>担当ロール</h2>\n<ul class=\"role-tags\">{tags}</ul></section>\n"
        )
    });
    let links = profile.x_id.as_ref().map_or_else(String::new, |x_id| {
        // 検証済みの `^[A-Za-z0-9_]{1,15}$` なので URL に埋めてよい
        format!(
            "<p class=\"links\">X: <a href=\"https://x.com/{id}\" rel=\"nofollow noopener noreferrer\" target=\"_blank\">@{id}</a></p>\n",
            id = escape(x_id)
        )
    });
    let body = format!(
        "<section class=\"profile\" aria-labelledby=\"profile-title\">\n\
         <div class=\"profile-head\">\n\
         <img class=\"avatar\" src=\"{avatar}\" alt=\"\" width=\"88\" height=\"88\" loading=\"lazy\" referrerpolicy=\"no-referrer\">\n\
         <div class=\"profile-names\"><p class=\"eyebrow\">Player</p>\n\
         <h1 id=\"profile-title\">{name}</h1>\n{trainer}</div>\n</div>\n\
         <div class=\"profile-metrics\">\n\
         <div class=\"metric\"><span class=\"metric-label\">レート</span><strong class=\"metric-value\">{rating}</strong></div>\n\
         <div class=\"metric\"><span class=\"metric-label\">確定した試合</span><strong class=\"metric-value\">{matches}</strong></div>\n\
         </div>\n{bio}{roles}{links}</section>\n",
        avatar = escape(&profile.avatar_url),
        name = escape(&profile.display_name),
        rating = profile.rating.round() as i64,
        matches = profile.matches,
    );
    page_with_styles(viewer, &profile.display_name, &body, PROFILE_STYLES)
}

/// ロールの表示名。**内部識別子を画面に出さない。**
fn role_label(role: &str) -> &str {
    match role {
        "top_carry" => "上キャリー",
        "top_support" => "サポート",
        "jungle" => "中央",
        "bot_carry" => "下キャリー",
        "bot_tank" => "タンク",
        other => other,
    }
}

/// 編集画面。**本人にだけ出す。**公開可否のチェックボックスもここにしか無い。
pub fn edit_page(page: EditPage<'_>) -> String {
    let EditPage {
        viewer,
        player_id,
        own,
        known_roles,
        current_blocks,
        block_query,
        search_results,
        error,
        saved,
    } = page;
    let notice = if saved {
        "<div class=\"notice\" role=\"status\"><p><strong>保存しました。</strong></p></div>\n"
            .to_owned()
    } else {
        String::new()
    };
    let error = error.map_or_else(String::new, |message| {
        format!(
            "<div class=\"error-summary\" role=\"alert\">{}</div>\n",
            escape(message)
        )
    });
    let checks = known_roles
        .iter()
        .map(|role| {
            let checked = if own.main_roles.iter().any(|chosen| chosen == role) {
                " checked"
            } else {
                ""
            };
            format!(
                "<label><input type=\"checkbox\" name=\"main_roles\" value=\"{role}\"{checked}> {label}</label>",
                role = escape(role),
                label = escape(role_label(role)),
            )
        })
        .collect::<String>();
    let public_checked = if own.main_roles_public {
        " checked"
    } else {
        ""
    };
    let block_form_key = uuid::Uuid::new_v4();
    let current_blocks = if current_blocks.is_empty() {
        "<p class=\"field-help\">現在ブロックしているプレイヤーはいません。</p>".to_owned()
    } else {
        let rows = current_blocks
            .iter()
            .map(|block| {
                let name = block
                    .display_name
                    .as_deref()
                    .or(block.trainer_name.as_deref())
                    .unwrap_or("表示名を取得できませんでした");
                let available = display_time(block.cooldown_ends_at());
                format!(
                    "<li><div class=\"block-person\"><strong>{name}</strong>\
                     <div class=\"block-meta\">解除可能: {available}</div></div>\
                     <form method=\"post\" action=\"/profile/blocks\">\
                     <input type=\"hidden\" name=\"action\" value=\"unblock\">\
                     <input type=\"hidden\" name=\"target\" value=\"{target}\">\
                     <input type=\"hidden\" name=\"key\" value=\"unblock:{form_key}:{target}\">\
                     <button class=\"button-secondary\" type=\"submit\"{disabled}>解除</button>\
                     </form></li>",
                    name = escape(name),
                    available = escape(&available),
                    target = escape(&block.blocked_player_id.to_string()),
                    form_key = block_form_key,
                    disabled = if block.can_unblock_at(chrono::Utc::now()) {
                        ""
                    } else {
                        " disabled"
                    },
                )
            })
            .collect::<String>();
        format!("<ul class=\"block-list\">{rows}</ul>")
    };
    let search_results = if block_query.trim().is_empty() {
        String::new()
    } else if search_results.is_empty() {
        "<p class=\"field-help\">一致するDiscord表示名は見つかりませんでした。</p>".to_owned()
    } else {
        let rows = search_results
            .iter()
            .map(|result| {
                format!(
                    "<li><div class=\"block-person\"><strong>{display_name}</strong>\
                     <div class=\"block-meta\">トレーナー名: {trainer_name}</div></div>\
                     <form method=\"post\" action=\"/profile/blocks\">\
                     <input type=\"hidden\" name=\"action\" value=\"block\">\
                     <input type=\"hidden\" name=\"target\" value=\"{target}\">\
                     <input type=\"hidden\" name=\"key\" value=\"block:{form_key}:{target}\">\
                     <button class=\"button-secondary\" type=\"submit\">ブロック</button>\
                     </form></li>",
                    display_name = escape(&result.display_name),
                    trainer_name = escape(result.trainer_name.as_deref().unwrap_or("未設定")),
                    target = escape(&result.player_id.to_string()),
                    form_key = block_form_key,
                )
            })
            .collect::<String>();
        format!("<ul class=\"block-search-results\">{rows}</ul>")
    };

    let body = format!(
        "<section aria-labelledby=\"edit-title\">\n<p class=\"eyebrow\">Profile</p>\n\
         <h1 id=\"edit-title\">プロフィールの編集</h1>\n\
         <p class=\"lead\">ここで保存した内容は、\
         <a href=\"/players/{player_id}\">公開プロフィール</a>に出ます。</p>\n\
         {notice}{error}\
         <form method=\"post\" action=\"/profile\">\n\
         <div class=\"field\"><label for=\"bio\">自己紹介</label>\n\
         <p class=\"field-help char-count\">{max}文字まで。</p>\n\
         <textarea id=\"bio\" name=\"bio\" rows=\"5\" maxlength=\"{max}\">{bio}</textarea></div>\n\
         <div class=\"field\"><label for=\"x_id\">X の ID</label>\n\
         <p class=\"field-help\">@ は入れずに、半角英数字とアンダースコアで 1〜15 文字。</p>\n\
         <input id=\"x_id\" name=\"x_id\" type=\"text\" maxlength=\"15\" value=\"{x_id}\"></div>\n\
         <fieldset class=\"field\"><legend>担当ロール</legend>\n\
         <div class=\"role-choice\">{checks}</div>\n\
         <p class=\"field-help\"><label><input type=\"checkbox\" name=\"main_roles_public\" value=\"1\"{public_checked}> \
         担当ロールを公開プロフィールに表示する</label></p>\n\
         <p class=\"field-help\">公開しない場合、公開プロフィールには\
         <strong>担当ロールの欄そのものが出ません</strong>（未設定の人と区別できません）。</p>\n\
         </fieldset>\n\
         <div class=\"actions\"><button type=\"submit\">保存する</button>\
         <a class=\"button button-secondary\" href=\"/players/{player_id}\">公開プロフィールを見る</a></div>\n\
         </form>\
         <section class=\"section\" aria-labelledby=\"blocks-title\">\n\
         <h2 id=\"blocks-title\">味方ブロック</h2>\
         <p class=\"field-help\">指定したプレイヤーと味方チームにならないようにします。相手チームには影響せず、相手に通知されません。</p>\
         {current_blocks}\
         <form method=\"get\" action=\"/profile\">\
         <div class=\"field\"><label for=\"block_query\">Discord表示名で追加</label>\
         <p class=\"field-help\">表示名の一部を入力して選択します。プレイヤーIDを入力する必要はありません。</p>\
         <input id=\"block_query\" name=\"block_query\" type=\"search\" value=\"{block_query}\" autocomplete=\"off\"></div>\
         <div class=\"actions\"><button class=\"button-secondary\" type=\"submit\">検索</button></div>\
         </form>\
         {search_results}\
         </section></section>\n",
        max = MAX_BIO_CHARS,
        bio = escape(&own.bio),
        x_id = escape(own.x_id.as_deref().unwrap_or("")),
        current_blocks = current_blocks,
        block_query = escape(block_query),
        search_results = search_results,
    );
    page_with_styles(viewer, "プロフィールの編集", &body, PROFILE_STYLES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> PublicProfile {
        PublicProfile {
            player_id: uuid::Uuid::nil(),
            display_name: "トレーナー".to_owned(),
            avatar_url: "https://cdn.discordapp.com/embed/avatars/1.png".to_owned(),
            trainer_name: Some("なまえ".to_owned()),
            rating: 1507.0,
            matches: 3,
            bio: String::new(),
            x_id: None,
            main_roles: None,
        }
    }

    /// **持ち込まれた文字列が HTML として解釈されないこと。**
    #[test]
    fn hostile_input_never_reaches_the_markup() {
        let hostile = "<script>alert('x')</script>\"><img src=x onerror=alert(1)>";
        let rendered = public_page(
            Viewer::ANONYMOUS,
            &PublicProfile {
                display_name: hostile.to_owned(),
                trainer_name: Some(hostile.to_owned()),
                bio: hostile.to_owned(),
                main_roles: Some(vec![hostile.to_owned()]),
                ..profile()
            },
        );
        // 生のタグとして出ていないこと（`=` や `(` は実体参照にならないので、
        // 「危ない文字列が含まれるか」ではなく**タグになっているか**で見る）
        assert!(!rendered.contains("<script>"), "{rendered}");
        assert!(!rendered.contains("<img src=x"), "{rendered}");
        assert!(!rendered.contains("\"><img"), "{rendered}");
        // エスケープされた形で出ている
        assert!(rendered.contains("&lt;script&gt;"), "{rendered}");
        assert!(rendered.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(rendered.contains("&quot;&gt;"), "{rendered}");
    }

    /// 非公開・未設定のときは、担当ロールの見出しごと出ない。
    #[test]
    fn the_role_section_is_absent_unless_published() {
        let hidden = public_page(Viewer::ANONYMOUS, &profile());
        assert!(!hidden.contains("担当ロール"), "{hidden}");
        let shown = public_page(
            Viewer::ANONYMOUS,
            &PublicProfile {
                main_roles: Some(vec!["jungle".to_owned()]),
                ..profile()
            },
        );
        assert!(shown.contains("担当ロール"), "{shown}");
        assert!(shown.contains("中央"), "{shown}");
        // 内部識別子は画面に出さない
        assert!(!shown.contains(">jungle<"), "{shown}");
    }

    /// アバターが無い人でも画面が壊れない（既定アバターの URL が入る）。
    #[test]
    fn a_player_without_an_avatar_still_renders() {
        let rendered = public_page(Viewer::ANONYMOUS, &profile());
        assert!(
            rendered.contains("https://cdn.discordapp.com/embed/avatars/1.png"),
            "{rendered}"
        );
        assert!(rendered.contains("class=\"avatar\""), "{rendered}");
        // 代替テキストは空（装飾。名前は隣に出ている）
        assert!(rendered.contains("alt=\"\""), "{rendered}");
    }

    /// X の欄は、設定されているときだけリンクになる。
    #[test]
    fn the_x_link_only_appears_when_set() {
        let without = public_page(Viewer::ANONYMOUS, &profile());
        assert!(!without.contains("x.com"), "{without}");
        let with = public_page(
            Viewer::ANONYMOUS,
            &PublicProfile {
                x_id: Some("someone".to_owned()),
                ..profile()
            },
        );
        assert!(with.contains("https://x.com/someone"), "{with}");
        assert!(with.contains("noopener"), "{with}");
    }

    #[test]
    fn block_controls_only_render_on_the_private_edit_page() {
        let target = uuid::Uuid::from_u128(42);
        let blocks = [CurrentBlock {
            blocked_player_id: target,
            display_name: Some("Global Name".to_owned()),
            trainer_name: Some("trainer".to_owned()),
            blocked_at: chrono::Utc::now(),
        }];
        let search = [BlockSearchResult {
            player_id: uuid::Uuid::from_u128(43),
            display_name: "Another Global Name".to_owned(),
            trainer_name: Some("other-trainer".to_owned()),
        }];
        let own = OwnProfile::default();
        let edit = edit_page(EditPage {
            viewer: Viewer::MEMBER,
            player_id: uuid::Uuid::nil(),
            own: &own,
            known_roles: &[],
            current_blocks: &blocks,
            block_query: "another",
            search_results: &search,
            error: None,
            saved: false,
        });
        assert!(edit.contains("action=\"/profile/blocks\""), "{edit}");
        assert!(edit.contains("Global Name"), "{edit}");
        assert!(edit.contains(&target.to_string()), "{edit}");

        let public = public_page(Viewer::ANONYMOUS, &profile());
        assert!(!public.contains("/profile/blocks"), "{public}");
        assert!(!public.contains("Global Name"), "{public}");
    }
}
