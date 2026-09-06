//! 管理パネルの設定。管理者は **環境変数の ID リスト**だけで持つ（00 §4 の方針:
//! DB に権限の概念を持ち込まない）。
//!
//! `INTERNAL_API_TOKEN` と同じ fail-closed: 未設定・空・空白だけなら
//! `None` を返し、呼び出し側は **パネルのルートをマウントしない**。
//! 設定された値に数字以外の混ざった token があれば起動に失敗する
//! （意図しない管理者化を黙って受け取らない）。

use std::env;
use std::fmt;

/// 環境変数名。`.env.example` と一致させる。
pub const ADMIN_DISCORD_USER_IDS: &str = "ADMIN_DISCORD_USER_IDS";

/// 設定の読み込みエラー。値そのものは管理 ID（公開情報）だが、理由だけを載せる。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdminConfigError {
    /// リストに数字でない token が混ざっている。
    #[error("invalid {name}: {token} is not a Discord user id")]
    InvalidToken {
        /// 環境変数名。
        name: &'static str,
        /// 数字でなかった token。
        token: String,
    },
}

/// 管理者の Discord ID リスト。ルート照合専用（DB には持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminIds(Vec<i64>);

impl AdminIds {
    pub fn contains(&self, discord_user_id: i64) -> bool {
        self.0.contains(&discord_user_id)
    }

    /// snowflake（文字列）で照合する。DB から読んだ ID はこの形で来る。
    ///
    /// 数字として読めない値は**照合せず false**。「読めないから通す」にはしない。
    pub fn contains_snowflake(&self, discord_user_id: &str) -> bool {
        discord_user_id
            .parse::<i64>()
            .is_ok_and(|id| self.contains(id))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 空リストは `parse_admin_ids` が `None` に畳むので、実質発生しない。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for AdminIds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // ID 自体は公開情報だが、ログには人数だけ出す（他の ID と混ぜて拡散させない）
        write!(f, "{} admin ids", self.0.len())
    }
}

/// 環境変数から読む。未設定・空・区切り文字だけなら `Ok(None)`（= マウントしない）。
pub fn admin_ids_from_env() -> Result<Option<AdminIds>, AdminConfigError> {
    parse_admin_ids(env::var(ADMIN_DISCORD_USER_IDS).ok().as_deref())
}

/// カンマと空白で区切られた ID リストを解釈する。純粋関数（テスト対象）。
///
/// 重複は落として保つ。順序は保持する（挙動に効かないが、ログの再現性）。
pub fn parse_admin_ids(raw: Option<&str>) -> Result<Option<AdminIds>, AdminConfigError> {
    let Some(raw) = raw.map(str::trim) else {
        return Ok(None);
    };
    let mut ids: Vec<i64> = Vec::new();
    for token in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        if token.is_empty() {
            continue;
        }
        let id = token
            .parse::<i64>()
            .map_err(|_| AdminConfigError::InvalidToken {
                name: ADMIN_DISCORD_USER_IDS,
                token: token.to_owned(),
            })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Ok(None);
    }
    Ok(Some(AdminIds(ids)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_blank_means_no_admin_panel() {
        // fail-closed: 未設定 / 空 / 空白 / 区切り文字だけ は全部 None
        assert_eq!(parse_admin_ids(None).unwrap(), None);
        assert_eq!(parse_admin_ids(Some("")).unwrap(), None);
        assert_eq!(parse_admin_ids(Some("   ")).unwrap(), None);
        assert_eq!(parse_admin_ids(Some(" , , ")).unwrap(), None);
    }

    #[test]
    fn a_list_parses_and_deduplicates() {
        let ids = parse_admin_ids(Some(
            "900000000000000001, 900000000000000002\n900000000000000001 900000000000000003",
        ))
        .unwrap()
        .expect("configured");
        assert!(ids.contains(900000000000000001));
        assert!(ids.contains(900000000000000002));
        assert!(ids.contains(900000000000000003));
        assert_eq!(ids.len(), 3, "重複は 1 つに寄せる");
        assert!(!ids.contains(1));
    }

    #[test]
    fn a_non_numeric_token_fails_the_startup() {
        // 黙って無視すると「効いていないのに効いているつもり」になる。起動で落とす
        let error = parse_admin_ids(Some("900000000000000001,root")).unwrap_err();
        assert!(matches!(error, AdminConfigError::InvalidToken { .. }));
        assert!(error.to_string().contains("root"));
    }

    #[test]
    fn the_display_keeps_the_ids_out_of_logs() {
        let ids = parse_admin_ids(Some("900000000000000001"))
            .unwrap()
            .unwrap();
        assert_eq!(ids.to_string(), "1 admin ids");
    }
}
