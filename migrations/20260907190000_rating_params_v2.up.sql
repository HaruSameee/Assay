-- 固定増減方式のパラメータを v2 へ（07 §2-2）。基本 11 → 7、格上・格下の補正は ±1 のまま。
--
-- | 版 | 基本 | 格上（勝ち/負け） | 格下（勝ち/負け） |
-- |---|---|---|---|
-- | 1 | 11 | +12 / −10 | +10 / −12 |
-- | 2 |  7 |  +8 /  −6 |  +6 /  −8 |
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
-- ビューは実体を持たないので、置き換えた瞬間から**過去の全試合が新しい params で
-- 引き直される**。これは 00 §3-2 / §3-3（レーティングは導出値、recompute が唯一の
-- 計算経路）どおりの意図した挙動であり、「当時の params で再生する」互換レイヤは作らない。
--
-- Rust 側の定数は `rating::BASE_DELTA` / `UPSET_ADJUSTMENT` / `RATING_GAP_THRESHOLD` と
-- `rating::RATING_PARAMS_VERSION`。両者が一致することは tests/rating.rs が検査する。
--
-- 列の構成は変えないので `create or replace` で済む（player_match_ratings は
-- このビューを select しているだけなので、作り直さない）。
-- しきい値 50 と 1e-9（`rating::GAP_EPSILON`）は据え置き。

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
        -- 引き分けは増減 0。nullified は kind が別なのでそもそもここに来ない
        when c.outcome = 'draw' then 0.0
        when c.outcome = fp.team then
            -- 勝ち: 格上 +8 / 格下 +6 / それ以外 +7
            case
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 >=  50.0 - 1e-9 then  8.0
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 <= -50.0 + 1e-9 then  6.0
                else 7.0
            end
        else
            -- 負け: 格上 -6 / 格下 -8 / それ以外 -7
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
    '確定した試合 1 件ぶんの増減（07 §2-2、params v2: 基本 7 / 格上 +8・-6 / 格下 +6・-8）。導出値であり実体を持たない。outcome は confirmed イベントの payload から、レートは formation_players.mu_at_match の凍結値から取る。disputed / voided / abandoned / nullified の formation は confirmed イベントを持たないので現れない。';
