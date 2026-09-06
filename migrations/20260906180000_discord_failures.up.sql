-- Discord API 呼び出し失敗の横断監査ログ。
--
-- formation に紐づく既存の記録の正は formation_events.payload のまま維持する。
-- この表はそれらを検索可能に索引化し、formation が存在しないマッチメイカの
-- 15 分サマリ投稿失敗も同じ場所へ追記する。

create table discord_failures (
    id                  bigserial   primary key,
    formation_id        uuid        references formations (id),
    formation_event_id  bigint      references formation_events (id),
    source_index        integer,
    kind                text        not null check (btrim(kind) <> ''),
    operation           text        not null check (btrim(operation) <> ''),
    error               text        not null check (btrim(error) <> ''),
    attempts            integer     not null check (attempts > 0),
    context             jsonb       not null default '{}'::jsonb
                                      check (jsonb_typeof(context) = 'object'),
    occurred_at         timestamptz not null,
    check (
        (formation_event_id is null and source_index is null)
        or (formation_event_id is not null and source_index is not null and source_index >= 0)
    ),
    unique (formation_event_id, source_index)
);

comment on table discord_failures is
    '追記専用: Discord API 呼び出し失敗の横断監査ログ。formation の副作用と失敗の正は formation_events.payload にも残し、formation を持たない運用ログ投稿の失敗もここへ集約する';
comment on column discord_failures.kind is
    '運用上の種別。summon_notification / assignment_table / host_prompt / matchmaker_run_log / matchmaker_summary など';
comment on column discord_failures.operation is
    '実際に失敗した Discord 操作。send:summon / edit:assignment:red など';
comment on column discord_failures.context is
    '秘密値を含めない構造化コンテキスト。マッチメイカ実行時刻や formation ID 一覧など';

create index discord_failures_kind_occurred_at_idx
    on discord_failures (kind, occurred_at desc);

create trigger discord_failures_forbid_update_delete
    before update or delete on discord_failures
    for each statement execute function forbid_update_delete();

-- 既存の formation event payload に記録済みの失敗も、同じ検索先へ移す。
insert into discord_failures (
    formation_id,
    formation_event_id,
    source_index,
    kind,
    operation,
    error,
    attempts,
    context,
    occurred_at
)
select
    event.formation_id,
    event.id,
    failure.ordinality::integer - 1,
    case
        when failure.value->>'operation' like '%summon%' then 'summon_notification'
        when failure.value->>'operation' like '%assignment%' then 'assignment_table'
        when failure.value->>'operation' like '%host_prompt%' then 'host_prompt'
        when failure.value->>'operation' like '%code%' then 'room_code'
        when failure.value->>'operation' like '%report%' then 'report_prompt'
        when failure.value->>'operation' like '%dispute%' then 'dispute_notification'
        when failure.value->>'operation' like 'respond:%' then 'interaction_response'
        else 'formation_discord_call'
    end,
    failure.value->>'operation',
    failure.value->>'error',
    greatest(coalesce((failure.value->>'attempts')::integer, 1), 1),
    jsonb_build_object('source', 'formation_event_payload'),
    event.occurred_at
from formation_events event
cross join lateral jsonb_array_elements(
    case
        when jsonb_typeof(event.payload->'discord_failures') = 'array'
            then event.payload->'discord_failures'
        else '[]'::jsonb
    end
) with ordinality as failure(value, ordinality)
where nullif(btrim(failure.value->>'operation'), '') is not null
  and nullif(btrim(failure.value->>'error'), '') is not null;
