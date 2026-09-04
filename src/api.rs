//! HTTP API（axum）。Discord アダプタと同じプロセスで動き、DB の接続プールを共有する。
//!
//! Phase 4 で必要なのは、`matched` の formation を作ってアダプタに渡す入口だけ
//! （マッチメイキング本体は無いので、手動テスト用の内部エンドポイントとして置く）。
//! 報告の受付（Phase 6）はここには無い。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::error;
use uuid::Uuid;

use crate::discord::{Adapter, team_from_name};
use crate::lifecycle::FormationId;
use crate::persistence::{NewFormation, NewFormationPlayer, RepositoryError};

/// ルーターが共有する状態。
#[derive(Clone)]
pub struct ApiState {
    /// Discord アダプタ（永続化層もここから使う）。
    pub adapter: Arc<Adapter>,
}

/// ルーターを組み立てる。
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/internal/formations", post(create_formation))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// `POST /internal/formations` の入力。マッチメイカが確定した 10 人。
#[derive(Debug, Deserialize)]
pub struct CreateFormationRequest {
    /// 参加者 10 人（各チーム 5 人）。
    pub players: Vec<PlayerSpec>,
    /// マッチング時点のパラメータ版数（凍結値）。無ければ 0。
    #[serde(default)]
    pub matchmaking_params_version: i32,
    /// 予測勝率（凍結値）。無ければ 0.5。
    #[serde(default = "default_win_prob")]
    pub predicted_win_prob: f32,
}

fn default_win_prob() -> f32 {
    0.5
}

/// 参加者 1 人分。
#[derive(Debug, Deserialize)]
pub struct PlayerSpec {
    /// `players.discord_user_id`（登録済みであること）。
    pub discord_user_id: i64,
    /// `red` / `blue`。
    pub team: String,
    /// `roles.id`。
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

/// `POST /internal/formations` の出力。
#[derive(Debug, Serialize)]
pub struct CreateFormationResponse {
    /// 作られた formation。スレッド作成はバックグラウンドで進む。
    pub formation_id: Uuid,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// API のエラー。
#[derive(Debug)]
pub enum ApiError {
    /// 入力が不正。
    BadRequest(String),
    /// 内部エラー（詳細はログ）。
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
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

async fn create_formation(
    State(state): State<ApiState>,
    Json(request): Json<CreateFormationRequest>,
) -> Result<(StatusCode, Json<CreateFormationResponse>), ApiError> {
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
        let team = team_from_name(&spec.team).ok_or_else(|| {
            ApiError::BadRequest(format!("team must be red or blue, got {:?}", spec.team))
        })?;
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
        // 存在しないロールなどの FK 違反は入力の問題として返す
        match &error {
            RepositoryError::Database(sqlx::Error::Database(db))
                if db.is_foreign_key_violation() =>
            {
                ApiError::BadRequest(format!("unknown reference: {}", db.message()))
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
