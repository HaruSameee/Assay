-- 公開プロフィールの材料。**どちらも追記専用で、現在値は最新行から導出する。**
--
-- 既定の権限方針（ops/app_privileges.sql）が select / insert しか与えないので、
-- 可変テーブルの許可リストには足さない。所有者にも効く保護トリガーで二重化する。

-- ----------------------------------------------------------------------
-- player_discord_snapshots: ログインのたびに Discord 側の見た目を積む。
-- ----------------------------------------------------------------------
--
-- 履歴を残すのは、**運営が係争を裁定するときに discord_user_id しか手掛かりが
-- 無い**という問題への対処。表示名は変わるので、当時の名前が残っていないと
-- 「あのとき誰だったか」を後から追えない。
--
-- 現在値の列を作らない。最新行（player_id ごとの id 最大）が現在値。
create table player_discord_snapshots (
    id           bigserial   primary key,
    player_id    uuid        not null references players (id),
    -- Discord の表示名（global_name、無ければ username）。空文字は入れない。
    display_name text        not null check (btrim(display_name) <> ''),
    -- アバターのハッシュ。**未設定なら null**（既定アバターを出す）。
    avatar_hash  text        check (avatar_hash is null or avatar_hash ~ '^[a-zA-Z0-9_]{1,64}$'),
    captured_at  timestamptz not null,
    created_at   timestamptz not null default now()
);

comment on table player_discord_snapshots is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。ログインのたびに Discord 側の表示名とアバターを積む。現在値は最新行から導出する。履歴を残すのは、係争の裁定で「当時この人が何と名乗っていたか」を追えるようにするため。';
comment on column player_discord_snapshots.avatar_hash is
    'Discord のアバターハッシュ。null なら未設定（既定アバターを出す）。CDN の URL はここから組み立てる。';

create index player_discord_snapshots_player_latest_idx
    on player_discord_snapshots (player_id, id desc);

create trigger player_discord_snapshots_forbid_update_delete
    before update or delete on player_discord_snapshots
    for each statement execute function forbid_update_delete();

-- ----------------------------------------------------------------------
-- player_profiles: 本人が編集する項目。編集のたびに 1 行積む。
-- ----------------------------------------------------------------------
--
-- **担当ロールの公開可否も同じ行に持つ。**別テーブルにすると「本文は新しいが
-- 公開可否は古い」という組み合わせが作れてしまい、非公開のつもりが公開される。
create table player_profiles (
    id                bigserial   primary key,
    player_id         uuid        not null references players (id),
    -- 自己紹介。200 文字まで（文字数であってバイト数ではない）。
    bio               text        not null check (char_length(bio) <= 200),
    -- X のユーザー名。`@` は含めない。未設定なら null。
    x_id              text        check (x_id ~ '^[A-Za-z0-9_]{1,15}$'),
    -- 担当ロール。`["jungle", ...]` の配列。未設定なら空配列。
    main_roles        jsonb       not null check (jsonb_typeof(main_roles) = 'array'),
    -- **本人が公開を選んだときだけ true。**既定は false（非公開）。
    main_roles_public boolean     not null,
    edited_at         timestamptz not null,
    created_at        timestamptz not null default now()
);

comment on table player_profiles is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。本人が編集するプロフィール。編集のたびに 1 行積み、現在値は最新行から導出する。';
comment on column player_profiles.x_id is
    'X のユーザー名（@ を含めない）。^[A-Za-z0-9_]{1,15}$ だけを許可する。URL はこの値から組み立てるので、ここを通らない値はアプリにも入らない。';
comment on column player_profiles.main_roles_public is
    '担当ロールを公開するか。**false のとき、公開ビューは main_roles のキー自体を返さない**（空配列を返すと「設定していない」ことが確定情報になり、未設定と非公開が区別できてしまう）。';

create index player_profiles_player_latest_idx
    on player_profiles (player_id, id desc);

create trigger player_profiles_forbid_update_delete
    before update or delete on player_profiles
    for each statement execute function forbid_update_delete();
