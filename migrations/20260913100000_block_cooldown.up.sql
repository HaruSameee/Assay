-- ブロック解除の待機日数（クールタイム）を運営が変えられるようにする。**追記専用。**
--
-- 05 §ブロックは「ブロックしてから 5 日間は解除できない」で、
-- `blocks::model::BLOCK_COOLDOWN_DAYS` が定数だった。当日の校正で日数を動かせるようにする。
-- 定数はアプリの**既定値**として残す（1 行も無ければ 5 日で、これが現在の本番の状態）。
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する。
--   * **状態列を作らない。**現在の日数は最新行（id 最大）から導出する。
--   * `player_blocks` は書き換えない。**既にあるブロックにも新しい日数が効く**
--     （解除可能時刻は blocked_at + 現在の日数で毎回導出する）。
--   * 0 日は「すぐ解除できる」。上限 365 日。

create table block_cooldown_changes (
    id              bigserial   primary key,
    -- ブロックしてから解除できるようになるまでの日数。0 なら待たない
    cooldown_days   integer     not null check (cooldown_days between 0 and 365),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table block_cooldown_changes is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。ブロック解除の待機日数の変更ログ。現在値は最新行（id 最大）から導出する（current_block_cooldown）。既にあるブロックにも効く（解除可能時刻は毎回導出）';
comment on column block_cooldown_changes.cooldown_days is
    'ブロックしてから解除できるようになるまでの日数。0〜365。0 なら待たない';
comment on column block_cooldown_changes.created_by is
    '操作した運営。discord:<id> の形';

create index block_cooldown_changes_id_desc_idx on block_cooldown_changes (id desc);

create trigger block_cooldown_changes_forbid_update_delete
    before update or delete on block_cooldown_changes
    for each statement execute function forbid_update_delete();

-- 日数を見る**唯一の共通の入口**。1 行も無ければ既定の 5 日。
create view current_block_cooldown as
select coalesce(latest.cooldown_days, 5) as cooldown_days
from (values (1)) as anchor (x)
left join lateral (
    select c.cooldown_days
    from block_cooldown_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_block_cooldown is
    '現在のブロック解除の待機日数。block_cooldown_changes の最新行から導出する。1 行も無ければ既定値 5 日';

-- ----------------------------------------------------------------------
-- 運営操作ログに block_cooldown_changed を足す。
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
        'block_cooldown_changed'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved / matchmaker_scheduled / match_readjudicated / block_cooldown_changed';
