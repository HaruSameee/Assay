alter table admin_actions drop constraint admin_actions_target_matches_kind;
alter table admin_actions add constraint admin_actions_target_matches_kind
    check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            when 'block_override_removed' then target_player_id is not null
            when 'report_entered' then target_player_id is not null
            when 'report_rejected' then target_player_id is not null
            else target_player_id is null
        end
    );

alter table admin_actions drop constraint admin_actions_kind_check;
alter table admin_actions add constraint admin_actions_kind_check
    check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set',
        'season_started',
        'block_override_removed',
        'report_entered',
        'report_rejected',
        'rating_params_changed',
        'rating_params_rejected',
        'matching_range_changed',
        'dispute_resolved',
        'matchmaker_scheduled',
        'match_readjudicated',
        'block_cooldown_changed',
        'ip_banned',
        'ip_unbanned',
        'shadow_banned',
        'shadow_ban_lifted'));

drop table queue_priority_grants;
