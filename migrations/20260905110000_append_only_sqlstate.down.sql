create or replace function forbid_update_delete() returns trigger
language plpgsql as $$
begin
    raise exception 'append-only table: % on % is not allowed', tg_op, tg_table_name;
end;
$$;
alter function forbid_update_delete() rename to reject_mutation;
