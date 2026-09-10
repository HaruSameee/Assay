-- マッチメイカの稼働スケジュール。**追記専用で、現在値は最新行から導出する。**
--
-- 「水金土日 21〜2 時」のような週次の稼働時間帯を運営が設定できるようにする。
-- これまでは手で停止・再開するしかなく、毎日の開始と終了を人がやっていた。
--
-- **手動の停止が常に勝つ。**障害やトラブルで緊急停止したのに、21 時になったら
-- スケジュールが勝手に再開する、という事故を起こさないため。効き方は:
--
--   実効的に止まっている = 手動で停止されている
--                        または（スケジュールが有効 かつ いま稼働時間外）
--
-- 手動の再開は「手動停止の解除」であって「スケジュール無視」ではない。
-- 稼働時間外に手動再開しても、スケジュールが止めたままにする（画面でそう書く）。
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する
--   * **状態列を作らない。**現在のスケジュールは最新行（id 最大）から導出する
--   * 停止・再開の履歴（admin_actions）とは別物。あちらは「人が押した操作」で、
--     こちらは「その操作の外側にある枠」である

create table matchmaker_schedule_changes (
    id              bigserial   primary key,
    -- スケジュールを使うか。false なら手動の停止・再開だけが効く
    enabled         boolean     not null,
    -- 稼働する曜日のビットマスク。**bit 0 = 月曜、bit 6 = 日曜**（ISO と同じ並び）。
    -- 「水金土日」なら 0b1101100 = 108
    days_mask       integer     not null check (days_mask between 0 and 127),
    -- 稼働開始（JST の 0 時からの分）。21:00 なら 1260
    start_minute    integer     not null check (start_minute between 0 and 1439),
    -- 稼働終了（JST の 0 時からの分）。2:00 なら 120。
    -- **start より小さければ日をまたぐ**（21:00 開始 → 翌 2:00 終了）。
    -- start と同じ値は「24 時間」ではなく「0 分」なので CHECK で禁じる
    end_minute      integer     not null check (end_minute between 0 and 1439),
    check (start_minute <> end_minute),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table matchmaker_schedule_changes is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。マッチメイカの週次稼働スケジュールの変更ログ。現在値は最新行（id 最大）から導出する（current_matchmaker_schedule）。手動の停止が常に勝つ';
comment on column matchmaker_schedule_changes.days_mask is
    '稼働する曜日のビットマスク。bit 0 = 月曜 … bit 6 = 日曜（ISO と同じ並び）。開始時刻の属する曜日で判定する（日をまたぐ枠は翌日側の曜日を見ない）';
comment on column matchmaker_schedule_changes.start_minute is
    '稼働開始。**JST** の 0 時からの分（21:00 = 1260）';
comment on column matchmaker_schedule_changes.end_minute is
    '稼働終了。**JST** の 0 時からの分（2:00 = 120）。start より小さければ日をまたぐ';

create index matchmaker_schedule_changes_id_desc_idx
    on matchmaker_schedule_changes (id desc);

create trigger matchmaker_schedule_changes_forbid_update_delete
    before update or delete on matchmaker_schedule_changes
    for each statement execute function forbid_update_delete();

-- current_season / current_rating_params と同じ形。**最新行（id 最大）**を採る。
-- 1 行も無ければ「スケジュールなし」＝これまでどおり手動だけで動く。
create view current_matchmaker_schedule as
select coalesce(latest.enabled, false)     as enabled,
       coalesce(latest.days_mask, 0)       as days_mask,
       coalesce(latest.start_minute, 0)    as start_minute,
       coalesce(latest.end_minute, 1)      as end_minute
from (values (1)) as anchor (x)
left join lateral (
    select c.enabled, c.days_mask, c.start_minute, c.end_minute
    from matchmaker_schedule_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_matchmaker_schedule is
    '現在のマッチメイカ稼働スケジュール。matchmaker_schedule_changes の最新行から導出する。1 行も無ければ enabled=false（手動だけで動く、これまでの挙動）。マッチメイカは 1 スキャンにつき 1 回だけこれを読む';

-- ----------------------------------------------------------------------
-- 運営操作ログに matchmaker_scheduled を足す。
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
        'matchmaker_scheduled'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved / matchmaker_scheduled';
