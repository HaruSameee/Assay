-- 直前の方針（打ち消し行の再打消しを許可）へ戻す。

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
    '打ち消す元の adjustment。打ち消し行の player_id は同じ、delta_mu / delta_sigma は元の逆符号でなければならない';
