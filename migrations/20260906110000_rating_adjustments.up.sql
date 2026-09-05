-- 運営が手動で行うレーティング調整。レーティングそのものは導出値であり、
-- player_ratings のような現在値テーブルを作ったり更新したりしない。
--
-- rating_adjustments は追記専用。アプリ用ロールの既定権限は SELECT / INSERT のみで、
-- ここでは所有者にも効く forbid_update_delete トリガーを追加する。
-- ops/app_privileges.sql の可変テーブル許可リストには加えない。

create table rating_adjustments (
    id              bigserial        primary key,
    player_id       uuid             not null references players (id),
    -- 表示スケール。内部スケールへの変換は rating::DISPLAY_SCALE (60) で行う。
    delta_mu        double precision not null,
    delta_sigma     double precision not null,
    reason          text             not null check (btrim(reason) <> ''),
    created_by      text             not null check (btrim(created_by) <> ''),
    created_at      timestamptz      not null,
    idempotency_key text             not null unique check (btrim(idempotency_key) <> ''),
    reverts_id      bigint           references rating_adjustments (id),
    check (reverts_id is null or reverts_id <> id)
);

create index rating_adjustments_player_id_id_idx
    on rating_adjustments (player_id, id);
-- 打ち消しチェーンは分岐させない。打ち消し自体をさらに打ち消すことはできる。
create unique index rating_adjustments_reverts_uniq
    on rating_adjustments (reverts_id)
    where reverts_id is not null;

comment on table rating_adjustments is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。運営による初期レーティング調整の入力ログ。delta_mu / delta_sigma は表示スケール（mu * 60、初期 mu は 1500、初期 sigma は 500）で保存し、recompute が内部スケールへ変換する。reverts_id は対象と逆符号の delta を持つ打ち消し行への参照。';
comment on column rating_adjustments.delta_mu is
    '表示スケールの mu 差分。recompute は全行を合算してから 60 で割り、内部 mu に変換する';
comment on column rating_adjustments.delta_sigma is
    '表示スケールの sigma 差分。大会実績など確かな強さの情報では負値にして初期 sigma を下げられる';
comment on column rating_adjustments.reason is
    '調整の根拠。空文字・空白だけは不可';
comment on column rating_adjustments.created_by is
    '調整を実行した運営の識別子。空文字・空白だけは不可';
comment on column rating_adjustments.idempotency_key is
    '同じ運営操作の再実行を識別する一意キー。同一キーだけを重複操作として扱う';
comment on column rating_adjustments.reverts_id is
    '打ち消す元の adjustment。打ち消し行の player_id は同じ、delta_mu / delta_sigma は元の逆符号でなければならない';

-- 手動 psql 操作でも、reverts_id と delta の食い違いで過去の recompute を壊せないようにする。
create function rating_adjustment_revert_must_cancel() returns trigger
language plpgsql as $$
declare
    reverted rating_adjustments%rowtype;
begin
    if new.reverts_id is null then
        return new;
    end if;

    select * into reverted from rating_adjustments where id = new.reverts_id;
    if not found then
        raise exception 'rating adjustment % does not exist', new.reverts_id
            using errcode = '23503';
    end if;
    if new.player_id <> reverted.player_id
       or new.delta_mu <> -reverted.delta_mu
       or new.delta_sigma <> -reverted.delta_sigma then
        raise exception 'rating adjustment reversal must negate the reverted adjustment for the same player'
            using errcode = '23514';
    end if;
    return new;
end;
$$;

create trigger rating_adjustments_revert_must_cancel
    before insert on rating_adjustments
    for each row execute function rating_adjustment_revert_must_cancel();
create trigger rating_adjustments_forbid_update_delete
    before update or delete on rating_adjustments
    for each statement execute function forbid_update_delete();
