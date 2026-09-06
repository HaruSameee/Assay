-- roles.display_name の扱いを実態に合わせる（コメントの差し替えだけ。データは変えない）。
--
-- Phase 1 のコメントは display_name を「UI 表示専用。変更してよい」としていたが、これは誤り。
-- ロールの表示名は現在 2 か所にある。
--
--   * DB の `roles.display_name`     — Discord の割り当て表が読む（`RosterEntry::role_display_name`）
--   * Rust の `queue::html::ROLE_OPTIONS` — Web のキュー画面が読む
--
-- 実行時に display_name だけを変えると、**Discord と Web で同じロールの表記が食い違う。**
-- Web 側を DB 参照にすれば一本化できるが、描画経路を非同期にする変更になるため、
-- 開催前は「実行時に変えない」という運用ルールで塞ぐ。表記を変えるときは
-- ROLE_OPTIONS と合わせて直し、デプロイする。
--
-- 既存のマイグレーションは書き直さず、コメントの差し替えを新しい行として積む。
comment on column roles.display_name is
    'UI 表示名。**実行時に変更しない。**Discord の割り当て表はこの列を読み、Web のキュー画面は Rust の queue::html::ROLE_OPTIONS を読むため、片方だけ変えると表記が食い違う。変更するときは両方を直してデプロイすること';

comment on table roles is
    '可変の参照データ（追記専用ではない）。正規値の 5 ロールはマイグレーションで入れる。sort_order は変更してよい。display_name は実行時に変更しない（列コメントを読むこと）。';
