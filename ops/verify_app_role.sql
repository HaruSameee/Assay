-- Run with assay_app's credentials; all probe rows/changes are rolled back.
\set ON_ERROR_STOP on
begin;
do $$
begin
    if current_user <> 'assay_app' then
        raise exception 'verification must connect as assay_app';
    end if;
    begin
        update formations set id = id;
        raise exception 'UPDATE unexpectedly succeeded';
    exception when insufficient_privilege then
        raise notice 'formations UPDATE rejected with SQLSTATE %', sqlstate;
    end;
    begin
        delete from formations;
        raise exception 'DELETE unexpectedly succeeded';
    exception when insufficient_privilege then
        raise notice 'formations DELETE rejected with SQLSTATE %', sqlstate;
    end;
end
$$;
insert into formations (id, created_at, matchmaking_params_version, predicted_win_prob)
values (gen_random_uuid(), now(), 1, 0.5);
select count(*) from formations;
insert into players (id, discord_user_id, created_at)
values (gen_random_uuid(), -1, now());
update players set created_at = now() where discord_user_id = -1;
update roles set display_name = display_name;
rollback;
