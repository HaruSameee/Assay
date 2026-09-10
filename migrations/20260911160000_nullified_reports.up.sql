-- 無効試合の原因になった人の通報。**追記専用。**
--
-- 「無効試合」を報告した人に、原因のプレイヤーを 1 人選ばせる。聞き専報告
-- （`quiet_reports`）と形は同じだが、3 つ違う:
--
--   * **相手チームも通報できる。**部屋を壊すのは味方とは限らない。聞き専は
--     ボイスチャットの話なので同チームに限っていたが、無効試合の原因は
--     10 人の誰でもありうる
--   * **1 人 1 票で選び直せない。**「原因は誰か」は 1 つの答えなので、
--     (formation, reporter) で一意にする。聞き専は「この人は聞き専だった」を
--     複数人に付けられるので (formation, reporter, target) だった
--   * **「誰も通報しない」を票として記録する。**`target_id` が null の行が
--     それにあたる。答えていない人と区別できないと、画面が催促を出し続ける
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する
--   * **状態列を作らない。**成立しているかは票を数えて導出する
--   * **試合が nullified で終わったときだけレートに効く。**通報は報告直後に
--     受け付けるので、無効試合にならなかった試合の票も残る。その票は記録として
--     残るだけで、`nullified_report_penalties` には現れない

create table nullified_reports (
    id           bigserial   primary key,
    formation_id uuid        not null references formations (id),
    -- 通報した人。**その試合で「無効試合」を報告した参加者だけ**（アプリ側で検証する）
    reporter_id  uuid        not null references players (id),
    -- 原因として選ばれた人。**null は「誰も通報しない」**を選んだという 1 票
    target_id    uuid        references players (id),
    created_at   timestamptz not null,
    -- 自分を原因として通報することはできない
    check (target_id is null or reporter_id <> target_id)
);

-- **1 人 1 票。**「原因は誰か」は 1 つの答えなので、選び直しも追加もできない。
-- 「誰も通報しない」を選んだ人もこの一意制約に入る
create unique index nullified_reports_once on nullified_reports (formation_id, reporter_id);
create index nullified_reports_target_idx on nullified_reports (formation_id, target_id);

comment on table nullified_reports is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。無効試合の原因として通報された人。1 人 1 票で選び直せない。target_id が null は「誰も通報しない」という 1 票。試合が nullified で終わったときだけ nullified_report_penalties に現れる';
comment on column nullified_reports.reporter_id is
    '通報した人。その試合で「無効試合」を報告した参加者だけが通報できる（アプリ側で検証する）';
comment on column nullified_reports.target_id is
    '原因として選ばれた人。**相手チームでもよい**（部屋を壊すのは味方とは限らない）。null は「誰も通報しない」';

create trigger nullified_reports_forbid_update_delete
    before update or delete on nullified_reports
    for each statement execute function forbid_update_delete();

-- ----------------------------------------------------------------------
-- 閾値とペナルティ幅。**追記専用で、現在値は最新行から導出する。**
-- ----------------------------------------------------------------------
create table nullified_report_param_changes (
    id              bigserial        primary key,
    -- 成立に必要な票数。通報できるのは自分を除く 9 人なので 9 まで。
    -- 3 未満だと 2 人の口裏合わせで −21 を撃てるので下限を 3 に置く
    threshold       integer          not null check (threshold between 3 and 9),
    -- ペナルティの大きさ（表示スケールの絶対値）。導出では符号を反転して使う
    penalty         double precision not null check (penalty > 0 and penalty <= 100),
    reason          text             not null check (btrim(reason) <> ''),
    created_by      text             not null check (btrim(created_by) <> ''),
    idempotency_key text             not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz      not null
);

comment on table nullified_report_param_changes is
    '追記専用: UPDATE / DELETE 禁止。無効試合の通報の閾値とペナルティ幅の変更ログ。現在値は最新行（id 最大）から導出する（current_nullified_report_params）';

create index nullified_report_param_changes_id_desc_idx
    on nullified_report_param_changes (id desc);

create trigger nullified_report_param_changes_forbid_update_delete
    before update or delete on nullified_report_param_changes
    for each statement execute function forbid_update_delete();

-- current_season / current_rating_params と同じ形。**最新行（id 最大）**を採る。
-- 1 行も無ければ既定値（5 票 / −21）。
create view current_nullified_report_params as
select coalesce(latest.threshold, 5)              as threshold,
       coalesce(latest.penalty, 21.0::double precision) as penalty
from (values (1)) as anchor (x)
left join lateral (
    select c.threshold, c.penalty
    from nullified_report_param_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_nullified_report_params is
    '現在の無効試合通報の閾値とペナルティ幅。nullified_report_param_changes の最新行から導出する。1 行も無ければ 5 票 / 21。閾値を見る場所はここ 1 つで、nullified_report_penalties がこれを引く';

-- ----------------------------------------------------------------------
-- nullified_report_penalties: 成立した通報の増減。
-- ----------------------------------------------------------------------
--
-- **試合が nullified で終わったものだけ。**通報は報告直後に受け付けるので、
-- 結局 confirmed になった試合の票もテーブルには残る。そちらはここに現れない。
--
-- 「誰も通報しない」（target_id が null）は数に入らない。数えるのは
-- 「その人を名指しした票」だけである。
create view nullified_report_penalties as
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

comment on view nullified_report_penalties is
    '無効試合の原因として成立した通報の増減（既定 −21）。試合が nullified で終わったものだけ。「誰も通報しない」の票は数に入らない。閾値もペナルティ幅も current_nullified_report_params から引くので、変えると過去の試合が新しい値で再判定される（docs/02 不変条件 3）。導出値であり実体を持たない';

-- ----------------------------------------------------------------------
-- player_current_ratings: 入口をもう 1 つ足す。**合算はここ 1 か所のまま。**
-- ----------------------------------------------------------------------
create or replace view player_current_ratings as
select p.id as player_id,
       1500.0::double precision
           + coalesce(adjustments.delta_mu, 0)
           + coalesce(matches.delta, 0)
           + coalesce(quiet.delta, 0)
           + coalesce(nullified_penalties.delta, 0) as rating,
       500.0::double precision + coalesce(adjustments.delta_sigma, 0) as sigma,
       coalesce(matches.played, 0::bigint) as matches
from players p
left join lateral (
    select sum(a.delta_mu) as delta_mu, sum(a.delta_sigma) as delta_sigma
    from season_rating_adjustments a
    where a.player_id = p.id
) adjustments on true
left join (
    select d.player_id, sum(d.delta) as delta, count(*)::bigint as played
    from player_match_rating_deltas d
    group by d.player_id
) matches on matches.player_id = p.id
left join (
    select q.player_id, sum(q.delta) as delta
    from quiet_report_penalties q
    group by q.player_id
) quiet on quiet.player_id = p.id
left join (
    select n.player_id, sum(n.delta) as delta
    from nullified_report_penalties n
    group by n.player_id
) nullified_penalties on nullified_penalties.player_id = p.id;

comment on view player_current_ratings is
    '現在のレート（表示スケール）。**合算する場所はここ 1 つだけ。**初期値 1500 + シーズン調整 + 確定した試合の増減 + 聞き専ペナルティ + 無効試合の通報ペナルティ。matches は確定した試合数（ペナルティは数に入らない）。導出値であり実体を持たない';

-- ----------------------------------------------------------------------
-- 運営操作ログには何も足さない。
-- ----------------------------------------------------------------------
--
-- 閾値とペナルティ幅を運営が変える画面はまだ作っていない（既定の 5 票 / −21 で
-- 動かす）。書き手のいない kind を CHECK に足すと、`tests/schema.rs` が見ている
-- 「SQL の CHECK と Rust の enum が一致すること」を満たすためだけに
-- 使われない列挙子が増える。**必要になったときに足す。**
