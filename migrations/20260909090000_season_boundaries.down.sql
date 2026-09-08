-- 境界の適用を外して、全履歴を対象に戻す。
create or replace view player_match_rating_deltas as
with confirmed as (
    select distinct on (e.formation_id)
        e.formation_id,
        e.payload->>'outcome' as outcome
    from formation_events e
    where e.kind = 'confirmed'
    order by e.formation_id, e.id desc
),
team_mu as (
    select fp.formation_id, fp.team, avg(fp.mu_at_match) as mean_mu
    from formation_players fp
    group by fp.formation_id, fp.team
)
select
    fp.player_id,
    fp.formation_id,
    fp.team,
    c.outcome,
    fp.mu_at_match * 60.0                as own_rating,
    opponent.mean_mu * 60.0              as opponent_mean_rating,
    case
        when c.outcome = 'draw' then 0.0
        when c.outcome = fp.team then 7.0
        else -7.0
    end::double precision as delta
from formation_players fp
join confirmed c on c.formation_id = fp.formation_id
join team_mu opponent
    on opponent.formation_id = fp.formation_id
   and opponent.team <> fp.team;

drop view season_rating_adjustments;
drop view current_season;
drop trigger season_boundaries_forbid_update_delete on season_boundaries;
drop table season_boundaries;
