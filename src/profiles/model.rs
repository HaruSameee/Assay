//! プロフィールの型と検証。**純粋**。DB も HTTP も知らない。
//!
//! 公開ビューと編集ビューで**型を分けてある**。公開側の型に
//! 「非公開のときの担当ロール」を入れる場所が無いので、描画からも JSON からも
//! 出しようがない（マッチ画面で相手チームのロールを消したのと同じ作り）。

use serde::{Deserialize, Serialize};

/// 自己紹介の上限（文字数。バイト数ではない）。
pub const MAX_BIO_CHARS: usize = 200;
/// 担当ロールに選べる数の上限（5 ロールしか無いので、それ以上は入力ミス）。
pub const MAX_MAIN_ROLES: usize = 5;

/// 入力の不備。**どの項目が悪いかを項目ごとに返す**（1つの真偽値に畳まない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// 自己紹介が長すぎる。
    BioTooLong {
        /// 実際の文字数。
        chars: usize,
    },
    /// X のユーザー名が形式に合わない。
    InvalidXId,
    /// 担当ロールに知らない値が混ざっている。
    UnknownRole {
        /// 受け取った値（画面に出すときはエスケープすること）。
        role: String,
    },
    /// 担当ロールが多すぎる、または重複している。
    TooManyRoles,
}

impl ProfileError {
    /// 画面に出す説明。
    pub fn message(&self) -> String {
        match self {
            Self::BioTooLong { chars } => {
                format!("自己紹介は{MAX_BIO_CHARS}文字までです（いまは{chars}文字）。")
            }
            Self::InvalidXId => {
                "X の ID は半角英数字とアンダースコアで 1〜15 文字です（@ は入れないでください）。"
                    .to_owned()
            }
            Self::UnknownRole { .. } => "選べないロールが含まれています。".to_owned(),
            Self::TooManyRoles => "担当ロールの選び方が正しくありません。".to_owned(),
        }
    }

    /// 機械可読な理由。
    pub fn code(&self) -> &'static str {
        match self {
            Self::BioTooLong { .. } => "bio_too_long",
            Self::InvalidXId => "invalid_x_id",
            Self::UnknownRole { .. } => "unknown_role",
            Self::TooManyRoles => "too_many_roles",
        }
    }
}

/// 保存する編集内容。**検証を通ったものだけがこの型になる。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEdit {
    /// 自己紹介（前後の空白は落とす）。
    pub bio: String,
    /// X のユーザー名。未設定なら `None`。
    pub x_id: Option<String>,
    /// 担当ロール。未設定なら空。
    pub main_roles: Vec<String>,
    /// **担当ロールを公開するか。**既定は false。
    pub main_roles_public: bool,
}

/// 入力を検証する。**すべてサーバ側で行う**（画面の `maxlength` は当てにしない）。
///
/// `known_roles` は `roles` テーブルの識別子。ここで一覧を複製しない。
pub fn validate_edit(
    bio: &str,
    x_id: Option<&str>,
    main_roles: &[String],
    main_roles_public: bool,
    known_roles: &[String],
) -> Result<ProfileEdit, ProfileError> {
    let bio = bio.trim();
    let chars = bio.chars().count();
    if chars > MAX_BIO_CHARS {
        return Err(ProfileError::BioTooLong { chars });
    }

    // `@` を付けて送ってくる人が必ずいるので、そこだけ落としてから検証する。
    // それ以外は一切直さない（直すと「入れたものと違う値が保存される」）
    let x_id = x_id
        .map(str::trim)
        .map(|value| value.trim_start_matches('@'))
        .filter(|value| !value.is_empty());
    if let Some(value) = x_id
        && !is_valid_x_id(value)
    {
        return Err(ProfileError::InvalidXId);
    }

    if main_roles.len() > MAX_MAIN_ROLES {
        return Err(ProfileError::TooManyRoles);
    }
    let mut seen: Vec<&str> = Vec::with_capacity(main_roles.len());
    for role in main_roles {
        if !known_roles.iter().any(|known| known == role) {
            return Err(ProfileError::UnknownRole { role: role.clone() });
        }
        if seen.contains(&role.as_str()) {
            return Err(ProfileError::TooManyRoles);
        }
        seen.push(role);
    }

    Ok(ProfileEdit {
        bio: bio.to_owned(),
        x_id: x_id.map(ToOwned::to_owned),
        main_roles: main_roles.to_vec(),
        main_roles_public,
    })
}

/// `^[A-Za-z0-9_]{1,15}$`。**DB の CHECK と同じ条件**をここでも掛ける。
///
/// URL はこの値から組み立てるので、ここを通らない値はアプリにも入らない。
/// 正規表現エンジンを足さずに書けるので依存を増やさない。
fn is_valid_x_id(value: &str) -> bool {
    (1..=15).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Discord 側の見た目（最新のスナップショットから導出した値）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordAppearance {
    /// 表示名。
    pub display_name: String,
    /// アバターのハッシュ。**未設定なら `None`**（既定アバターを出す）。
    pub avatar_hash: Option<String>,
}

/// Discord CDN のアバター URL。
///
/// **`discord_user_id` が URL に入る。**ページの URL には出さない方針だが、
/// アバターを CDN から出す以上、画像の URL には必ず入る（呼ぶ側が承知していること）。
/// ハッシュが無ければ既定アバター（id から決まる 6 種類）を返す。
pub fn avatar_url(discord_user_id: i64, avatar_hash: Option<&str>) -> String {
    match avatar_hash {
        Some(hash) => {
            format!("https://cdn.discordapp.com/avatars/{discord_user_id}/{hash}.png?size=128")
        }
        // 新しいユーザー名体系の既定アバター（(id >> 22) % 6）
        None => {
            let index = (discord_user_id >> 22).rem_euclid(6);
            format!("https://cdn.discordapp.com/embed/avatars/{index}.png")
        }
    }
}

/// 公開プロフィールとして返す値。
///
/// **担当ロールは `Option` で持つ。**非公開のときも未設定のときも `None` になり、
/// `skip_serializing_if` でキーごと消える。空配列を返すと「設定していない」ことが
/// 確定情報になり、未設定と非公開が区別できてしまう。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicProfile {
    /// `players.id`。**`discord_user_id` は入れない。**
    pub player_id: uuid::Uuid,
    /// Discord の表示名（最新のスナップショット）。
    pub display_name: String,
    /// アバターの URL。
    pub avatar_url: String,
    /// トレーナー名（登録フォームで申告したもの）。
    pub trainer_name: Option<String>,
    /// 表示スケールのレート（導出値）。
    pub rating: f64,
    /// 確定した試合数。
    pub matches: i64,
    /// 自己紹介。
    pub bio: String,
    /// X のユーザー名。未設定なら**キーごと消える**。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x_id: Option<String>,
    /// 担当ロール。**公開を選んでいるときだけキーが載る。**
    /// 未設定と非公開は、どちらもキーが無い状態で区別できない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_roles: Option<Vec<String>>,
    /// キャラプール画像の URL。未設定なら**キーごと消える**。
    ///
    /// アップロードした場合は `/players/{id}/pool.png`、URL を貼った場合は
    /// そのままの URL。**担当ロールの公開設定とは独立**で、画像を置いた人は出る
    /// （ロールを隠したい人と、絵を見せたい人は別）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_image_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> Vec<String> {
        [
            "top_carry",
            "top_support",
            "jungle",
            "bot_carry",
            "bot_tank",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
    }

    #[test]
    fn the_bio_is_measured_in_characters_not_bytes() {
        let long = "あ".repeat(MAX_BIO_CHARS);
        assert!(validate_edit(&long, None, &[], false, &roles()).is_ok());
        let too_long = "あ".repeat(MAX_BIO_CHARS + 1);
        assert_eq!(
            validate_edit(&too_long, None, &[], false, &roles()),
            Err(ProfileError::BioTooLong {
                chars: MAX_BIO_CHARS + 1
            })
        );
    }

    /// **X の ID は形式を通ったものだけ。**URL をここから組み立てるので、
    /// 通ってしまうと外部サイトへの誘導に使われる。
    #[test]
    fn only_a_plain_x_handle_is_accepted() {
        for good in ["a", "A_1", "_", "0123456789abcde"] {
            let edit = validate_edit("", Some(good), &[], false, &roles()).expect(good);
            assert_eq!(edit.x_id.as_deref(), Some(good));
        }
        // 先頭の @ だけは落とす（付けて送る人が必ずいる）
        assert_eq!(
            validate_edit("", Some("@name"), &[], false, &roles())
                .unwrap()
                .x_id
                .as_deref(),
            Some("name")
        );
        for bad in [
            "0123456789abcdef", // 16 文字
            "with space",
            "with-hyphen",
            "with.dot",
            "https://x.com/someone", // URL ごと
            "someone/status/1",      // パス付き
            "javascript:alert(1)",
            "あ",
            "name?a=1",
            "<script>",
            "a\nb",
        ] {
            assert_eq!(
                validate_edit("", Some(bad), &[], false, &roles()),
                Err(ProfileError::InvalidXId),
                "{bad:?} が通っている"
            );
        }
        // 空・空白だけは「未設定」
        for empty in ["", "   ", "@"] {
            assert_eq!(
                validate_edit("", Some(empty), &[], false, &roles())
                    .unwrap()
                    .x_id,
                None,
                "{empty:?}"
            );
        }
    }

    #[test]
    fn unknown_or_duplicated_roles_are_refused() {
        assert!(validate_edit("", None, &["jungle".to_owned()], true, &roles()).is_ok());
        assert_eq!(
            validate_edit("", None, &["support".to_owned()], true, &roles()),
            Err(ProfileError::UnknownRole {
                role: "support".to_owned()
            })
        );
        assert_eq!(
            validate_edit(
                "",
                None,
                &["jungle".to_owned(), "jungle".to_owned()],
                true,
                &roles()
            ),
            Err(ProfileError::TooManyRoles)
        );
        let all_six = vec!["jungle".to_owned(); 6];
        assert_eq!(
            validate_edit("", None, &all_six, true, &roles()),
            Err(ProfileError::TooManyRoles)
        );
    }

    /// **未設定と非公開は、どちらもキーが無い。**区別できない形にする。
    #[test]
    fn the_main_roles_key_disappears_unless_published() {
        let base = PublicProfile {
            player_id: uuid::Uuid::nil(),
            display_name: "名無し".to_owned(),
            avatar_url: "https://example.test/a.png".to_owned(),
            trainer_name: None,
            rating: 1500.0,
            matches: 0,
            bio: String::new(),
            x_id: None,
            main_roles: None,
            pool_image_url: None,
        };
        let hidden = serde_json::to_value(&base).unwrap();
        assert!(hidden.get("main_roles").is_none(), "{hidden}");
        assert!(hidden.get("x_id").is_none(), "{hidden}");

        let shown = serde_json::to_value(PublicProfile {
            main_roles: Some(vec!["jungle".to_owned()]),
            ..base
        })
        .unwrap();
        assert_eq!(shown["main_roles"], serde_json::json!(["jungle"]));
    }

    #[test]
    fn a_missing_avatar_falls_back_to_the_default_one() {
        let with = avatar_url(1234, Some("abc123"));
        assert_eq!(
            with,
            "https://cdn.discordapp.com/avatars/1234/abc123.png?size=128"
        );
        let without = avatar_url(1_234_567_890_123_456_789, None);
        assert!(
            without.starts_with("https://cdn.discordapp.com/embed/avatars/"),
            "{without}"
        );
        assert!(without.ends_with(".png"), "{without}");
    }
}
