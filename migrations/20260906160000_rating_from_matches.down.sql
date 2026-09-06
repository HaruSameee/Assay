-- 依存の順に落とす（集計ビューが明細ビューを参照している）。
drop index if exists formation_events_confirmed_idx;
drop view if exists player_match_ratings;
drop view if exists player_match_rating_deltas;
