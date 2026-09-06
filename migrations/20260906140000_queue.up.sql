-- キューのデータモデル（07-マッチング仕様 §3・§4-5）。
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
-- 不変条件と、それをここでどう守るか:
--   * **キューは可変テーブル**。追記専用にしない。投入・設定変更・離脱が頻繁に起き、
--     マッチが成立すれば行は消える。残すべき履歴は formation 側（formations /
--     formation_players / formation_events）に既にあり、キューの出入りを追記ログにしても
--     読まれないログが増えるだけになる。
--     したがって forbid_update_delete トリガーは**付けない**。代わりに
--     ops/app_privileges.sql の可変テーブル許可リストに足す（理由もそこに1行で残す）。
--     ops/check_app_role.sql の可変一覧と tests/schema.rs が、権限とトリガーの
--     組合せが「可変」側で揃っていることを両方向に検査する。
--   * **1プレイヤー1エントリ**。player_id の unique で二重キューを DB が拒否する
--     （アプリ側の「既にキュー中か」の判定だけに頼らない）。
--   * マッチング範囲は上方向 +95 固定なので**列を持たない**（07 §3。ユーザーが
--     変更できない値を保存すると、後から「保存値と定数のどちらが正か」が分からなくなる）。
--     下方向だけを down_range として持ち、95〜200 を CHECK で強制する。
--   * ロール希望は最大3つ・優先度付き。SQL で扱うので別テーブルにする。
--     同一エントリ内で role も priority もそれぞれ一意（priority の CHECK と
--     unique の組合せで、1エントリの行数は自動的に 3 以下になる）。
--
-- id / timestamp 列に DB 側の default は付けない（既存マイグレーションと同じ方針。
-- 値はアプリケーションが与える）。

-- ----------------------------------------------------------------------
-- queue_entries: キュー中のプレイヤー。1プレイヤー1行。
-- ----------------------------------------------------------------------
create table queue_entries (
    id           uuid        primary key,
    player_id    uuid        not null unique references players (id),
    enqueued_at  timestamptz not null,
    updated_at   timestamptz not null,
    down_range   integer     not null check (down_range between 95 and 200)
);

create index queue_entries_enqueued_at_idx
    on queue_entries (enqueued_at);

comment on table queue_entries is
    '可変の参照データ（追記専用ではない）。キュー中のプレイヤー。マッチ成立・離脱で行を消す。履歴は formations 側に残るので、ここに追記ログを作らない。';
comment on column queue_entries.player_id is
    '1プレイヤー1エントリ。unique が二重キューを DB レベルで拒否する';
comment on column queue_entries.enqueued_at is
    '最初にキューへ入った時刻。待ち時間の算出に使う（マッチメイカは待ち時間の合計が最大になる組を選ぶ。飢餓を防ぐ指標）。設定を変えても更新しない: 更新すると、範囲を広げた人ほど後回しになり、広げる動機を潰す';
comment on column queue_entries.updated_at is
    '希望ロール・範囲を最後に変えた時刻。運用の観察用で、マッチングの入力にはしない';
comment on column queue_entries.down_range is
    '下方向のマッチング範囲（07 §3）。95〜200。上方向は +95 固定でユーザーが変更できないため列にしない';

-- ----------------------------------------------------------------------
-- queue_entry_roles: 希望ロール。最大3つ、優先度付き。
-- ----------------------------------------------------------------------
create table queue_entry_roles (
    entry_id  uuid     not null references queue_entries (id) on delete cascade,
    role      text     not null references roles (id),
    priority  smallint not null check (priority between 1 and 3),
    primary key (entry_id, role),
    unique (entry_id, priority)
);

comment on table queue_entry_roles is
    '可変の参照データ（追記専用ではない）。queue_entries に従属する希望ロール。エントリの削除で cascade する。';
comment on column queue_entry_roles.role is
    'roles.id への参照（不変の識別子）';
comment on column queue_entry_roles.priority is
    '1 が第1希望。1〜3。(entry_id, role) の主キーと (entry_id, priority) の unique により、同一エントリで role も priority も重複せず、行数は自動的に 3 以下になる';
