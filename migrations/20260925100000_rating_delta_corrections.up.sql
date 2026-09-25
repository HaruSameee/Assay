-- レート増減の**補正**（運営操作）。**追記専用。**
--
-- 増減幅（current_rating_params.base_delta）を変えると全試合が引き直される。
-- 「シーズン終盤だけ増減を大きくする」のような、**期間を区切った上乗せ / 差し引き**は
-- それでは表せない。そこで、期間（試合の成立時刻 formations.created_at が
-- [starts_at, ends_at) に入る試合）に対して増減の**大きさ**へ amount を足す補正を持つ。
--
--   勝ち: +(base_delta + 補正の合計)   負け: −(base_delta + 補正の合計)   引き分け: 0
--
-- 大きさは 0 を下回らない（負の補正を重ねても勝って下がることはない）。期間が重なる
-- 補正は合算する。**過去の試合にも効く**（期間に入っていれば引き直される。docs/02
-- 不変条件 3 のとおり、レートは常に現在の params と補正から導出する）。
--
-- 取り消しは rating_delta_correction_revocations に 1 行積む（補正の行は消さない）。
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する
--   * 状態列を作らない。効いている補正は「取り消しの行が無いもの」から導出する

create table rating_delta_corrections (
    id              bigserial        primary key,
    -- 増減の大きさへ足す値（表示スケール）。正なら増減が大きく、負なら小さくなる
    amount          double precision not null
        check (amount <> 0 and amount between -50 and 50),
    -- 試合の成立時刻がこの範囲に入れば効く。ends_at が null なら終わりなし
    starts_at       timestamptz      not null,
    ends_at         timestamptz      check (ends_at is null or ends_at > starts_at),
    reason          text             not null check (btrim(reason) <> ''),
    created_by      text             not null check (btrim(created_by) <> ''),
    idempotency_key text             not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz      not null
);

comment on table rating_delta_corrections is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。期間を区切ったレート増減の補正。成立時刻が [starts_at, ends_at) の試合で、増減の大きさに amount を足す（重なれば合算、大きさは 0 未満にならない）。取り消しは rating_delta_correction_revocations';

create trigger rating_delta_corrections_forbid_update_delete
    before update or delete on rating_delta_corrections
    for each statement execute function forbid_update_delete();

create table rating_delta_correction_revocations (
    id              bigserial   primary key,
    correction_id   bigint      not null unique references rating_delta_corrections (id),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table rating_delta_correction_revocations is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。レート増減の補正の取り消し。1 つの補正につき 1 行まで';

create trigger rating_delta_correction_revocations_forbid_update_delete
    before update or delete on rating_delta_correction_revocations
    for each statement execute function forbid_update_delete();

-- 効いている補正（取り消されていないもの）。
create view active_rating_delta_corrections as
select c.id, c.amount, c.starts_at, c.ends_at, c.reason, c.created_by, c.created_at
from rating_delta_corrections c
where not exists (
    select 1 from rating_delta_correction_revocations r where r.correction_id = c.id
);

comment on view active_rating_delta_corrections is
    '取り消されていないレート増減の補正。player_match_rating_deltas がここを引く';

-- ----------------------------------------------------------------------
-- player_match_rating_deltas: 増減の大きさに補正を足す。列の構成は変えない。
-- ----------------------------------------------------------------------
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
        when c.outcome = fp.team then greatest(p.base_delta + correction.amount, 0.0)
        else -greatest(p.base_delta + correction.amount, 0.0)
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
cross join lateral (
    select coalesce(sum(r.amount), 0.0)::double precision as amount
    from active_rating_delta_corrections r
    where f.created_at >= r.starts_at
      and (r.ends_at is null or f.created_at < r.ends_at)
) correction
where f.created_at >= s.started_at;

comment on view player_match_rating_deltas is
    '確定した試合 1 件ぶんの増減（07 §2-2）。勝ち +(base_delta + 補正) / 負け −(base_delta + 補正)、大きさは 0 未満にならない。補正は成立時刻が期間に入る active_rating_delta_corrections の合計。「確定した試合」は formation_final_outcomes の kind = confirmed。増減幅も補正も現在値から引くので、変えると過去の試合も引き直される（docs/02 不変条件 3）。現在のシーズンに入る試合だけを返す。導出値であり実体を持たない';

-- ----------------------------------------------------------------------
-- 運営操作ログに rating_correction_added / rating_correction_revoked を足す。
-- ----------------------------------------------------------------------
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
        'queue_priority_raised',
        'rating_correction_added',
        'rating_correction_revoked'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved / matchmaker_scheduled / match_readjudicated / block_cooldown_changed / ip_banned / ip_unbanned / shadow_banned / shadow_ban_lifted / queue_priority_raised / rating_correction_added / rating_correction_revoked';
