-- IP だけの表に戻す。Discord ユーザー ID の行は写す先が無いので落ちる。
create table ip_ban_changes (
    id              bigserial   primary key,
    ip              text        not null check (btrim(ip) <> ''),
    kind            text        not null check (kind in ('banned', 'unbanned')),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);
create index ip_ban_changes_ip_id_desc_idx on ip_ban_changes (ip, id desc);
create trigger ip_ban_changes_forbid_update_delete
    before update or delete on ip_ban_changes
    for each statement execute function forbid_update_delete();

insert into ip_ban_changes (ip, kind, reason, created_by, idempotency_key, created_at)
select subject, kind, reason, created_by, 'shadow_ban_changes:' || idempotency_key, created_at
from shadow_ban_changes where subject_kind = 'ip' order by id;

create view current_ip_bans as
select latest.ip, latest.reason, latest.created_by, latest.created_at as banned_at
from (
    select distinct on (c.ip) c.ip, c.kind, c.reason, c.created_by, c.created_at
    from ip_ban_changes c
    order by c.ip, c.id desc
) latest
where latest.kind = 'banned';

drop view current_shadow_bans;
drop table shadow_ban_changes;

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
        'ip_unbanned'));
