-- 増減幅を定数へ戻す。**積んだ変更ログは消さない**——追記専用の表なので、
-- ロールバックしても行は残す（消せば「無かったこと」になり、不変条件 1 に反する）。

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
join formations f on f.id = fp.formation_id
cross join current_season s
join confirmed c on c.formation_id = fp.formation_id
join team_mu opponent
    on opponent.formation_id = fp.formation_id
   and opponent.team <> fp.team
where f.created_at >= s.started_at;

drop view if exists current_rating_params;

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
        'report_rejected'));
