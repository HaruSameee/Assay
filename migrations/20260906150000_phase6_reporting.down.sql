alter table formation_events drop constraint formation_events_kind_check;
alter table formation_events add constraint formation_events_kind_check
    check (kind in (
        'vc_assigned',
        'host_assigned',
        'host_accepted',
        'host_declined',
        'host_timeout',
        'code_posted',
        'confirmed',
        'disputed',
        'voided',
        'abandoned'
    ));

alter table match_reports drop constraint match_reports_outcome_check;
alter table match_reports add constraint match_reports_outcome_check
    check (outcome in ('red', 'blue', 'draw'));

drop index match_reports_idempotency_key_uniq;
alter table match_reports drop column idempotency_key;
