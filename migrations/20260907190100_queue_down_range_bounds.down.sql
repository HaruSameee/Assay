-- 旧範囲（95〜200）へ戻す。30〜94 の行が残りうるので not valid で足す。
alter table queue_entries drop constraint queue_entries_down_range_check;
alter table queue_entries add constraint queue_entries_down_range_check
    check (down_range between 95 and 200) not valid;

comment on column queue_entries.down_range is
    '下方向のマッチング範囲（07 §3）。95〜200。上方向は +95 固定でユーザーが変更できないため列にしない';
