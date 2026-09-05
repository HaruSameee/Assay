//! HTTP API（axum）。
//!
//! セキュリティ: `/internal/formations` は `INTERNAL_API_TOKEN` が設定されているときだけ
//! マウントし、`Authorization: Bearer <token>` を要求する。本番ではトークンを設定しないか、
//! リバースプロキシで `/internal/` を外から遮断する。

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{error, warn};
use uuid::Uuid;

use crate::discord::Adapter;
use crate::lifecycle::FormationId;
use crate::persistence::{NewFormation, NewFormationPlayer, RepositoryError, Team};

#[derive(Clone)]
pub struct ApiState {
    pub adapter: Arc<Adapter>,
    pub internal_token: Option<Arc<str>>,
}

pub fn router(state: ApiState) -> Router {
    let mut router = Router::new().route("/healthz", get(healthz));
    if state.internal_token.is_some() {
        router = router.route("/internal/formations", post(create_formation));
    } else {
        warn!("INTERNAL_API_TOKEN is not set; /internal/formations is not mounted");
    }
    router.with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
pub struct CreateFormationRequest {
    /// 参加者 10 人（各チーム 5 人、ロールはチーム内で重複しない）
    pub players: Vec<PlayerSpec>,
    /// マッチング時点のパラメータ版数（凍結）。無ければ 0。
    #[serde(default)]
    pub matchmaking_params_version: i32,
    /// 予測勝率（凍結）。無ければ 0.5。
    #[serde(default = "default_win_prob")]
    pub predicted_win_prob: f32,
}

fn default_win_prob() -> f32 {
    0.5
}

#[derive(Debug, Deserialize)]
pub struct PlayerSpec {
    /// `players.discord_user_id`
    pub discord_user_id: i64,
    /// `red` / `blue`
    pub team: String,
    /// `roles.id`
    pub role: String,
    /// フレックスで埋められたか。無ければ false。
    #[serde(default)]
    pub role_was_flexed: bool,
    /// マッチング時点の μ。無ければ 25.0。
    #[serde(default = "default_mu")]
    pub mu_at_match: f64,
    /// マッチング時点の σ。無ければ 25.0 / 3。
    #[serde(default = "default_sigma")]
    pub sigma_at_match: f64,
}

fn default_mu() -> f64 {
    25.0
}

fn default_sigma() -> f64 {
    25.0 / 3.0
}

#[derive(Debug, Serialize)]
pub struct CreateFormationResponse {
    /// VC の割当・投稿はバックグラウンドで進む（202）。
    pub formation_id: Uuid,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    BadRequest(String),
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            ),
        };
        (status, Json(ErrorBody { error })).into_response()
    }
}

impl From<RepositoryError> for ApiError {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::InvalidFormation(reason) => Self::BadRequest(reason),
            other => {
                error!(error = %other, "repository error");
                Self::Internal
            }
        }
    }
}

/// 長さに依存しない比較（トークンの照合にタイミング攻撃を使わせないため）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for (x, y) in a.iter().zip(b.iter().chain(std::iter::repeat(&0))) {
        diff |= x ^ y;
    }
    diff == 0
}

fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), ApiError> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or(ApiError::Unauthorized)?;
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

async fn create_formation(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<CreateFormationRequest>,
) -> Result<(StatusCode, Json<CreateFormationResponse>), ApiError> {
    let expected = state
        .internal_token
        .as_deref()
        .ok_or(ApiError::Unauthorized)?;
    authorize(&headers, expected)?;

    let repo = state.adapter.repository();
    let discord_ids: Vec<i64> = request
        .players
        .iter()
        .map(|player| player.discord_user_id)
        .collect();
    let known = repo.find_players_by_discord_ids(&discord_ids).await?;
    let unknown: Vec<String> = discord_ids
        .iter()
        .filter(|id| !known.contains_key(id))
        .map(|id| id.to_string())
        .collect();
    if !unknown.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "unregistered discord users: {}",
            unknown.join(", ")
        )));
    }

    let mut players = Vec::with_capacity(request.players.len());
    for spec in &request.players {
        let team: Team = spec
            .team
            .parse()
            .map_err(|error| ApiError::BadRequest(format!("team: {error}")))?;
        players.push(NewFormationPlayer {
            player_id: known[&spec.discord_user_id],
            team,
            role: spec.role.clone(),
            role_was_flexed: spec.role_was_flexed,
            mu_at_match: spec.mu_at_match,
            sigma_at_match: spec.sigma_at_match,
        });
    }
    let formation = NewFormation {
        id: FormationId(Uuid::new_v4()),
        created_at: Utc::now(),
        matchmaking_params_version: request.matchmaking_params_version,
        predicted_win_prob: request.predicted_win_prob,
        players,
    };
    repo.create_formation(&formation).await.map_err(|error| {
        // 存在しないロール（FK）や同一チーム同一ロール（unique）は入力の問題として返す
        match &error {
            RepositoryError::Database(sqlx::Error::Database(db))
                if db.is_foreign_key_violation() || db.is_unique_violation() =>
            {
                ApiError::BadRequest(format!("invalid formation: {}", db.message()))
            }
            _ => ApiError::from(error),
        }
    })?;

    let adapter = state.adapter.clone();
    let id = formation.id;
    tokio::spawn(async move {
        if let Err(error) = adapter.start_formation(id).await {
            error!(formation = %id, %error, "start_formation failed");
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(CreateFormationResponse {
            formation_id: formation.id.0,
        }),
    ))
}
