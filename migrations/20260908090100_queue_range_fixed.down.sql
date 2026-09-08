-- 旧範囲（30〜100）へ戻す。50 以外の行が無くても not valid で足す（対称にしておく）。
alter table queue_entries drop constraint queue_entries_down_range_check;
alter table queue_entries add constraint queue_entries_down_range_check
    check (down_range between 30 and 100) not valid;

comment on column queue_entries.down_range is
    '下方向のマッチング範囲（07 §3）。30〜100。上方向は +95 固定でユーザーが変更できないため列にしない。旧範囲（95〜200）で入った行が残りうるので、CHECK は not valid で足してある。読み出し側は queue::model::clamp_down_range で 100 に丸める';
