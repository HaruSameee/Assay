-- マッチング優先度の付与（運営操作）。**追記専用。**
--
-- マッチメイカは「いちばん長く待っている人」から基準を取る（07 §4-3）。運営が
-- 特定の人を次の試合で先に組みたいとき（長く取り残された人、障害で流れた人）は、
-- **待ち時間に加算する秒数**を付与する。実際の待ち時間（画面の表示）は変えない。
--
-- 付与は players 単位で持つ（キューの行に持つと、離脱して入り直した時点で消える）。
-- **効くのは「付与より後に成立した試合が無い」あいだ**: 付与後にその人の formation が
-- 1 つできれば、以降は数えない（1 回ぶんの優先。消費の印は書かず、formations から導出する）。
-- 同じ人に複数付与すれば合算する。
--
-- 不変条件:
--   * 追記専用。UPDATE / DELETE 権限を与えない。forbid_update_delete で二重化する
--   * 状態列を持たない。「まだ効いているか」は formation_players / formations から導出する

create table queue_priority_grants (
    id              bigserial   primary key,
    player_id       uuid        not null references players (id),
    -- 待ち時間に加算する秒数。1 分〜3 時間
    bonus_seconds   integer     not null check (bonus_seconds between 60 and 10800),
    reason          text        not null check (btrim(reason) <> ''),
    created_by      text        not null check (btrim(created_by) <> ''),
    idempotency_key text        not null unique check (btrim(idempotency_key) <> ''),
    created_at      timestamptz not null
);

comment on table queue_priority_grants is
    '追記専用: UPDATE / DELETE 禁止（forbid_update_delete トリガーで拒否）。運営が付与したマッチング優先度（待ち時間への加算秒）。付与より後にその人の formation が無いあいだだけ効く（1 回ぶん）。同じ人への複数付与は合算';
comment on column queue_priority_grants.bonus_seconds is
    '待ち時間に加算する秒数（マッチメイカの基準順と最小費用流の費用にだけ効く。画面の待ち時間は変えない）';

create index queue_priority_grants_player_id_id_desc_idx
    on queue_priority_grants (player_id, id desc);

create trigger queue_priority_grants_forbid_update_delete
    before update or delete on queue_priority_grants
    for each statement execute function forbid_update_delete();

-- ----------------------------------------------------------------------
-- 運営操作ログに queue_priority_raised を足す（対象プレイヤー必須）。
-- ----------------------------------------------------------------------
alter table admin_actions drop constraint admin_actions_kind_check;
alter table admin_actions add constraint admin_actions_kind_check
    check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set',
        'season_started',
        'block_override_removed',
        'report_entered',
        'report_rejected',
        'rating_params_changed',
        'rating_params_rejected',
        'matching_range_changed',
        'dispute_resolved',
        'matchmaker_scheduled',
        'match_readjudicated',
        'block_cooldown_changed',
        'ip_banned',
        'ip_unbanned',
        'shadow_banned',
        'shadow_ban_lifted',
        'queue_priority_raised'));

alter table admin_actions drop constraint admin_actions_target_matches_kind;
alter table admin_actions add constraint admin_actions_target_matches_kind
    check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            when 'block_override_removed' then target_player_id is not null
            when 'report_entered' then target_player_id is not null
            when 'report_rejected' then target_player_id is not null
            when 'queue_priority_raised' then target_player_id is not null
            else target_player_id is null
        end
    );

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved / matchmaker_scheduled / match_readjudicated / block_cooldown_changed / ip_banned / ip_unbanned / shadow_banned / shadow_ban_lifted / queue_priority_raised';
