-- 試合結果をレートに反映する導出経路（07-マッチング仕様 §2-2）。
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
-- 不変条件（破らないこと）:
--   * **レーティングは導出値**（00 §3-2）。現在値のテーブルを作らない。ここで足すのは
--     ビューだけで、実体は持たない。読むたびに追記ログから計算し直す。
--   * **recompute が唯一の計算経路**（00 §3-3）。インクリメンタル更新の入口を作らない。
--     ビューは差分更新できないので、この性質が構造的に保証される。
--   * 入力はすべて既存の追記済みデータ。新しい列も新しい書き込み先も増やさない。
--
-- 増減が **`mu_at_match`（その試合の開始時点の凍結値）だけ**から決まることが効いている。
-- 現在レートを参照しないので、1 試合ぶんの増減は他の試合と独立で、畳み込みは**単なる総和**
-- になる。順序に依存せず、同じログからは必ず同じ値が出る。
--
-- 表示スケールの 60 は Rust 側の `rating::DISPLAY_SCALE` と一致させること
-- （`tests/rating.rs` が両者の一致を検査する）。しきい値 50 と基本値 11 は
-- `rating::RATING_GAP_THRESHOLD` / `rating::BASE_DELTA` と同じ意味。
--
-- 比較に足している 1e-9 は `rating::GAP_EPSILON`。凍結値は内部スケール（表示値 / 60）で
-- 保存されるので、表示スケールへ戻すと 1550 が 1549.9999999999998 になる。素朴に
-- `>= 50.0` で比較すると、**仕様が「50 以上」と書いている境界ちょうどが 50 未満に落ちる。**

-- ----------------------------------------------------------------------
-- player_match_rating_deltas: 確定した 1 試合ぶんの、1 プレイヤーから見た増減。
-- ----------------------------------------------------------------------
create view player_match_rating_deltas as
with confirmed as (
    -- formation ごとに **最新の confirmed 1 件**だけを採る。
    -- disputed からスタッフ裁定で confirmed に進む経路があり（04 §1）、
    -- 状態機械は confirmed -> confirmed も許すので、複数行ありうる。
    select distinct on (e.formation_id)
        e.formation_id,
        e.payload->>'outcome' as outcome
    from formation_events e
    where e.kind = 'confirmed'
    order by e.formation_id, e.id desc
),
team_mu as (
    -- チームごとの平均 μ（内部スケール）。相手チーム 5 人の平均を引くために使う。
    select fp.formation_id, fp.team, avg(fp.mu_at_match) as mean_mu
    from formation_players fp
    group by fp.formation_id, fp.team
)
select
    fp.player_id,
    fp.formation_id,
    fp.team,
    c.outcome,
    -- 表示スケールに直した凍結値
    fp.mu_at_match * 60.0                as own_rating,
    opponent.mean_mu * 60.0              as opponent_mean_rating,
    case
        -- 引き分けは増減 0。nullified は kind が別なのでそもそもここに来ない
        when c.outcome = 'draw' then 0.0
        when c.outcome = fp.team then
            -- 勝ち: 格上 +12 / 格下 +10 / それ以外 +11
            case
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 >=  50.0 - 1e-9 then  12.0
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 <= -50.0 + 1e-9 then  10.0
                else 11.0
            end
        else
            -- 負け: 格上 -10 / 格下 -12 / それ以外 -11
            case
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 >=  50.0 - 1e-9 then -10.0
                when (opponent.mean_mu - fp.mu_at_match) * 60.0 <= -50.0 + 1e-9 then -12.0
                else -11.0
            end
    end::double precision as delta
from formation_players fp
join confirmed c on c.formation_id = fp.formation_id
join team_mu opponent
    on opponent.formation_id = fp.formation_id
   and opponent.team <> fp.team;

comment on view player_match_rating_deltas is
    '確定した試合 1 件ぶんの増減（07 §2-2）。導出値であり実体を持たない。outcome は confirmed イベントの payload から、レートは formation_players.mu_at_match の凍結値から取る。disputed / voided / abandoned / nullified の formation は confirmed イベントを持たないので現れない。';

-- ----------------------------------------------------------------------
-- player_match_ratings: プレイヤーごとの合計。キューとマッチメイカが読む。
-- ----------------------------------------------------------------------
create view player_match_ratings as
select player_id, sum(delta)::double precision as delta
from player_match_rating_deltas
group by player_id;

comment on view player_match_ratings is
    'プレイヤーごとの試合による増減の合計（表示スケール）。現在レート = 1500 + rating_adjustments の合計 + この値。実体を持たないので、読むたびに追記ログから再計算される。';

-- 確定イベントの絞り込みと、プレイヤー単位の集計を速くする。
create index formation_events_confirmed_idx
    on formation_events (formation_id, id desc)
    where kind = 'confirmed';
