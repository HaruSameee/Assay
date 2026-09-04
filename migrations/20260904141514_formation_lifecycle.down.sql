-- formation_lifecycle のロールバック。
--
-- 開発環境で up を検証・やり直すためのもの。
-- formations / formation_players / formation_events / match_reports は追記専用の試合ログであり、
-- データが入った環境でこれを実行するとログが失われる。運用環境で「取り消し」の手段として
-- 使ってはならない（取り消しは打ち消しレコードで表現する）。
--
-- 依存の逆順で落とす。トリガーと bigserial のシーケンスはテーブルと共に削除される。
-- 関数はテーブル（＝トリガー）を落とした後に消す。
drop table match_reports;
drop table formation_events;
drop table formation_players;
drop table formations;
drop table roles;
drop table players;
drop function reject_mutation();
