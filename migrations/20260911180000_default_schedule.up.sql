-- 稼働スケジュールの既定値を **水金土日 21:00〜翌 2:00（JST）** にする。
--
-- これが実際の開催枠なので、1 行も設定していない状態でそのまま動くようにする。
-- 以前の既定は「スケジュールを使わない（enabled=false）」で、毎回運営が
-- 手で止めて手で再開する運用に戻ってしまっていた。
--
-- **手動の停止は引き続き常に勝つ。**この変更は枠を与えるだけで、
-- 手で止めているものを再開させることはない。
--
-- 曜日は bit 0 = 月曜 の並びなので、水(2)+金(4)+土(5)+日(6) = 4+16+32+64 = 116。
-- 21:00 = 1260 分、2:00 = 120 分（start > end なので日をまたぐ）。
--
-- **既存の行は書き換えない。**変えるのは「1 行も無いとき」に返す値だけで、
-- 運営が既に設定していればそちらが優先される（最新行が正）。
create or replace view current_matchmaker_schedule as
select coalesce(latest.enabled, true)      as enabled,
       coalesce(latest.days_mask, 116)     as days_mask,
       coalesce(latest.start_minute, 1260) as start_minute,
       coalesce(latest.end_minute, 120)    as end_minute
from (values (1)) as anchor (x)
left join lateral (
    select c.enabled, c.days_mask, c.start_minute, c.end_minute
    from matchmaker_schedule_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_matchmaker_schedule is
    '現在のマッチメイカ稼働スケジュール。matchmaker_schedule_changes の最新行から導出する。1 行も無ければ既定の水金土日 21:00〜翌 2:00（JST）。マッチメイカは 1 スキャンにつき 1 回だけこれを読む。手動の停止は常にこれより優先される';
