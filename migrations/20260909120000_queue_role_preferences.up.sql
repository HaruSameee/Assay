-- キューのロール希望を追記して残す。**次にキュー画面を開いたときのプリフィル用。**
--
-- `queue_entries` / `queue_entry_roles` は「いまキューに居るか」を表す可変の状態で、
-- 離脱すると消える。そこに履歴は残らないので、選んだ内容は別に追記する
-- （登録フォームのプリフィルが `player_declarations` の最新行から導出するのと同じ形）。
--
-- 追記専用。現在値の列は持たず、**最新行から導出する**。
-- 既定の権限方針（ops/app_privileges.sql）が select / insert しか与えないので、
-- 可変テーブルの許可リストには足さない。所有者にも効く保護トリガーで二重化する。
create table queue_role_preferences (
    id         bigserial   primary key,
    player_id  uuid        not null references players (id),
    chosen_at  timestamptz not null,
    -- 選んだ希望そのもの。`[{"role": "jungle", "priority": 1}, ...]` の配列。
    -- roles への外部キーは張らない: ロールの識別子が将来変わっても、
    -- **過去に何を選んだか**の記録は書き換えない（読む側が知らない値を捨てる）。
    roles      jsonb       not null check (jsonb_typeof(roles) = 'array'),
    created_at timestamptz not null default now()
);

comment on table queue_role_preferences is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。キュー投入時のロール希望の記録。次回のキュー画面のプリフィルに使い、現在値は最新行（player_id ごとの id 最大）から導出する。マッチングは queue_entry_roles を読むので、この表は表示にしか使わない。';
comment on column queue_role_preferences.chosen_at is
    'プレイヤーが選んだ時刻（キューに入った、または設定を更新した時刻）。';
comment on column queue_role_preferences.roles is
    '[{"role": "<roles.id>", "priority": 1..3}] の配列。読む側が知らないロールは捨てる。';

-- player_id ごとの最新行を1行だけ引くための索引。
create index queue_role_preferences_player_latest_idx
    on queue_role_preferences (player_id, id desc);

create trigger queue_role_preferences_forbid_update_delete
    before update or delete on queue_role_preferences
    for each statement execute function forbid_update_delete();
