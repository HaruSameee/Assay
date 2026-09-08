//! 「いま画面を見ている人」。共通ヘッダーが出す導線はこれだけで決まる。
//!
//! 判定材料は 2 つしかない。
//!
//! - セッションがあるか（[`Viewer::signed_in`]）
//! - その人の Discord ID が `ADMIN_DISCORD_USER_IDS` に載っているか（[`Viewer::is_admin`]）
//!
//! **DB に権限の概念は持ち込まない**（00 §4）。管理者リストは起動時に確定した
//! [`AdminIds`] だけが正で、リクエスト側から管理者を名乗る経路は無い。
//! リストが未設定なら誰も管理者にならない（fail-closed。管理パネル自体も
//! `main.rs` がマウントしない）。
//!
//! ここは**表示の判定だけ**を担う。`/admin` へのアクセス可否は
//! [`crate::admin::routes`] が同じリストで別途照合しており、ヘッダーにリンクが
//! 出ないことはアクセス制御ではない（リンクを消しても 404 は 404 のまま）。

use crate::admin::AdminIds;

use super::model::PlayerProfile;

/// 画面を見ている人。ヘッダーの描画にはこれしか渡さない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Viewer {
    signed_in: bool,
    admin: bool,
    /// ランキングを公開しているか。**導線を出すかどうかだけ**に使う
    /// （`/ranking` 自体の可否はルート側の判定が正）。
    ranking_public: bool,
}

impl Viewer {
    /// 未ログイン。ヘッダーに導線を出さない。
    pub const ANONYMOUS: Self = Self {
        signed_in: false,
        admin: false,
        ranking_public: false,
    };

    /// ログイン済みの一般参加者。
    pub const MEMBER: Self = Self {
        signed_in: true,
        admin: false,
        ranking_public: false,
    };

    /// ログイン済みの管理者。**[`Audience`] の照合を通ったときだけ作ること。**
    pub const ADMIN: Self = Self {
        signed_in: true,
        admin: true,
        ranking_public: false,
    };

    /// セッションがあるか。
    pub fn signed_in(self) -> bool {
        self.signed_in
    }

    /// 管理パネルへの導線を出す相手か。未ログインなら常に false。
    pub fn is_admin(self) -> bool {
        self.signed_in && self.admin
    }

    /// ランキングへの導線を出すか。**未ログインでも出す**（公開ページなので）。
    pub fn ranking_public(self) -> bool {
        self.ranking_public
    }

    /// ランキングの公開状態を載せる。
    pub fn with_ranking(mut self, public: bool) -> Self {
        self.ranking_public = public;
        self
    }
}

/// 管理者リストを持ち回るための入れ物。各トラックの `*State` が 1 つ持つ。
///
/// `None`（未設定）は「管理者は 1 人もいない」。テスト用の既定値もこれなので、
/// 渡し忘れたときに管理者の導線が出ることはない。
#[derive(Debug, Clone, Default)]
pub struct Audience {
    admins: Option<AdminIds>,
    /// ランキングを公開しているか。既定は**非公開**（導線を出さない側に倒す）。
    ranking_public: bool,
}

impl Audience {
    /// 起動時に確定したリストから作る。
    pub fn new(admins: Option<AdminIds>) -> Self {
        Self {
            admins,
            ranking_public: false,
        }
    }

    /// ランキングの公開状態を渡す。**渡さなければ非公開**（導線は出ない）。
    pub fn with_ranking(mut self, public: bool) -> Self {
        self.ranking_public = public;
        self
    }

    /// 公開状態だけ差し替える（各トラックの `*State` が使う）。
    pub fn set_ranking(&mut self, public: bool) {
        self.ranking_public = public;
    }

    /// Discord ID（snowflake 文字列）から見ている人を決める。
    ///
    /// 数字として読めない ID は照合しない = 管理者にしない。
    pub fn viewer_for(&self, discord_user_id: &str) -> Viewer {
        let admin = self
            .admins
            .as_ref()
            .is_some_and(|admins| admins.contains_snowflake(discord_user_id));
        let viewer = if admin { Viewer::ADMIN } else { Viewer::MEMBER };
        viewer.with_ranking(self.ranking_public)
    }

    /// プロフィールから決める。`None`（セッションはあるが行が無い）は未ログイン扱い。
    pub fn viewer_of(&self, profile: Option<&PlayerProfile>) -> Viewer {
        match profile {
            Some(profile) => self.viewer_for(&profile.discord_user_id),
            None => Viewer::ANONYMOUS.with_ranking(self.ranking_public),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::parse_admin_ids;

    fn audience(raw: Option<&str>) -> Audience {
        Audience::new(parse_admin_ids(raw).expect("valid list"))
    }

    #[test]
    fn an_anonymous_viewer_gets_no_links() {
        assert!(!Viewer::ANONYMOUS.signed_in());
        assert!(!Viewer::ANONYMOUS.is_admin());
        // 既定は未ログイン。導線が出る側に倒れない
        assert_eq!(Viewer::default(), Viewer::ANONYMOUS);
    }

    #[test]
    fn only_a_listed_discord_id_becomes_an_admin() {
        let audience = audience(Some("900000000000000001, 900000000000000002"));
        assert!(audience.viewer_for("900000000000000001").is_admin());
        assert!(audience.viewer_for("900000000000000002").is_admin());
        // 載っていない ID は一般参加者。ログインはしている
        let member = audience.viewer_for("900000000000000003");
        assert!(member.signed_in());
        assert!(!member.is_admin());
    }

    #[test]
    fn no_list_means_nobody_is_an_admin() {
        // 未設定なら管理パネル自体がマウントされない。導線も出さない
        let audience = audience(None);
        assert!(!audience.viewer_for("900000000000000001").is_admin());
        assert!(audience.viewer_for("900000000000000001").signed_in());
        assert!(
            !Audience::default()
                .viewer_for("900000000000000001")
                .is_admin()
        );
    }

    #[test]
    fn an_unreadable_snowflake_is_never_an_admin() {
        let audience = audience(Some("900000000000000001"));
        assert!(!audience.viewer_for("").is_admin());
        assert!(!audience.viewer_for("900000000000000001x").is_admin());
        assert!(!audience.viewer_for(" 900000000000000001").is_admin());
    }

    #[test]
    fn a_missing_player_row_is_treated_as_signed_out() {
        assert_eq!(audience(None).viewer_of(None), Viewer::ANONYMOUS);
    }
}
