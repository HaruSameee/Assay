alter table admin_actions
    drop constraint admin_actions_block_override_payload_check,
    drop constraint admin_actions_target_matches_kind,
    drop constraint admin_actions_kind_check,
    add constraint admin_actions_kind_check check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set',
        'season_started'
    )),
    add constraint admin_actions_target_matches_kind check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            else target_player_id is null
        end
    );

drop trigger if exists player_blocks_forbid_update_delete on player_blocks;
drop table if exists player_blocks;
