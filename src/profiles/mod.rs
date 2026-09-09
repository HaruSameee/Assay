//! 公開プロフィール。
//!
//! **どちらのテーブルも追記専用で、現在値は最新行から導出する。**
//! ログインのたびに Discord 側の表示名とアバターを積み（[`repository::ProfileRepository::record_appearance`]）、
//! 本人の編集も 1 回ごとに 1 行積む。
//!
//! **担当ロールは本人が公開を選んだときだけ出る。**非公開のときも未設定のときも、
//! 公開ビューの型（[`model::PublicProfile`]）では `None` になり、JSON からキーごと消える。
//! 空配列を返すと「設定していない」ことが確定情報になり、未設定と非公開が区別できてしまう。

pub mod html;
pub mod model;
pub mod repository;
pub mod routes;

pub use model::{
    DiscordAppearance, MAX_BIO_CHARS, MAX_MAIN_ROLES, ProfileEdit, ProfileError, PublicProfile,
    avatar_url, validate_edit,
};
pub use repository::{OwnProfile, PlayerDiscordIdentity, ProfileRepository};
pub use routes::{ProfileState, router};
