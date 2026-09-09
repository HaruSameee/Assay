//! Player-owned ally blocks remain append-only under the application role.

use chrono::{TimeDelta, TimeZone, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::blocks::{BLOCK_COOLDOWN_DAYS, BlockRepository, UserBlockError};
use unite_matchsystem::lifecycle::PlayerId;
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
    blocks
        .block(player(1), player(3), "block-two", at(1))
        .await
        .unwrap();
    // The same idempotency key is the only no-op condition.
    blocks
        .block(player(1), player(2), "block-one", at(2))
        .await
        .unwrap();

    let current = blocks.current_for(player(1)).await.unwrap();
    assert_eq!(current.len(), 2);
    assert_eq!(current[0].blocked_player_id, player(3).0);

    let capacity = blocks
        .block(player(1), player(4), "block-three", at(3))
        .await
        .unwrap_err();
    assert!(matches!(
        capacity,
        UserBlockError::AllowanceFull { allowance: 2 }
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
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].blocked_player_id, player(3).0);

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
