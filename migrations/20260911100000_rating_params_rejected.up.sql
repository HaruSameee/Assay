-- 運営操作ログに rating_params_rejected を足す（増減幅の変更を断った記録）。
--
-- 既存のマイグレーションは書き直さない。CHECK を張り替える形で積む。
--
-- **断ったこと自体を記録に残す。**進行中の formation があるときに増減幅を変えようと
-- したら拒否するが、試みがあった事実は残す（画面から入口を塞いでも直接 POST は届くし、
-- 「なぜあのとき変えられなかったか」を後から追えるようにしておく）。
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
        'rating_params_rejected'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected';
