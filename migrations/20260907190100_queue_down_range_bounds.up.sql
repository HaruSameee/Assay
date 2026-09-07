-- 下方向のマッチング範囲を 95〜200 から 30〜100 へ（07 §3）。
--
-- 既存のマイグレーションは書き直さず、新規ファイルとして積む（本番適用済みのため）。
--
-- 変更の意味:
--   * これまで: デフォルト −95（＝最も狭い）、−200 まで**広げられる**だけ
--   * これから: デフォルト −95、−30 から −100 の範囲で**狭くも広くも**できる
--
-- 上方向は +95 固定のまま（今回は変えない）。**その帰結として、−96〜−100 は決して
-- binding にならない**: 両立条件は `max − min ≤ 95` かつ 各人 `rating_i − min ≤ d_i` で、
-- 前者が既に 95 で頭を押さえているため、d_i が 95 より広くても候補は増えない。
-- これは承知のうえの暫定状態であり、バグではない（上方向を可変にする改修が入るまで）。
--
-- 既存行の扱い:
--   **一括 UPDATE をしない。**旧範囲で入った −96〜−200 の行が残りうるので、新しい CHECK は
--   `not valid` で足す。PostgreSQL は `not valid` の CHECK も INSERT / UPDATE には効かせるので、
--   これ以降に入る値と、設定を変更した行はすべて 30〜100 に制限される。既存行はそのまま残り、
--   アプリが読み出し時に 100 へクランプする（`queue::model::clamp_down_range`）。
--   ここで validate すると、旧範囲の行を持つ本番で ALTER が失敗する。

alter table queue_entries drop constraint queue_entries_down_range_check;
alter table queue_entries add constraint queue_entries_down_range_check
    check (down_range between 30 and 100) not valid;

comment on column queue_entries.down_range is
    '下方向のマッチング範囲（07 §3）。30〜100。上方向は +95 固定でユーザーが変更できないため列にしない。旧範囲（95〜200）で入った行が残りうるので、CHECK は not valid で足してある。読み出し側は queue::model::clamp_down_range で 100 に丸める';
