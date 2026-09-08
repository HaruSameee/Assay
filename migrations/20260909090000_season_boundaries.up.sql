-- シーズン境界（導出の起点）。**レートのリセットではない。**
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
-- 不変条件（docs/02 の 1〜3 をすべて満たす）:
--   * **試合ログは 1 行も消さない。**境界より前の formation / formation_players /
--     formation_events / match_reports / rating_adjustments はそのまま残る。
--     この表が変えるのは「導出がどこから読み始めるか」だけ。
--   * **レーティングは導出値**。現在値のテーブルは無いままで、UPDATE もしない。
--   * **recompute が唯一の計算経路**。境界そのものも追記ログなので、
--     境界を含めて全履歴から完全再構築できる。
--   * 追記専用。UPDATE / DELETE 権限を与えない（既定のホワイトリスト方式なので、
--     ops/app_privileges.sql に足さなければそうなる）。所有者にも効く
--     forbid_update_delete トリガーで二重化する。
--   * **状態列を作らない。**現在のシーズンは最新行から導出する（current_season）。

create table season_boundaries (
    id              bigserial   primary key,
    effective_at    timestamptz not null,
    label           text        not null check (btrim(label) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table season_boundaries is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。導出の起点。この時刻より前の試合と調整は集計に入らないが、ログ自体は残る。現在のシーズンは最新行（id 最大）から導出する';
comment on column season_boundaries.effective_at is
    'この時刻以降に作られた formation と、この時刻以降に追記された rating_adjustments だけが導出に入る。formations.created_at で判定する（confirmed の時刻ではない。境界前に作られた試合が後から裁定されても集計に入らない）';
comment on column season_boundaries.label is
    '運営が付けたシーズン名。管理パネルの 2 段目でこの文字列を手入力させる（誤操作防止）';
comment on column season_boundaries.created_by is
    '操作した運営。discord:<id> の形';

create index season_boundaries_id_desc_idx on season_boundaries (id desc);

create trigger season_boundaries_forbid_update_delete
    before update or delete on season_boundaries
    for each statement execute function forbid_update_delete();

-- ----------------------------------------------------------------------
-- current_season: 境界を適用する**唯一の共通の入口**。
-- ----------------------------------------------------------------------
--
-- 導出側の条件式をあちこちにコピーしない。境界を見る場所はこのビューだけで、
-- 下の 2 ビューと formations_without_report_from がこれを引く。
--
-- **最新行（id 最大）**を採る。max(effective_at) にしないのは、誤って未来の時刻で
-- 入れてしまったときに、より前の時刻の境界を**追記して**戻せるようにするため
-- （追記専用の系では、取り消しは新しい行で表現する）。
-- 1 行も無ければ -infinity。すなわち全履歴が対象で、これが既定の状態。
create view current_season as
select coalesce(
    (select b.effective_at from season_boundaries b order by b.id desc limit 1),
    '-infinity'::timestamptz
) as started_at;

comment on view current_season is
    '現在のシーズンの開始時刻（導出の起点）。season_boundaries の最新行から導出する。境界が無ければ -infinity で全履歴が対象。境界を適用する条件はここに 1 つだけ置き、各導出経路はこのビューを引く';

-- ----------------------------------------------------------------------
-- season_rating_adjustments: 現在のシーズンに入る調整だけ。
-- ----------------------------------------------------------------------
--
-- 打ち消し行の扱い: 境界より前の調整を、境界より後で打ち消した行が残りうる。
-- 元が集計に入らないのに打ち消しだけ入ると、逆符号の差分がひとりでに効いてしまう。
-- 対象がシーズン内にある打ち消しだけを通す。
create view season_rating_adjustments as
select a.id, a.player_id, a.delta_mu, a.delta_sigma, a.reason, a.created_by,
       a.created_at, a.idempotency_key, a.reverts_id
from rating_adjustments a
cross join current_season s
where a.created_at >= s.started_at
  and (
      a.reverts_id is null
      or exists (
          select 1 from rating_adjustments original
          where original.id = a.reverts_id and original.created_at >= s.started_at
      )
  );

comment on view season_rating_adjustments is
    '現在のシーズンに入る rating_adjustments。導出はこのビューだけを読む（rating_adjustments を直接読まない）。境界より前の調整を後から打ち消した行は、対象が集計外なので通さない';

-- ----------------------------------------------------------------------
-- player_match_rating_deltas: 境界より前の試合を集計から外す。
-- ----------------------------------------------------------------------
--
-- 列の構成は変えないので create or replace。判定は **formations.created_at**。
-- confirmed イベントの時刻にしないのは、境界前に作られて境界後に裁定された試合
-- （disputed → confirmed）が紛れ込むため。created_at は挿入後に動かない。
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

comment on view player_match_rating_deltas is
    '確定した試合 1 件ぶんの増減（07 §2-2、params v3: 勝ち +7 / 負け -7 のフラット）。現在のシーズン（current_season）に入る試合だけを返す。導出値であり実体を持たない。境界より前の試合はログに残ったまま、ここに現れないだけ';
