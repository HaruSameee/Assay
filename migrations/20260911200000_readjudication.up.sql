-- 終わった試合の裁定をやり直せるようにする。**追記のみ。**
--
-- 「無効試合にするか微妙な試合を各自の判断で報告し、確定した後で
--  『やっぱり無効試合にしよう』となる」ことがある。10 人ぶんのレートを
-- 1 人ずつ調整で打ち消すのは間違えやすく、増減幅を変えた後に引き直されもしない。
--
-- **新しいテーブルは作らない。**再裁定は formation_events に終端イベント
-- （confirmed / nullified / voided）をもう 1 行積むだけで表す。レートは導出値なので、
-- 最後の終端イベントが変われば 10 人ぶんの増減は自動的に引き直される
-- （docs/02 不変条件 2・3）。
--
-- これに伴い、**「確定した試合」の定義を「confirmed イベントがある」から
-- 「最後の終端イベントが confirmed」に変える。**これまでは再裁定が無かったので
-- 両者は同じだったが、confirmed の後に nullified を積めるようになると
-- 前者では無効にした試合のレートが残ってしまう。定義を 1 か所
-- （formation_final_outcomes）に置き、レート・聞き専・無効試合通報の 3 つの
-- 導出がすべてそこを引く。

-- ----------------------------------------------------------------------
-- formation_final_outcomes: 試合ごとの**最後の**終端イベント。
-- ----------------------------------------------------------------------
--
-- 「試合はどう終わったか」を読む場所はここ 1 つ。`abandoned` は含めない
-- （試合が始まる前の終わり方で、これらと同じ formation に並ぶことは無い）。
-- `disputed` は終端ではないので含めない（裁定されるまで結果が無い）。
create view formation_final_outcomes as
select distinct on (e.formation_id)
    e.formation_id,
    e.id                  as event_id,
    e.kind,
    e.payload->>'outcome' as outcome,
    e.occurred_at
from formation_events e
where e.kind in ('confirmed', 'nullified', 'voided')
order by e.formation_id, e.id desc;

comment on view formation_final_outcomes is
    '試合ごとの最後の終端イベント（confirmed / nullified / voided）。「確定した試合」は kind = confirmed の行。再裁定で終端イベントが積み直されると、この行が入れ替わり、レート・聞き専・無効試合通報の導出がすべて追従する。導出値であり実体を持たない';

-- ----------------------------------------------------------------------
-- player_match_rating_deltas: 最後の終端が confirmed の試合だけ。
-- ----------------------------------------------------------------------
--
-- 列の構成は変えないので create or replace。confirmed の後に nullified が積まれた
-- 試合は、ここに現れなくなる（10 人ぶんの増減が同時に消える）。
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

comment on view player_match_rating_deltas is
    '確定した試合 1 件ぶんの増減（07 §2-2。勝ち +base_delta / 負け -base_delta のフラット）。「確定した試合」は formation_final_outcomes の kind = confirmed（再裁定で nullified に変わった試合は消える）。増減幅は current_rating_params から引くので、params を変えると過去の全試合が新しい値で引き直される（docs/02 不変条件 3）。現在のシーズン（current_season）に入る試合だけを返す。導出値であり実体を持たない';

-- ----------------------------------------------------------------------
-- quiet_report_penalties: 同じく最後の終端が confirmed の試合だけ。
-- ----------------------------------------------------------------------
create or replace view quiet_report_penalties as
with tally as (
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
join formation_final_outcomes c
    on c.formation_id = t.formation_id and c.kind = 'confirmed'
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
    '閾値に達した聞き専報告のペナルティ（1 試合につき -1）。最後の終端が confirmed の試合だけ（formation_final_outcomes）。導出値であり実体を持たない。閾値を変えると過去の試合も再判定される。votes 列はプレイヤー向けの経路から読まないこと（票数は秘匿）';

-- ----------------------------------------------------------------------
-- nullified_report_penalties: 最後の終端が nullified の試合だけ。
-- ----------------------------------------------------------------------
--
-- これまでは「nullified イベントが 1 つでもあれば」だった。再裁定で nullified から
-- confirmed に戻した試合の通報ペナルティが残らないよう、最後の終端で見る。
create or replace view nullified_report_penalties as
with tally as (
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
join formation_final_outcomes n
    on n.formation_id = t.formation_id and n.kind = 'nullified'
join formation_players fp
    on fp.formation_id = t.formation_id and fp.player_id = t.target_id
join formations f on f.id = t.formation_id
cross join current_season s
cross join current_nullified_report_params p
where f.created_at >= s.started_at
  and t.votes >= p.threshold;

comment on view nullified_report_penalties is
    '無効試合の原因として成立した通報の増減（既定 -21）。最後の終端が nullified の試合だけ（formation_final_outcomes）。「誰も通報しない」の票は数に入らない。閾値もペナルティ幅も current_nullified_report_params から引くので、変えると過去の試合が新しい値で再判定される（docs/02 不変条件 3）。導出値であり実体を持たない';

-- ----------------------------------------------------------------------
-- 運営操作ログに match_readjudicated を足す。
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
        'match_readjudicated'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved / matchmaker_scheduled / match_readjudicated';
