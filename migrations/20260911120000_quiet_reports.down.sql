-- 合算からペナルティの入口を外す。**積んだ報告は消さない**（追記専用のため）。
create or replace view player_current_ratings as
select
    p.id as player_id,
    1500.0::double precision
        + coalesce(adjustments.delta_mu, 0::double precision)
        + coalesce(matches.delta, 0::double precision) as rating,
    500.0::double precision
        + coalesce(adjustments.delta_sigma, 0::double precision) as sigma,
    coalesce(matches.played, 0::bigint) as matches
from players p
left join lateral (
    select sum(a.delta_mu) as delta_mu, sum(a.delta_sigma) as delta_sigma
    from season_rating_adjustments a where a.player_id = p.id
) adjustments on true
left join (
    select d.player_id, sum(d.delta) as delta, count(*)::bigint as played
    from player_match_rating_deltas d group by d.player_id
) matches on matches.player_id = p.id;

drop view if exists quiet_report_penalties;
drop view if exists current_quiet_report_params;
