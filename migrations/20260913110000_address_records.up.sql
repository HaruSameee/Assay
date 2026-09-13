-- 特定のプレイヤーの接続元（IP・User-Agent）を記録する。**追記専用。**
--
-- 迷惑行為のサブアカウントを特定するため。**全員は記録しない。**記録するのは
-- 環境変数 IP_WATCH_DISCORD_USER_IDS に並べた Discord ユーザー ID の人だけで、
-- 対象でなければこの表には 1 行も入らない（アプリ側で判定する。この表に
-- 「誰を監視するか」は持たない: 環境変数だけを変えれば記録が始まる / 止まる）。
--
-- 記録する契機はログイン（OAuth コールバック）とキュー投入。セッションは 14 日
-- 持つのでログインだけでは疎になるため、開催日ごとに必ず通るキュー投入でも取る。
--
-- ヘッダは**生のまま**残す。サイトは Cloudflare 経由なので、本当の接続元は
-- CF-Connecting-IP にあり、X-Forwarded-For は「本人, Cloudflare」の並びになる。
-- どちらか片方に潰すと後から検証できないので両方持つ。
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する
--   * 運営パネル（/admin/player）だけが読む。プレイヤー向けの経路からは読まない

create table player_address_records (
    id                bigserial   primary key,
    player_id         uuid        not null references players (id),
    -- 何の契機で取ったか
    source            text        not null check (source in ('login', 'queue')),
    -- Cloudflare が付ける本当の接続元。Cloudflare を経由していなければ null
    cf_connecting_ip  text,
    -- プロキシの並び（生のまま）
    x_forwarded_for   text,
    user_agent        text,
    seen_at           timestamptz not null
);

comment on table player_address_records is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。IP_WATCH_DISCORD_USER_IDS に並べたプレイヤーだけの接続元の記録（ログイン・キュー投入）。運営パネルだけが読む';
comment on column player_address_records.cf_connecting_ip is
    'Cloudflare が付ける本当の接続元（CF-Connecting-IP）。Cloudflare を経由していなければ null';
comment on column player_address_records.x_forwarded_for is
    'X-Forwarded-For を生のまま。Cloudflare 経由なら「本人, Cloudflare」の並び';

create index player_address_records_player_id_id_idx
    on player_address_records (player_id, id desc);

create trigger player_address_records_forbid_update_delete
    before update or delete on player_address_records
    for each statement execute function forbid_update_delete();
