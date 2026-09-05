-- Preserve the existing statement-level triggers and their function dependency.
alter function reject_mutation() rename to forbid_update_delete;
create or replace function forbid_update_delete() returns trigger
language plpgsql as $$
begin
    raise exception 'append-only table: % on % is not allowed', tg_op, tg_table_name
        using errcode = '0A000';
end;
$$;
