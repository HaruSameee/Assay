-- レート増減幅（BASE_DELTA）を運営が変えられるようにする。**追記専用。**
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
-- 不変条件（docs/02 の 1〜3 をすべて満たす）:
--   * **試合ログは 1 行も消さない。**この表が変えるのは「導出が何を掛けるか」だけで、
--     formation_events / match_reports は一切触らない。
--   * **レーティングは導出値**。現在値のテーブルは無いままで、UPDATE もしない。
--     増減幅も現在値の列を作らず、最新行から導出する（current_rating_params）。
--   * **recompute が唯一の計算経路**。params を変えると
--     player_match_rating_deltas が過去の全試合を**新しい値で引き直す**。
--     これは docs/02 の不変条件 3（常に現行 params を全履歴に適用する）そのもので、
--     意図した挙動である。「当時の params で再生する」互換レイヤは作らない。
--   * 追記専用。UPDATE / DELETE 権限を与えない（既定のホワイトリスト方式なので、
--     ops/app_privileges.sql に足さなければそうなる）。所有者にも効く
--     forbid_update_delete トリガーで二重化する。
--   * **状態列を作らない。**現在の増減幅は最新行（id 最大）から導出する。

create table rating_param_changes (
    id              bigserial        primary key,
    -- 版数。既存の RATING_PARAMS_VERSION と同じ採番（単調増加の整数）を引き継ぐ。
    -- 変更のたびに 1 つ上げる。アプリが直前の版を読んで +1 した値を書く
    params_version  integer          not null check (params_version > 0),
    -- 勝ちの増減幅（表示スケール）。負けは同じ幅の逆符号。
    -- 0 以下は「勝っても負けても動かない」、大きすぎる値は 1 試合で順位が壊れる
    base_delta      double precision not null check (base_delta > 0 and base_delta <= 50),
    reason          text             not null check (btrim(reason) <> ''),
    created_by      text             not null check (btrim(created_by) <> ''),
    idempotency_key text             not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz      not null
);

comment on table rating_param_changes is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。レートの増減幅の変更ログ。現在値は最新行（id 最大）から導出する（current_rating_params）。行を積むと player_match_rating_deltas が過去の全試合を新しい値で引き直す';
comment on column rating_param_changes.params_version is
    'レーティング params の版数。既存の RATING_PARAMS_VERSION と同じ整数採番で、変更のたびに 1 つ上げる。試合には凍結しない（当時の params で再生する互換レイヤは作らない）';
comment on column rating_param_changes.base_delta is
    '勝ちの増減幅（表示スケール）。負けは -base_delta、引き分けは 0。1 行も無ければアプリの既定値 7 が使われる';
comment on column rating_param_changes.created_by is
    '操作した運営。discord:<id> の形';

create index rating_param_changes_id_desc_idx on rating_param_changes (id desc);

create trigger rating_param_changes_forbid_update_delete
    before update or delete on rating_param_changes
    for each statement execute function forbid_update_delete();

-- ----------------------------------------------------------------------
-- current_rating_params: 増減幅を適用する**唯一の共通の入口**。
-- ----------------------------------------------------------------------
--
-- current_season と同じ形にしてある。導出側の条件式をあちこちにコピーせず、
-- 見る場所をこのビュー 1 つに集める。
--
-- **最新行（id 最大）**を採る。誤って極端な値を入れてしまったときは、
-- 元の値を**追記して**戻す（追記専用の系では、取り消しは新しい行で表現する）。
-- 1 行も無ければアプリの既定値（7 / v3）で、これが現在の本番の状態である。
create view current_rating_params as
select coalesce(latest.base_delta, 7.0::double precision) as base_delta,
       coalesce(latest.params_version, 3)                 as params_version
from (values (1)) as anchor (x)
left join lateral (
    select c.base_delta, c.params_version
    from rating_param_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_rating_params is
    '現在のレート params（増減幅と版数）。rating_param_changes の最新行から導出する。1 行も無ければ既定値 7 / v3。増減幅を見る場所はここ 1 つで、player_match_rating_deltas がこれを引く';

-- ----------------------------------------------------------------------
-- player_match_rating_deltas: 増減幅を定数からビューへ移す。
-- ----------------------------------------------------------------------
--
-- 列の構成は変えないので create or replace。ここが params を引くようになることで、
-- **recompute が自動的に効く**——値を変えた瞬間に、過去の全試合の増減が
-- 新しい値で計算し直される。試合ログの行数は変わらない。
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

comment on view player_match_rating_deltas is
    '確定した試合 1 件ぶんの増減（07 §2-2。勝ち +base_delta / 負け -base_delta のフラット）。増減幅は current_rating_params から引くので、params を変えると過去の全試合が新しい値で引き直される（docs/02 不変条件 3）。現在のシーズン（current_season）に入る試合だけを返す。導出値であり実体を持たない';

-- ----------------------------------------------------------------------
-- 運営操作ログに rating_params_changed を足す。
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
        'rating_params_changed'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed';
