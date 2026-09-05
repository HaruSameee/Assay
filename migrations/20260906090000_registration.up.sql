-- 登録・認証トラック（05-登録トラック仕様 §2）。
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
-- 不変条件:
--   * players は可変の参照データ。追記専用4テーブルには引き続き含めない。
--     トレーナー名の変更も BAN も通常の UPDATE でよい（ops/app_privileges.sql の可変一覧のまま）。
--   * 申告レートは可変列に置かない。可変列に置いて上書きすると、レート seed が
--     レートの導出経路の一部である以上、過去の recompute が再現しなくなる。
--     申告は player_declarations への追記で表現し、現在値は最新の行から導出する。
--   * player_declarations は追記専用。新テーブルの既定権限は SELECT / INSERT だけなので
--     （ops/app_privileges.sql）、ここでは所有者にも効く forbid_update_delete トリガーを付ける。
--     tests/schema.rs が権限とトリガーの両方を検査する。

-- ----------------------------------------------------------------------
-- players: 登録の受け皿。列を足すだけで、可変である性質は変えない。
-- ----------------------------------------------------------------------
alter table players add column trainer_name  text;
alter table players add column status        text not null default 'active'
    check (status in ('active', 'suspended'));
alter table players add column registered_at timestamptz;

comment on column players.trainer_name is
    'ゲーム内のトレーナー名（自己申告）。他の参加者から見える前提で告知する。変更してよい';
comment on column players.status is
    'active または suspended。suspended はキューの登録ゲートで弾く。変更してよい';
comment on column players.registered_at is
    '登録フォームを最初に送信した時刻。NULL は「ログインしたが未登録」。この列の有無が登録ゲートの判定';

-- ----------------------------------------------------------------------
-- player_declarations: 本家ランク・レートの自己申告ログ。追記のみ。
-- 申告し直しは新しい行で表現し、seed は「その時点で最新の申告」から導出する。
-- ----------------------------------------------------------------------
create table player_declarations (
    id           bigserial   primary key,
    player_id    uuid        not null references players (id),
    declared_at  timestamptz not null,
    rank         text        not null,
    rating       integer,
    raw          jsonb       not null
);

create index player_declarations_player_idx
    on player_declarations (player_id, id);

comment on table player_declarations is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。本家ランク・レートの自己申告。申告し直しは新しい行で表現する。現在の申告値は (player_id, id) の最新行から導出する。レート seed の入力なので、可変列に置き換えてはならない（過去の recompute が再現しなくなる）';
comment on column player_declarations.rank is
    '申告された本家ランクの識別子（beginner / super / hyper / elite / expert / master / legend）。表示名ではなく不変の識別子を入れる。CHECK は付けない: 本家のランク体系が変わったときにマイグレーションを要求しないため';
comment on column player_declarations.rating is
    '申告された本家レート。マスター / レジェンドのときのみ。それ以外は NULL';
comment on column player_declarations.raw is
    '送信されたフォームの生データ。パースや正規化に失敗しても、受け取ったものをそのまま保存する（外部 API が無いので、書かなかったデータは永久に取得不能）';

create trigger player_declarations_forbid_update_delete
    before update or delete on player_declarations
    for each statement execute function forbid_update_delete();
