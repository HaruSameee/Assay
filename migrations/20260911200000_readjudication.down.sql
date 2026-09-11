-- 「確定した試合」の定義を「confirmed イベントがある」に戻す。
-- **積んだ再裁定のイベントは消さない**（追記専用のため）。戻した後は、
-- 再裁定で nullified にした試合のレートが再び数えられることに注意。

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
        when c.outcome = fp.team then p.base_delta
        else -p.base_delta
    end::double precision as delta
from formation_players fp
join formations f on f.id = fp.formation_id
cross join current_season s
cross join current_rating_params p
join confirmed c on c.formation_id = fp.formation_id
join team_mu opponent
    on opponent.formation_id = fp.formation_id
   and opponent.team <> fp.team
where f.created_at >= s.started_at;

create or replace view quiet_report_penalties as
with confirmed as (
    select distinct on (e.formation_id)
        e.formation_id,
        e.payload->>'outcome' as outcome
    from formation_events e
    where e.kind = 'confirmed'
    order by e.formation_id, e.id desc
),
tally as (
    select q.formation_id, q.target_id, count(*)::bigint as votes
    from quiet_reports q
    group by q.formation_id, q.target_id
)
select
    t.formation_id,
    t.target_id                  as player_id,
    t.votes,
    (-1.0)::double precision     as delta
from tally t
join confirmed c on c.formation_id = t.formation_id
join formation_players fp
    on fp.formation_id = t.formation_id and fp.player_id = t.target_id
join formations f on f.id = t.formation_id
cross join current_season s
cross join current_quiet_report_params p
where f.created_at >= s.started_at
  and c.outcome in ('red', 'blue')
  and t.votes >= case
        when c.outcome = fp.team then p.winner_threshold
        else p.loser_threshold
      end;

create or replace view nullified_report_penalties as
with nullified as (
    select distinct e.formation_id
    from formation_events e
    where e.kind = 'nullified'
),
tally as (
    select r.formation_id, r.target_id, count(*)::bigint as votes
    from nullified_reports r
    where r.target_id is not null
    group by r.formation_id, r.target_id
)
select
    t.formation_id,
    t.target_id as player_id,
    t.votes,
    (-p.penalty)::double precision as delta
from tally t
join nullified n on n.formation_id = t.formation_id
join formation_players fp
    on fp.formation_id = t.formation_id and fp.player_id = t.target_id
join formations f on f.id = t.formation_id
cross join current_season s
cross join current_nullified_report_params p
where f.created_at >= s.started_at
  and t.votes >= p.threshold;

drop view formation_final_outcomes;

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
