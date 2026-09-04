//! 永続化層のエラー。

use crate::lifecycle::{FormationId, TransitionError};

use super::model::ReportId;

/// [`super::FormationRepository`] のエラー。
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    /// formation が存在しない。
    #[error("formation {0} not found")]
    FormationNotFound(FormationId),
    /// formation の構成が不正（人数、チーム分け、重複）。
    #[error("invalid formation: {0}")]
    InvalidFormation(String),
    /// 状態機械が遷移を拒否した。何も書かれていない。
    #[error("transition rejected: {0}")]
    Rejected(#[from] TransitionError),
    /// 同じ冪等キーが、内容の違うイベントに既に使われている。
    #[error(
        "idempotency key {idempotency_key:?} is already used by a different event of formation {formation_id}"
    )]
    IdempotencyKeyReused {
        /// 対象の formation。
        formation_id: FormationId,
        /// 再利用されたキー。
        idempotency_key: String,
    },
    /// 報告を受け付けない状態（コード開示前、または試合不成立）。
    ///
    /// 理由となる状態は載せない。報告者に返してよいのは「受理できなかった」までで、
    /// 確定状態や係争の有無を推測させる情報を戻り値に持たせない。
    #[error("formation {formation_id} is not accepting reports")]
    ReportsNotAccepted {
        /// 対象の formation。
        formation_id: FormationId,
    },
    /// `supersedes_id` が、同じプレイヤー・同じ formation の報告を指していない。
    #[error("report {0} does not belong to this player and formation, so it cannot be superseded")]
    SupersedeMismatch(ReportId),
    /// その報告は既に別の報告に打ち消されている（訂正チェーンは分岐しない）。
    #[error("report {0} has already been superseded")]
    AlreadySuperseded(ReportId),
    /// DB エラー（制約違反を含む）。
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}
