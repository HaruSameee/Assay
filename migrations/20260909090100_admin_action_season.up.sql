-- 運営操作ログに season_started を足す（管理パネルからのシーズン開始）。
--
-- 既存のマイグレーションは書き直さない。CHECK を張り替える形で積む。
-- season_started は特定のプレイヤーに紐づかないので、target_player_id は null 側
-- （admin_actions_target_matches_kind の else 分岐）に入る。
alter table admin_actions drop constraint admin_actions_kind_check;
alter table admin_actions add constraint admin_actions_kind_check
    check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set',
        'season_started'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started';
