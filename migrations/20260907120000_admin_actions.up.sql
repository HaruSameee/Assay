-- 運営の当日操作ログ（マッチメイカの停止・再開 / キューからの強制離脱 / ブロック枠の変更）。
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
-- 不変条件と、それをここでどう守るか:
--   * **追記専用**。UPDATE / DELETE を書かない。既定権限（SELECT / INSERT）のままにし、
--     ops/app_privileges.sql の可変テーブル許可リストには足さない。所有者にも効く
--     forbid_update_delete トリガーで二重化する。tests/schema.rs が権限とトリガーの
--     両方を検査する。
--   * **現在の状態を列に持たない。**「マッチメイカが停止中か」「ブロック枠が何枠か」は
--     この表の最新行から導出する（formation_events と同じ設計）。停止フラグを可変列に
--     すると、誰がいつ止めたかが上書きで消え、「止めたまま忘れる」を追跡できなくなる。
--   * **停止状態はプロセスの外に置く。**メモリに持つと再起動で勝手に再開する。
--   * 冪等は「同一 idempotency_key のみ no-op」（00 §5）。状態ベースの冪等にしない。
--   * kind は CHECK で閉じる。導出側は知らない kind を無視できるが、書き手はこの4種類だけ。
--
-- id / timestamp に DB 側の default は付けない（既存マイグレーションと同じ方針。
-- 値はアプリケーションが与える）。

create table admin_actions (
    id                    bigserial   primary key,
    kind                  text        not null check (kind in (
                                          'matchmaker_paused',
                                          'matchmaker_resumed',
                                          'queue_removed',
                                          'block_allowance_set')),
    actor_discord_user_id bigint      not null,
    target_player_id      uuid        references players (id),
    reason                text        check (reason is null or btrim(reason) <> ''),
    payload               jsonb       not null default '{}'::jsonb
                                        check (jsonb_typeof(payload) = 'object'),
    occurred_at           timestamptz not null,
    idempotency_key       text        not null unique check (btrim(idempotency_key) <> ''),
    -- 対象者の要否は kind が決める。プレイヤー向けの操作は必ず対象を持ち、
    -- マッチメイカの停止・再開は誰か1人に紐づかない
    constraint admin_actions_target_matches_kind check (
        case kind
            when 'queue_removed'       then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            else target_player_id is null
        end
    ),
    -- ブロック枠は payload の値が正。0〜99 の整数だけを受ける（桁数の打ち間違いを DB で弾く）
    constraint admin_actions_block_allowance_shape check (
        kind <> 'block_allowance_set'
        or (jsonb_typeof(payload->'allowance') = 'number'
            and (payload->>'allowance') ~ '^(0|[1-9][0-9]?)$')
    )
);

comment on table admin_actions is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。運営の当日操作ログ。マッチメイカの停止状態とブロック枠の現在値は、この表の最新行から導出する（状態列を作らない）';
comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set';
comment on column admin_actions.actor_discord_user_id is
    '操作した運営の Discord ID。ADMIN_DISCORD_USER_IDS との照合を通ったセッションから自動で埋める（フォームからは受け取らない）';
comment on column admin_actions.target_player_id is
    '操作の対象プレイヤー。マッチメイカの停止・再開では NULL';
comment on column admin_actions.reason is
    '運営が書いた理由。queue_removed では本人のキュー画面にそのまま表示する（任意）';
comment on column admin_actions.payload is
    '秘密値を含めない構造化コンテキスト。block_allowance_set は {"allowance": N}';

-- 「最新の1行」を引く索引。停止状態はマッチメイカの実行ごとに読む
create index admin_actions_kind_id_idx
    on admin_actions (kind, id desc);
create index admin_actions_target_kind_id_idx
    on admin_actions (target_player_id, kind, id desc);

create trigger admin_actions_forbid_update_delete
    before update or delete on admin_actions
    for each statement execute function forbid_update_delete();
