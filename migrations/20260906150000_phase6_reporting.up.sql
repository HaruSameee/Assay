-- Phase 6: Discord report idempotency and a distinct unanimous-invalid terminal state.
-- Existing rows receive unique legacy keys while the column is added. New rows must provide a key.

alter table match_reports
    add column idempotency_key text not null
        default ('legacy:' || gen_random_uuid()::text)
        check (btrim(idempotency_key) <> '');

alter table match_reports alter column idempotency_key drop default;

create unique index match_reports_idempotency_key_uniq
    on match_reports (idempotency_key);

comment on column match_reports.idempotency_key is
    'Discord interaction ID. Existing pre-Phase-6 rows use a generated legacy key; no default is retained';

alter table match_reports drop constraint match_reports_outcome_check;
alter table match_reports add constraint match_reports_outcome_check
    check (outcome in ('red', 'blue', 'draw', 'nullified'));

comment on column match_reports.outcome is
    '報告者が主張する結果。red / blue / draw / nullified。draw は成立した試合の引き分け、nullified は試合不成立への合意であり混同しない';

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
        'nullified',
        'voided',
        'abandoned'
    ));
