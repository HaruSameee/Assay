-- player_blocks is an append-only ally-block log. The current relation is
-- derived from the latest row per player_id / blocked_player_id pair.
create table player_blocks (
    id bigserial primary key,
    player_id uuid not null references players(id),
    blocked_player_id uuid not null references players(id),
    kind text not null check (kind in ('blocked', 'unblocked')),
    created_at timestamptz not null,
    idempotency_key text not null unique check (btrim(idempotency_key) <> ''),
    check (player_id <> blocked_player_id)
);

comment on table player_blocks is
    'Append-only ally-block log. Current relations derive from the latest row per player pair.';
comment on column player_blocks.kind is
    'blocked prevents the pair being teammates; unblocked cancels the latest blocked relation.';

create index player_blocks_player_pair_latest_idx
    on player_blocks (player_id, blocked_player_id, id desc);
create index player_blocks_blocked_player_pair_latest_idx
    on player_blocks (blocked_player_id, player_id, id desc);

create trigger player_blocks_forbid_update_delete
    before update or delete on player_blocks
    for each statement execute function forbid_update_delete();

-- Admin overrides remain append-only. target_player_id is the player whose
-- outgoing block was removed; payload.blocked_player_id identifies the peer.
alter table admin_actions
    drop constraint admin_actions_kind_check,
    add constraint admin_actions_kind_check check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set',
        'season_started',
        'block_override_removed'
    ));

alter table admin_actions
    drop constraint admin_actions_target_matches_kind,
    add constraint admin_actions_target_matches_kind check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            when 'block_override_removed' then target_player_id is not null
            else target_player_id is null
        end
    );

alter table admin_actions
    add constraint admin_actions_block_override_payload_check check (
        kind <> 'block_override_removed'
        or (
            jsonb_typeof(payload -> 'blocked_player_id') = 'string'
            and (payload ->> 'blocked_player_id') ~
                '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
        )
    );
