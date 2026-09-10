create or replace view current_quiet_report_params as
select coalesce(latest.winner_threshold, 3) as winner_threshold,
       coalesce(latest.loser_threshold, 4)  as loser_threshold
from (values (1)) as anchor (x)
left join lateral (
    select c.winner_threshold, c.loser_threshold
    from quiet_report_param_changes c
    order by c.id desc
    limit 1
) latest on true;

alter table quiet_report_param_changes drop constraint quiet_report_param_changes_winner_threshold_check;
alter table quiet_report_param_changes drop constraint quiet_report_param_changes_loser_threshold_check;
alter table quiet_report_param_changes add constraint quiet_report_param_changes_winner_threshold_check
    check (winner_threshold between 1 and 4) not valid;
alter table quiet_report_param_changes add constraint quiet_report_param_changes_loser_threshold_check
    check (loser_threshold between 1 and 4) not valid;
