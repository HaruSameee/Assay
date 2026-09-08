-- params v2（基本 7 / 格上 +8・-6 / 格下 +6・-8）へ戻す。
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
        when c.outcome = fp.team then
            case
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 >=  50.0 - 1e-9 then  8.0
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 <= -50.0 + 1e-9 then  6.0
                else 7.0
            end
        else
            case
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 >=  50.0 - 1e-9 then -6.0
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 <= -50.0 + 1e-9 then -8.0
                else -7.0
            end
    end::double precision as delta
from formation_players fp
join confirmed c on c.formation_id = fp.formation_id
join team_mu opponent
    on opponent.formation_id = fp.formation_id
   and opponent.team <> fp.team;

comment on view player_match_rating_deltas is
    '確定した試合 1 件ぶんの増減（07 §2-2、params v2: 基本 7 / 格上 +8・-6 / 格下 +6・-8）。導出値であり実体を持たない。';
