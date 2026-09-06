//! axum サーバ + Discord クライアントを単一プロセスで動かす。どちらかが終了したらプロセス全体を終了させる。

use std::env;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use unite_matchsystem::admin::{
    ADMIN_DISCORD_USER_IDS, AdminRepository, AdminState, admin_ids_from_env, router as admin_router,
};
use unite_matchsystem::api::{ApiState, router};
use unite_matchsystem::discord::{
    Adapter, DiscordConfig, Handler, SerenityGateway, build_client, resolve_vc_pool,
};
use unite_matchsystem::lifecycle::{DefaultHostSelector, LifecycleConfig};
use unite_matchsystem::matchmaker::{Matchmaker, MatchmakerConfig, MatchmakerRepository};
use unite_matchsystem::persistence::{FormationRepository, VcAssignment};
use unite_matchsystem::queue::{QueueRepository, QueueState, router as queue_router};
use unite_matchsystem::rating::RatingAdjustmentRepository;
use unite_matchsystem::registration::{
    AuthConfig, AuthState, HttpOAuthClient, RegistrationRepository, report_gate_disabled_from_env,
    router as registration_router,
};
use unite_matchsystem::status::{StatusRepository, StatusState, router as status_router};

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";
const INTERNAL_API_TOKEN: &str = "INTERNAL_API_TOKEN";
const MIGRATOR_DATABASE_URL: &str = "MIGRATOR_DATABASE_URL";
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match env::args().nth(1).as_deref() {
        Some("migrate") => return migrate().await,
        Some(command) => return Err(format!("unknown command: {command}").into()),
        None => {}
    }

    let database_url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let listen_addr = env::var("HTTP_LISTEN_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.into());
    let internal_token: Option<Arc<str>> = env::var(INTERNAL_API_TOKEN)
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .map(Arc::from);
    let discord_config = DiscordConfig::from_env()?;
    let auth_config = AuthConfig::from_env()?;
    let matchmaker_config = MatchmakerConfig::from_env()?;
    // 管理者 ID 未設定は「パネル無し」で正常起動（fail-closed）。数字でない値は起動で落とす
    let admin_ids = admin_ids_from_env()?;
    let report_gate_disabled = report_gate_disabled_from_env()?;
    if report_gate_disabled {
        warn!(
            "REPORT_GATE_DISABLED=true: report gate is disabled; registration gate remains active"
        );
    }
    let lifecycle = LifecycleConfig::default();

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;
    let repo = FormationRepository::new(pool.clone());
    let registration_repo =
        RegistrationRepository::new(pool.clone()).with_report_gate_disabled(report_gate_disabled);
    let queue_repo = QueueRepository::new(pool.clone());
    let ratings_repo = RatingAdjustmentRepository::new(pool.clone());
    let status_repo = StatusRepository::new(pool.clone());
    let admin_store = AdminRepository::new(pool.clone());
    let matchmaker_repo = MatchmakerRepository::new(pool);

    let handler = Arc::new(Handler::default());
    let mut client = build_client(&discord_config.token, handler.clone()).await?;
    let gateway = Arc::new(SerenityGateway::new(client.http.clone()));
    // VC プールは設定が名前・ログが ID で持つ。解決できなければ起動しない
    let vc_pool = resolve_vc_pool(
        &*gateway,
        discord_config.guild_id,
        discord_config.vc_pool_size,
    )
    .await?;
    // マッチメイカも同じプールから選ぶ。占有状況は DB から導出するので、これは候補一覧でしかない
    let vc_assignments: Vec<VcAssignment> = vc_pool
        .iter()
        .map(|pair| VcAssignment {
            label: pair.label.clone(),
            red_channel_id: pair.red.get(),
            blue_channel_id: pair.blue.get(),
        })
        .collect();
    let matchmaker = Matchmaker::new(matchmaker_repo, vc_assignments, matchmaker_config);
    let adapter = Arc::new(Adapter::new(
        repo,
        gateway,
        discord_config,
        vc_pool,
        lifecycle.clone(),
        Box::new(DefaultHostSelector::from_config(&lifecycle)),
    ));
    handler.attach(adapter.clone());
    let timeout_adapter = adapter.clone();

    // 認証設定が欠けていたら起動しない（本番で「ログインだけ無い」状態を作らない）
    let auth = AuthState::new(
        auth_config.clone(),
        Arc::new(HttpOAuthClient::new(&auth_config)?),
        registration_repo.clone(),
    );
    info!(
        redirect_uri = %auth_config.redirect_uri,
        cookie_domain = %auth_config.cookie_domain,
        "registration routes mounted"
    );

    let listener = TcpListener::bind(&listen_addr).await?;
    info!(%listen_addr, "http server listening");
    let app = router(ApiState {
        adapter,
        internal_token,
    })
    .merge(registration_router(auth))
    .merge(queue_router(QueueState::new(
        auth_config.clone(),
        registration_repo,
        queue_repo,
    )))
    .merge(status_router(StatusState::new(
        auth_config.clone(),
        ratings_repo,
        status_repo,
    )));
    info!("status routes mounted");
    // fail-closed: 管理者 ID が未設定ならルート自体をマウントしない
    // （INTERNAL_API_TOKEN と同じ方針。設定してもセッションの Discord ID 照合が正）
    let app = match admin_ids {
        Some(ids) => {
            info!(admins = %ids, "admin routes mounted");
            app.merge(admin_router(AdminState::new(auth_config, ids, admin_store)))
        }
        None => {
            warn!(
                "{ADMIN_DISCORD_USER_IDS} is not set: admin routes are not mounted \
                 (GET /admin will 404 for everyone)"
            );
            app
        }
    };

    let mut discord_task = tokio::spawn(async move { client.start().await });
    let mut http_task = tokio::spawn(async move { axum::serve(listener, app).await });
    // 期限の正は DB のイベント時刻。これは状態を持たない起床契機であり、起動時と同じ
    // `resume_unfinished` を呼ぶだけにして二重のリカバリ経路を作らない。
    let mut timeout_scan = tokio::time::interval(Duration::from_secs(5));
    // マッチメイカ（07 §4-1: 3 分ごと）。1 回の実行が 1 トランザクションで完結するので、
    // 状態を跨いで持つものが無く、プロセス再起動は次のティックから自然に続く。
    // 遅れを取り戻すために連続で走らせない（Delay）。同時実行は DB の advisory lock が防ぐ。
    let mut matchmaker_scan = tokio::time::interval(matchmaker.interval());
    matchmaker_scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    info!(
        interval_seconds = matchmaker.interval().as_secs(),
        "matchmaker loop started"
    );

    loop {
        tokio::select! {
            result = &mut discord_task => {
                match result {
                    Ok(Ok(())) => info!("discord client stopped"),
                    Ok(Err(error)) => error!(%error, "discord client failed"),
                    Err(error) => error!(%error, "discord task panicked"),
                }
                break;
            }
            result = &mut http_task => {
                match result {
                    Ok(Ok(())) => info!("http server stopped"),
                    Ok(Err(error)) => error!(%error, "http server failed"),
                    Err(error) => error!(%error, "http server panicked"),
                }
                break;
            }
            _ = timeout_scan.tick() => {
                if let Err(error) = timeout_adapter.resume_unfinished().await {
                    error!(%error, "timeout recovery scan failed");
                }
            }
            _ = matchmaker_scan.tick() => {
                // 成立した formation は `vc_assigned` まで書かれた状態でコミットされる。
                // 割り当て表とホスト打診は上の `resume_unfinished` が出す（入口を 2 つ作らない）
                match matchmaker.run_once(chrono::Utc::now()).await {
                    Ok(outcome) => info!(
                        queued = outcome.candidates,
                        formed = outcome.created.len(),
                        vc_exhausted = outcome.vc_exhausted,
                        "matchmaker run finished"
                    ),
                    Err(error) => error!(%error, "matchmaker run failed"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// assay_migrator 接続だけで全マイグレーションを適用する。通常起動で呼ばないこと。
async fn migrate() -> Result<(), Box<dyn Error>> {
    let database_url =
        env::var(MIGRATOR_DATABASE_URL).map_err(|_| "MIGRATOR_DATABASE_URL is not set")?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(())
}
