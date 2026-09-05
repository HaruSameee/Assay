//! 登録フォームの最小限の HTML。テンプレートエンジンは入れない。
//!
//! サイト名・ロゴ・配色は設定値として外出しする方針（03-技術仕様 §8）なので、ここには
//! 固有名を書かない。文言だけで、見た目は最小限にとどめる。
//!
//! ユーザー由来の値（トレーナー名、Discord の表示名）は必ず [`escape`] を通す。

use crate::lifecycle::FormationId;

use super::gate::QueueEligibility;
use super::model::{PlayerProfile, Rank};

/// HTML の特殊文字を実体参照にする。属性値にもテキストにも使える。
pub fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// ページの外枠。
pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <style>\n\
         body{{font-family:system-ui,sans-serif;line-height:1.7;max-width:40rem;margin:2rem auto;padding:0 1rem}}\n\
         label{{display:block;margin:1rem 0 .25rem;font-weight:600}}\n\
         input,select{{font:inherit;padding:.4rem;width:100%;box-sizing:border-box}}\n\
         button{{font:inherit;margin-top:1.5rem;padding:.5rem 1.2rem}}\n\
         .note{{color:#555;font-size:.9rem}}\n\
         .blocked{{border-left:4px solid #b00;padding-left:.8rem}}\n\
         </style>\n</head>\n<body>\n{body}\n</body>\n</html>\n",
        title = escape(title),
    )
}

/// 未ログインのときの入口。
pub fn login_page(login_path: &str) -> String {
    page(
        "ログイン",
        &format!(
            "<h1>ログイン</h1>\n\
             <p>Discord アカウントでログインします。要求するのはユーザー識別（identify）だけで、\
             メールアドレスや参加サーバーの一覧は取得しません。</p>\n\
             <p><a href=\"{path}\">Discord でログイン</a></p>\n",
            path = escape(login_path),
        ),
    )
}

/// 登録フォーム。既に申告があれば初期値に入れる。
pub fn registration_form(
    action: &str,
    discord_display_name: &str,
    profile: Option<&PlayerProfile>,
    error: Option<&str>,
) -> String {
    let trainer_name = profile
        .and_then(|profile| profile.trainer_name.as_deref())
        .unwrap_or_default();
    let declared = profile.and_then(|profile| profile.latest_declaration.as_ref());
    let current_rank = declared.and_then(|declaration| declaration.rank);
    let current_rating = declared
        .and_then(|declaration| declaration.rating)
        .map(|rating| rating.to_string())
        .unwrap_or_default();

    let mut options = String::new();
    for rank in Rank::ALL {
        let selected = if Some(rank) == current_rank {
            " selected"
        } else {
            ""
        };
        options.push_str(&format!(
            "<option value=\"{value}\"{selected}>{label}</option>\n",
            value = escape(rank.as_str()),
            label = escape(rank.display_name()),
        ));
    }

    let error_block = error
        .map(|message| format!("<p class=\"blocked\">{}</p>\n", escape(message)))
        .unwrap_or_default();
    let registered_note = if profile.is_some_and(PlayerProfile::is_registered) {
        "<p class=\"note\">登録済みです。内容を変えて再送信すると、新しい申告として記録されます\
         （過去の申告は残ります）。</p>\n"
    } else {
        ""
    };

    page(
        "登録",
        &format!(
            "<h1>登録</h1>\n\
             <p class=\"note\">Discord: {discord}</p>\n\
             {registered_note}{error_block}\
             <form method=\"post\" action=\"{action}\">\n\
             <label for=\"trainer_name\">トレーナー名（ゲーム内名）</label>\n\
             <input id=\"trainer_name\" name=\"trainer_name\" maxlength=\"64\" required value=\"{trainer}\">\n\
             <p class=\"note\">他の参加者から見えます。トレーナー情報の公開が参加条件です。</p>\n\
             <label for=\"rank\">本家ランク</label>\n\
             <select id=\"rank\" name=\"rank\" required>\n{options}</select>\n\
             <label for=\"rating\">本家レート（マスター・レジェンドのみ）</label>\n\
             <input id=\"rating\" name=\"rating\" inputmode=\"numeric\" value=\"{rating}\">\n\
             <p class=\"note\">参加資格は{entry}以上です。未満でも登録は受け付けて記録します。\
             上がったら再申告してください。</p>\n\
             <button type=\"submit\">送信</button>\n</form>\n",
            discord = escape(discord_display_name),
            action = escape(action),
            trainer = escape(trainer_name),
            rating = escape(&current_rating),
            entry = escape(Rank::ENTRY_REQUIREMENT.display_name()),
        ),
    )
}

/// 登録後の確認ページ。キューに入れるかどうかを、2つのゲートごとに説明する。
pub fn registration_result(profile: &PlayerProfile, eligibility: &QueueEligibility) -> String {
    let trainer = profile.trainer_name.as_deref().unwrap_or("(未設定)");
    let declared = profile
        .latest_declaration
        .as_ref()
        .map(|declaration| {
            let rank = declaration
                .rank
                .map(Rank::display_name)
                .unwrap_or("(不明なランク)");
            match declaration.rating {
                Some(rating) => format!("{rank} / {rating}"),
                None => rank.to_owned(),
            }
        })
        .unwrap_or_else(|| "(申告なし)".to_owned());

    let status = if eligibility.is_allowed() {
        "<p>キューに参加できます。</p>\n".to_owned()
    } else {
        let mut block =
            String::from("<div class=\"blocked\">\n<p>いまはキューに入れません。</p>\n<ul>\n");
        for message in eligibility.messages() {
            block.push_str(&format!("<li>{}</li>\n", escape(&message)));
        }
        block.push_str("</ul>\n</div>\n");
        block
    };

    page(
        "登録内容",
        &format!(
            "<h1>登録内容</h1>\n\
             <dl>\n<dt>トレーナー名</dt><dd>{trainer}</dd>\n\
             <dt>直近の申告</dt><dd>{declared}</dd>\n</dl>\n\
             {status}\
             <p><a href=\"/register\">申告し直す</a></p>\n",
            trainer = escape(trainer),
            declared = escape(&declared),
        ),
    )
}

/// 失敗ページ。理由は短く、内部情報は載せない。
pub fn error_page(title: &str, message: &str) -> String {
    page(
        title,
        &format!(
            "<h1>{title}</h1>\n<p>{message}</p>\n<p><a href=\"/\">最初に戻る</a></p>\n",
            title = escape(title),
            message = escape(message),
        ),
    )
}

/// 未報告の formation を並べる（デバッグ・スタッフ向けの短い表示）。
pub fn pending_reports_list(pending: &[FormationId]) -> String {
    let mut list = String::from("<ul>\n");
    for formation in pending {
        list.push_str(&format!("<li>{}</li>\n", escape(&formation.to_string())));
    }
    list.push_str("</ul>\n");
    list
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::lifecycle::PlayerId;
    use crate::registration::gate::RegistrationBlock;
    use crate::registration::model::{Declaration, PlayerStatus};

    fn profile(trainer: &str) -> PlayerProfile {
        PlayerProfile {
            player_id: PlayerId(Uuid::from_u128(1)),
            discord_user_id: "1001".into(),
            trainer_name: Some(trainer.to_owned()),
            status: Some(PlayerStatus::Active),
            registered_at: Some(Utc.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap()),
            latest_declaration: Some(Declaration {
                declared_at: Utc.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap(),
                rank: Some(Rank::Legend),
                rating: Some(1500),
            }),
        }
    }

    #[test]
    fn escaping_covers_every_html_metacharacter() {
        assert_eq!(
            escape(r#"<script>alert("x" & 'y')</script>"#),
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#39;y&#39;)&lt;/script&gt;"
        );
        assert_eq!(escape("ふつうの名前"), "ふつうの名前");
    }

    #[test]
    fn a_hostile_trainer_name_cannot_break_out_of_the_form() {
        let hostile = r#"" onfocus="alert(1)"#;
        let rendered = registration_form("/register", "discord", Some(&profile(hostile)), None);
        assert!(!rendered.contains("onfocus=\"alert"), "{rendered}");
        assert!(
            rendered.contains("&quot; onfocus=&quot;alert(1)"),
            "{rendered}"
        );
    }

    #[test]
    fn the_form_lists_every_rank_and_preselects_the_latest_declaration() {
        let rendered = registration_form("/register", "名前", Some(&profile("トレーナー")), None);
        for rank in Rank::ALL {
            assert!(
                rendered.contains(&format!("value=\"{}\"", rank.as_str())),
                "{} missing",
                rank.as_str()
            );
            assert!(rendered.contains(rank.display_name()), "{rank}");
        }
        assert!(rendered.contains("value=\"legend\" selected"), "{rendered}");
        assert!(rendered.contains("value=\"1500\""), "{rendered}");
        assert!(rendered.contains("value=\"トレーナー\""), "{rendered}");
    }

    #[test]
    fn the_form_works_for_a_first_time_visitor() {
        let rendered = registration_form(
            "/register",
            "名前",
            None,
            Some("レートを数字で入れてください"),
        );
        assert!(rendered.contains("レートを数字で入れてください"));
        assert!(!rendered.contains("selected"), "nothing preselected");
        assert!(!rendered.contains("登録済みです"));
    }

    #[test]
    fn the_result_page_explains_each_gate_separately() {
        let eligibility = QueueEligibility {
            registration: Some(RegistrationBlock::RankBelowEntry {
                declared: Some(Rank::Master),
            }),
            pending_reports: vec![crate::lifecycle::FormationId(Uuid::from_u128(9))],
        };
        let rendered = registration_result(&profile("トレーナー"), &eligibility);
        assert!(rendered.contains("いまはキューに入れません"));
        assert!(rendered.contains("参加資格は"), "{rendered}");
        assert!(rendered.contains("未報告の試合が1件"), "{rendered}");

        let allowed = registration_result(&profile("トレーナー"), &QueueEligibility::default());
        assert!(allowed.contains("キューに参加できます"));
        assert!(!allowed.contains("入れません"));
    }

    #[test]
    fn every_page_is_a_complete_document() {
        for rendered in [
            login_page("/auth/login"),
            registration_form("/register", "名前", None, None),
            registration_result(&profile("t"), &QueueEligibility::default()),
            error_page("エラー", "やり直してください"),
        ] {
            assert!(rendered.starts_with("<!doctype html>"), "{rendered}");
            assert!(rendered.contains("<meta charset=\"utf-8\">"));
            assert!(rendered.trim_end().ends_with("</html>"));
        }
    }

    #[test]
    fn the_pending_list_escapes_its_entries() {
        let rendered = pending_reports_list(&[crate::lifecycle::FormationId(Uuid::from_u128(1))]);
        assert!(rendered.contains("00000000-0000-0000-0000-000000000001"));
    }
}
