-- Phase 1: 試合ライフサイクルのスキーマ
-- 出典: 04-試合ライフサイクル仕様 §6「スキーマ（追記専用）」＋ Phase 1 修正指示（2026-09-04）
--       ＋ Phase 2 レビュー（2026-09-05: kind の CHECK、actor_player_id → subject_player_id）
--
-- 不変条件（破らないこと）:
--   1. formations / formation_players / formation_events / match_reports の4テーブルは追記専用。
--      アプリケーションコードから UPDATE / DELETE を発行しない。DB 側でも reject_mutation
--      トリガーが UPDATE / DELETE を文単位で拒否する。取り消しは打ち消しレコード
--      （例: match_reports.supersedes_id）で表現する。
--   2. formations に state カラムを置かない。現在状態は formation_events から導出する。
--      一般則: formation 生成時点で確定していない値は、列ではなくイベントにする。
--   3. match_reports.raw_result は報告された生データをそのまま保持する。パースに失敗しても保存する。
--   4. formation_events の順序の正は id（挿入順）。occurred_at は記録用で、順序決定には使わない。
--
-- players / roles は可変の参照データであり、追記専用の対象外。
-- 仕様どおり、id / timestamp 列に DB 側の default は付けていない（値はアプリケーションが与える）。
-- 実行時の主たる防御であるアプリ用ロールへの REVOKE は ops/revoke_app_role_mutations.sql にあり、
-- マイグレーションからは呼ばない（ロールの用意はマイグレーションの責務ではない）。

-- ----------------------------------------------------------------------
-- 追記専用テーブルを守るトリガー関数（UPDATE / DELETE を文単位で拒否）
-- ----------------------------------------------------------------------
create function reject_mutation() returns trigger language plpgsql as $$
begin
    raise exception '% は追記専用テーブルです。UPDATE / DELETE は禁止されています', tg_table_name;
end;
$$;

-- ----------------------------------------------------------------------
-- players: 可変の参照データ。Phase 1 では最小構成。登録・認証の列は別トラックで足す。
-- 追記専用テーブルは後から掃除できないため、存在しない player_id を書けないよう FK の参照先として置く。
-- ----------------------------------------------------------------------
create table players (
    id               uuid        primary key,
    discord_user_id  bigint      not null unique,
    created_at       timestamptz not null
);

comment on table players is
    '可変の参照データ（追記専用ではない）。Phase 1 は最小構成で、登録・認証まわりの列は別トラックで追加する。';

-- ----------------------------------------------------------------------
-- roles: ロールの lookup。識別子（不変・ログが参照する）と表示名（可変）を分ける。
-- 初期データは入れない。正規値が未定であり、プレースホルダを入れるとログから参照されて永久に残るため。
-- ----------------------------------------------------------------------
create table roles (
    id            text    primary key,
    display_name  text    not null,
    sort_order    integer not null
);

comment on table roles is
    '可変の参照データ（追記専用ではない）。初期データは入れない。テストはフィクスチャで INSERT する。';
comment on column roles.id is
    '不変の識別子。英字スネークケース。formation_players.role から参照される';
comment on column roles.display_name is
    'UI 表示専用。変更してよい';

-- ----------------------------------------------------------------------
-- formations: マッチメイカが10人を確定させた時点で1行。以後不変。
-- 生成時点で確定していない値（Discord スレッド ID など）は列にせず formation_events に積む。
-- ----------------------------------------------------------------------
create table formations (
    id                          uuid        primary key,
    created_at                  timestamptz not null,
    -- マッチング時点の凍結値
    matchmaking_params_version  integer     not null,
    predicted_win_prob          real        not null
);

comment on table formations is
    '追記専用: UPDATE / DELETE 禁止（reject_mutation トリガーで拒否）。マッチ確定時点の凍結値のみを持つ。生成時点で確定していない値は列にせず formation_events に積む。状態カラムは置かず、現在状態は formation_events から導出する。';
comment on column formations.matchmaking_params_version is
    'マッチング時点のマッチメイキングパラメータ版数（凍結値）';
comment on column formations.predicted_win_prob is
    'マッチメイカが成立時に算出した予測勝率（凍結値）';

-- ----------------------------------------------------------------------
-- formation_players: 編成。1 formation につき10行。
-- ----------------------------------------------------------------------
create table formation_players (
    formation_id     uuid             not null references formations (id),
    player_id        uuid             not null references players (id),
    team             smallint         not null check (team in (0, 1)),
    role             text             not null references roles (id),
    role_was_flexed  boolean          not null,
    mu_at_match      double precision not null,
    sigma_at_match   double precision not null,
    primary key (formation_id, player_id)
);

create index formation_players_player_id_idx
    on formation_players (player_id);

comment on table formation_players is
    '追記専用: UPDATE / DELETE 禁止（reject_mutation トリガーで拒否）。マッチング時点の編成と μ・σ の凍結値。';
comment on column formation_players.team is
    '0 または 1';
comment on column formation_players.role is
    'roles.id への参照（不変の識別子）';
comment on column formation_players.role_was_flexed is
    'フレックスで埋められたか。フレックス補償の入力';
comment on column formation_players.mu_at_match is
    'マッチング時点の μ（凍結値。recompute の影響を受けない）';
comment on column formation_players.sigma_at_match is
    'マッチング時点の σ（凍結値。recompute の影響を受けない）';

-- ----------------------------------------------------------------------
-- formation_events: 状態遷移ログ。追記のみ。現在状態はこれを id 順に畳み込んで導出する。
-- ----------------------------------------------------------------------
create table formation_events (
    id                 bigserial   primary key,
    formation_id       uuid        not null references formations (id),
    occurred_at        timestamptz not null,
    -- 種別。値は Rust 側の FormationEventKind::ALL と一致させる（tests/schema.rs で検証）
    kind               text        not null check (kind in (
        'thread_created', 'host_assigned', 'host_accepted', 'host_declined',
        'host_timeout', 'code_posted', 'confirmed', 'disputed', 'voided', 'abandoned'
    )),
    -- このイベントが対象とするプレイヤー（行為者ではない）
    subject_player_id  uuid        references players (id),
    payload            jsonb       not null,
    idempotency_key    text        not null,
    unique (formation_id, idempotency_key)
);

create index formation_events_formation_id_occurred_at_idx
    on formation_events (formation_id, occurred_at);
create index formation_events_formation_id_id_idx
    on formation_events (formation_id, id);

-- Discord のインタラクションは thread_id で飛んでくるので、thread_created イベントから逆引きする
create unique index formation_events_thread_id_idx
    on formation_events ((payload->>'discord_thread_id'))
    where kind = 'thread_created';

comment on table formation_events is
    '追記専用: UPDATE / DELETE 禁止（reject_mutation トリガーで拒否）。formation の状態遷移ログ。現在状態は formation の行を id 順に畳み込んで導出する（順序の正は id）。';
comment on column formation_events.occurred_at is
    '記録用の発生時刻。アプリが値を与えるため、複数プロセス間のクロックスキューで逆転しうる。順序の正は id（挿入順）であり、この列を順序決定に使わない';
comment on column formation_events.kind is
    'イベント種別。CHECK の値は Rust 側の FormationEventKind::ALL と一致させる（種別を増やすときはマイグレーションを追加する）';
comment on column formation_events.subject_player_id is
    'このイベントが対象とするプレイヤー（行為者ではない）。システム起因の host_assigned / host_timeout でも対象者を入れる。行為者と対象が異なり、行為者を記録する必要が出た場合は payload に入れる（列は増やさない）';
comment on column formation_events.payload is
    'イベントの生データ。thread_created: {"discord_thread_id": "..."} / code_posted: {"room_code": "..."} / abandoned: {"reason": "host_candidates_exhausted" | "not_enough_players" | "thread_creation_failed"}。個人の特定（誰が来なかったか等）は入れない';
comment on column formation_events.idempotency_key is
    'アプリが供給する冪等キー。(formation_id, idempotency_key) の unique で、Discord API の再送による同一イベントの二重追記を防ぐ。Phase 4 で Discord のインタラクション ID を使う想定';

-- ----------------------------------------------------------------------
-- match_reports: 結果報告。1人1行、追記のみ。取り消しは打ち消しレコード（supersedes_id）。
-- 報告者は当該 formation の参加者に限る（formation_players への複合 FK で DB レベルで保証）。
-- ----------------------------------------------------------------------
create table match_reports (
    id             bigserial   primary key,
    formation_id   uuid        not null references formations (id),
    player_id      uuid        not null,
    reported_at    timestamptz not null,
    outcome        text        not null check (outcome in ('team_0', 'team_1', 'draw')),
    raw_result     jsonb       not null,
    supersedes_id  bigint      references match_reports (id),
    foreign key (formation_id, player_id)
        references formation_players (formation_id, player_id)
);

create index match_reports_formation_id_player_id_idx
    on match_reports (formation_id, player_id);
create index match_reports_player_id_idx
    on match_reports (player_id);
-- 訂正チェーンは分岐しない: 1つの報告を打ち消せるのは1件だけ（「最新の報告」が一意に決まる）
create unique index match_reports_supersedes_uniq
    on match_reports (supersedes_id)
    where supersedes_id is not null;

comment on table match_reports is
    '追記専用: UPDATE / DELETE 禁止（reject_mutation トリガーで拒否）。報告の取り消し・訂正は supersedes_id を持つ新しい行で表現する。他人の報告内容は本人の報告前に返さない（API レベルでブラインド）。報告者は複合 FK により当該試合の参加者に限られる。';
comment on column match_reports.outcome is
    '報告者が主張する結果。team_0 / team_1 / draw。引き分けを NULL で表現しない（未報告と区別できなくなる）';
comment on column match_reports.raw_result is
    '報告された生データ（スクリーンショット URL を含む）。パースに失敗しても必ず保存する';
comment on column match_reports.supersedes_id is
    'この行が打ち消す（置き換える）以前の報告の id。同じプレイヤー・同じ formation の報告に限る。1つの報告を打ち消せるのは1件だけ。勝敗判定は最新の報告を使うが、報告一致率は各プレイヤーの最初の報告で測る';

-- ----------------------------------------------------------------------
-- 追記専用トリガー。対象は4テーブルのみ（players / roles は可変の参照データなので付けない）。
-- for each statement: 行数に比例したオーバーヘッドが無く、0行にマッチする UPDATE も弾ける。
-- ----------------------------------------------------------------------
create trigger formations_reject_mutation
    before update or delete on formations
    for each statement execute function reject_mutation();
create trigger formation_players_reject_mutation
    before update or delete on formation_players
    for each statement execute function reject_mutation();
create trigger formation_events_reject_mutation
    before update or delete on formation_events
    for each statement execute function reject_mutation();
create trigger match_reports_reject_mutation
    before update or delete on match_reports
    for each statement execute function reject_mutation();
