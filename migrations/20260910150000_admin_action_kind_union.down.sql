-- 直前の `admin_action_report_rejected` マイグレーションの状態へ戻す。
alter table admin_actions drop constraint admin_actions_target_matches_kind;
alter table admin_actions add constraint admin_actions_target_matches_kind
    check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
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
        'report_entered',
        'report_rejected'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / report_entered / report_rejected';
