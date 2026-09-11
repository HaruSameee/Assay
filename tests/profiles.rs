//! 公開プロフィールの統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。
//!
//! 検証すること:
//! - **担当ロールは、未設定 / 設定して非公開 / 設定して公開 の3状態で、
//!   キーが JSON に載るかどうかが変わる。**未設定と非公開は区別できない
//! - **ログインが回帰していない。**スナップショットの追記に失敗してもログインは通る
//! - 表示名とアバターはログインのたびに追記され、現在値は最新行から導出する
//! - 検証はすべてサーバ側（画面の `maxlength` を通さない値でも弾く）

mod common;

use std::sync::Arc;

use axum::Router;
use axum::http::{StatusCode, header};
use common::{APP_ROLE, APP_ROLE_PASSWORD, Browser, FakeOAuth, body_text, config, player, sign_in};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::blocks::BlockRepository;
use unite_matchsystem::lifecycle::PlayerId;
use unite_matchsystem::profiles::{ProfileRepository, ProfileState, router as profile_router};
use unite_matchsystem::registration::{
    AuthState, RegistrationRepository, router as registration_router,
};

struct AppDb {
    pool: PgPool,
    profiles: ProfileRepository,
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
            profiles: ProfileRepository::new(pool.clone()),
            blocks: BlockRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            pool,
        }
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

/// 登録ルート（ログイン経路）とプロフィールのルートを並べたアプリ。
fn app(db: &AppDb, oauth: Arc<FakeOAuth>) -> Router {
    registration_router(
        AuthState::new(config(), oauth, db.registration.clone()).with_profiles(db.profiles.clone()),
    )
    .merge(profile_router(ProfileState::new(
        config(),
        db.profiles.clone(),
        db.registration.clone(),
        db.blocks.clone(),
    )))
}

/// 本物のログイン経路（偽 OAuth）でセッションを張る。
async fn signed_in(
    db: &AppDb,
    oauth: &Arc<FakeOAuth>,
    discord_user_id: &str,
) -> (Browser, PlayerId) {
    oauth.set_user_id(discord_user_id);
    let mut browser = Browser::new(app(db, oauth.clone()));
    let id = sign_in(&mut browser, &db.registration, discord_user_id).await;
    (browser, id)
}

/// 公開プロフィールの JSON を取る（未ログインでも見られる）。
async fn public_json(db: &AppDb, oauth: &Arc<FakeOAuth>, player_id: PlayerId) -> Value {
    let mut anonymous = Browser::new(app(db, oauth.clone()));
    let response = anonymous.get(&format!("/players/{}", player_id.0)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("json body")
}

/// プロフィールを保存する（本人のフォーム送信）。
async fn save(browser: &mut Browser, body: &str) -> StatusCode {
    browser.post_form("/profile", body).await.status()
}

// ----------------------------------------------------------------------
// 担当ロールの3状態
// ----------------------------------------------------------------------

/// **未設定 / 設定して非公開 / 設定して公開 の3状態。**
///
/// 未設定と非公開は、どちらも `main_roles` のキーが無い状態で区別できない。
/// 空配列を返すと「設定していない」ことが確定情報になる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_main_roles_key_only_appears_when_published(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2001"));
    let (mut browser, me) = signed_in(&db, &oauth, "2001").await;

    // 1. 未設定
    let unset = public_json(&db, &oauth, me).await;
    println!("1. 未設定:            {unset}");
    assert!(unset.get("main_roles").is_none(), "{unset}");

    // 2. 設定して非公開（チェックボックスを送らない = 非公開）
    assert_eq!(
        save(
            &mut browser,
            "bio=&x_id=&main_roles=jungle&main_roles=bot_tank"
        )
        .await,
        StatusCode::OK
    );
    let private = public_json(&db, &oauth, me).await;
    println!("2. 設定して非公開:    {private}");
    assert!(
        private.get("main_roles").is_none(),
        "非公開なのにキーがある: {private}"
    );
    // **未設定のときと同じ形**（区別できない）
    assert_eq!(unset, private, "未設定と非公開が区別できてしまう");

    // 3. 設定して公開
    assert_eq!(
        save(
            &mut browser,
            "bio=&x_id=&main_roles=jungle&main_roles=bot_tank&main_roles_public=1"
        )
        .await,
        StatusCode::OK
    );
    let public = public_json(&db, &oauth, me).await;
    println!("3. 設定して公開:      {public}");
    assert_eq!(
        public["main_roles"],
        serde_json::json!(["jungle", "bot_tank"])
    );

    // 公開したあと非公開に戻すと、またキーごと消える
    assert_eq!(
        save(
            &mut browser,
            "bio=&x_id=&main_roles=jungle&main_roles=bot_tank"
        )
        .await,
        StatusCode::OK
    );
    let hidden_again = public_json(&db, &oauth, me).await;
    println!("4. 公開→非公開に戻す: {hidden_again}");
    assert!(hidden_again.get("main_roles").is_none(), "{hidden_again}");

    // 追記専用: 3 回の保存が 3 行残っている（上書きではない）
    let rows: i64 = sqlx::query_scalar("select count(*) from player_profiles")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 3, "上書きされている");
    println!("player_profiles の行数: {rows}（保存のたびに追記）");

    db.done().await;
}

/// 公開プロフィールに `discord_user_id` も、ブロックのような内部情報も載らない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_public_profile_carries_no_internal_identifiers(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2002"));
    let (mut browser, me) = signed_in(&db, &oauth, "2002").await;
    oauth.set_display_name(Some("Target Global Name"));
    let (_, target) = signed_in(&db, &oauth, "2009").await;

    let search = browser.get("/profile?block_query=Target").await;
    assert_eq!(search.status(), StatusCode::OK);
    let search_html = body_text(search).await;
    assert!(search_html.contains("Target Global Name"), "{search_html}");
    assert!(search_html.contains(&target.0.to_string()), "{search_html}");
    let response = browser
        .post_form(
            "/profile/blocks",
            &format!("action=block&target={}&key=block-privacy", target.0),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(db.blocks.current_for(me).await.unwrap().len(), 1);

    for player_id in [me, target] {
        let json = public_json(&db, &oauth, player_id).await;
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        println!("public profile JSON for {player_id:?}: {json}");
        println!("public profile keys for {player_id:?}: {keys:?}");
        for forbidden in [
            "discord_user_id",
            "main_roles_public",
            "blocked",
            "blocks",
            "block_allowance",
        ] {
            assert!(
                json.get(forbidden).is_none(),
                "{forbidden} leaked for {player_id:?}: {json}"
            );
        }
    }
    // アバター未設定のときは、既定アバターの URL に discord_user_id が入らない
    let json = public_json(&db, &oauth, me).await;
    let body = serde_json::to_string(&json).unwrap();
    assert_eq!(
        body.matches("2002").count(),
        0,
        "discord_user_id が出ている: {body}"
    );

    // **アバターを設定すると、CDN の URL に discord_user_id が入る。**
    // ページの URL には出さない方針だが、画像の URL には必ず入る（CDN の仕様）
    oauth.set_avatar(Some("abc123"));
    let (_, same) = signed_in(&db, &oauth, "2002").await;
    assert_eq!(same, me);
    let json = public_json(&db, &oauth, me).await;
    println!("アバターを設定したときの URL: {}", json["avatar_url"]);
    assert!(json["avatar_url"].as_str().unwrap().contains("2002"));
    // それでも本体のキーには出ない
    for forbidden in ["discord_user_id", "main_roles_public"] {
        assert!(json.get(forbidden).is_none(), "{forbidden}: {json}");
    }
    db.done().await;
}

// ----------------------------------------------------------------------
// ログインの回帰
// ----------------------------------------------------------------------

/// **ログイン経路が回帰していない。**スナップショットを積んでも行き先は変わらない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn signing_in_still_works_and_records_the_appearance(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2003"));
    oauth.set_display_name(Some("むかしの名前"));
    oauth.set_avatar(Some("abc123"));

    let (_, me) = signed_in(&db, &oauth, "2003").await;
    let appearance = db.profiles.appearance(me).await.unwrap().expect("recorded");
    assert_eq!(appearance.display_name, "むかしの名前");
    assert_eq!(appearance.avatar_hash.as_deref(), Some("abc123"));

    // 名前を変えて入り直すと、**最新行が現在値**になる（古い行は残る）
    oauth.set_display_name(Some("いまの名前"));
    oauth.set_avatar(None);
    let (_, again) = signed_in(&db, &oauth, "2003").await;
    assert_eq!(again, me, "同じ Discord ID なら同じプレイヤー");
    let appearance = db.profiles.appearance(me).await.unwrap().expect("recorded");
    assert_eq!(appearance.display_name, "いまの名前");
    assert_eq!(appearance.avatar_hash, None, "アバターを外した状態も残る");

    let history: Vec<String> =
        sqlx::query_scalar("select display_name from player_discord_snapshots order by id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(history, vec!["むかしの名前", "いまの名前"]);
    println!("表示名の履歴: {history:?}（係争の裁定で当時の名前を追える）");

    // アバターを外した人も公開プロフィールが壊れない（既定アバターになる）
    let json = public_json(&db, &oauth, me).await;
    assert!(
        json["avatar_url"]
            .as_str()
            .unwrap()
            .starts_with("https://cdn.discordapp.com/embed/avatars/"),
        "{json}"
    );
    println!("アバター未設定のときの URL: {}", json["avatar_url"]);

    db.done().await;
}

/// **スナップショットが積めなくてもログインは通る。**
///
/// ここで落とすと、プロフィールの不具合だけで全員が参加できなくなる。
/// 表示名が空（`global_name` も `username` も空）という、CHECK に触れる形で試す。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_failed_snapshot_does_not_break_sign_in(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2004"));
    oauth.set_display_name(Some("   "));

    let (_, me) = signed_in(&db, &oauth, "2004").await;
    // ログインは通っている（player 行がある）
    assert!(db.profiles.find_player(me.0).await.unwrap().is_some());
    // スナップショットは積まれていない
    assert!(db.profiles.appearance(me).await.unwrap().is_none());
    // それでも公開プロフィールは出る
    let json = public_json(&db, &oauth, me).await;
    assert_eq!(json["display_name"], "（未取得）");
    println!("表示名が取れないときの公開プロフィール: {json}");

    db.done().await;
}

// ----------------------------------------------------------------------
// 検証はサーバ側
// ----------------------------------------------------------------------

/// **X の ID は形式を通ったものだけ保存される。**画面の `maxlength` を通さない値で試す。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_x_id_is_validated_on_the_server(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2005"));
    let (mut browser, me) = signed_in(&db, &oauth, "2005").await;

    // フォームを経由せず、画面の制限を無視した値を直接送る
    for hostile in [
        "https%3A%2F%2Fx.com%2Fsomeone", // URL ごと
        "someone%2Fstatus%2F1",          // パス付き
        "javascript%3Aalert(1)",
        "0123456789abcdef", // 16 文字
        "with%20space",
        "%3Cscript%3E",
    ] {
        let status = save(&mut browser, &format!("bio=&x_id={hostile}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hostile} が通っている");
    }
    // 1 行も保存されていない
    let rows: i64 = sqlx::query_scalar("select count(*) from player_profiles")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "不正な値が保存されている");
    println!("不正な x_id を 6 通り送って、保存された行数: {rows}");

    // 正しい値は通り、公開プロフィールにリンクとして出る
    assert_eq!(
        save(&mut browser, "bio=&x_id=%40someone").await,
        StatusCode::OK
    );
    let json = public_json(&db, &oauth, me).await;
    assert_eq!(json["x_id"], "someone", "先頭の @ だけ落として保存する");

    db.done().await;
}

/// 自己紹介の長さは**文字数**で見る。DB の CHECK と同じ条件。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_bio_length_is_enforced_on_the_server(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2006"));
    let (mut browser, me) = signed_in(&db, &oauth, "2006").await;

    let long = "%E3%81%82".repeat(201); // 「あ」× 201
    assert_eq!(
        save(&mut browser, &format!("bio={long}&x_id=")).await,
        StatusCode::BAD_REQUEST
    );
    let ok = "%E3%81%82".repeat(200);
    assert_eq!(
        save(&mut browser, &format!("bio={ok}&x_id=")).await,
        StatusCode::OK
    );
    let json = public_json(&db, &oauth, me).await;
    assert_eq!(json["bio"].as_str().unwrap().chars().count(), 200);
    println!("200 文字は通り、201 文字は 400 で断られる");

    db.done().await;
}

/// **アプリ用ロールからは書き換えも削除もできない**（どちらの表も追記専用）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn both_tables_are_append_only(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2007"));
    let (mut browser, _) = signed_in(&db, &oauth, "2007").await;
    assert_eq!(save(&mut browser, "bio=hello&x_id=").await, StatusCode::OK);

    for sql in [
        "update player_profiles set bio = 'x'",
        "delete from player_profiles",
        "update player_discord_snapshots set display_name = 'x'",
        "delete from player_discord_snapshots",
    ] {
        let error = sqlx::query(sql).execute(&db.pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501"),
            "{sql}"
        );
    }
    db.done().await;
}

/// 存在しないプレイヤーは 404。UUID として読めない値も 404。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_unknown_player_is_not_found(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("2008"));
    let mut browser = Browser::new(app(&db, oauth.clone()));
    for path in [
        format!("/players/{}", player(999).0),
        "/players/not-a-uuid".to_owned(),
    ] {
        assert_eq!(
            browser.get(&path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    db.done().await;
}

// ----------------------------------------------------------------------
// キャラプールの画像
// ----------------------------------------------------------------------
//
// **1 人 1 枚で、貼り直しは置き換え。**`player_profiles` が追記専用なのと違い、
// ここは可変の参照データとして扱う（自己紹介を直すたびに 3MB 積まないため）。

/// PNG のシグネチャ + 指定バイトのダミー。
fn png(extra: usize) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend(std::iter::repeat_n(0u8, extra));
    bytes
}

/// **アップロードした PNG が配信され、公開プロフィールに出る。**
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_uploaded_png_is_served_and_shown(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("5000"));
    let (mut browser, player_id) = signed_in(&db, &oauth, "5001").await;

    let image = png(64);
    let response = browser
        .post_multipart("/profile/pool-image", Some(("image", &image)), &[])
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "保存できていない");

    // 公開 JSON に配信 URL が載る
    let public = public_json(&db, &oauth, player_id).await;
    assert_eq!(
        public["pool_image_url"],
        json!(format!("/players/{}/pool.png", player_id.0))
    );

    // その URL から本物のバイト列が返る
    let mut anonymous = Browser::new(app(&db, oauth.clone()));
    let served = anonymous
        .get(&format!("/players/{}/pool.png", player_id.0))
        .await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(
        served.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    // **型を推測させない。**中身は検査済みだが配信側でも固定する
    assert_eq!(
        served.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    let bytes = axum::body::to_bytes(served.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), image.as_slice(), "中身が変わっている");
    println!("アップロード: {} バイトがそのまま配信された", bytes.len());

    db.done().await;
}

/// **URL を貼るとそのまま出る。**アップロードはしない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_pasted_url_is_shown_as_is(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("5000"));
    let (mut browser, player_id) = signed_in(&db, &oauth, "5002").await;

    let response = browser
        .post_multipart(
            "/profile/pool-image",
            None,
            &[("image_url", "https://example.test/pool.png")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let public = public_json(&db, &oauth, player_id).await;
    assert_eq!(
        public["pool_image_url"],
        json!("https://example.test/pool.png")
    );
    // 配信の口からは何も出ない（アップロードしていない）
    let mut anonymous = Browser::new(app(&db, oauth.clone()));
    let served = anonymous
        .get(&format!("/players/{}/pool.png", player_id.0))
        .await;
    assert_eq!(served.status(), StatusCode::NOT_FOUND);
    println!("URL 貼り付け: そのまま公開、配信の口は 404");

    db.done().await;
}

/// **PNG 以外と 3MB 超は断る。**行も増えない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_bad_image_is_refused(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("5000"));
    let (mut browser, player_id) = signed_in(&db, &oauth, "5003").await;

    // JPEG のシグネチャ
    let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0];
    let response = browser
        .post_multipart("/profile/pool-image", Some(("image", &jpeg)), &[])
        .await;
    let status = response.status();
    let page = body_text(response).await;
    // 検証で断ったときは 400 + 編集画面（既存の流儀）
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("PNG の画像を選んでください"), "{page}");

    // 3MB + 1 バイト
    let too_big = png(3 * 1024 * 1024 - 8 + 1);
    let response = browser
        .post_multipart("/profile/pool-image", Some(("image", &too_big)), &[])
        .await;
    let page = body_text(response).await;
    assert!(page.contains("3MB"), "{page}");

    // http は断る
    let response = browser
        .post_multipart(
            "/profile/pool-image",
            None,
            &[("image_url", "http://example.test/a.png")],
        )
        .await;
    let page = body_text(response).await;
    assert!(page.contains("https:// で始まる"), "{page}");

    // 1 行も入っていない
    let rows: i64 = sqlx::query_scalar("select count(*) from player_pool_images")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "断ったのに保存されている");
    let public = public_json(&db, &oauth, player_id).await;
    assert!(public.get("pool_image_url").is_none(), "{public}");
    println!("PNG でない・3MB 超・http: どれも拒否、0 行");

    db.done().await;
}

/// **貼り直しは置き換え。**行は増えず、古い画像は残らない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn re_uploading_replaces_the_image(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("5000"));
    let (mut browser, player_id) = signed_in(&db, &oauth, "5004").await;

    let first = png(16);
    browser
        .post_multipart("/profile/pool-image", Some(("image", &first)), &[])
        .await;
    let second = png(32);
    browser
        .post_multipart("/profile/pool-image", Some(("image", &second)), &[])
        .await;

    let rows: i64 = sqlx::query_scalar("select count(*) from player_pool_images")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "**1 人 1 行**のはず");
    let mut anonymous = Browser::new(app(&db, oauth.clone()));
    let served = anonymous
        .get(&format!("/players/{}/pool.png", player_id.0))
        .await;
    let bytes = axum::body::to_bytes(served.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes.len(), second.len(), "古い画像が残っている");

    // URL に貼り替えると、アップロードは消える
    browser
        .post_multipart(
            "/profile/pool-image",
            None,
            &[("image_url", "https://example.test/b.png")],
        )
        .await;
    let mut anonymous = Browser::new(app(&db, oauth.clone()));
    let served = anonymous
        .get(&format!("/players/{}/pool.png", player_id.0))
        .await;
    assert_eq!(served.status(), StatusCode::NOT_FOUND, "PNG が残っている");
    let public = public_json(&db, &oauth, player_id).await;
    assert_eq!(
        public["pool_image_url"],
        json!("https://example.test/b.png")
    );

    // 空で送ると消える
    browser
        .post_multipart("/profile/pool-image", None, &[("image_url", "")])
        .await;
    let public = public_json(&db, &oauth, player_id).await;
    assert!(public.get("pool_image_url").is_none(), "{public}");
    println!("貼り直し・URL へ切替・消去: すべて 1 行のまま");

    db.done().await;
}

/// **担当ロールの公開設定とは独立。**ロールを隠していても画像は出る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_pool_image_is_independent_of_the_role_visibility(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = Arc::new(FakeOAuth::new("5000"));
    let (mut browser, player_id) = signed_in(&db, &oauth, "5005").await;

    // ロールを設定して**非公開**にする
    assert_eq!(
        save(&mut browser, "bio=&x_id=&main_roles=jungle").await,
        StatusCode::OK
    );
    browser
        .post_multipart(
            "/profile/pool-image",
            None,
            &[("image_url", "https://example.test/pool.png")],
        )
        .await;

    let public = public_json(&db, &oauth, player_id).await;
    assert!(public.get("main_roles").is_none(), "ロールが漏れている");
    assert_eq!(
        public["pool_image_url"],
        json!("https://example.test/pool.png"),
        "画像まで隠れている"
    );
    println!("ロール非公開 + 画像あり: ロールは隠れ、画像は出る");

    db.done().await;
}
