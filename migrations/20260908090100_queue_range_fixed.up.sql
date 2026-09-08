-- マッチング範囲を上下とも 50 固定にする（07 §3）。
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
--   * これまで: 上方向 +95 固定、下方向は 30〜100 でユーザーが調整
--   * これから: **上下とも 50 固定。ユーザーは調整できない**
--
-- 群の幅（max − min）が 50 になる。seed の差 100（1500 / 1600）と組み合わせると、
-- 2 つの群は初期状態で分離する。境界の差ちょうど 50 は**組める側**（比較は `<=`）。
--
-- `queue_entries.down_range` の列は残す。列を落とすと本番のロールバック手段が減り、
-- 過去にどの幅で入っていたかも消えるため。**新しい行には必ず 50 を書く**
-- （`queue::model::DOWN_RANGE`）。上方向は元から列を持たない。
--
-- 既存行の扱い:
--   **一括 UPDATE をしない。**旧仕様（95〜200 / 30〜100）の行が残りうるので、新しい CHECK は
--   `not valid` で足す。PostgreSQL は `not valid` の CHECK も INSERT / UPDATE には効かせるので、
--   これ以降に入る値と、設定を変更した行はすべて 50 に制限される。既存行はそのまま残り、
--   アプリが読み出し時に 50 へ丸める（`queue::model::clamp_down_range`）。

alter table queue_entries drop constraint queue_entries_down_range_check;
alter table queue_entries add constraint queue_entries_down_range_check
    check (down_range = 50) not valid;

comment on column queue_entries.down_range is
    '下方向のマッチング範囲（07 §3）。50 固定。上方向も 50 固定だが、ユーザーが変更できないため列にしない。旧仕様（95〜200 / 30〜100）で入った行が残りうるので、CHECK は not valid で足してある。読み出し側は queue::model::clamp_down_range で 50 に丸める';
