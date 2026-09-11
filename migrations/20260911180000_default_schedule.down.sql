create or replace view current_matchmaker_schedule as
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
