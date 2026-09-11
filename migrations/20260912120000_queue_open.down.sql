-- キュー受付開始の列を外し、ビューを稼働枠だけに戻す。
-- 戻した後はキューにいつでも入れる（以前の挙動）。

drop view current_matchmaker_schedule;

create view current_matchmaker_schedule as
select coalesce(latest.enabled, true)      as enabled,
       coalesce(latest.days_mask, 116)     as days_mask,
       coalesce(latest.start_minute, 1260) as start_minute,
       coalesce(latest.end_minute, 120)    as end_minute
from (values (1)) as anchor (x)
left join lateral (
    select c.enabled, c.days_mask, c.start_minute, c.end_minute
    from matchmaker_schedule_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_matchmaker_schedule is
    '現在のマッチメイカ稼働スケジュール。matchmaker_schedule_changes の最新行から導出する。1 行も無ければ既定の水金土日 21:00〜翌 2:00（JST）。マッチメイカは 1 スキャンにつき 1 回だけこれを読む。手動の停止は常にこれより優先される';

alter table matchmaker_schedule_changes
    drop constraint matchmaker_schedule_changes_queue_open_before_start,
    drop column queue_open_minute;
