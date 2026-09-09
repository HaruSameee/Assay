-- 運営操作ログに report_rejected を足す（運営の報告入力を断った記録）。
--
-- 既存のマイグレーションは書き直さない。CHECK を張り替える形で積む。
--
-- **断ったこと自体を記録に残す。**係争中・確定済みの試合に運営が報告を入れようと
-- したとき、`match_reports` には 1 行も積まないが、試みがあった事実は残す
-- （画面から入口を外しても直接 POST は届くので、黙って捨てない）。
-- 対象プレイヤーが決まる操作なので、`target_player_id` は必須側に入る。
alter table admin_actions drop constraint admin_actions_kind_check;
alter table admin_actions add constraint admin_actions_kind_check
    check (kind in (
        'matchmaker_paused',
        'matchmaker_resumed',
        'queue_removed',
        'block_allowance_set',
        'season_started',
        'report_entered',
        'report_rejected'));

alter table admin_actions drop constraint admin_actions_target_matches_kind;
alter table admin_actions add constraint admin_actions_target_matches_kind
    check (
        case kind
            when 'queue_removed' then target_player_id is not null
            when 'block_allowance_set' then target_player_id is not null
            when 'report_entered' then target_player_id is not null
            when 'report_rejected' then target_player_id is not null
            else target_player_id is null
        end
    );

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / report_entered / report_rejected';
