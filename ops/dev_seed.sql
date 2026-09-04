-- テスト用ギルドで Phase 4 を手で通すための参照データ（開発用。本番では使わない）。
--
-- roles は本番では初期データを持たない（マイグレーションの方針）。手動テストでは
-- Phase 1〜3 で確定した 5 ロールを入れる。players は手動テストに参加する Discord ユーザーを入れる。
-- players / roles は可変の参照データなので、何度実行しても差し支えない（on conflict do nothing）。
--
-- 使い方:
--   psql "$DATABASE_URL" -v u1=<discord user id> ... -f ops/dev_seed.sql
--   （または下の values を書き換えて実行する）

insert into roles (id, display_name, sort_order) values
    ('top_carry',   'top carry',   1),
    ('top_support', 'top support', 2),
    ('jungle',      'jungle',      3),
    ('bot_carry',   'bot carry',   4),
    ('bot_tank',    'bot tank',    5)
on conflict (id) do nothing;

-- 参加者。discord_user_id を実際のユーザーに差し替える。1 人で 10 枠を試すことはできない
-- （formation_players の主キーは (formation_id, player_id)。同じ人は 1 formation に 1 回だけ）。
insert into players (id, discord_user_id, created_at) values
    (gen_random_uuid(), 100000000000000001, now()),
    (gen_random_uuid(), 100000000000000002, now()),
    (gen_random_uuid(), 100000000000000003, now()),
    (gen_random_uuid(), 100000000000000004, now()),
    (gen_random_uuid(), 100000000000000005, now()),
    (gen_random_uuid(), 100000000000000006, now()),
    (gen_random_uuid(), 100000000000000007, now()),
    (gen_random_uuid(), 100000000000000008, now()),
    (gen_random_uuid(), 100000000000000009, now()),
    (gen_random_uuid(), 100000000000000010, now())
on conflict (discord_user_id) do nothing;
