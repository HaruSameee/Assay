-- Phase 1 のコメントに戻す。
comment on column roles.display_name is
    'UI 表示専用。変更してよい';

comment on table roles is
    '可変の参照データ（追記専用ではない）。正規値の 5 ロールはマイグレーションで入れる。display_name / sort_order は変更してよい。';
