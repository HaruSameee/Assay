//! 登録・認証トラック（05-登録トラック仕様）。
//!
//! Discord OAuth2 でログインし、署名付き Cookie でセッションを張り、登録フォームで
//! トレーナー情報と申告レートを受け取る。キューのゲートの判定もここに置く。
//!
//! 不変条件（このモジュールで守るもの）:
//! - **scope は `identify` だけ。**`email` も `guilds` も要求しない。
//! - `state` は使い捨て。短命な署名付き Cookie に入れ、コールバックで突き合わせたら
//!   **成否によらず即座に破棄**する。
//! - `client_secret` はトークン交換のリクエストボディにだけ入れる。ログ・エラー・レスポンスに
//!   出さない（[`config::Secret`] は `Debug` が伏字、`Display` は無い）。
//! - Discord のユーザー ID は snowflake。**文字列で持つ**。DB の `bigint` との変換は
//!   [`repository`] の境界だけで行う。
//! - リダイレクト URI と Cookie の Domain は環境変数から読む。Domain は先頭ドットを拒否する
//!   （親ドメインにすると個人サイトと Cookie を共有する）。
//! - Cookie に入れるのは **player の識別子と発行時刻だけ**。権限もロールも入れない。
//! - 申告レートは可変列に置かない。`player_declarations` に追記し、現在値は最新行から導出する。
//! - 追記専用テーブルに UPDATE / DELETE を書かない。`SELECT ... FOR UPDATE` も使わない。
//! - キューのゲートは**3つに分けたまま**返す（[`gate`]）。1つのフラグに畳むと、
//!   キュー画面で「なぜ入れないのか」を説明できなくなる。
//! - **共通ヘッダーは [`html::page`] の 1 か所だけ**。キュー・戦績・管理も同じ外枠を通る。
//!   画面ごとに `<header>` を書かない。出す導線は [`Viewer`] だけで決まり、
//!   管理パネルの導線は `ADMIN_DISCORD_USER_IDS` に載っている人にしか出さない。

pub mod config;
pub mod gate;
pub mod html;
pub mod model;
pub mod oauth;
pub mod repository;
pub mod routes;
pub mod session;
pub mod viewer;

pub use config::{
    AuthConfig, AuthConfigError, Secret, report_gate_disabled_from_env, site_base_url,
};
pub use gate::{
    QueueEligibility, RegistrationBlock, active_formation_blocks_queue, evaluate,
    formation_blocks_queue,
};
pub use model::{Declaration, NewDeclaration, PlayerProfile, PlayerStatus, Rank};
pub use oauth::{DiscordUser, HttpOAuthClient, OAuthClient, OAuthError};
pub use repository::{RegistrationError, RegistrationRepository};
pub use routes::{AuthState, router};
pub use session::{CurrentSession, Session, SessionError};
pub use viewer::{Audience, Viewer};
