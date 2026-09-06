//! 戦績画面のエンドポイント。
//!
//! - `GET /status` 自分のレート・総合成績・直近の試合履歴
//!
//! **対象はセッションからしか取らない。**クエリパラメータの `player_id` は
//! 受け取らない（受け取らないので、指定しても効きようがない）。他人の履歴を
//! 見る経路はこの時点で存在しない。

use axum::Router;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::Key;
use tracing::error;

use crate::admin::AdminIds;
use crate::lifecycle::PlayerId;
use crate::rating::RatingAdjustmentRepository;
use crate::registration::html::error_page;
use crate::registration::{Audience, AuthConfig, CurrentSession, Viewer};

use super::html::{HISTORY_LIMIT, StatusView, status_page};
use super::repository::StatusRepository;

/// 戦績画面が持つ状態。セッションの読み方は登録トラックと同じ。
#[derive(Clone)]
pub struct StatusState {
    /// セッション Cookie を読むための設定。
    pub config: AuthConfig,
    /// Cookie の署名鍵（`config` から導出したものを保持する）。
    key: Key,
    /// 現在レートの導出（既存経路をそのまま使う）。
    pub ratings: RatingAdjustmentRepository,
    /// 履歴の読み取り。
    pub history: StatusRepository,
    /// 共通ヘッダーに管理パネルの導線を出す相手の判定。既定は「管理者なし」。
    audience: Audience,
}

impl StatusState {
    pub fn new(
        config: AuthConfig,
        ratings: RatingAdjustmentRepository,
        history: StatusRepository,
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            key,
            ratings,
            history,
            audience: Audience::default(),
        }
    }

    /// 管理者リストを渡す。**渡さなければ誰にも管理パネルの導線を出さない。**
    pub fn with_admins(mut self, admins: Option<AdminIds>) -> Self {
        self.audience = Audience::new(admins);
        self
    }
}

impl FromRef<StatusState> for Key {
    fn from_ref(state: &StatusState) -> Self {
        state.key.clone()
    }
}

impl FromRef<StatusState> for AuthConfig {
    fn from_ref(state: &StatusState) -> Self {
        state.config.clone()
    }
}

/// ルーターを組み立てる。
pub fn router(state: StatusState) -> Router {
    Router::new()
        .route("/status", get(show_status))
        .with_state(state)
}

async fn show_status(State(app): State<StatusState>, session: Option<CurrentSession>) -> Response {
    // セッションが唯一の入口。無ければ登録画面と同じ扱いで再ログインへ戻す
    let Some(CurrentSession(session)) = session else {
        return (
            StatusCode::UNAUTHORIZED,
            Html(error_page(
                Viewer::ANONYMOUS,
                "セッションの有効期限が切れました",
                "ログイン状態を確認できませんでした。入口に戻って、Discord で再度ログインしてください。",
            )),
        )
            .into_response();
    };

    // 3 つの読み取りはどれも追記ログからの導出。ここで何かを保存・更新はしない
    let (rating, summary, history) = match (
        app.ratings.display_rating(session.player_id).await,
        app.history.summary(session.player_id).await,
        app.history.history(session.player_id, HISTORY_LIMIT).await,
    ) {
        (Ok(rating), Ok(summary), Ok(history)) => (rating, summary, history),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
            error!(%error, "could not build the status page");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(error_page(
                    Viewer::ANONYMOUS,
                    "エラー",
                    "戦績の取得に失敗しました。時間をおいてお試しください。",
                )),
            )
                .into_response();
        }
    };

    Html(status_page(
        viewer(&app, session.player_id).await,
        &StatusView {
            rating,
            summary,
            history,
        },
    ))
    .into_response()
}

/// 共通ヘッダー用の見ている人。読めなければ導線を出さない側へ倒す。
async fn viewer(app: &StatusState, player_id: PlayerId) -> Viewer {
    match app.history.discord_user_id(player_id).await {
        Ok(Some(discord_user_id)) => app.audience.viewer_for(&discord_user_id),
        Ok(None) => Viewer::MEMBER,
        Err(error) => {
            error!(%error, "could not read the discord id for the header");
            Viewer::MEMBER
        }
    }
}
