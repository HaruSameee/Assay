//! エラー型。不正な遷移は panic ではなくこれを返す。

use super::ids::PlayerId;
use super::state::{FormationEventKind, FormationState};

/// [`super::apply`] が返すエラー。**INSERT 前の検証**で使う。
///
/// 永続化済みの列を読む [`super::derive_state`] はこれを返さない。適用できなかったイベントは
/// スキップして [`super::Anomaly`] に記録し、状態は必ず返す。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// 現在の状態ではそのイベントを受け付けない。
    #[error("invalid transition: {event} is not accepted in {state:?} (event {idempotency_key})")]
    InvalidTransition {
        /// 遷移元の状態。
        state: FormationState,
        /// 拒否したイベントの種別。
        event: FormationEventKind,
        /// イベントの対象プレイヤー。
        subject: Option<PlayerId>,
        /// イベントの冪等キー。
        idempotency_key: String,
    },
    /// プレイヤーを対象とするイベントに対象プレイヤーが無い。
    #[error("{event} requires a subject player (event {idempotency_key})")]
    MissingSubject {
        /// イベントの種別。
        event: FormationEventKind,
        /// イベントの冪等キー。
        idempotency_key: String,
    },
    /// 対象プレイヤーが、いま打診中または承諾済みのホストと一致しない。
    ///
    /// 前の候補についての遅れたタイマーなど、古くなったイベントもここに落ちる。
    /// 検証で弾かれるので永続化されない。
    #[error(
        "{event}: subject {actual} is not the current host {expected} (event {idempotency_key})"
    )]
    SubjectMismatch {
        /// イベントの種別。
        event: FormationEventKind,
        /// 現在のホスト（候補）。
        expected: PlayerId,
        /// イベントの対象プレイヤー。
        actual: PlayerId,
        /// イベントの冪等キー。
        idempotency_key: String,
    },
    /// payload の形がイベント種別に合わない（`code_posted` に部屋コードが無い、など）。
    #[error("{event}: payload does not match the event kind (event {idempotency_key})")]
    PayloadMismatch {
        /// イベントの種別。
        event: FormationEventKind,
        /// イベントの冪等キー。
        idempotency_key: String,
    },
}

/// 文字列から列挙型へ変換できなかった。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {what}: {value}")]
pub struct UnknownValue {
    /// 何の値か（`formation event kind` など）。
    pub what: &'static str,
    /// 与えられた文字列。
    pub value: String,
}
