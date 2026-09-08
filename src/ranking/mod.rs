//! 公開ランキング。**既存の導出結果を読むだけ**で、集計表もキャッシュも作らない。
//!
//! 掲載条件は「**確定した試合が 5 試合以上**」。`player_match_rating_deltas` は
//! `confirmed` イベントのある試合しか返さないので、disputed / voided / nullified は
//! 構造的に数に入らない（アプリ側で除外条件を書いていない）。
//!
//! 並び順は**レート降順のみ**。勝率で並べ替える経路を作らない — 5 試合では勝率が
//! 6 通りしか取れず、5 戦 5 勝が 1 位に出てしまう（docs/02 §「順位の方が拡散される」）。
//!
//! シーズン境界にも自動的に従う。境界の条件は `current_season` ビュー 1 か所にあり、
//! `player_match_rating_deltas` / `season_rating_adjustments` がそれを引いている。
//!
//! **公開スイッチは環境変数。**既定は非公開で、開幕直後は窓 50 の分離により上位が
//! シード組で固定されるため、いつ公開するかを運営が決められる形にしてある。

pub mod html;
pub mod repository;
pub mod routes;

pub use repository::{RankedPlayer, RankingPage, RankingRepository};
pub use routes::{RANKING_PUBLIC, RankingState, ranking_is_public, router};
