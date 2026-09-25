-- 補正を外した増減に戻す（20260911200000_readjudication と同じ定義）。
create or replace view player_match_rating_deltas as
with team_mu as (
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
join formation_final_outcomes c
    on c.formation_id = fp.formation_id and c.kind = 'confirmed'
join team_mu opponent
    on opponent.formation_id = fp.formation_id
   and opponent.team <> fp.team
where f.created_at >= s.started_at;

drop view active_rating_delta_corrections;
drop table rating_delta_correction_revocations;
drop table rating_delta_corrections;

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
        'matchmaker_scheduled',
        'match_readjudicated',
        'block_cooldown_changed',
        'ip_banned',
        'ip_unbanned',
        'shadow_banned',
        'shadow_ban_lifted',
        'queue_priority_raised'));
