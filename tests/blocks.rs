//! Player-owned ally blocks remain append-only under the application role.

use chrono::{TimeDelta, TimeZone, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::blocks::{BLOCK_COOLDOWN_DAYS, BlockRepository, UserBlockError};
use unite_matchsystem::lifecycle::PlayerId;
use unite_matchsystem::profiles::ProfileRepository;
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";

async fn app_pool(owner: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            (*owner.connect_options())
                .clone()
                .username(APP_ROLE)
                .password(APP_ROLE_PASSWORD),
        )
        .await
        .expect("connect as app role")
}

fn player(id: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(id))
}

fn at(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seconds)
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn user_blocks_are_append_only_capacity_limited_and_cooldown_limited(owner: PgPool) {
    let app = app_pool(&owner).await;
    let blocks = BlockRepository::new(app.clone());

    blocks
        .block(player(1), player(2), "block-one", at(0))
        .await
        .unwrap();
    // The same idempotency key is the only no-op condition.
    blocks
        .block(player(1), player(2), "block-one", at(2))
        .await
        .unwrap();

    let current = blocks.current_for(player(1)).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].blocked_player_id, player(2).0);

    let capacity = blocks
        .block(player(1), player(3), "block-two", at(3))
        .await
        .unwrap_err();
    assert!(matches!(
        capacity,
        UserBlockError::AllowanceFull { allowance: 1 }
    ));

    let cooldown = blocks
        .unblock(player(1), player(2), "unblock-early", at(4))
        .await
        .unwrap_err();
    assert!(matches!(cooldown, UserBlockError::Cooldown { .. }));

    let available_at = at(0) + TimeDelta::days(BLOCK_COOLDOWN_DAYS);
    blocks
        .unblock(player(1), player(2), "unblock-after-cooldown", available_at)
        .await
        .unwrap();
    let current = blocks.current_for(player(1)).await.unwrap();
    assert!(current.is_empty());

    for (pool, expected) in [(&app, "42501"), (&owner, "0A000")] {
        for statement in [
            "update player_blocks set id = id",
            "delete from player_blocks",
        ] {
            let error = sqlx::query(statement).execute(pool).await.unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some(expected),
                "{statement}"
            );
        }
    }
    app.close().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn received_block_counts_are_derived_from_current_events_and_sorted(owner: PgPool) {
    let app = app_pool(&owner).await;
    let blocks = BlockRepository::new(app.clone());

    blocks
        .block(player(1), player(5), "one-five", at(0))
        .await
        .unwrap();
    blocks
        .block(player(2), player(5), "two-five", at(1))
        .await
        .unwrap();
    blocks
        .block(player(3), player(6), "three-six", at(2))
        .await
        .unwrap();
    blocks
        .block(player(4), player(7), "four-seven", at(3))
        .await
        .unwrap();
    blocks
        .unblock(
            player(4),
            player(7),
            "four-seven-unblock",
            at(3) + TimeDelta::days(BLOCK_COOLDOWN_DAYS),
        )
        .await
        .unwrap();

    let counts = blocks.received_block_counts().await.unwrap();
    assert_eq!(counts.len(), 2, "解除済みの対象は集計しない");
    assert_eq!((counts[0].player_id, counts[0].count), (player(5).0, 2));
    assert_eq!((counts[1].player_id, counts[1].count), (player(6).0, 1));
    app.close().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_admin_can_override_the_cooldown_and_the_action_is_recorded(owner: PgPool) {
    let app = app_pool(&owner).await;
    let blocks = BlockRepository::new(app.clone());
    blocks
        .block(player(1), player(2), "block-one", at(0))
        .await
        .unwrap();

    blocks
        .override_unblock(
            9988,
            player(1),
            player(2),
            Some("mistaken block".to_owned()),
            "admin-override-one",
            at(1),
        )
        .await
        .unwrap();
    // Replaying precisely the same admin idempotency key is a no-op.
    blocks
        .override_unblock(
            9988,
            player(1),
            player(2),
            Some("mistaken block".to_owned()),
            "admin-override-one",
            at(2),
        )
        .await
        .unwrap();

    assert!(blocks.current_for(player(1)).await.unwrap().is_empty());
    let row: (String, i64, Uuid, String) = sqlx::query_as(
        "select kind, actor_discord_user_id, target_player_id, payload->>'blocked_player_id' \
         from admin_actions where idempotency_key = 'admin-override-one'",
    )
    .fetch_one(&app)
    .await
    .unwrap();
    assert_eq!(row.0, "block_override_removed");
    assert_eq!(row.1, 9988);
    assert_eq!(row.2, player(1).0);
    assert_eq!(row.3, player(2).0.to_string());

    let action_count: i64 = sqlx::query_scalar(
        "select count(*) from admin_actions where idempotency_key = 'admin-override-one'",
    )
    .fetch_one(&app)
    .await
    .unwrap();
    assert_eq!(action_count, 1);
    app.close().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_admin_override_requires_a_uuid_target_in_its_payload(owner: PgPool) {
    let app = app_pool(&owner).await;
    let error = sqlx::query!(
        "insert into admin_actions \
         (kind, actor_discord_user_id, target_player_id, reason, payload, occurred_at, idempotency_key) \
         values ('block_override_removed', $1, $2, null, $3, $4, $5)",
        9988_i64,
        player(1).0,
        serde_json::json!({ "blocked_player_id": "not-a-uuid" }),
        at(0),
        "invalid-block-override-payload",
    )
    .execute(&app)
    .await
    .unwrap_err();
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    app.close().await;
}

/// **ブロック検索は 3 つの入口で引ける。**トレーナー名・Discord 表示名・ユーザー ID。
///
/// 表示名だけで引いていた頃は、本番の 53 人中 44 人で display_name と trainer_name が
/// 食い違っており、画面に出ているトレーナー名を打っても 0 件だった。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_search_finds_people_by_trainer_name_display_name_and_user_id(owner: PgPool) {
    let app = app_pool(&owner).await;
    let blocks = BlockRepository::new(app.clone());
    let profiles = ProfileRepository::new(app.clone());

    // player 2: トレーナー名と Discord 表示名が食い違う（本番で 8 割がこの形）
    set_trainer_name(&app, player(2), "ハチワ冷笑").await;
    profiles
        .record_appearance(player(2), "ばちお", None, at(0))
        .await
        .unwrap();
    // player 3: スナップショットがまだ無い（バックフィルで取れなかった人）
    set_trainer_name(&app, player(3), "地下貯蔵庫").await;
    // player 4: 表示名だけある（トレーナー名は未設定）
    profiles
        .record_appearance(player(4), "L0tus__", None, at(0))
        .await
        .unwrap();

    let ids = |found: &[unite_matchsystem::blocks::BlockSearchResult]| {
        found.iter().map(|row| row.player_id).collect::<Vec<_>>()
    };

    // 1. **トレーナー名で引ける。**以前はここが 0 件だった
    let found = blocks
        .search_display_names(player(1), "ハチワ")
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![player(2).0], "トレーナー名で引けない");
    assert_eq!(found[0].trainer_name.as_deref(), Some("ハチワ冷笑"));
    assert_eq!(found[0].display_name.as_deref(), Some("ばちお"));
    println!(
        "トレーナー名「ハチワ」→ {:?} / {:?}",
        found[0].trainer_name, found[0].display_name
    );

    // 2. **Discord 表示名でも引ける**（従来どおり）
    let found = blocks
        .search_display_names(player(1), "ばちお")
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![player(2).0], "表示名で引けない");

    // 3. **Discord ユーザー ID で引ける。**reference の player 2 は 1002
    let found = blocks
        .search_display_names(player(1), "1002")
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![player(2).0], "ユーザー ID で引けない");
    println!("ユーザーID「1002」→ {:?}", found[0].trainer_name);

    // 部分一致では引かない。桁の一部が合った無関係な人を出さない（誤ブロックは 5 日間解けない）
    let found = blocks.search_display_names(player(1), "100").await.unwrap();
    assert!(ids(&found).is_empty(), "ID の部分一致で出ている: {found:?}");

    // 4. **スナップショットが無い人も、トレーナー名で見つかる**
    let found = blocks
        .search_display_names(player(1), "地下")
        .await
        .unwrap();
    assert_eq!(
        ids(&found),
        vec![player(3).0],
        "スナップショット無しが出ない"
    );
    assert_eq!(found[0].display_name, None, "表示名は未取得のまま返す");

    // 5. トレーナー名が未設定でも、表示名で見つかる
    let found = blocks
        .search_display_names(player(1), "L0tus")
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![player(4).0]);
    assert_eq!(found[0].trainer_name, None);

    // 自分自身は出ない
    set_trainer_name(&app, player(1), "じぶん").await;
    let found = blocks
        .search_display_names(player(1), "じぶん")
        .await
        .unwrap();
    assert!(ids(&found).is_empty(), "自分が候補に出ている");

    app.close().await;
}

/// テスト用にトレーナー名を入れる（`players` は可変の参照データ）。
async fn set_trainer_name(pool: &PgPool, who: PlayerId, name: &str) {
    sqlx::query("update players set trainer_name = $2, registered_at = now() where id = $1")
        .bind(who.0)
        .bind(name)
        .execute(pool)
        .await
        .expect("set the trainer name");
}
