use sqlx::{PgConnection, PgPool};

use crate::admin::repository::append_action_on;
use crate::admin::{
    ActionOutcome, AdminActionKind, AdminStoreError, DEFAULT_BLOCK_ALLOWANCE, NewAdminAction,
};
use crate::lifecycle::{PlayerId, Timestamp};
use crate::persistence::repository::lock_vc_pool;

use super::model::{
    AdminBlockOverrideError, BlockKind, BlockSearchResult, CurrentBlock, IncomingBlock,
    ReceivedBlockCount, UserBlockError, cooldown_ends_at,
};

#[derive(Debug, Clone)]
pub struct BlockRepository {
    pool: PgPool,
}

impl BlockRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn current_for(&self, player_id: PlayerId) -> Result<Vec<CurrentBlock>, sqlx::Error> {
        let rows = sqlx::query!(
            "select latest.blocked_player_id, latest.created_at as \"blocked_at!\", \
                    snapshot.display_name as \"display_name?\", target.trainer_name \
             from ( \
                 select distinct on (blocked_player_id) blocked_player_id, kind, created_at \
                 from player_blocks \
                 where player_id = $1 \
                 order by blocked_player_id, id desc \
             ) latest \
             join players target on target.id = latest.blocked_player_id \
             left join lateral ( \
                 select display_name from player_discord_snapshots \
                 where player_id = latest.blocked_player_id \
                 order by id desc limit 1 \
             ) snapshot on true \
             where latest.kind = 'blocked' \
             order by latest.created_at desc, latest.blocked_player_id",
            player_id.0,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| CurrentBlock {
                blocked_player_id: row.blocked_player_id,
                display_name: row.display_name,
                trainer_name: row.trainer_name,
                blocked_at: row.blocked_at,
            })
            .collect())
    }

    pub async fn search_display_names(
        &self,
        player_id: PlayerId,
        query: &str,
    ) -> Result<Vec<BlockSearchResult>, sqlx::Error> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            "select snapshot.player_id, snapshot.display_name, target.trainer_name \
             from ( \
                 select distinct on (player_id) player_id, display_name \
                 from player_discord_snapshots \
                 order by player_id, id desc \
             ) snapshot \
             join players target on target.id = snapshot.player_id \
             where snapshot.player_id <> $1 \
               and position(lower($2) in lower(snapshot.display_name)) > 0 \
             order by snapshot.display_name, snapshot.player_id \
             limit 20",
            player_id.0,
            query,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| BlockSearchResult {
                player_id: row.player_id,
                display_name: row.display_name,
                trainer_name: row.trainer_name,
            })
            .collect())
    }

    pub async fn blocked_by(&self, player_id: PlayerId) -> Result<Vec<IncomingBlock>, sqlx::Error> {
        let rows = sqlx::query!(
            "select latest.player_id, latest.created_at as \"blocked_at!\", \
                    snapshot.display_name as \"display_name?\", source.trainer_name \
             from ( \
                 select distinct on (player_id) player_id, kind, created_at \
                 from player_blocks \
                 where blocked_player_id = $1 \
                 order by player_id, id desc \
             ) latest \
             join players source on source.id = latest.player_id \
             left join lateral ( \
                 select display_name from player_discord_snapshots \
                 where player_id = latest.player_id \
                 order by id desc limit 1 \
             ) snapshot on true \
             where latest.kind = 'blocked' \
             order by latest.created_at desc, latest.player_id",
            player_id.0,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| IncomingBlock {
                player_id: row.player_id,
                display_name: row.display_name,
                trainer_name: row.trainer_name,
                blocked_at: row.blocked_at,
            })
            .collect())
    }

    /// 現在有効な受信ブロックを、件数の多い順で集計する。
    ///
    /// 管理画面だけが使う。履歴全体ではなく、各ブロッカー・対象ペアの最新行から
    /// 導出するため、解除済みの行は数えない。
    pub async fn received_block_counts(&self) -> Result<Vec<ReceivedBlockCount>, sqlx::Error> {
        let rows = sqlx::query!(
            "select latest.blocked_player_id, \
                    snapshot.display_name as \"display_name?\", target.trainer_name, \
                    count(*)::bigint as \"count!\" \
             from ( \
                 select distinct on (player_id, blocked_player_id) \
                        player_id, blocked_player_id, kind \
                 from player_blocks \
                 order by player_id, blocked_player_id, id desc \
             ) latest \
             join players target on target.id = latest.blocked_player_id \
             left join lateral ( \
                 select display_name from player_discord_snapshots \
                 where player_id = latest.blocked_player_id \
                 order by id desc limit 1 \
             ) snapshot on true \
             where latest.kind = 'blocked' \
             group by latest.blocked_player_id, snapshot.display_name, target.trainer_name \
             order by count(*) desc, latest.blocked_player_id"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ReceivedBlockCount {
                player_id: row.blocked_player_id,
                display_name: row.display_name,
                trainer_name: row.trainer_name,
                count: usize::try_from(row.count).unwrap_or(usize::MAX),
            })
            .collect())
    }

    pub async fn block(
        &self,
        player_id: PlayerId,
        blocked_player_id: PlayerId,
        idempotency_key: &str,
        created_at: Timestamp,
    ) -> Result<(), UserBlockError> {
        if player_id == blocked_player_id {
            return Err(UserBlockError::SelfTarget);
        }
        let mut tx = self.pool.begin().await?;
        lock_vc_pool(&mut tx).await?;
        if event_exists_on(&mut tx, idempotency_key).await? {
            tx.commit().await?;
            return Ok(());
        }
        if current_block_on(&mut tx, player_id, blocked_player_id)
            .await?
            .is_some()
        {
            return Err(UserBlockError::AlreadyBlocked);
        }
        let allowance = block_allowance_on(&mut tx, player_id).await?;
        let active = active_count_on(&mut tx, player_id).await?;
        if active >= i64::from(allowance) {
            return Err(UserBlockError::AllowanceFull { allowance });
        }
        append_event_on(
            &mut tx,
            player_id,
            blocked_player_id,
            BlockKind::Blocked,
            idempotency_key,
            created_at,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn unblock(
        &self,
        player_id: PlayerId,
        blocked_player_id: PlayerId,
        idempotency_key: &str,
        created_at: Timestamp,
    ) -> Result<(), UserBlockError> {
        let mut tx = self.pool.begin().await?;
        lock_vc_pool(&mut tx).await?;
        if event_exists_on(&mut tx, idempotency_key).await? {
            tx.commit().await?;
            return Ok(());
        }
        let Some(blocked_at) = current_block_on(&mut tx, player_id, blocked_player_id).await?
        else {
            return Err(UserBlockError::NotBlocked);
        };
        let available_at = cooldown_ends_at(blocked_at);
        if created_at < available_at {
            return Err(UserBlockError::Cooldown { available_at });
        }
        append_event_on(
            &mut tx,
            player_id,
            blocked_player_id,
            BlockKind::Unblocked,
            idempotency_key,
            created_at,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn override_unblock(
        &self,
        actor_discord_user_id: i64,
        player_id: PlayerId,
        blocked_player_id: PlayerId,
        reason: Option<String>,
        idempotency_key: &str,
        occurred_at: Timestamp,
    ) -> Result<(), AdminBlockOverrideError> {
        let mut tx = self.pool.begin().await?;
        lock_vc_pool(&mut tx).await?;
        if admin_action_exists_on(&mut tx, idempotency_key).await? {
            tx.commit().await?;
            return Ok(());
        }
        let Some(_) = current_block_on(&mut tx, player_id, blocked_player_id).await? else {
            return Err(AdminBlockOverrideError::NotBlocked);
        };
        let action = NewAdminAction {
            kind: AdminActionKind::BlockOverrideRemoved,
            actor_discord_user_id,
            target_player_id: Some(player_id.0),
            reason,
            payload: serde_json::json!({ "blocked_player_id": blocked_player_id.0 }),
            occurred_at,
            idempotency_key: idempotency_key.to_owned(),
        };
        if append_action_on(&mut tx, &action).await? == ActionOutcome::Duplicate {
            tx.commit().await?;
            return Ok(());
        }
        append_event_on(
            &mut tx,
            player_id,
            blocked_player_id,
            BlockKind::Unblocked,
            &format!("admin-override:{idempotency_key}"),
            occurred_at,
        )
        .await
        .map_err(AdminStoreError::from)?;
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) async fn current_block_on(
    connection: &mut PgConnection,
    player_id: PlayerId,
    blocked_player_id: PlayerId,
) -> Result<Option<Timestamp>, sqlx::Error> {
    let row = sqlx::query!(
        "select kind, created_at from player_blocks \
         where player_id = $1 and blocked_player_id = $2 \
         order by id desc limit 1",
        player_id.0,
        blocked_player_id.0,
    )
    .fetch_optional(connection)
    .await?;
    Ok(row.and_then(|row| (row.kind == BlockKind::Blocked.as_str()).then_some(row.created_at)))
}

pub(crate) async fn append_event_on(
    connection: &mut PgConnection,
    player_id: PlayerId,
    blocked_player_id: PlayerId,
    kind: BlockKind,
    idempotency_key: &str,
    created_at: Timestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "insert into player_blocks \
         (player_id, blocked_player_id, kind, created_at, idempotency_key) \
         values ($1, $2, $3, $4, $5)",
        player_id.0,
        blocked_player_id.0,
        kind.as_str(),
        created_at,
        idempotency_key,
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn event_exists_on(
    connection: &mut PgConnection,
    idempotency_key: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "select exists(select 1 from player_blocks where idempotency_key = $1) as \"exists!\"",
        idempotency_key,
    )
    .fetch_one(connection)
    .await
}

async fn admin_action_exists_on(
    connection: &mut PgConnection,
    idempotency_key: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "select exists(select 1 from admin_actions where idempotency_key = $1) as \"exists!\"",
        idempotency_key,
    )
    .fetch_one(connection)
    .await
}

async fn active_count_on(
    connection: &mut PgConnection,
    player_id: PlayerId,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        "select count(*)::bigint as \"count!\" from ( \
             select distinct on (blocked_player_id) kind \
             from player_blocks where player_id = $1 \
             order by blocked_player_id, id desc \
         ) latest where kind = 'blocked'",
        player_id.0,
    )
    .fetch_one(connection)
    .await
}

async fn block_allowance_on(
    connection: &mut PgConnection,
    player_id: PlayerId,
) -> Result<i32, sqlx::Error> {
    let allowance = sqlx::query_scalar!(
        "select (payload ->> 'allowance')::integer as \"allowance!\" from admin_actions \
         where kind = 'block_allowance_set' and target_player_id = $1 \
         order by id desc limit 1",
        player_id.0,
    )
    .fetch_optional(connection)
    .await?;
    Ok(allowance.unwrap_or(DEFAULT_BLOCK_ALLOWANCE))
}
