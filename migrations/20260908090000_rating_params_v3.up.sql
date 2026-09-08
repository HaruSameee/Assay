-- 固定増減方式のパラメータを v3 へ（07 §2-2）。**格上・格下の分岐を削除する。**
--
-- | 版 | 基本 | 格上（勝ち/負け） | 格下（勝ち/負け） |
-- |---|---|---|---|
-- | 1 | 11 | +12 / −10 | +10 / −12 |
-- | 2 |  7 |  +8 /  −6 |  +6 /  −8 |
-- | 3 |  7 |  +7 /  −7 |  +7 /  −7 |（分岐なし）
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
-- ビューは実体を持たないので、置き換えた瞬間から**過去の全試合が新しい params で
-- 引き直される**。00 §3-2 / §3-3 どおりの意図した挙動で、互換レイヤは作らない。
--
-- 分岐が消えたので、しきい値 50 とその境界比較（`>= 50.0 - 1e-9`）も無くなった。
-- Rust 側も `RATING_GAP_THRESHOLD` / `UPSET_ADJUSTMENT` / `GAP_EPSILON` を削除してある。
-- 残るのは `rating::BASE_DELTA` だけで、一致は tests/rating.rs が検査する。
--
-- 列の構成は変えないので `create or replace` で済む。

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
    -- 表示スケールに直した凍結値。増減には効かないが、07 §7 の比較で使うので残す
    fp.mu_at_match * 60.0                as own_rating,
    opponent.mean_mu * 60.0              as opponent_mean_rating,
    case
        -- 引き分けは増減 0。nullified は kind が別なのでそもそもここに来ない
        when c.outcome = 'draw' then 0.0
        when c.outcome = fp.team then 7.0
        else -7.0
    end::double precision as delta
from formation_players fp
join confirmed c on c.formation_id = fp.formation_id
join team_mu opponent
    on opponent.formation_id = fp.formation_id
   and opponent.team <> fp.team;

comment on view player_match_rating_deltas is
    '確定した試合 1 件ぶんの増減（07 §2-2、params v3: 勝ち +7 / 負け -7 のフラット。格上・格下の分岐なし）。導出値であり実体を持たない。outcome は confirmed イベントの payload から、レートは formation_players.mu_at_match の凍結値から取る。disputed / voided / abandoned / nullified の formation は confirmed イベントを持たないので現れない。';
