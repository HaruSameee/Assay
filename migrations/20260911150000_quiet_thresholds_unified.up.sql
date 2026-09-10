-- 聞き専ペナルティの必要票数を**勝ち負けとも 3 に統一する**。
--
-- 以前は勝ち 3 / 負け 4（対象者を除いた 4 人の満場一致）で、負け直後の報告が
-- 「戦犯選出」になるのを防ぐ意図だった。実際には**負け側で成立しない**という
-- 別の偏りになっていた。聞き専は勝っても負けても同じ迷惑なのに、負けた試合でだけ
-- 1 人の反対で潰せるのは、抑止としても記録としても筋が悪い。
--
-- 戦犯選出への歯止めは、非対称な閾値ではなく**下限を 3 に上げること**で持たせる。
-- 2 人の口裏合わせで成立すると、聞き専の判定ではなく私怨の道具になる。
--
-- **既存の行は書き換えない。**この表は追記専用で、現在値は最新行から導出する。
-- 変えるのは CHECK の下限（1 -> 3）と、1 行も無いときの既定値（4 -> 3）だけ。
-- 過去に積んだ 4 の行はそのまま残り、履歴として読める。
--
-- **過去の試合は新しい閾値で再判定される**（docs/02 不変条件 3）。適用時点の
-- 本番では最大票数が 2 なので、この変更でペナルティが増える試合は無い。

alter table quiet_report_param_changes drop constraint quiet_report_param_changes_winner_threshold_check;
alter table quiet_report_param_changes drop constraint quiet_report_param_changes_loser_threshold_check;
alter table quiet_report_param_changes add constraint quiet_report_param_changes_winner_threshold_check
    check (winner_threshold between 3 and 4) not valid;
alter table quiet_report_param_changes add constraint quiet_report_param_changes_loser_threshold_check
    check (loser_threshold between 3 and 4) not valid;

comment on column quiet_report_param_changes.winner_threshold is
    '対象者のチームが勝った試合で必要な票数。3〜4（報告できるのは対象者を除いた 4 人）。1 行も無ければ既定値 3';
comment on column quiet_report_param_changes.loser_threshold is
    '対象者のチームが負けた試合で必要な票数。3〜4。勝ち側と同じ 3 が既定。1 行も無ければ既定値 3';

-- 1 行も無いときの既定値を 3/3 に揃える。列の構成は変えないので create or replace
create or replace view current_quiet_report_params as
select coalesce(latest.winner_threshold, 3) as winner_threshold,
       coalesce(latest.loser_threshold, 3)  as loser_threshold
from (values (1)) as anchor (x)
left join lateral (
    select c.winner_threshold, c.loser_threshold
    from quiet_report_param_changes c
    order by c.id desc
    limit 1
) latest on true;

comment on view current_quiet_report_params is
    '現在の聞き専報告の閾値。quiet_report_param_changes の最新行から導出する。1 行も無ければ既定値 3/3。閾値を見る場所はここ 1 つで、quiet_report_penalties がこれを引く';
