-- 独立して追加された運営操作の種別を、最終状態として統合する。
--
-- `block_override_removed` と運営報告の種別は別ブランチで追加されたため、
-- 先行マイグレーションがそれぞれ CHECK を張り替えるだけでは、後から適用された方が
-- もう片方の種別を除外してしまう。既存マイグレーションは書き換えず、この追従
-- マイグレーションで全種別を許可する。
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
        'report_rejected'));

alter table admin_actions drop constraint admin_actions_target_matches_kind;
alter table admin_actions add constraint admin_actions_target_matches_kind
    check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            when 'block_override_removed' then target_player_id is not null
            when 'report_entered' then target_player_id is not null
            when 'report_rejected' then target_player_id is not null
            else target_player_id is null
        end
    );

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected';
