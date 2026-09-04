-- アプリ用ロールから、追記専用4テーブルの UPDATE / DELETE を剥奪する。
--
-- 実行時の主たる防御はこの REVOKE。マイグレーション内の reject_mutation トリガーは二重防御。
-- ロールの用意はマイグレーションの責務ではないため、このファイルはマイグレーションから呼ばず、
-- デプロイ手順の中で実行する（ロール未作成の環境でマイグレーションが落ちる、または黙って
-- スキップされるほうが害が大きい）。
-- マイグレーション用ロール（テーブル所有者）とアプリ用ロールは分けること。分ける作業自体は別途。
--
-- 使い方:
--   psql "$DATABASE_URL" -v app_role=<アプリ用ロール名> -f ops/revoke_app_role_mutations.sql
--
-- 注意: REVOKE は付与済みの権限を取り消すだけ。アプリ用ロールに GRANT ALL や
--       ALTER DEFAULT PRIVILEGES で広く権限を与えている場合に意味を持ち、
--       最初から SELECT / INSERT しか GRANT していなければ no-op になる。

revoke update, delete on
    formations, formation_players, formation_events, match_reports
    from :"app_role";
