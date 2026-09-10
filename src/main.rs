//! axum サーバ + Discord クライアントを単一プロセスで動かす。どちらかが終了したらプロセス全体を終了させる。

use std::env;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serenity::http::Http;
use serenity::model::id::UserId;
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use unite_matchsystem::admin::{
    ADMIN_DISCORD_USER_IDS, AdminRepository, AdminState, admin_ids_from_env, router as admin_router,
};
use unite_matchsystem::api::{ApiState, router};
use unite_matchsystem::blocks::BlockRepository;
use unite_matchsystem::discord::{
    Adapter, DiscordConfig, Handler, SerenityGateway, build_client, resolve_vc_pool,
};
use unite_matchsystem::lifecycle::{DefaultHostSelector, LifecycleConfig};
use unite_matchsystem::match_screen::{MatchRepository, MatchState, router as match_router};
use unite_matchsystem::matchmaker::{
    MATCHMAKING_PARAMS_VERSION, Matchmaker, MatchmakerConfig, MatchmakerRepository,
};
use unite_matchsystem::persistence::{FormationRepository, VcAssignment};
use unite_matchsystem::profiles::{ProfileRepository, ProfileState, router as profile_router};
use unite_matchsystem::queue::{
    DOWN_RANGE, QueueRepository, QueueState, UP_RANGE, router as queue_router,
};
use unite_matchsystem::quiet::{QuietReportConfig, QuietReportRepository};
use unite_matchsystem::ranking::{
    RANKING_PUBLIC, RankingRepository, RankingState, ranking_is_public, router as ranking_router,
};
use unite_matchsystem::rating::{BASE_DELTA, RATING_PARAMS_VERSION, RatingAdjustmentRepository};
use unite_matchsystem::registration::{
    AuthConfig, AuthState, HttpOAuthClient, RegistrationRepository, report_gate_disabled_from_env,
    router as registration_router, site_base_url,
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
        Some("backfill-discord-appearances") => return backfill_discord_appearances().await,
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
    // 打診の期限と候補人数は当日の校正対象。0 は起動で落とす
    let lifecycle = LifecycleConfig::from_env()?;
    info!(
        host_accept_timeout_seconds = lifecycle.host_accept_timeout.as_secs(),
        host_candidate_limit = lifecycle.max_host_candidates,
        "host prompt settings loaded"
    );
    // 確定条件は当日に校正しうる。どの値で走っているかを起動ログに残す
    info!(
        report_window_minutes = lifecycle.report_window.as_secs() / 60,
        report_majority_threshold = lifecycle.report_majority_threshold,
        "report settlement settings loaded"
    );
    // **コード上の既定値**。実際に適用されている値は DB から読んで下でもう一度出す
    // （レート増減幅は管理パネルの最新行が正で、この定数は「一度も変えていないとき」の値）。
    info!(
        default_rating_params_version = RATING_PARAMS_VERSION,
        default_base_delta = BASE_DELTA,
        matchmaking_params_version = MATCHMAKING_PARAMS_VERSION,
        down_range = DOWN_RANGE,
        up_range = UP_RANGE,
        "compiled-in params defaults"
    );

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
    let admin_store = AdminRepository::new(pool.clone())
        .with_majority_threshold(lifecycle.report_majority_threshold);
    // **いま実際に全履歴へ適用されている値。**管理パネルの最新行から導出したもので、
    // 上のログに出た定数ではない。どちらで走っているかを起動時に必ず見えるようにする
    match admin_store.rating_params().await {
        Ok(params) => info!(
            rating_params_version = params.params_version,
            base_delta = params.base_delta,
            source = if params.params_version == RATING_PARAMS_VERSION {
                "compiled-in default"
            } else {
                "rating_param_changes (admin panel)"
            },
            "rating params in effect"
        ),
        Err(error) => {
            warn!(%error, "could not read the rating params in effect");
        }
    }
    // 聞き専報告。閾値は環境変数が正だが、レートの合算は player_current_ratings 1 か所に
    // 集約してあるので、判定に使う閾値も SQL から見えるよう起動時に同期する
    let quiet_repo = QuietReportRepository::new(pool.clone());
    let quiet_config = QuietReportConfig::from_env()?;
    match quiet_repo.sync_thresholds(quiet_config, Utc::now()).await {
        Ok(changed) => info!(
            quiet_winner_threshold = quiet_config.winner_threshold,
            quiet_loser_threshold = quiet_config.loser_threshold,
            appended = changed,
            "quiet report thresholds in effect"
        ),
        Err(error) => {
            // 同期できないと過去の閾値のまま判定される。黙って進めない
            error!(%error, "could not sync the quiet report thresholds");
            return Err(error.into());
        }
    }
    let block_repo = BlockRepository::new(pool.clone());
    let match_pool = pool.clone();
    let ranking_pool = pool.clone();
    let matchmaker_repo = MatchmakerRepository::new(pool.clone());

    // 停止状態は DB が正（メモリに持たない）。**起動ログに必ず出す** — 止めたまま再起動して
    // 「動いているつもりで動いていない」ことに気付かないのが、当日いちばん危ない
    match admin_store.matchmaker_pause().await {
        Ok(Some(pause)) => warn!(
            paused_by = pause.actor_discord_user_id,
            paused_since = %pause.since,
            reason = pause.reason.as_deref().unwrap_or("(なし)"),
            "MATCHMAKER IS PAUSED: no matches will be formed until it is resumed from /admin"
        ),
        Ok(None) => info!("matchmaker is running (not paused)"),
        // 読めなくても起動は続ける。マッチメイカは実行ごとに自分で読み直す
        Err(error) => warn!(%error, "could not read the matchmaker pause state at startup"),
    }

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
    // 通知に載せるマッチ画面 URL の起点。リダイレクト URI から導出するので、
    // ログインの行き先と別のホストを指すことがない
    let site = site_base_url(&auth_config.redirect_uri);
    let room_map = discord_config.room_map.clone();
    let room_checklist = discord_config.room_checklist.clone();
    let adapter = Arc::new(Adapter::new(
        repo.clone(),
        gateway,
        discord_config,
        vc_pool,
        lifecycle.clone(),
        Box::new(DefaultHostSelector::from_config(&lifecycle)),
        site.clone(),
    ));
    handler.attach(adapter.clone());
    let timeout_adapter = adapter.clone();
    let matchmaker_log_adapter = adapter.clone();

    // 認証設定が欠けていたら起動しない（本番で「ログインだけ無い」状態を作らない）
    //
    // 管理者リストは全トラックの State に配る。共通ヘッダーが管理パネルの導線を
    // 出すかどうかの判定に使うだけで、`/admin` のアクセス制御は admin ルート側の
    // 照合が正（リンクが無くても 404 は 404）。
    // ログインのたびに Discord 側の見た目を積む。**積めなくてもログインは通す**
    let profile_repo = ProfileRepository::new(pool.clone());
    // ランキングの公開スイッチ。**未設定なら非公開**（導線も出さない）
    let ranking_public = ranking_is_public(std::env::var(RANKING_PUBLIC).ok().as_deref());
    info!(ranking_public, "ranking visibility resolved");
    let auth = AuthState::new(
        auth_config.clone(),
        Arc::new(HttpOAuthClient::new(&auth_config)?),
        registration_repo.clone(),
    )
    .with_admins(admin_ids.clone())
    .with_ranking(ranking_public)
    .with_profiles(profile_repo.clone());
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
    .merge(queue_router(
        QueueState::new(auth_config.clone(), registration_repo.clone(), queue_repo)
            .with_admins(admin_ids.clone())
            .with_ranking(ranking_public),
    ))
    .merge(status_router(
        StatusState::new(auth_config.clone(), ratings_repo, status_repo)
            .with_admins(admin_ids.clone())
            .with_ranking(ranking_public),
    ))
    // 操作はすべてここ。Discord にはインタラクションを残さない
    .merge(match_router(
        MatchState::new(
            auth_config.clone(),
            repo,
            MatchRepository::new(match_pool),
            quiet_repo.clone(),
            lifecycle.clone(),
            &room_map,
            &room_checklist,
        )
        .with_admins(admin_ids.clone())
        .with_ranking(ranking_public),
    ))
    .merge(profile_router(
        ProfileState::new(
            auth_config.clone(),
            profile_repo,
            registration_repo.clone(),
            block_repo.clone(),
        )
        .with_admins(admin_ids.clone())
        .with_ranking(ranking_public),
    ))
    .merge(ranking_router(
        RankingState::new(
            auth_config.clone(),
            RankingRepository::new(ranking_pool),
            registration_repo,
            ranking_public,
        )
        .with_admins(admin_ids.clone())
        .with_ranking(ranking_public),
    ));
    info!(site = %site, "status and match routes mounted");
    // fail-closed: 管理者 ID が未設定ならルート自体をマウントしない
    // （INTERNAL_API_TOKEN と同じ方針。設定してもセッションの Discord ID 照合が正）
    let app = match admin_ids {
        Some(ids) => {
            info!(admins = %ids, "admin routes mounted");
            app.merge(admin_router(AdminState::new(
                auth_config,
                ids,
                admin_store,
                block_repo,
                quiet_repo,
            )))
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
    let mut matchmaker_summary = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(15 * 60),
        Duration::from_secs(15 * 60),
    );
    matchmaker_summary.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                let now = chrono::Utc::now();
                match matchmaker.run_once(now).await {
                    Ok(outcome) => {
                        info!(
                            queued = outcome.candidates,
                            formed = outcome.created.len(),
                            vc_exhausted = outcome.vc_exhausted,
                            paused = outcome.paused,
                            "matchmaker run finished"
                        );
                        if let Err(error) = matchmaker_log_adapter
                            .publish_matchmaker_run_log(&outcome, now)
                            .await
                        {
                            error!(%error, "could not record matchmaker Discord log failure");
                        }
                    }
                    Err(error) => error!(%error, "matchmaker run failed"),
                }
            }
            _ = matchmaker_summary.tick() => {
                let now = chrono::Utc::now();
                // 集計やDiscordの再試行で次の3分実行を待たせない。これは15分タイマーの
                // 実行場所を分けるだけで、別の回復・マッチメイカループは作らない。
                let summary_matchmaker = matchmaker.clone();
                let summary_adapter = matchmaker_log_adapter.clone();
                // 集計・Discord送信は次回のマッチング走査を待たせない。
                std::mem::drop(tokio::spawn(async move {
                    match summary_matchmaker.summary(now).await {
                        Ok(summary) => {
                            if let Err(error) = summary_adapter
                                .publish_matchmaker_summary(&summary, now)
                                .await
                            {
                                error!(%error, "could not record matchmaker summary Discord failure");
                            }
                        }
                        Err(error) => error!(%error, "matchmaker summary query failed"),
                    }
                }));
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

async fn backfill_discord_appearances() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let discord_config = DiscordConfig::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let profiles = ProfileRepository::new(pool.clone());
    let players = profiles.players_without_appearance().await?;
    let http = Http::new(&discord_config.token);
    let mut saved = 0usize;
    let mut unavailable = 0usize;

    for player in players {
        let Ok(discord_user_id) = u64::try_from(player.discord_user_id) else {
            unavailable += 1;
            continue;
        };
        let member = match discord_config
            .guild_id
            .member(&http, UserId::new(discord_user_id))
            .await
        {
            Ok(member) => member,
            Err(_) => {
                unavailable += 1;
                continue;
            }
        };
        let Some(display_name) = member
            .user
            .global_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            unavailable += 1;
            continue;
        };
        let avatar_hash = member.user.avatar.as_ref().map(ToString::to_string);
        profiles
            .record_appearance(
                player.player_id,
                display_name,
                avatar_hash.as_deref(),
                chrono::Utc::now(),
            )
            .await?;
        saved += 1;
    }
    info!(saved, unavailable, "discord appearance backfill completed");
    pool.close().await;
    Ok(())
}
