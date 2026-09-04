-- テスト用ギルドで Phase 4 を手で通すための参照データ（開発用。本番では使わない）。
--
-- roles の 5 行はマイグレーションが入れるので、ここでは players だけ。
-- players は可変の参照データなので、何度実行しても差し支えない（on conflict do nothing）。
--
-- 使い方: 下の discord_user_id を手動テストに参加する実際のユーザーに差し替えて
--   psql "$DATABASE_URL" -f ops/dev_seed.sql
-- 1 人で 10 枠を試すことはできない（formation_players の主キーは (formation_id, player_id)）。
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
