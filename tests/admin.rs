//! 運営用レート調整パネル（`/admin`）の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。
//!
//! 検証すること（不変条件との対応）:
//! - 管理者 ID 未設定（`parse_admin_ids -> None`）なら **ルートをマウントしない**。
//!   `main.rs` と同じ分岐をテスト側でも通す（`INTERNAL_API_TOKEN` の前例と同じ）
//! - 管理者判定はセッションの Discord ID だけ。リスト外・セッション無しは **404**
//!   （403 を返さない）。クエリやフォームで管理者を名乗る経路は無い
//! - 理由は必須（空は行にしない）。`created_by` は行動中の管理者から自動
//! - 冪等: **同一 idempotency_key のみ** no-op。キーが違うと同じ内容でも別操作
//! - 打ち消しは `reverts_id` の既存機構。二重打ち消し（unique index）と
//!   打ち消しの再打ち消し（トリガー）は DB が拒否し、アプリは回避しない
//! - 調整はレートと**マッチング範囲の判定**に効く（up 固定幅 95）
//!
//! セッションは `tests/common` の本物のログイン経路（偽 OAuth）で張る。

mod common;

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use common::{
    APP_ROLE, APP_ROLE_PASSWORD, Browser, FakeOAuth, at, body_text, config, player, sign_in,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::admin::{
    AdminIds, AdminRepository, AdminState, parse_admin_ids, router as admin_router,
};
use unite_matchsystem::blocks::BlockRepository;
use unite_matchsystem::lifecycle::PlayerId;
use unite_matchsystem::matchmaker::ROLES;
use unite_matchsystem::queue::QueueRepository;
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
use unite_matchsystem::quiet::QuietReportRepository;
use unite_matchsystem::rating::RatingAdjustmentRepository;
use unite_matchsystem::registration::{
    AuthState, RegistrationRepository, router as registration_router,
};

/// .env.example に入れている管理者の 1 人目。
const ADMIN_ID: i64 = 900000000000000001;

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

/// `main.rs` と同じ分岐: `Some(ids)` のときだけ管理者ルートをマウントする。
/// （本番の入口は `admin_ids_from_env()` で、これは env を `parse_admin_ids` に
/// 通したもの。未設定・空は `None` = マウントしない）
/// 土台の登録ルートはログイン（セッション確立）に必要なだけ。
fn app_with_admin(db: &AppDb, oauth: Arc<FakeOAuth>, ids: Option<AdminIds>) -> Router {
    // `main.rs` と同じく、リストは登録トラックにも配る（共通ヘッダーの導線の判定に使う）
    let base = registration_router(
        AuthState::new(config(), oauth, db.registration.clone()).with_admins(ids.clone()),
    );
    match ids {
        Some(ids) => base.merge(admin_router(AdminState::new(
            config(),
            ids,
            db.store.clone(),
            db.blocks.clone(),
            QuietReportRepository::new(db.pool.clone()),
        ))),
        None => base,
    }
}

/// 管理者としてログイン済みのブラウザ。
async fn admin_browser(db: &AppDb, oauth: &Arc<FakeOAuth>, ids: Option<AdminIds>) -> Browser {
    let id = ADMIN_ID.to_string();
    oauth.set_user_id(&id);
    let mut browser = Browser::new(app_with_admin(db, oauth.clone(), ids));
    let signed_in = sign_in(&mut browser, &db.registration, &id).await;
    assert_eq!(signed_in, player(1), "the seeded admin row is reused");
    browser
}

/// 一般プレイヤーとしてログイン済みのブラウザ。
async fn player_browser(
    db: &AppDb,
    oauth: &Arc<FakeOAuth>,
    ids: Option<AdminIds>,
    n: u128,
) -> Browser {
    let id = (1000 + n as i64).to_string();
    oauth.set_user_id(&id);
    let mut browser = Browser::new(app_with_admin(db, oauth.clone(), ids));
    let signed_in = sign_in(&mut browser, &db.registration, &id).await;
    assert_eq!(signed_in, player(n), "the seeded player row is reused");
    browser
}

struct AppDb {
    pool: PgPool,
    ratings: RatingAdjustmentRepository,
    queue: QueueRepository,
    store: AdminRepository,
    blocks: BlockRepository,
    registration: RegistrationRepository,
}

impl AppDb {
    async fn connect(owner: &PgPool) -> Self {
        let options = (*owner.connect_options())
            .clone()
            .username(APP_ROLE)
            .password(APP_ROLE_PASSWORD);
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .connect_with(options)
            .await
            .expect("connect as the app role");
        Self {
            ratings: RatingAdjustmentRepository::new(pool.clone()),
            queue: QueueRepository::new(pool.clone()),
            store: AdminRepository::new(pool.clone()),
            blocks: BlockRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            pool,
        }
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

async fn seed_player(db: &AppDb, n: u128, discord_user_id: i64, trainer: Option<&str>) {
    sqlx::query(
        "insert into players (id, discord_user_id, created_at, trainer_name) \
         values ($1, $2, now(), $3)",
    )
    .bind(player(n).0)
    .bind(discord_user_id)
    .bind(trainer)
    .execute(&db.pool)
    .await
    .expect("seed a player");
}

/// 1 番が管理者、2 番が調整対象、3 以降は一般（discord_user_id は 1000 + n）。
async fn seed_cast(db: &AppDb, count: u128) {
    seed_player(db, 1, ADMIN_ID, Some("管理者")).await;
    for n in 2..=count {
        seed_player(db, n, 1000 + n as i64, Some(&format!("t{n}"))).await;
    }
}

async fn adjustment_count(db: &AppDb) -> i64 {
    sqlx::query_scalar("select count(*) from rating_adjustments")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

async fn adjustment_id(db: &AppDb, key: &str) -> i64 {
    sqlx::query_scalar("select id from rating_adjustments where idempotency_key = $1")
        .bind(key)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

async fn enqueue(db: &AppDb, who: PlayerId, role: &str) {
    db.queue
        .enqueue(
            who,
            &QueueRequest {
                roles: vec![RolePreference {
                    role: role.to_owned(),
                    priority: 1,
                }],
            },
            at(0),
        )
        .await
        .expect("enqueue");
}

async fn queued_rating(db: &AppDb, who: PlayerId) -> f64 {
    db.queue
        .queued_players()
        .await
        .unwrap()
        .into_iter()
        .find(|queued| queued.player_id == who)
        .expect("player is queued")
        .rating
}

/// パネルの調整フォーム相当の POST。
fn adjust_body(target: PlayerId, key: &str, delta_mu: &str, reason: &str) -> String {
    format!(
        "player={}&key={key}&delta_mu={delta_mu}&delta_sigma=0&reason={reason}",
        target.0.simple()
    )
}

// ----------------------------------------------------------------------
// fail-closed: 未設定ならルート自体が無い
// ----------------------------------------------------------------------

/// 管理者 ID 未設定では `/admin*` が**存在しない**（管理者のセッションでも 404）。
/// 設定した瞬間に管理者だけが見える。これが `main.rs` の分岐と同じ形であることが要点。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_admin_routes_are_absent_when_the_ids_are_unset(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    let oauth = Arc::new(FakeOAuth::new("1002"));

    // env 未設定と同じ値（None）を通すと、ルートはマウントされない。
    // **管理者のセッションを持っていても** 404（axum の素の Not Found）
    let mut admin = admin_browser(&db, &oauth, parse_admin_ids(None).unwrap()).await;
    let player_uri = format!("/admin/player?player={}", player(2).0.simple());
    for (method, uri) in [
        ("GET", "/admin"),
        ("GET", player_uri.as_str()),
        ("POST", "/admin/adjustments"),
        ("POST", "/admin/revert"),
    ] {
        let response = if method == "GET" {
            admin.get(uri).await
        } else {
            admin.post_form(uri, "").await
        };
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {uri} must not exist without admin ids"
        );
    }
    // 応答身は空（= ルートを通過前の axum 既定の 404）。マウント済みなら
    // 非管理者でも独自の「見つかりません」ページになる（次のテストで検査）。
    // 同じブラウザ・同じ Cookie のまま、この body 差が「ルート有無」の判別になる
    let bare = body_text(admin.get("/admin").await).await;
    assert!(
        bare.is_empty(),
        "unmounted routes give the empty default 404: {bare}"
    );
    // 空・空白だけの設定も未設定と同じ（fail-closed）
    let mut blank = admin_browser(&db, &oauth, parse_admin_ids(Some("  ,  ")).unwrap()).await;
    assert_eq!(blank.get("/admin").await.status(), StatusCode::NOT_FOUND);

    // 対照群: 設定すれば管理者のセッションで見える（同じブラウザ操作のままだが、
    // ルートがある管理者のセッションなら 200）
    let mut mounted = admin_browser(
        &db,
        &oauth,
        parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap(),
    )
    .await;
    let page = body_text(mounted.get("/admin").await).await;
    assert!(page.contains("レート調整パネル"), "{page}");

    db.done().await;
}

/// リスト外のセッション・セッション無しは 404（403 ではない）。
/// フォームで `created_by` を送っても管理者を名乗れない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_session_outside_the_list_gets_404_not_403(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    let ids = || parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap();
    let oauth = Arc::new(FakeOAuth::new("1002"));

    // セッション無し
    let mut anonymous = Browser::new(app_with_admin(&db, oauth.clone(), ids()));
    let response = anonymous.get("/admin").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let page = body_text(response).await;
    assert!(page.contains("見つかりません"), "{page}");
    assert!(!page.contains("レート調整パネル"), "{page}");

    // 一般プレイヤー（discord 1002）
    let mut nope = player_browser(&db, &oauth, ids(), 2).await;
    assert_eq!(nope.get("/admin").await.status(), StatusCode::NOT_FOUND);

    // 書き込みも同じ。プレイヤーを名乗れても行は残らない
    let submit = nope
        .post_form(
            "/admin/adjustments",
            &adjust_body(player(2), "forged", "1000", "i-am-admin"),
        )
        .await;
    assert_eq!(submit.status(), StatusCode::NOT_FOUND);
    assert_eq!(adjustment_count(&db).await, 0);

    // クエリで `admin=true` を足しても判定は変わらない（判定材料はセッションだけ）
    assert_eq!(
        nope.get("/admin?admin=true").await.status(),
        StatusCode::NOT_FOUND
    );

    db.done().await;
}

// ----------------------------------------------------------------------
// 理由必須・created_by 自動・レート反映
// ----------------------------------------------------------------------

/// 理由が空（空白のみ含む）は行にしない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_empty_reason_is_rejected_without_a_row(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    let oauth = Arc::new(FakeOAuth::new("1002"));
    let mut admin = admin_browser(
        &db,
        &oauth,
        parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap(),
    )
    .await;

    for (tag, reason) in [("empty", ""), ("blank", "%20%20%20")] {
        let body = adjust_body(player(2), &format!("key-{tag}"), "10", reason);
        let response = admin.post_form("/admin/adjustments", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let page = body_text(response).await;
        assert!(page.contains("理由は必須です"), "reason={reason:?}: {page}");
    }
    assert_eq!(adjustment_count(&db).await, 0);

    // 数値でない増減も同じ（行にしない）
    let body = adjust_body(player(2), "key-nan", "10x", "seed+fix");
    let page = body_text(admin.post_form("/admin/adjustments", &body).await).await;
    assert!(page.contains("数値で入力"), "{page}");
    assert_eq!(adjustment_count(&db).await, 0);

    db.done().await;
}

/// 調整は `created_by` を行動中の管理者から自動で埋め、レートに効く。
/// フォームに `created_by` を混ぜても無視される。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_adjustment_is_appended_under_the_acting_admin(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    let oauth = Arc::new(FakeOAuth::new("1002"));
    let mut admin = admin_browser(
        &db,
        &oauth,
        parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap(),
    )
    .await;

    let body = format!(
        "{}&created_by=discord:999",
        adjust_body(player(2), "fix-1", "10", "seed+fix")
    );
    let page = body_text(admin.post_form("/admin/adjustments", &body).await).await;
    assert!(page.contains("追記しました"), "{page}");

    let (created_by, delta_mu, reverts_id): (String, f64, Option<i64>) = sqlx::query_as(
        "select created_by, delta_mu, reverts_id from rating_adjustments where idempotency_key = 'fix-1'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        created_by,
        format!("discord:{ADMIN_ID}"),
        "フォーム値は使わない"
    );
    assert_eq!(delta_mu, 10.0);
    assert_eq!(reverts_id, None);

    // 表示スケールのままレートに反映される
    assert_eq!(db.ratings.display_rating(player(2)).await.unwrap(), 1510.0);
    assert_eq!(db.ratings.display_rating(player(3)).await.unwrap(), 1500.0);

    db.done().await;
}

// ----------------------------------------------------------------------
// 冪等
// ----------------------------------------------------------------------

/// **同一 idempotency_key のみ** no-op。キーが違うと同じ内容でも別の行になる
/// （状態ベースの冪等にしない — 同じ操作の定義はキーだけ）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_same_idempotency_key_never_appends_twice(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    let oauth = Arc::new(FakeOAuth::new("1002"));
    let mut admin = admin_browser(
        &db,
        &oauth,
        parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap(),
    )
    .await;
    let body = adjust_body(player(2), "twice", "10", "seed+fix");

    let first = admin.post_form("/admin/adjustments", &body).await;
    assert!(body_text(first).await.contains("追記しました"));
    assert_eq!(adjustment_count(&db).await, 1);

    // 同じフォームの再送信（同じキー）: no-op で、行は増えない
    let again = admin.post_form("/admin/adjustments", &body).await;
    let page = body_text(again).await;
    assert!(page.contains("行は増やしていません"), "{page}");
    assert_eq!(adjustment_count(&db).await, 1);
    assert_eq!(db.ratings.display_rating(player(2)).await.unwrap(), 1510.0);

    // キーが違うと同じ内容でも**別の操作**（ここを no-op にすると二重調整が消えない）
    let other = admin
        .post_form(
            "/admin/adjustments",
            &adjust_body(player(2), "third-time", "10", "seed+fix"),
        )
        .await;
    assert!(body_text(other).await.contains("追記しました"));
    assert_eq!(adjustment_count(&db).await, 2);
    assert_eq!(db.ratings.display_rating(player(2)).await.unwrap(), 1520.0);

    db.done().await;
}

// ----------------------------------------------------------------------
// 打ち消し（reverts_id の既存機構）
// ----------------------------------------------------------------------

/// 打ち消しは逆符号の行を追記する。二重打ち消しは unique index が、
/// 打ち消しの再打ち消しはトリガーが拒否する（アプリは回避せず、出た違反を翻訳する）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_revert_negates_and_cannot_be_repeated_or_reversed(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    let oauth = Arc::new(FakeOAuth::new("1002"));
    let mut admin = admin_browser(
        &db,
        &oauth,
        parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap(),
    )
    .await;

    admin
        .post_form(
            "/admin/adjustments",
            &adjust_body(player(2), "orig", "25", "seed+fix"),
        )
        .await;
    assert_eq!(db.ratings.display_rating(player(2)).await.unwrap(), 1525.0);
    let original = adjustment_id(&db, "orig").await;

    // 打ち消し -> 相殺されて 1500 に戻る
    let page = body_text(
        admin
            .post_form(
                "/admin/revert",
                &format!("adjustment_id={original}&key=revert:orig"),
            )
            .await,
    )
    .await;
    assert!(page.contains("打ち消しました"), "{page}");
    assert_eq!(db.ratings.display_rating(player(2)).await.unwrap(), 1500.0);
    assert_eq!(adjustment_count(&db).await, 2);
    let reversal = adjustment_id(&db, "revert:orig").await;
    let (negating, reverts): (f64, Option<i64>) =
        sqlx::query_as("select delta_mu, reverts_id from rating_adjustments where id = $1")
            .bind(reversal)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(negating, -25.0);
    assert_eq!(reverts, Some(original));

    // 二重打ち消し: キーを変えても unique index が拒否し、行は増えない
    let page = body_text(
        admin
            .post_form(
                "/admin/revert",
                &format!("adjustment_id={original}&key=revert:again"),
            )
            .await,
    )
    .await;
    assert!(page.contains("既に打ち消されています"), "{page}");
    assert_eq!(adjustment_count(&db).await, 2);

    // 打ち消しの再打ち消し: トリガーが拒否する
    let page = body_text(
        admin
            .post_form(
                "/admin/revert",
                &format!("adjustment_id={reversal}&key=revert:of-revert"),
            )
            .await,
    )
    .await;
    assert!(
        page.contains("打ち消し行そのものは打ち消せません"),
        "{page}"
    );
    assert_eq!(adjustment_count(&db).await, 2);

    // 同じ打ち消しフォームの再送信（同じキー）は新しい行にしない
    let page = body_text(
        admin
            .post_form(
                "/admin/revert",
                &format!("adjustment_id={original}&key=revert:orig"),
            )
            .await,
    )
    .await;
    assert!(page.contains("行は増やしていません"), "{page}");
    assert_eq!(adjustment_count(&db).await, 2);

    db.done().await;
}

// ----------------------------------------------------------------------
// マッチング範囲への反映
// ----------------------------------------------------------------------

/// パネルからの調整はレートだけでなく**マッチング範囲の判定**にも効く
/// （上方向は固定幅 95。1500 + 100 = 1600 は 1500 と組めない）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_adjustment_moves_the_matching_range(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 11).await;
    let oauth = Arc::new(FakeOAuth::new("1002"));
    let mut admin = admin_browser(
        &db,
        &oauth,
        parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap(),
    )
    .await;

    // 2 番と 11 番はどちらも 1500。相互に範囲内
    enqueue(&db, player(2), ROLES[0]).await;
    enqueue(&db, player(11), ROLES[1]).await;
    assert_eq!(queued_rating(&db, player(2)).await, 1500.0);
    assert_eq!(
        db.queue
            .in_range_summary(player(2))
            .await
            .unwrap()
            .map(|s| s.total),
        Some(1)
    );

    // パネルで +100 -> 1600。差 100 は上方向の固定幅 95 の外
    admin
        .post_form(
            "/admin/adjustments",
            &adjust_body(player(2), "range-1", "100", "seed+fix"),
        )
        .await;
    assert_eq!(queued_rating(&db, player(2)).await, 1600.0);
    assert_eq!(
        db.queue
            .in_range_summary(player(2))
            .await
            .unwrap()
            .map(|s| s.total),
        Some(0),
        "1600 は 1500 と組めない"
    );

    // 打ち消すと範囲判定も戻る（追記ログの畳み込みなので当然、という形）
    let original = adjustment_id(&db, "range-1").await;
    admin
        .post_form(
            "/admin/revert",
            &format!("adjustment_id={original}&key=revert:range-1"),
        )
        .await;
    assert_eq!(queued_rating(&db, player(2)).await, 1500.0);
    assert_eq!(
        db.queue
            .in_range_summary(player(2))
            .await
            .unwrap()
            .map(|s| s.total),
        Some(1)
    );

    db.done().await;
}

/// 検索はトレーナー名（部分一致）と Discord ID（完全一致）。
/// Discord の表示名は DB に保存していないため、表示名検索はできない（レポートに明記）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn players_are_found_by_trainer_name_or_discord_id(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_player(&db, 2, 2002, Some("ハル")).await;
    seed_player(&db, 3, 2003, Some("ナル")).await;

    let by_name = db.store.search_players("ハル", 10).await.unwrap();
    assert_eq!(by_name.len(), 1, "{by_name:?}");
    assert_eq!(by_name[0].player_id, player(2).0);
    assert_eq!(by_name[0].trainer_name.as_deref(), Some("ハル"));

    let by_id = db.store.search_players("2003", 10).await.unwrap();
    assert_eq!(by_id.len(), 1, "{by_id:?}");
    assert_eq!(by_id[0].player_id, player(3).0);

    assert!(db.store.search_players("zzz", 10).await.unwrap().is_empty());

    db.done().await;
}

/// 管理パネルは管理者の画面にだけリンクされる。
///
/// リンクを出すことはアクセス制御ではない（`/admin` は非管理者に 404 を返し続ける）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_admin_panel_is_reachable_and_linked_only_for_admins(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    let ids = || parse_admin_ids(Some(&ADMIN_ID.to_string())).unwrap();
    let oauth = Arc::new(FakeOAuth::new("1002"));

    // 管理者: パネルを開け、各画面のヘッダーに導線がある。
    let mut admin = admin_browser(&db, &oauth, ids()).await;
    let panel = body_text(admin.get("/admin").await).await;
    assert!(panel.contains("レート調整パネル"), "{panel}");
    assert!(panel.contains("<a href=\"/status\">戦績</a>"), "{panel}");
    assert!(panel.contains("href=\"/admin\""), "{panel}");
    // 「管理」の有無はヘッダーだけで見る。本文にはトレーナー名「管理者」が出る
    let header_of = |body: &str| {
        let start = body.find("<header").expect("a header");
        let end = body.find("</header>").expect("a header end") + "</header>".len();
        body[start..end].to_owned()
    };
    let elsewhere = body_text(admin.get("/register").await).await;
    let admin_header = header_of(&elsewhere);
    assert!(elsewhere.contains("href=\"/admin\""), "{elsewhere}");
    assert!(
        admin_header.contains("<a href=\"/admin\">管理</a>"),
        "{admin_header}"
    );

    // 一般プレイヤー: 同じ構成。/admin は 404 のまま
    let mut nope = player_browser(&db, &oauth, ids(), 2).await;
    let page = body_text(nope.get("/register").await).await;
    let player_header = header_of(&page);
    assert!(page.contains("<a href=\"/status\">戦績</a>"), "{page}");
    assert!(page.contains("action=\"/auth/logout\""), "{page}");
    assert!(!page.contains("href=\"/admin\""), "{page}");
    assert!(!player_header.contains("admin"), "{player_header}");

    // 管理者だけに導線があるため、ヘッダーは一致しない。
    assert_ne!(
        admin_header, player_header,
        "管理者と一般参加者でヘッダーが違う"
    );

    let denied = nope.get("/admin").await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    // 404 の画面はログイン前と同じ見た目。ここからもパネルの存在は読めない
    let denied = body_text(denied).await;
    assert!(!denied.contains("href=\"/admin\""), "{denied}");
    assert!(!denied.contains("href=\"/status\""), "{denied}");

    // リスト未設定（パネル未マウント）の構成でも同じ
    let mut unset = admin_browser(&db, &oauth, None).await;
    let page = body_text(unset.get("/register").await).await;
    assert!(!page.contains("admin"), "{page}");
    assert_eq!(unset.get("/admin").await.status(), StatusCode::NOT_FOUND);

    db.done().await;
}
