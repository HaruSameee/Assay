-- 聞き専報告。**味方からの複数票でレートを 1 下げる。**
--
-- docs/02 の却下ログ「パフォーマンス指標をレートに入れない（全部自己申告になり、
-- 他 9 人がクロスチェックできない）」に対する、運営判断による明示的な例外。
-- **自己申告ではなく他者からの複数票**なので、却下の理由（クロスチェックできない）は
-- ここには当たらない。抑止はレート値（-1）ではなく、運営が通報履歴を確認することで行う。
--
-- 不変条件:
--   * **追記専用。**UPDATE / DELETE は書かない（forbid_update_delete で二重化）。
--   * **レーティングは導出値。**ペナルティも現在値の列を持たず、票を数えて閾値と
--     比べるだけ。recompute から完全に再構築できる（インクリメンタルな加算はしない）。
--   * **票数と閾値は API にも HTML にも出さない。**サーバー側でそもそも送らない
--     （docs/09）。この表を読む経路は運営パネルとレート導出だけ。
--   * 対象は**同じ formation の自チーム**のみ。自分自身は不可。DB でも CHECK で弾く。

create table quiet_reports (
    id              bigserial   primary key,
    formation_id    uuid        not null references formations (id),
    -- 報告した人。**対象と同じチームであることはアプリ側で検証する**
    -- （チームは formation_players から導出できるので、ここには持たない）
    reporter_id     uuid        not null references players (id),
    -- 報告された人。
    target_id       uuid        not null references players (id),
    reported_at     timestamptz not null,
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    -- 自分自身は対象にできない
    constraint quiet_reports_not_self check (reporter_id <> target_id)
);

-- **同じ formation で、同じ人が同じ相手を二度報告できない。**
-- 冪等キーが同じなら no-op、違うキーで同じ組み合わせなら 23505 で弾かれる
-- （アプリ側で「もう報告したか」を先に見る状態ベースの判定はしない）。
create unique index quiet_reports_once
    on quiet_reports (formation_id, reporter_id, target_id);
create index quiet_reports_target_idx on quiet_reports (target_id);
create index quiet_reports_reporter_idx on quiet_reports (reporter_id);

comment on table quiet_reports is
    '追記専用: UPDATE / DELETE 禁止。聞き専報告の生ログ。誰が誰を報告したかは運営パネルからしか読めない（他プレイヤーの API レスポンスには一切現れない）。票数と閾値は画面にも API にも出さない';
comment on column quiet_reports.reporter_id is
    '報告した人。対象と同じチームであること・確定後であることはアプリ側で検証する（formation_players と formation_events から導出できるので列には持たない）';

create trigger quiet_reports_forbid_update_delete
    before update or delete on quiet_reports
    for each statement execute function forbid_update_delete();

-- ----------------------------------------------------------------------
-- 閾値。**正は環境変数**で、ここはそれを SQL から見えるようにした写し。
-- ----------------------------------------------------------------------
--
-- 閾値は勝ち側・負け側で別々（負け側は 5 人から対象者を除いた 4 人しか報告できないので、
-- 4 票は満場一致にあたる）。環境変数で当日に変えられるようにするのが仕様だが、
-- **レートの合算は player_current_ratings 1 か所に集約してある**ので、判定に使う閾値も
-- SQL から見えなければならない。そこで起動時に環境変数の値をここへ追記する
-- （値が現在の導出値と同じなら何も書かない）。
--
-- 追記専用にしてあるので、閾値を変えた履歴も残る。recompute は常に最新行を使うため、
-- **閾値を変えると過去の試合も新しい閾値で再判定される**（docs/02 不変条件 3）。
create table quiet_report_param_changes (
    id                bigserial   primary key,
    -- 勝ったチームの報告で必要な票数。
    winner_threshold  integer     not null check (winner_threshold between 1 and 4),
    -- 負けたチームの報告で必要な票数。
    loser_threshold   integer     not null check (loser_threshold between 1 and 4),
    source            text        not null check (btrim(source) <> ''),
    created_at        timestamptz not null
);

comment on table quiet_report_param_changes is
    '追記専用: UPDATE / DELETE 禁止。聞き専報告の閾値の変更ログ。正は環境変数で、これは SQL から見えるようにした写し（起動時に同期する）。現在値は最新行から導出する';

create index quiet_report_param_changes_id_desc_idx
    on quiet_report_param_changes (id desc);

create trigger quiet_report_param_changes_forbid_update_delete
    before update or delete on quiet_report_param_changes
    for each statement execute function forbid_update_delete();

create view current_quiet_report_params as
select coalesce(latest.winner_threshold, 3) as winner_threshold,
       coalesce(latest.loser_threshold, 4)  as loser_threshold
from (values (1)) as anchor (x)
left join lateral (
    select c.winner_threshold, c.loser_threshold
    from quiet_report_param_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_quiet_report_params is
    '現在の聞き専報告の閾値。最新行から導出する。1 行も無ければ既定値（勝ち 3 / 負け 4）。閾値を見る場所はここ 1 つで、quiet_report_penalties がこれを引く';

-- ----------------------------------------------------------------------
-- quiet_report_penalties: 閾値に達した試合ぶんのペナルティ。
-- ----------------------------------------------------------------------
--
-- **票数はここから外へ出さない。**このビューを読むのはレート導出と運営パネルだけで、
-- プレイヤー向けの経路は誰も読まない。
--
-- 閾値は**対象者のチームが勝ったか負けたか**で変わる。報告できるのは同じチームの
-- メンバーだけなので、対象者のチーム＝報告者のチームである。
--
-- 引き分けは対象外。勝敗が無く閾値が決まらないため（nullified / voided / disputed が
-- そもそも confirmed に現れないのと同じ理由）。
create view quiet_report_penalties as
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

comment on view quiet_report_penalties is
    '閾値に達した聞き専報告のペナルティ（1 試合につき -1）。導出値であり実体を持たない。閾値を変えると過去の試合も再判定される。votes 列はプレイヤー向けの経路から読まないこと（票数は秘匿）';

-- ----------------------------------------------------------------------
-- player_current_ratings: 入口を 1 本足す。
-- ----------------------------------------------------------------------
--
-- **合算はここ 1 か所のまま。**ペナルティを足すのもここだけで、キュー・マッチメイカ・
-- マッチ画面・管理パネル・プロフィール・ランキングは何も変えなくてよい。
create or replace view player_current_ratings as
select
    p.id                                                     as player_id,
    1500.0::double precision
        + coalesce(adjustments.delta_mu, 0::double precision)
        + coalesce(matches.delta, 0::double precision)
        + coalesce(penalties.delta, 0::double precision)     as rating,
    500.0::double precision
        + coalesce(adjustments.delta_sigma, 0::double precision) as sigma,
    coalesce(matches.played, 0::bigint)                      as matches
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
) penalties on penalties.player_id = p.id;

comment on view player_current_ratings is
    '現在レート・σ・確定した試合数を 1 か所で合算する唯一のビュー。1500 + Σ season_rating_adjustments + Σ player_match_rating_deltas + Σ quiet_report_penalties。導出値であり実体を持たない。レートを読む経路はすべてここを引くこと（式を各モジュールの SQL に書かない）';
