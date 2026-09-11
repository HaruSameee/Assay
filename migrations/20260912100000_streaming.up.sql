-- 配信中かどうかの申告。**キューに入るときに本人が付け、成立時に formation へ写す。**
--
-- 配信は一律で許可されているが、「していること」は明示すべきという運用の判断。
-- キュー画面のチェックボックス 1 つで申告し、マッチ画面で参加者全員（相手チームを含む）に
-- 「配信中」として表示する。
--
-- 不変条件:
--   * `queue_entries.streaming` は可変の状態（キュー設定の 1 つ。離脱で消える）。
--   * `formation_players.streaming` は成立時点のスナップショット。追記専用の表に
--     列を足すだけで、UPDATE / DELETE 禁止はそのまま。
--   * レートにも試合の判定にも一切関与しない（表示のためだけの値）。

alter table queue_entries
    add column streaming boolean not null default false;

comment on column queue_entries.streaming is
    '配信中として申告しているか。キュー画面のチェックボックス。成立時に formation_players.streaming へ写す';

alter table formation_players
    add column streaming boolean not null default false;

comment on column formation_players.streaming is
    '成立時点で配信中と申告していたか（queue_entries.streaming のスナップショット）。マッチ画面で全員に表示する。導出には関与しない';
