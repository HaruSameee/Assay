//! マッチ画面（成立から確定までの1枚）。
//!
//! **Discord は通知だけ、操作は全部サイト。**ホスト打診・ルーム ID の投稿・報告は
//! すべてここのエンドポイントで受ける。Discord のボタンとモーダルは廃止した。
//! 通知は残す — サイトを開いていない人に「あなたの番です」と伝える手段が他に無く、
//! 打診には期限があるので、通知が無いと画面を開いている人だけが進行できる仕組みになる。
//!
//! 不変条件（このモジュールで守るもの）:
//!
//! - **状態機械（[`crate::lifecycle`]）の遷移そのものを変えない。**入口が Discord から
//!   HTTP に変わるだけで、積むイベントも検証も既存の永続化層に任せる。
//! - **報告は API レベルでブラインド**（00 §3-5）。他人の報告内容も**件数も**、
//!   HTML にもレスポンスにも入らない。[`model`] の型に置き場所を作っていない。
//! - **相手チームのロールを出さない。**自チームは [`model::TeammateView`]、相手は
//!   [`model::OpponentView`] と型が分かれており、後者に `role` が無い。
//!   レートは公開方針（00 §4）なので隠さない。
//! - **`player_id` はセッションからしか来ない。**他人の formation も他人の報告も触れない。
//! - **Discord への投稿はここからは出さない。**HTTP はイベントを積むだけで、通知は
//!   既存の再開ループ（5 秒スキャン）が DB の状態から出す。復帰の入口を 2 つ作らない
//!   （00 §7 Phase 5 の申し送り）。プロセスが落ちても、続きは同じ経路から進む。

pub mod html;
pub mod model;
pub mod repository;
pub mod routes;

pub use model::{AffordanceView, MatchView, OpponentView, StageView, TeammateView, VoiceView};
pub use repository::{MatchPlayer, MatchRepository, VoicePair};
pub use routes::{MatchState, router};
