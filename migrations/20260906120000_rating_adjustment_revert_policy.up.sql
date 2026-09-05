-- 打ち消しをさらに打ち消すと、追記列だけから有効な調整を追う運用が複雑になる。
-- 個別調整の訂正は元の行を打ち消した上で、新しい独立した調整として追記する。
-- 既存行を変更しないため、この方針変更はトリガーの検証だけを置き換える。

create or replace function rating_adjustment_revert_must_cancel() returns trigger
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
    if reverted.reverts_id is not null then
        raise exception 'rating adjustment reversal cannot itself be reversed'
            using errcode = '23514';
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

comment on column rating_adjustments.reverts_id is
    '打ち消す元の adjustment。元は打ち消し行ではなく、player_id は同じ、delta_mu / delta_sigma は元の逆符号でなければならない。一つの元を打ち消せる行は一つだけ。';
