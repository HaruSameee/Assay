//! ランキングの画面と JSON。
//!
//! - `GET /ranking` レート降順の一覧（HTML / JSON）
//!
//! **公開スイッチは環境変数。**既定は非公開で、非公開のあいだは 404 を返す
//! （403 だと「あるが見せない」ことが分かる。管理パネルと同じ方針）。
//! 開幕直後は窓 50 の分離により上位がシード組で固定されるため、いつ公開するかを
//! 運営が決められるようにしてある。

use axum::extract::{FromRef, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::cookie::Key;
use serde::{Deserialize, Serialize};
use tracing::error;
use uuid::Uuid;

use crate::admin::AdminIds;
use crate::registration::html::error_page;
use crate::registration::{Audience, AuthConfig, CurrentSession, RegistrationRepository, Viewer};

use super::repository::{MINIMUM_MATCHES, RankingRepository};

/// 公開／非公開を切り替える環境変数。**未設定なら非公開。**
pub const RANKING_PUBLIC: &str = "RANKING_PUBLIC";

/// 環境変数を読む。`true` / `1` / `yes` を公開とみなす（それ以外はすべて非公開）。
///
/// **判定を甘くしない。**綴り間違いで意図せず公開されるより、出ないほうがよい。
pub fn ranking_is_public(raw: Option<&str>) -> bool {
    matches!(
        raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("true" | "1" | "yes")
    )
}

/// ランキングのルーターが持つ状態。
#[derive(Clone)]
pub struct RankingState {
    pub config: AuthConfig,
    pub ranking: RankingRepository,
    pub registration: RegistrationRepository,
    /// **公開しているか。**false なら 404。
    public: bool,
    key: Key,
    audience: Audience,
}

impl RankingState {
    /// 組み立てる。`public` は起動時に環境変数から決めた値。
    pub fn new(
        config: AuthConfig,
        ranking: RankingRepository,
        registration: RegistrationRepository,
        public: bool,
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            ranking,
            registration,
            public,
            key,
            audience: Audience::default().with_ranking(public),
        }
    }

    /// 管理者リストを渡す。渡さなければ誰にも管理パネルの導線を出さない。
    pub fn with_admins(mut self, admins: Option<AdminIds>) -> Self {
        self.audience = Audience::new(admins).with_ranking(self.public);
        self
    }

    /// 他のトラックと同じ形を保つための受け口。**公開状態は `new` で決まる**ので、
    /// ここで渡された値が食い違っていたら `new` 側が正。
    pub fn with_ranking(mut self, public: bool) -> Self {
        self.public = public;
        self.audience.set_ranking(public);
        self
    }
}

impl FromRef<RankingState> for Key {
    fn from_ref(state: &RankingState) -> Self {
        state.key.clone()
    }
}

impl FromRef<RankingState> for AuthConfig {
    fn from_ref(state: &RankingState) -> Self {
        state.config.clone()
    }
}

/// ルーターを組み立てる。**非公開でもルート自体はマウントする**（404 を返す）。
pub fn router(state: RankingState) -> Router {
    Router::new().route("/ranking", get(show)).with_state(state)
}

/// ページ番号。**並べ替えの指定は受け取らない**（レート降順のみ）。
#[derive(Debug, Default, Deserialize)]
struct RankingQuery {
    #[serde(default)]
    page: Option<i64>,
}

/// JSON で返す 1 行。
#[derive(Debug, Serialize)]
struct RankedPlayerView {
    rank: i64,
    player_id: Uuid,
    display_name: Option<String>,
    trainer_name: Option<String>,
    rating: f64,
    matches: i64,
}

/// JSON で返す 1 ページ。
#[derive(Debug, Serialize)]
struct RankingView {
    players: Vec<RankedPlayerView>,
    page: i64,
    pages: i64,
    total: i64,
    /// 掲載条件（画面と JSON で同じ値を出す）。
    minimum_matches: i64,
}

async fn show(
    State(app): State<RankingState>,
    session: Option<CurrentSession>,
    Query(query): Query<RankingQuery>,
    headers: HeaderMap,
) -> Response {
    let html = accepts_html(&headers);
    // **非公開なら存在ごと伏せる。**403 だと「あるが見せない」ことが分かる
    if !app.public {
        return not_found(html);
    }
    let page = match app.ranking.page(query.page.unwrap_or(1)).await {
        Ok(page) => page,
        Err(error) => {
            error!(%error, "could not read the ranking");
            return internal_error(html);
        }
    };
    if html {
        let viewer = viewer(&app, session.as_ref()).await;
        return (
            StatusCode::OK,
            Html(super::html::ranking_page(viewer, &page)),
        )
            .into_response();
    }
    let view = RankingView {
        players: page
            .players
            .into_iter()
            .map(|player| RankedPlayerView {
                rank: player.rank,
                player_id: player.player_id,
                display_name: player.display_name,
                trainer_name: player.trainer_name,
                rating: player.rating,
                matches: player.matches,
            })
            .collect(),
        page: page.page,
        pages: page.pages,
        total: page.total,
        minimum_matches: MINIMUM_MATCHES,
    };
    (StatusCode::OK, Json(view)).into_response()
}

/// 共通ヘッダーの見ている人。
async fn viewer(app: &RankingState, session: Option<&CurrentSession>) -> Viewer {
    let Some(CurrentSession(session)) = session else {
        return Viewer::ANONYMOUS.with_ranking(app.public);
    };
    match app.registration.find_by_player_id(session.player_id).await {
        Ok(profile) => app.audience.viewer_of(profile.as_ref()),
        Err(error) => {
            error!(%error, "could not read the profile for the header");
            Viewer::MEMBER.with_ranking(app.public)
        }
    }
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().starts_with("text/html"))
        })
}

fn not_found(html: bool) -> Response {
    if html {
        return (
            StatusCode::NOT_FOUND,
            Html(error_page(
                Viewer::ANONYMOUS,
                "見つかりません",
                "そのページはありません。",
            )),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}

fn internal_error(html: bool) -> Response {
    if html {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(error_page(
                Viewer::ANONYMOUS,
                "エラーが発生しました",
                "時間をおいてもう一度お試しください。",
            )),
        )
            .into_response();
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "internal_error" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **既定は非公開。**綴り間違いで意図せず公開されない。
    #[test]
    fn the_ranking_is_private_unless_explicitly_enabled() {
        for public in ["true", "TRUE", " true ", "1", "yes", "Yes"] {
            assert!(ranking_is_public(Some(public)), "{public:?}");
        }
        for private in [
            None,
            Some(""),
            Some("false"),
            Some("0"),
            Some("no"),
            Some("公開"),
            Some("ture"),
            Some("on"),
        ] {
            assert!(!ranking_is_public(private), "{private:?}");
        }
    }
}
