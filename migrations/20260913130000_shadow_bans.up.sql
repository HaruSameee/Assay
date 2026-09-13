-- シャドウバンの対象を IP だけでなく Discord ユーザー ID でも指定できるようにする。
--
-- ip_ban_changes は「IP 1 種類」の表だった。対象の種類（subject_kind）を持つ
-- shadow_ban_changes に置き換え、既存の行はそのまま写す（追記専用の性質は変えない。
-- 写した後で旧表を落とすのは、同じ意味の表を 2 つ残さないため）。
--
-- 判定はキュー投入時に 1 回: 接続元の IP が BAN 中、**または**本人の Discord ユーザー ID が
-- BAN 中なら queue_entries.shadow_banned を立てる。効き方は IP のときと同じ
-- （断らず、組まれず、本人には何も告げない。効くのは次の投入から）。
--
-- 不変条件は ip_ban_changes と同じ: 追記専用、状態列を持たない、
-- 現在値は (subject_kind, subject) ごとの最新行から導出する。

create table shadow_ban_changes (
    id              bigserial   primary key,
    -- 'ip' か 'discord_user'
    subject_kind    text        not null check (subject_kind in ('ip', 'discord_user')),
    -- IP の文字列、または Discord ユーザー ID の 10 進文字列
    subject         text        not null check (btrim(subject) <> ''),
    kind            text        not null check (kind in ('banned', 'unbanned')),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table shadow_ban_changes is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。シャドウバンの変更ログ。対象は IP か Discord ユーザー ID。いま BAN 中かは対象ごとの最新行が banned かで導出する（current_shadow_bans）';
comment on column shadow_ban_changes.subject is
    'subject_kind が ip なら接続元の IP（CF-Connecting-IP、無ければ X-Forwarded-For の先頭と完全一致）、discord_user なら Discord ユーザー ID の 10 進文字列';

create index shadow_ban_changes_subject_id_desc_idx
    on shadow_ban_changes (subject_kind, subject, id desc);

create trigger shadow_ban_changes_forbid_update_delete
    before update or delete on shadow_ban_changes
    for each statement execute function forbid_update_delete();

-- 既存の IP の行を写す（id の順を保つ）。冪等キーは衝突しないよう接頭辞を付ける
insert into shadow_ban_changes
    (subject_kind, subject, kind, reason, created_by, idempotency_key, created_at)
select 'ip', ip, kind, reason, created_by, 'ip_ban_changes:' || idempotency_key, created_at
from ip_ban_changes
order by id;

drop view current_ip_bans;
drop table ip_ban_changes;

-- いま BAN 中の対象。対象ごとの最新行が banned のものだけ。
create view current_shadow_bans as
select latest.subject_kind, latest.subject, latest.reason, latest.created_by,
       latest.created_at as banned_at
from (
    select distinct on (c.subject_kind, c.subject)
        c.subject_kind, c.subject, c.kind, c.reason, c.created_by, c.created_at
    from shadow_ban_changes c
    order by c.subject_kind, c.subject, c.id desc
) latest
where latest.kind = 'banned';

comment on view current_shadow_bans is
    'いま BAN 中の対象（IP / Discord ユーザー ID）。shadow_ban_changes の対象ごとの最新行が banned のもの。キュー投入時に接続元の IP と本人の Discord ユーザー ID の両方をここと突き合わせる';

comment on column queue_entries.shadow_banned is
    '投入時点で接続元の IP か本人の Discord ユーザー ID が current_shadow_bans にあったか。真ならマッチメイカは候補から外し、他の参加者の「同じ試合に入れる人」にも数えない。本人には何も告げない。BAN 一覧を変えても既存行は書き換えない（次の投入で判定し直す）';

-- ----------------------------------------------------------------------
-- 運営操作ログに shadow_banned / shadow_ban_lifted を足す（ip_banned / ip_unbanned は
-- 過去の行のために残す）。
-- ----------------------------------------------------------------------
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

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved / matchmaker_scheduled / match_readjudicated / block_cooldown_changed / ip_banned / ip_unbanned / shadow_banned / shadow_ban_lifted';
