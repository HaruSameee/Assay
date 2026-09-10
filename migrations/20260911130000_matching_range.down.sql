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
        'rating_params_rejected'));

alter table queue_entries drop constraint queue_entries_down_range_check;
alter table queue_entries add constraint queue_entries_down_range_check
    check (down_range = 50) not valid;

drop view current_matching_ranges;
drop table matching_range_changes;
