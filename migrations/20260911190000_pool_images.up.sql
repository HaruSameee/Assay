-- キャラプールの画像。**1 人 1 枚で、上書きできる。**
--
-- プロフィールの担当ロールに添える画像で、アップロード（PNG）か画像 URL の
-- どちらかを持つ。
--
-- **この表は追記専用にしない。**`player_profiles` は追記専用だが、そこに画像を
-- 入れると自己紹介を 1 文字直すたびに 3MB が再保存される。専用の表にして
-- 1 人 1 行に固定し、貼り直しは置き換え（UPSERT）にする。
-- 扱いは `players.trainer_name` と同じ**可変の参照データ**で、試合のログでも
-- レートの入力でもない。消えても導出は 1 つも変わらない。
--
-- 容量の上限が読めることを優先した: 3MB × 参加者数。**DB に置くので
-- pg_dump にそのまま入り、バックアップの手順を増やさずに済む。**
create table player_pool_images (
    player_id  uuid        primary key references players (id),
    -- アップロードされた PNG。URL を貼った場合は null
    image      bytea,
    -- 貼り付けられた画像 URL。アップロードした場合は null
    image_url  text,
    updated_at timestamptz not null,
    -- **両方入れることはできない。**どちらを出すかが決まらなくなる。
    -- 両方 null は「設定していない」で、消したときの形
    check (image is null or image_url is null),
    -- 3MB。アプリ側でも弾くが、経路を増やしても破れないようにここでも縛る
    check (image is null or octet_length(image) <= 3 * 1024 * 1024),
    check (image_url is null or (btrim(image_url) <> '' and length(image_url) <= 2048))
);

comment on table player_pool_images is
    'キャラプールの画像。1 人 1 行で、貼り直しは置き換え（追記専用ではない）。可変の参照データで、試合のログでもレートの入力でもない。image と image_url は排他で、両方 null は未設定';
comment on column player_pool_images.image is
    'アップロードされた PNG のバイト列。3MB まで（CHECK とアプリの両方で縛る）。URL を貼った場合は null';
comment on column player_pool_images.image_url is
    '貼り付けられた画像 URL（https のみ。検証はアプリ側）。アップロードした場合は null';
