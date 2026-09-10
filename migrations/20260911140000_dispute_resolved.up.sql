-- 係争（disputed）を運営が裁定できるようにする。**追記のみ。**
--
-- これまで disputed の解決経路はどこにも無かった。運営チャンネルへ通知が飛ぶだけで、
-- 画面にも Discord にも結果を反映する手段が無く、係争中の試合は永久に残っていた。
-- 残ると:
--   * その試合で未報告のプレイヤーが**報告ゲートで永久にキューへ入れない**
--     （`requires_reports()` に disputed が入っている）
--   * シーズン境界を引くと、後から裁定しても新シーズンのレートに入らない
--     （境界は `formations.created_at` で切るため）
--
-- **新しいテーブルは作らない。**裁定は formation_events に終端イベント
-- （confirmed / nullified / voided）を 1 行積むだけで表す。状態機械が
-- `Disputed -> Confirmed | Nullified | Voided` を許しているので、通常の
-- 追記経路（append_event）にそのまま乗る。ここで足すのは運営操作ログの種別だけ。

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
        'dispute_resolved'));

comment on column admin_actions.kind is
    'matchmaker_paused / matchmaker_resumed / queue_removed / block_allowance_set / season_started / block_override_removed / report_entered / report_rejected / rating_params_changed / rating_params_rejected / matching_range_changed / dispute_resolved';
