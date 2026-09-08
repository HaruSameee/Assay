alter table admin_actions drop constraint admin_actions_kind_check;
alter table admin_actions add constraint admin_actions_kind_check
    check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set';
