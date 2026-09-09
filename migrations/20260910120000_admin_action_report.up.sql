-- 運営操作ログに report_entered を足す（未報告のプレイヤーに運営が報告を入れた）。
--
-- 既存のマイグレーションは書き直さない。CHECK を張り替える形で積む。
--
-- **報告そのものは `match_reports` に追記する。**この行は「誰がいつ入れたか」の記録で、
-- 勝敗の集計には使わない（集計は従来どおり各プレイヤーの最新報告を読む）。
-- 対象プレイヤーが決まる操作なので、`target_player_id` は必須側に入る。
alter table admin_actions drop constraint admin_actions_kind_check;
alter table admin_actions add constraint admin_actions_kind_check
    check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set',
        'season_started',
        'report_entered'));

alter table admin_actions drop constraint admin_actions_target_matches_kind;
alter table admin_actions add constraint admin_actions_target_matches_kind
    check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            when 'report_entered' then target_player_id is not null
            else target_player_id is null
        end
    );

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / report_entered';
