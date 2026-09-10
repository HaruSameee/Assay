-- マッチング範囲（上下の幅）を運営が変えられるようにする。**追記専用。**
--
-- 07 §3 は上下 50 固定で、`queue::model::UP_RANGE` / `DOWN_RANGE` が定数だった。
-- 当日の校正で幅を動かせるようにする。定数はアプリの**既定値**として残す
-- （1 行も無ければ 50/50 で、これが現在の本番の状態）。
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する。
--   * **状態列を作らない。**現在の幅は最新行（id 最大）から導出する。
--   * 過去に積んだ queue_entries.down_range は**書き換えない**。読み出し側が
--     現在の幅に丸める（`queue::model::clamp_down_range`）。これは
--     20260907190100 / 20260908090100 と同じ扱いで、追記でない書き込みを増やさない。
--   * この表は**試合の履歴に影響しない**。効くのは次のスキャン以降の選抜だけで、
--     確定済みの試合も進行中の formation も再計算されない
--     （rating_param_changes と違い、recompute の対象ではない）。

create table matching_range_changes (
    id              bigserial   primary key,
    -- 上方向の幅（表示スケール）。アンカーより上を何点まで同じ試合に入れるか
    up_range        integer     not null check (up_range between 10 and 500),
    -- 下方向の幅（表示スケール）。上と別々に置けるが、UI は同時に書く
    down_range      integer     not null check (down_range between 10 and 500),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table matching_range_changes is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。マッチング範囲の変更ログ。現在値は最新行（id 最大）から導出する（current_matching_ranges）。効くのは次のスキャン以降の選抜だけで、確定済みの試合は再計算されない';
comment on column matching_range_changes.up_range is
    '上方向のマッチング範囲（07 §3）。ウィンドウの下端からこの幅までを同じ試合に入れる。10〜500';
comment on column matching_range_changes.down_range is
    '下方向のマッチング範囲（07 §3）。queue_entries.down_range に書く値でもあり、読み出し時の丸め先でもある。10〜500';
comment on column matching_range_changes.created_by is
    '操作した運営。discord:<id> の形';

create index matching_range_changes_id_desc_idx on matching_range_changes (id desc);

create trigger matching_range_changes_forbid_update_delete
    before update or delete on matching_range_changes
    for each statement execute function forbid_update_delete();

-- ----------------------------------------------------------------------
-- current_matching_ranges: 幅を見る**唯一の共通の入口**。
-- ----------------------------------------------------------------------
--
-- current_season / current_rating_params と同じ形。**最新行（id 最大）**を採る。
-- 誤って極端な値を入れたときは、元の値を**追記して**戻す。
-- 1 行も無ければアプリの既定値（50/50）で、これが現在の本番の状態である。
create view current_matching_ranges as
select coalesce(latest.up_range, 50)   as up_range,
       coalesce(latest.down_range, 50) as down_range
from (values (1)) as anchor (x)
left join lateral (
    select c.up_range, c.down_range
    from matching_range_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_matching_ranges is
    '現在のマッチング範囲。matching_range_changes の最新行から導出する。1 行も無ければ既定値 50/50。マッチメイカは 1 スキャンにつき 1 回だけこれを読む（同じトランザクションなので途中で変わらない）';

-- ----------------------------------------------------------------------
-- queue_entries.down_range の CHECK を広げる。
-- ----------------------------------------------------------------------
--
-- 20260908090100 が `= 50` に絞ったが、幅が可変になったので通らなくなる。
-- 表と同じ 10〜500 にする。**旧仕様（95〜200 / 30〜100）で入った行が残る**ので
-- not valid のままにする（既存行を一括 UPDATE しない）。
alter table queue_entries drop constraint queue_entries_down_range_check;
alter table queue_entries add constraint queue_entries_down_range_check
    check (down_range between 10 and 500) not valid;

comment on column queue_entries.down_range is
    '下方向のマッチング範囲（07 §3）。投入時点の current_matching_ranges.down_range を書く。運営が幅を変えても既存行は書き換えず、読み出し側が queue::model::clamp_down_range で現在の幅に丸める。旧仕様（95〜200 / 30〜100 / 50 固定）の行が残りうるので CHECK は not valid';

-- ----------------------------------------------------------------------
-- 運営操作ログに matching_range_changed を足す。
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
        'matching_range_changed'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed';
