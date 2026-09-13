-- IP による「実質 BAN」（シャドウバン）。**追記専用。**
--
-- 迷惑行為の人を表立って BAN すると荒れるので、断らずに**組まれないようにする**。
-- 運営が並べた IP からキューに入った行には shadow_banned を立て、マッチメイカは
-- その行を候補から外す。本人の画面はいつもどおり「キュー中」のままで、
-- 何も告げない。他の参加者からも見えない（範囲内の人数に数えない）。
--
-- IP は **キュー投入のたびに** ヘッダ（CF-Connecting-IP、無ければ X-Forwarded-For の先頭）
-- を BAN 一覧と突き合わせ、**結果の真偽だけ**を行に書く。全員の IP は記録しない
-- （記録するのは IP_WATCH_DISCORD_USER_IDS の対象だけ。player_address_records）。
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する。
--   * **状態列を作らない。**いま BAN 中かは IP ごとの最新行（id 最大）が banned か
--     で導出する（current_ip_bans）。解除は unbanned を 1 行積む。
--   * queue_entries.shadow_banned は投入時点の判定の**写し**。BAN 一覧を変えても
--     既にある行は書き換えない（次の投入で判定し直す）。

create table ip_ban_changes (
    id              bigserial   primary key,
    -- 接続元の IP（文字列のまま。IPv4 / IPv6 どちらも）
    ip              text        not null check (btrim(ip) <> ''),
    kind            text        not null check (kind in ('banned', 'unbanned')),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table ip_ban_changes is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。IP のシャドウバンの変更ログ。いま BAN 中かは IP ごとの最新行が banned かで導出する（current_ip_bans）';
comment on column ip_ban_changes.ip is
    '接続元の IP。キュー投入時のヘッダ（CF-Connecting-IP、無ければ X-Forwarded-For の先頭）と完全一致で突き合わせる';

create index ip_ban_changes_ip_id_desc_idx on ip_ban_changes (ip, id desc);

create trigger ip_ban_changes_forbid_update_delete
    before update or delete on ip_ban_changes
    for each statement execute function forbid_update_delete();

-- いま BAN 中の IP。IP ごとの最新行が banned のものだけ。
create view current_ip_bans as
select latest.ip, latest.reason, latest.created_by, latest.created_at as banned_at
from (
    select distinct on (c.ip) c.ip, c.kind, c.reason, c.created_by, c.created_at
    from ip_ban_changes c
    order by c.ip, c.id desc
) latest
where latest.kind = 'banned';

comment on view current_ip_bans is
    'いま BAN 中の IP。ip_ban_changes の IP ごとの最新行が banned のもの。キュー投入時にここと突き合わせる';

-- 投入時点で BAN 中の IP から来ていたか。マッチメイカはこの行を候補から外す。
alter table queue_entries
    add column shadow_banned boolean not null default false;

comment on column queue_entries.shadow_banned is
    '投入時点で接続元が current_ip_bans にあったか。真ならマッチメイカは候補から外し、他の参加者の「範囲内 N 人」にも数えない。本人には何も告げない。BAN 一覧を変えても既存行は書き換えない（次の投入で判定し直す）';

-- ----------------------------------------------------------------------
-- 運営操作ログに ip_banned / ip_unbanned を足す。
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
        'ip_unbanned'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved / matchmaker_scheduled / match_readjudicated / block_cooldown_changed / ip_banned / ip_unbanned';
