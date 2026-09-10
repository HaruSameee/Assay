create or replace view player_current_ratings as
select p.id as player_id,
       1500.0::double precision
           + coalesce(adjustments.delta_mu, 0)
           + coalesce(matches.delta, 0)
           + coalesce(quiet.delta, 0) as rating,
       500.0::double precision + coalesce(adjustments.delta_sigma, 0) as sigma,
       coalesce(matches.played, 0::bigint) as matches
from players p
left join lateral (
    select sum(a.delta_mu) as delta_mu, sum(a.delta_sigma) as delta_sigma
    from season_rating_adjustments a
    where a.player_id = p.id
) adjustments on true
left join (
    select d.player_id, sum(d.delta) as delta, count(*)::bigint as played
    from player_match_rating_deltas d
    group by d.player_id
) matches on matches.player_id = p.id
left join (
    select q.player_id, sum(q.delta) as delta
    from quiet_report_penalties q
    group by q.player_id
) quiet on quiet.player_id = p.id;

drop view nullified_report_penalties;
drop view current_nullified_report_params;
drop table nullified_report_param_changes;
drop table nullified_reports;
