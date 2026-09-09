//! 登録・認証画面の HTML。テンプレートエンジンや外部 CSS は使わない。
//!
//! ユーザー由来の値（トレーナー名、Discord の表示名）は必ず [`escape`] を通す。
//!
//! **共通ヘッダーはここ 1 か所にしかない**（[`site_header`]）。キュー・戦績・ランキング・
//! プロフィール・管理の各画面も [`page`] / [`page_with_styles`] を通るので、ヘッダーを
//! 直すと全画面が直る。画面ごとに `<header>` を書かないこと（片方だけ直る事故を防ぐ）。
//! 出す導線は [`Viewer`] だけで決まり、未ログインには何も出さない。
//!
//! **例外はマッチ画面だけ**（[`page_without_nav`]）。試合中に他画面へ移れる導線を作ると
//! 報告ボタンから離れる経路が増えるので、ナビゲーションを丸ごと出さない。
//!
//! **`/admin` へのリンクはどのヘッダーにも出さない。**非管理者の `/admin` は 404 を
//! 返す方針なので、リンクの有無がそのまま「この人は管理者か」の判別材料になる。

use crate::lifecycle::FormationId;

use super::gate::QueueEligibility;
use super::model::{PlayerProfile, Rank};
use super::viewer::Viewer;

const STYLES: &str = r#"
:root {
  color-scheme: dark;
  --bg: #101114;
  --surface: #191b20;
  --surface-strong: #22252c;
  --border: #343944;
  --text: #f4f6f8;
  --muted: #aeb4bf;
  --accent: #58d6a3;
  --accent-strong: #35b987;
  --discord: #5865f2;
  --discord-hover: #4752c4;
  --danger: #ff737b;
  --danger-bg: #2a191d;
  --focus: #8bb8ff;
}
* { box-sizing: border-box; }
html { background: var(--bg); }
body {
  min-width: 0;
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  font-size: 16px;
  line-height: 1.65;
}
a { color: var(--accent); text-underline-offset: 0.2em; }
a:hover { color: #8ce8c2; }
.shell { width: min(100% - 32px, 680px); margin: 0 auto; }
.site-header {
  display: flex;
  min-height: 72px;
  align-items: center;
  justify-content: space-between;
  border-bottom: 1px solid var(--border);
}
.brand { color: var(--text); font-size: 1.2rem; font-weight: 750; text-decoration: none; }
.brand:hover { color: var(--text); }
.context { color: var(--muted); font-size: 0.78rem; text-transform: uppercase; }
/* ヘッダーの導線。375px でも 1 行に収める。JS のメニューは作らない */
.site-nav { display: flex; min-width: 0; align-items: center; gap: 18px; }
.site-nav form { margin: 0; }
.site-nav a,
.site-nav button {
  min-height: 0;
  border: 0;
  border-radius: 0;
  padding: 0;
  background: none;
  color: var(--muted);
  font: inherit;
  font-size: 0.92rem;
  font-weight: 650;
  white-space: nowrap;
  text-decoration: none;
  cursor: pointer;
}
.site-nav a:hover, .site-nav button:hover { background: none; color: var(--text); }
main { padding: 48px 0 64px; }
.eyebrow { margin: 0 0 8px; color: var(--accent); font-size: 0.78rem; font-weight: 700; text-transform: uppercase; }
h1 { margin: 0 0 16px; font-size: 2rem; line-height: 1.25; }
h2 { margin: 0 0 12px; font-size: 1.1rem; line-height: 1.35; }
p { margin: 0 0 16px; }
.lead { max-width: 38rem; color: #dce0e6; font-size: 1.08rem; }
.muted, .field-help, .fine-print { color: var(--muted); }
.fine-print, .field-help { font-size: 0.875rem; }
.status-line { display: flex; align-items: center; gap: 8px; color: var(--muted); font-size: 0.9rem; }
.status-dot { width: 8px; height: 8px; flex: 0 0 8px; border-radius: 50%; background: var(--accent); }
.section { margin-top: 32px; padding-top: 24px; border-top: 1px solid var(--border); }
.requirement { margin: 28px 0; padding: 16px 0 16px 18px; border-left: 3px solid var(--accent); }
.requirement p:last-child { margin-bottom: 0; }
.button,
button {
  display: inline-flex;
  min-height: 46px;
  align-items: center;
  justify-content: center;
  gap: 8px;
  border: 1px solid transparent;
  border-radius: 6px;
  padding: 10px 18px;
  font: inherit;
  font-weight: 700;
  line-height: 1.3;
  text-align: center;
  text-decoration: none;
  cursor: pointer;
}
.button-primary, .actions button[type="submit"] { background: var(--accent); color: #07150f; }
.button-primary:hover, .actions button[type="submit"]:hover { background: var(--accent-strong); color: #07150f; }
.button-secondary { border-color: var(--border); background: var(--surface); color: var(--text); }
.button-secondary:hover { border-color: #596170; background: var(--surface-strong); color: var(--text); }
.button-discord { width: 100%; max-width: 320px; background: var(--discord); color: #fff; }
.button-discord:hover { background: var(--discord-hover); color: #fff; }
.actions { display: flex; flex-wrap: wrap; gap: 12px; margin-top: 28px; }
.actions form { margin: 0; }
.form-header { margin-bottom: 32px; }
.field { margin: 0 0 24px; }
label { display: block; margin-bottom: 7px; font-weight: 700; }
input,
select {
  width: 100%;
  min-height: 48px;
  border: 1px solid var(--border);
  border-radius: 5px;
  background: var(--surface);
  color: var(--text);
  padding: 10px 12px;
  font: inherit;
}
input::placeholder { color: #747b88; }
input:focus,
select:focus,
.button:focus-visible,
button:focus-visible,
a:focus-visible { outline: 3px solid var(--focus); outline-offset: 2px; }
input[aria-invalid="true"], select[aria-invalid="true"] { border-color: var(--danger); }
.field-help { margin: 7px 0 0; }
.field-error { margin: 7px 0 0; color: var(--danger); font-size: 0.9rem; font-weight: 650; }
.notice,
.error-summary,
.blocked {
  margin: 24px 0;
  padding: 14px 16px;
  border-left: 3px solid var(--accent);
  background: var(--surface);
}
.error-summary,
.blocked { border-left-color: var(--danger); background: var(--danger-bg); }
.notice p:last-child, .error-summary p:last-child, .blocked p:last-child { margin-bottom: 0; }
.details { margin: 24px 0 0; }
.details div { display: grid; grid-template-columns: minmax(120px, 0.65fr) minmax(0, 1.35fr); gap: 16px; padding: 15px 0; border-bottom: 1px solid var(--border); }
.details dt { color: var(--muted); }
.details dd { min-width: 0; margin: 0; overflow-wrap: anywhere; font-weight: 650; }
.blocked ul { margin: 8px 0 0; padding-left: 1.25rem; }
.site-footer { padding: 22px 0 32px; border-top: 1px solid var(--border); color: var(--muted); font-size: 0.78rem; }
@media (max-width: 480px) {
  .shell { width: min(100% - 24px, 680px); }
  .site-header { min-height: 60px; }
  .context { display: none; }
  .site-nav { gap: 14px; }
  .site-nav a, .site-nav button { font-size: 0.85rem; }
  main { padding: 34px 0 48px; }
  h1 { font-size: 1.7rem; }
  .actions { display: grid; grid-template-columns: 1fr; }
  .actions .button, .actions button, .actions form { width: 100%; }
  .details div { grid-template-columns: 1fr; gap: 3px; }
}
"#;

/// 登録フォームに表示する値。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrationFormValues {
    pub trainer_name: String,
    pub rank: String,
    pub rating: String,
}

impl RegistrationFormValues {
    pub fn from_profile(profile: &PlayerProfile) -> Self {
        let declaration = profile.latest_declaration.as_ref();
        Self {
            trainer_name: profile.trainer_name.clone().unwrap_or_default(),
            rank: declaration
                .and_then(|declaration| declaration.rank)
                .map(|rank| rank.as_str().to_owned())
                .unwrap_or_default(),
            rating: declaration
                .and_then(|declaration| declaration.rating)
                .map(|rating| rating.to_string())
                .unwrap_or_default(),
        }
    }
}

/// 項目ごとの入力エラー。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrationFormErrors {
    pub trainer_name: Option<&'static str>,
    pub rank: Option<&'static str>,
    pub rating: Option<&'static str>,
}

impl RegistrationFormErrors {
    pub fn is_empty(&self) -> bool {
        self.trainer_name.is_none() && self.rank.is_none() && self.rating.is_none()
    }
}

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

/// 全画面共通のヘッダー。**定義はここだけ。**画面ごとに複製しないこと。
///
/// - 左: サービス名。ログイン済みならキュー画面へ、未ログインなら入口へ。
/// - 右: 戦績とログアウト。**管理者にだけ**管理パネルを足す。
///
/// 未ログインには導線をひとつも出さない（行き先がどれもログインを要求するため）。
/// 代わりに、これまでどおりサービスの説明だけを添える。
fn site_header(viewer: Viewer) -> String {
    // ランキングは**公開ページ**なので、公開しているなら未ログインにも導線を出す。
    // 非公開のあいだはリンクを出さない（`/ranking` 自体もルート側が 404 を返す。
    // リンクが無いことはアクセス制御ではない）
    let ranking = if viewer.ranking_public() {
        "<a href=\"/ranking\">ランキング</a>\n"
    } else {
        ""
    };
    if !viewer.signed_in() {
        if ranking.is_empty() {
            return String::from(
                "<header class=\"site-header\">\n\
                 <a class=\"brand\" href=\"/\">Assay</a>\n\
                 <span class=\"context\">Pokemon UNITE community match</span>\n</header>\n",
            );
        }
        return format!(
            "<header class=\"site-header\">\n\
             <a class=\"brand\" href=\"/\">Assay</a>\n\
             <nav class=\"site-nav\" aria-label=\"メニュー\">\n{ranking}</nav>\n</header>\n"
        );
    }
    // **管理パネルへのリンクは出さない。**非管理者の `/admin` は 404 を返す方針なので、
    // リンクの有無がそのまま「この人は管理者か」の判別材料になってしまう。
    // 運営は URL を直接開く（アクセス制御は /admin 側の照合が正）
    format!(
        "<header class=\"site-header\">\n\
         <a class=\"brand\" href=\"/queue\">Assay</a>\n\
         <nav class=\"site-nav\" aria-label=\"メニュー\">\n\
         <a href=\"/status\">戦績</a>\n{ranking}\
         <a href=\"/profile\">プロフィール</a>\n\
         <form method=\"post\" action=\"/auth/logout\">\
         <button type=\"submit\">ログアウト</button></form>\n\
         </nav>\n</header>\n"
    )
}

/// ナビゲーションを持たないヘッダー。**マッチ画面専用。**
///
/// 試合中に他画面へ移れる導線を作らない（報告ボタンから離れる経路を増やさない）。
/// ブランド名もリンクにしない——押せばキュー画面へ飛べてしまう。
fn bare_header() -> String {
    String::from(
        "<header class=\"site-header\">\n\
         <span class=\"brand\">Assay</span>\n\
         <span class=\"context\">試合が終わるまでこの画面で操作します</span>\n</header>\n",
    )
}

/// ページの外枠。
pub fn page(viewer: Viewer, title: &str, body: &str) -> String {
    page_with_styles(viewer, title, body, "")
}

/// 登録画面と同じシェルに、画面固有の CSS を追加する。**共通ヘッダー付き。**
pub fn page_with_styles(viewer: Viewer, title: &str, body: &str, extra_styles: &str) -> String {
    shell(&site_header(viewer), title, body, extra_styles)
}

/// 共通ヘッダーを出さないページ。**マッチ画面だけがこれを使う。**
pub fn page_without_nav(title: &str, body: &str, extra_styles: &str) -> String {
    shell(&bare_header(), title, body, extra_styles)
}

fn shell(header: &str, title: &str, body: &str, extra_styles: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"theme-color\" content=\"#101114\">\n\
         <title>{title} | Assay</title>\n<style>{STYLES}{extra_styles}</style>\n</head>\n<body>\n\
         <div class=\"shell\">\n{header}\
         <main>{body}</main>\n\
         <footer class=\"site-footer\">Assay は非公式のコミュニティ運営サービスです。</footer>\n\
         </div>\n</body>\n</html>\n",
        title = escape(title),
        extra_styles = extra_styles,
    )
}

/// 未ログインのときの入口。
pub fn login_page(login_path: &str) -> String {
    // ここは定義上いつも未ログイン
    page(
        Viewer::ANONYMOUS,
        "ログイン",
        &format!(
            "<section aria-labelledby=\"login-title\">\n\
             <p class=\"eyebrow\">Pokemon UNITE community ranked match</p>\n\
             <h1 id=\"login-title\">Assay</h1>\n\
             <p class=\"lead\">決まった時間にソロで参加し、10人で試合を組む非公式ランクマッチです。\
             試合結果から独自レートを記録します。</p>\n\
             <div class=\"requirement\">\n<h2>参加資格</h2>\n\
             <p>ポケモンユナイト本家ランクが<strong>レジェンド以上</strong>であること。</p>\n</div>\n\
             <a class=\"button button-discord\" href=\"{path}\">Discord でログイン</a>\n\
             <p class=\"fine-print\">Discord から取得するのはユーザー識別情報（identify）のみです。\
             メールアドレスや参加サーバー一覧は取得しません。</p>\n\
             </section>\n",
            path = escape(login_path),
        ),
    )
}

fn field_error(id: &str, message: Option<&str>) -> String {
    message
        .map(|message| {
            format!(
                "<p id=\"{id}-error\" class=\"field-error\" role=\"alert\">{}</p>\n",
                escape(message)
            )
        })
        .unwrap_or_default()
}

fn field_attributes(id: &str, message: Option<&str>, help_id: &str) -> String {
    match message {
        Some(_) => format!(" aria-invalid=\"true\" aria-describedby=\"{help_id} {id}-error\""),
        None => format!(" aria-describedby=\"{help_id}\""),
    }
}

/// 登録フォーム。既存値を初期表示し、エラー時には送信値を保持する。
pub fn registration_form(
    viewer: Viewer,
    action: &str,
    profile: &PlayerProfile,
    values: &RegistrationFormValues,
    errors: &RegistrationFormErrors,
) -> String {
    let mut options = String::from("<option value=\"\">選択してください</option>\n");
    for rank in Rank::ALL {
        let selected = if values.rank == rank.as_str() {
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

    let registered = profile.is_registered();
    let heading = if registered {
        "登録内容を変更"
    } else {
        "参加登録"
    };
    let submit_label = if registered {
        "変更内容を保存"
    } else {
        "登録する"
    };
    let registration_note = if registered {
        "<div class=\"notice\"><p>登録済みです。変更を保存すると新しい申告として追記され、\
         過去の申告も記録に残ります。</p></div>\n"
    } else {
        ""
    };
    let error_summary = if errors.is_empty() {
        String::new()
    } else {
        String::from(
            "<div class=\"error-summary\" role=\"alert\"><p><strong>入力内容を確認してください。</strong></p>\
             <p>修正が必要な項目の下に理由を表示しています。</p></div>\n",
        )
    };

    let trainer_error = field_error("trainer_name", errors.trainer_name);
    let rank_error = field_error("rank", errors.rank);
    let rating_error = field_error("rating", errors.rating);
    let trainer_attributes =
        field_attributes("trainer_name", errors.trainer_name, "trainer_name-help");
    let rank_attributes = field_attributes("rank", errors.rank, "rank-help");
    let rating_attributes = field_attributes("rating", errors.rating, "rating-help");

    page(
        viewer,
        heading,
        &format!(
            "<section aria-labelledby=\"registration-title\">\n\
             <div class=\"form-header\">\n<p class=\"eyebrow\">Registration</p>\n\
             <h1 id=\"registration-title\">{heading}</h1>\n\
             <p class=\"lead\">参加資格とマッチングのために、現在のトレーナー情報を申告してください。</p>\n\
             <p class=\"status-line\"><span class=\"status-dot\" aria-hidden=\"true\"></span>Discord 認証済み</p>\n\
             </div>\n{registration_note}{error_summary}\
             <form method=\"post\" action=\"{action}\" novalidate>\n\
             <div class=\"field\">\n\
             <label for=\"trainer_name\">トレーナー名</label>\n\
             <input id=\"trainer_name\" name=\"trainer_name\" maxlength=\"64\" autocomplete=\"off\" required\
             value=\"{trainer}\"{trainer_attributes}>\n\
             <p id=\"trainer_name-help\" class=\"field-help\">ゲーム内で現在使っている名前を入力してください。\
             <strong>トレーナー名は他の参加者に公開されます。</strong></p>\n{trainer_error}</div>\n\
             <div class=\"field\">\n\
             <label for=\"rank\">本家ランク</label>\n\
             <select id=\"rank\" name=\"rank\" required{rank_attributes}>\n{options}</select>\n\
             <p id=\"rank-help\" class=\"field-help\">現在の本家ランクを選択してください。参加資格はレジェンド以上です。</p>\n\
             {rank_error}</div>\n\
             <div class=\"field\">\n\
             <label for=\"rating\">本家レート</label>\n\
             <input id=\"rating\" name=\"rating\" inputmode=\"numeric\" pattern=\"[0-9]*\" placeholder=\"例: 2600\"\
             value=\"{rating}\"{rating_attributes}>\n\
             <p id=\"rating-help\" class=\"field-help\">マスターまたはレジェンドの場合に、現在の数値を入力してください。\
             参加資格の確認と、運営による初期レート調整の参考にします。\
             Assay のレートには自動反映されず、初期値は一律1500です。</p>\n\
             {rating_error}</div>\n\
             <div class=\"actions\"><button type=\"submit\">{submit_label}</button></div>\n\
             </form>\n\
             </section>\n",
            heading = heading,
            registration_note = registration_note,
            error_summary = error_summary,
            action = escape(action),
            trainer = escape(&values.trainer_name),
            trainer_attributes = trainer_attributes,
            trainer_error = trainer_error,
            options = options,
            rank_attributes = rank_attributes,
            rank_error = rank_error,
            rating = escape(&values.rating),
            rating_attributes = rating_attributes,
            rating_error = rating_error,
            submit_label = submit_label,
        ),
    )
}

/// 登録後の確認ページ。キューに入れるかどうかを、3つのゲートごとに説明する。
pub fn registration_result(
    viewer: Viewer,
    profile: &PlayerProfile,
    eligibility: &QueueEligibility,
) -> String {
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
        "<div class=\"notice\"><p><strong>キューに参加できます。</strong></p>\
         <p class=\"muted\">準備ができたらキュー画面へ進んでください。</p></div>\n"
            .to_owned()
    } else {
        let mut block = String::from(
            "<div class=\"blocked\" role=\"status\">\n<p><strong>いまはキューに入れません。</strong></p>\n<ul>\n",
        );
        for message in eligibility.messages() {
            block.push_str(&format!("<li>{}</li>\n", escape(&message)));
        }
        block.push_str("</ul>\n</div>\n");
        block
    };

    page(
        viewer,
        "登録内容",
        &format!(
            "<section aria-labelledby=\"result-title\">\n\
             <p class=\"eyebrow\">Registration complete</p>\n\
             <h1 id=\"result-title\">登録内容</h1>\n\
             <p class=\"lead\">現在登録されている内容です。変更はいつでも新しい申告として保存できます。</p>\n\
             <dl class=\"details\">\n<div><dt>トレーナー名</dt><dd>{trainer}</dd></div>\n\
             <div><dt>直近の申告</dt><dd>{declared}</dd></div>\n</dl>\n\
             <p class=\"fine-print\">申告した本家レートは運営判断の参考情報です。\
             Assay のレートには自動反映されず、初期値は一律1500です。</p>\n\
             {status}\
             <div class=\"actions\">\n\
             <a class=\"button button-primary\" href=\"/queue\">キュー画面へ</a>\n\
             <a class=\"button button-secondary\" href=\"/register\">登録内容を変更</a>\n\
             </div>\n\
             </section>\n",
            trainer = escape(trainer),
            declared = escape(&declared),
        ),
    )
}

/// 失敗ページ。理由は短く、内部情報は載せない。
///
/// ヘッダーはわかっている範囲で出す。セッションを確認できなかった経路（ログイン失敗、
/// 期限切れ）は [`Viewer::ANONYMOUS`] で呼ぶこと。
pub fn error_page(viewer: Viewer, title: &str, message: &str) -> String {
    page(
        viewer,
        title,
        &format!(
            "<section aria-labelledby=\"error-title\">\n\
             <p class=\"eyebrow\">Request error</p>\n\
             <h1 id=\"error-title\">{title}</h1>\n\
             <div class=\"error-summary\"><p>{message}</p></div>\n\
             <div class=\"actions\"><a class=\"button button-primary\" href=\"/\">入口に戻る</a></div>\n\
             </section>\n",
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

    fn values(profile: &PlayerProfile) -> RegistrationFormValues {
        RegistrationFormValues::from_profile(profile)
    }

    fn unregistered_profile() -> PlayerProfile {
        PlayerProfile {
            player_id: PlayerId(Uuid::from_u128(2)),
            discord_user_id: "1002".into(),
            trainer_name: None,
            status: Some(PlayerStatus::Active),
            registered_at: None,
            latest_declaration: None,
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
        let profile = profile(hostile);
        let rendered = registration_form(
            Viewer::MEMBER,
            "/register",
            &profile,
            &values(&profile),
            &RegistrationFormErrors::default(),
        );
        assert!(!rendered.contains("onfocus=\"alert"), "{rendered}");
        assert!(
            rendered.contains("&quot; onfocus=&quot;alert(1)"),
            "{rendered}"
        );
    }

    #[test]
    fn the_form_lists_every_rank_and_preselects_the_latest_declaration() {
        let profile = profile("トレーナー");
        let rendered = registration_form(
            Viewer::MEMBER,
            "/register",
            &profile,
            &values(&profile),
            &RegistrationFormErrors::default(),
        );
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
        let profile = unregistered_profile();
        let submitted = RegistrationFormValues {
            trainer_name: "入力中".into(),
            rank: "legend".into(),
            rating: "not-a-number".into(),
        };
        let errors = RegistrationFormErrors {
            rating: Some("本家レートを数字で入力してください。"),
            ..RegistrationFormErrors::default()
        };
        let rendered =
            registration_form(Viewer::MEMBER, "/register", &profile, &submitted, &errors);
        assert!(rendered.contains("本家レートを数字で入力してください。"));
        assert!(rendered.contains("value=\"入力中\""));
        assert!(rendered.contains("value=\"not-a-number\""));
        assert!(rendered.contains("id=\"rating-error\""));
        assert!(rendered.contains("aria-invalid=\"true\""));
        assert!(!rendered.contains("登録済みです"));
    }

    #[test]
    fn the_form_discloses_trainer_name_visibility() {
        let profile = profile("トレーナー");
        let rendered = registration_form(
            Viewer::MEMBER,
            "/register",
            &profile,
            &values(&profile),
            &RegistrationFormErrors::default(),
        );
        assert!(rendered.contains("トレーナー名は他の参加者に公開されます"));
        assert!(rendered.contains("Assay のレートには自動反映されず"));
        assert!(rendered.contains("初期値は一律1500です"));
    }

    #[test]
    fn the_result_page_explains_each_gate_separately_and_links_to_queue() {
        let eligibility = QueueEligibility {
            registration: Some(RegistrationBlock::RankBelowEntry {
                declared: Some(Rank::Master),
            }),
            active_formation: Some(crate::lifecycle::FormationId(Uuid::from_u128(8))),
            pending_reports: vec![crate::lifecycle::FormationId(Uuid::from_u128(9))],
        };
        let rendered = registration_result(Viewer::MEMBER, &profile("トレーナー"), &eligibility);
        assert!(rendered.contains("いまはキューに入れません"));
        assert!(rendered.contains("参加資格は"), "{rendered}");
        assert!(rendered.contains("マッチが成立しています"), "{rendered}");
        assert!(rendered.contains("未報告の試合が1件"), "{rendered}");
        assert!(rendered.contains("href=\"/queue\""), "{rendered}");

        // 進行中ゲートだけでも、その理由だけが出る
        let active_only = registration_result(
            Viewer::MEMBER,
            &profile("トレーナー"),
            &QueueEligibility {
                active_formation: Some(crate::lifecycle::FormationId(Uuid::from_u128(8))),
                ..QueueEligibility::default()
            },
        );
        assert!(
            active_only.contains("マッチが成立しています"),
            "{active_only}"
        );
        assert!(!active_only.contains("未報告の試合"), "{active_only}");

        let allowed = registration_result(
            Viewer::MEMBER,
            &profile("トレーナー"),
            &QueueEligibility::default(),
        );
        assert!(allowed.contains("キューに参加できます"));
        assert!(!allowed.contains("入れません"));
        assert!(allowed.contains("Assay のレートには自動反映されず"));
    }

    #[test]
    fn pages_include_mobile_layout_and_accessible_focus_styles() {
        let rendered = login_page("/auth/login");
        assert!(rendered.contains("@media (max-width: 480px)"));
        assert!(rendered.contains("width: min(100% - 24px, 680px)"));
        assert!(rendered.contains(":focus-visible"));
        assert!(rendered.contains("background: var(--discord)"));
    }

    #[test]
    fn every_page_is_a_complete_document() {
        let profile = profile("t");
        for rendered in [
            login_page("/auth/login"),
            registration_form(
                Viewer::MEMBER,
                "/register",
                &profile,
                &values(&profile),
                &RegistrationFormErrors::default(),
            ),
            registration_result(Viewer::MEMBER, &profile, &QueueEligibility::default()),
            error_page(Viewer::MEMBER, "エラー", "やり直してください"),
        ] {
            assert!(rendered.starts_with("<!doctype html>"), "{rendered}");
            assert!(rendered.contains("<meta charset=\"utf-8\">"));
            assert!(rendered.contains("<meta name=\"viewport\""));
            assert!(rendered.trim_end().ends_with("</html>"));
        }
    }

    #[test]
    fn the_pending_list_escapes_its_entries() {
        let rendered = pending_reports_list(&[crate::lifecycle::FormationId(Uuid::from_u128(1))]);
        assert!(rendered.contains("00000000-0000-0000-0000-000000000001"));
    }

    // ------------------------------------------------------------------
    // 共通ヘッダー
    // ------------------------------------------------------------------

    /// 描画済みのページからヘッダーだけを切り出す。
    fn header_of(rendered: &str) -> &str {
        let start = rendered
            .find("<header class=\"site-header\">")
            .unwrap_or_else(|| panic!("no header in {rendered}"));
        let end = rendered[start..]
            .find("</header>")
            .expect("the header is closed")
            + start
            + "</header>".len();
        assert!(
            !rendered[end..].contains("<header"),
            "ヘッダーは 1 ページに 1 つ: {rendered}"
        );
        &rendered[start..end]
    }

    #[test]
    fn a_signed_out_visitor_gets_no_links_in_the_header() {
        for rendered in [
            login_page("/auth/login"),
            error_page(Viewer::ANONYMOUS, "エラー", "やり直してください"),
        ] {
            let header = header_of(&rendered);
            assert!(!header.contains("href=\"/status\""), "{header}");
            assert!(!header.contains("href=\"/admin\""), "{header}");
            assert!(!header.contains("/auth/logout"), "{header}");
            assert!(!header.contains("href=\"/queue\""), "{header}");
            // 入口へのリンクだけは残す（ログインの導線であって、画面間の移動ではない）
            assert!(
                header.contains("<a class=\"brand\" href=\"/\">Assay</a>"),
                "{header}"
            );
        }
    }

    #[test]
    fn a_signed_in_member_gets_status_and_logout_but_not_the_admin_panel() {
        let page = error_page(Viewer::MEMBER, "エラー", "やり直してください");
        let header = header_of(&page);
        assert!(
            header.contains("<a class=\"brand\" href=\"/queue\">Assay</a>"),
            "{header}"
        );
        assert!(header.contains("href=\"/status\""), "{header}");
        assert!(header.contains("action=\"/auth/logout\""), "{header}");
        // 非管理者にはパネルの存在を知らせない（/admin 自体も 404 を返す）
        assert!(!header.contains("href=\"/admin\""), "{header}");
        assert!(!header.contains("管理"), "{header}");
    }

    /// **管理者と非管理者でヘッダーが 1 バイトも違わないこと。**
    ///
    /// `/admin` は非管理者に 404 を返す方針なので、リンクの有無がそのまま
    /// 「この人は管理者か」の判別材料になる。運営は URL を直接開く。
    #[test]
    fn the_header_is_identical_for_admins_and_everyone_else() {
        for public in [false, true] {
            let admin = header_of(&error_page(
                Viewer::ADMIN.with_ranking(public),
                "エラー",
                "やり直してください",
            ))
            .to_owned();
            let member = header_of(&error_page(
                Viewer::MEMBER.with_ranking(public),
                "エラー",
                "やり直してください",
            ))
            .to_owned();
            assert_eq!(
                admin, member,
                "ランキング公開={public} で管理者とそれ以外のヘッダーが違う"
            );
            assert!(!admin.contains("/admin"), "{admin}");
            assert!(!admin.contains("管理"), "{admin}");
            println!(
                "ランキング公開={public}: 管理者と一般のヘッダーは一致（{} バイト）",
                admin.len()
            );
        }
    }

    /// **ランキングが非公開のあいだはリンクを出さない。**公開したら未ログインにも出す。
    #[test]
    fn the_ranking_link_follows_the_public_switch() {
        for viewer in [Viewer::ANONYMOUS, Viewer::MEMBER, Viewer::ADMIN] {
            let private = header_of(&error_page(viewer, "エラー", "やり直してください")).to_owned();
            assert!(
                !private.contains("href=\"/ranking\""),
                "非公開なのにリンクがある: {private}"
            );
            let public = header_of(&error_page(
                viewer.with_ranking(true),
                "エラー",
                "やり直してください",
            ))
            .to_owned();
            assert!(
                public.contains("href=\"/ranking\""),
                "公開なのにリンクが無い: {public}"
            );
        }
        println!("ランキング: 非公開ならリンク無し / 公開なら未ログインにも出る");
    }

    /// **マッチ画面にはヘッダーの導線を出さない。**
    ///
    /// 試合中に他画面へ移れる経路を作らない（報告ボタンから離れる経路を増やさない）。
    /// ブランド名もリンクにしない。
    #[test]
    fn the_match_screen_has_no_navigation_at_all() {
        let page = page_without_nav("試合中", "<p>本文</p>", "");
        let header = header_of(&page);
        for link in [
            "href=\"/queue\"",
            "href=\"/status\"",
            "href=\"/ranking\"",
            "href=\"/profile\"",
            "href=\"/admin\"",
            "href=\"/\"",
            "/auth/logout",
            "<nav",
        ] {
            assert!(!header.contains(link), "{link} が残っている: {header}");
        }
        assert!(
            header.contains("<span class=\"brand\">Assay</span>"),
            "{header}"
        );
        println!("マッチ画面のヘッダー: {}", header.trim());
    }

    /// **ヘッダーの定義が 1 か所であることの検査。**
    ///
    /// 登録・戦績・管理の各画面から切り出したヘッダーが文字単位で一致すること。
    /// どれかの画面が自前のヘッダーを持ち始めたら、ここで落ちる
    /// （キュー画面の同じ検査は `queue::html` 側にある）。
    #[test]
    fn every_screen_renders_the_same_header() {
        use crate::admin::html::{MatchmakerView, SearchView, search_page};
        use crate::status::StatusSummary;
        use crate::status::html::{StatusView, status_page};

        let profile = profile("トレーナー");
        let member = [
            registration_form(
                Viewer::MEMBER,
                "/register",
                &profile,
                &values(&profile),
                &RegistrationFormErrors::default(),
            ),
            registration_result(Viewer::MEMBER, &profile, &QueueEligibility::default()),
            error_page(Viewer::MEMBER, "エラー", "やり直してください"),
            status_page(
                Viewer::MEMBER,
                &StatusView {
                    rating: 1500.0,
                    summary: StatusSummary::default(),
                    history: Vec::new(),
                },
            ),
        ];
        let expected = header_of(&member[0]).to_owned();
        for rendered in &member {
            assert_eq!(header_of(rendered), expected, "{rendered}");
        }

        // 管理パネルも同じ外枠を通る。**管理者向けの差分はもう無い**
        let admin = search_page(&SearchView {
            matchmaker: MatchmakerView {
                paused: None,
                form_key: "k",
            },
            query: "",
            players: &[],
            recent: &[],
            received_blocks: &[],
            notice: None,
        });
        assert_eq!(
            header_of(&admin),
            expected,
            "管理パネルのヘッダーが他の画面と違う",
        );

        // プロフィール編集とランキングも同じヘッダーを通る
        let own = crate::profiles::OwnProfile::default();
        let edit = crate::profiles::html::edit_page(crate::profiles::html::EditPage {
            viewer: Viewer::MEMBER,
            player_id: uuid::Uuid::nil(),
            own: &own,
            known_roles: &[],
            current_blocks: &[],
            block_query: "",
            search_results: &[],
            error: None,
            saved: false,
        });
        assert_eq!(
            header_of(&edit),
            expected,
            "プロフィール編集のヘッダーが違う"
        );
        println!(
            "同じヘッダーを出す画面: 登録 / 登録結果 / エラー / 戦績 / 管理 / プロフィール編集"
        );
    }

    #[test]
    fn the_header_survives_a_narrow_screen_without_a_menu_script() {
        let rendered = error_page(Viewer::ADMIN, "エラー", "やり直してください");
        // 375px でも 1 行に収める前提。折りたたみの JS は持たない
        assert!(rendered.contains(".site-nav a, .site-nav button { font-size: 0.85rem; }"));
        assert!(rendered.contains("white-space: nowrap"));
        assert!(!rendered.contains("<script"), "{rendered}");
    }

    /// レビュー用に各画面の HTML を丸ごと出す（`queue::html` の同名テストと対）。
    ///
    /// `cargo test --lib registration::html -- --ignored --nocapture` で取り出す。
    #[ignore = "レビュー用の出力。--ignored で明示的に走らせる"]
    #[test]
    fn dump_every_page_for_review() {
        use crate::admin::html::{MatchmakerView, SearchView, search_page};
        use crate::status::StatusSummary;
        use crate::status::html::{StatusView, status_page};

        let profile = profile("トレーナー");

        println!("========== 1. 入口 / 未ログイン ==========");
        println!("{}", login_page("/auth/login"));

        println!("========== 2. 登録フォーム（登録済み・プリフィル） ==========");
        println!(
            "{}",
            registration_form(
                Viewer::MEMBER,
                "/register",
                &profile,
                &values(&profile),
                &RegistrationFormErrors::default(),
            )
        );

        println!("========== 3. 登録内容（保存後） ==========");
        println!(
            "{}",
            registration_result(Viewer::MEMBER, &profile, &QueueEligibility::default())
        );

        println!("========== 4. 戦績 ==========");
        println!(
            "{}",
            status_page(
                Viewer::MEMBER,
                &StatusView {
                    rating: 1522.0,
                    summary: StatusSummary::default(),
                    history: Vec::new(),
                },
            )
        );

        println!("========== 5. 管理パネル（管理者のみ） ==========");
        println!(
            "{}",
            search_page(&SearchView {
                matchmaker: MatchmakerView {
                    paused: None,
                    form_key: "k"
                },
                query: "",
                players: &[],
                recent: &[],
                received_blocks: &[],
                notice: None,
            })
        );

        println!("========== 6. エラー（未ログイン扱い） ==========");
        println!(
            "{}",
            error_page(Viewer::ANONYMOUS, "エラー", "やり直してください")
        );
    }
}
