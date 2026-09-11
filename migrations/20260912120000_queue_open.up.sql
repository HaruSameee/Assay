-- キューの受付開始時刻をスケジュールに足す。**規定は開催日の 20:45（JST）。**
--
-- これまでキューにはいつでも入れた。稼働時間外に入った人は、画面で「止まっている」
-- と告げられたまま待つだけになり、前日から入りっぱなしの行も残る。受付を
-- 「稼働開始の少し前から」に絞り、開催日の夜に集まる形にする。
--
-- 列は **絶対時刻（JST の 0 時からの分）** で持つ。運営は「20:45 から」と言うので、
-- 「開始の何分前」で持つと画面との往復で毎回引き算が要る。判定側は
-- 稼働開始との差（リード分）に直して使う（`MatchmakerSchedule::queue_is_open`）。
--
-- 制約: 受付開始は稼働開始と同じか、その前 **12 時間以内**。差が 0 なら
-- 「稼働開始と同時に受付」。稼働開始の後（受付が始まる前に組み始める）は禁じる。
--
-- **既存行は書き換えない**（追記専用）。列を足すときの既定値 1245（20:45）だけを
-- 既存行に与え、その後に既定を外す。以降の行は必ず明示する。

alter table matchmaker_schedule_changes
    add column queue_open_minute integer not null default 1245
        check (queue_open_minute between 0 and 1439),
    add constraint matchmaker_schedule_changes_queue_open_before_start
        check (((start_minute - queue_open_minute + 1440) % 1440) <= 720);

alter table matchmaker_schedule_changes
    alter column queue_open_minute drop default;

comment on column matchmaker_schedule_changes.queue_open_minute is
    'キュー受付開始。**JST** の 0 時からの分（20:45 = 1245）。稼働開始と同じか、その前 12 時間以内。稼働開始との差をリードとして使い、受付枠 = 稼働枠をリードぶん前に伸ばしたもの';

-- 1 行も無いときの既定は 20:45（稼働開始 21:00 の 15 分前）。
create or replace view current_matchmaker_schedule as
select coalesce(latest.enabled, true)           as enabled,
       coalesce(latest.days_mask, 116)          as days_mask,
       coalesce(latest.start_minute, 1260)      as start_minute,
       coalesce(latest.end_minute, 120)         as end_minute,
       coalesce(latest.queue_open_minute, 1245) as queue_open_minute
from (values (1)) as anchor (x)
left join lateral (
    select c.enabled, c.days_mask, c.start_minute, c.end_minute, c.queue_open_minute
    from matchmaker_schedule_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_matchmaker_schedule is
    '現在のマッチメイカ稼働スケジュール。matchmaker_schedule_changes の最新行から導出する。1 行も無ければ既定の水金土日 21:00〜翌 2:00（JST）、キュー受付は 20:45 から。マッチメイカは 1 スキャンにつき 1 回だけこれを読む。手動の停止は常にこれより優先される';
