//! 状態、イベント種別、遷移関数、永続化済み列からの導出。

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use super::error::{TransitionError, UnknownValue};
use super::ids::{PlayerId, Timestamp};

/// formation の現在状態。DB には持たず、`formation_events` の列から導出する。
///
/// 仕様 §1 の状態に、bot の再起動後に処理を再開するために必要な最小限のデータ
/// （打診中のホスト、試した候補数、部屋コード）を持たせている。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormationState {
    /// マッチメイカが10人を確定した直後。VC 割当待ち。タイムアウトなし。
    Matched,
    /// VC 割当済み。ホストを指名し、承諾を待つ。
    AwaitingHost {
        /// 打診中のホスト候補。`None` は「次の候補を指名する前」。
        pending: Option<PlayerId>,
        /// これまでに打診した候補の人数。候補上限（`LifecycleConfig::max_host_candidates`）との比較に使う。
        tried: u32,
    },
    /// ホストが「部屋を作る」を押した。部屋コードの投稿を待つ。
    AwaitingCode {
        /// 承諾したホスト。
        host: PlayerId,
        /// これまでに打診した候補の人数。
        tried: u32,
    },
    /// コードを9人に開示済み。試合中および報告受付中。
    ///
    /// 仕様 §1 の `in_progress` と `awaiting_reports` を統合したもの。
    /// 報告の到着はイベントにしないので、この2つは状態として区別しない。
    InProgress {
        /// 部屋を建てたホスト。
        host: PlayerId,
        /// 現在有効な部屋コード。貼り直しがあれば最新のもの。
        room_code: String,
    },
    /// 勝敗確定。レート計算の入力になる。
    Confirmed,
    /// 報告が拮抗。スタッフ介入待ち。裁定で `Confirmed` または `Voided` へ進む。
    Disputed,
    /// 報告者全員が試合無効で一致した。`Voided`（報告ゼロ）とは区別する。
    Nullified,
    /// 試合が成立しなかった。レートに影響させない。報告義務も生じない。
    Voided,
    /// 解散（ホスト候補の全滅、人が揃わない、VC 無し、Discord への投稿失敗）。報告義務は生じない。
    Abandoned,
}

impl FormationState {
    /// 仕様 §1 の状態名（snake_case）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::AwaitingHost { .. } => "awaiting_host",
            Self::AwaitingCode { .. } => "awaiting_code",
            Self::InProgress { .. } => "in_progress",
            Self::Confirmed => "confirmed",
            Self::Disputed => "disputed",
            Self::Nullified => "nullified",
            Self::Voided => "voided",
            Self::Abandoned => "abandoned",
        }
    }

    /// これ以上**自動では**遷移しない状態か。`Disputed` はスタッフ裁定で進むので終端ではない。
    ///
    /// `Confirmed` / `Nullified` / `Voided` の 3 つは運営の再裁定で互いに移れる
    /// （[`apply`] を見よ）。それでも終端と呼ぶのは、報告の評価・期限走査・再開処理が
    /// これらの状態を二度と触らないからである。動かすのは運営の 1 行だけ。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Confirmed | Self::Nullified | Self::Voided | Self::Abandoned
        )
    }

    /// 参加者に報告義務が生じる状態か（報告ゲートの前提条件）。
    ///
    /// コード開示後に試合が行われた formation だけが対象。`Voided` / `Abandoned` は試合が
    /// 成立していないので報告義務は無い（仕様 §5）。確定後・係争中も未報告者の義務は残る
    /// （確定後に解除すると「報告せず他9人の確定を待つ」が最適戦略になる）。
    /// プレイヤー単位の解除（本人が報告済みか）は報告フロー側の責務。
    pub fn requires_reports(&self) -> bool {
        matches!(
            self,
            Self::InProgress { .. } | Self::Confirmed | Self::Disputed | Self::Nullified
        )
    }
}

impl fmt::Display for FormationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// `formation_events.kind` の種別。DB の文字列と 1:1（[`FormationEventKind::as_str`]）。
///
/// DB の CHECK 制約はこの一覧から作る（`tests/schema.rs` で一致を検証）。
/// 状態を変えないイベントは無い。記録だけしたい事実（`host_no_show`）は
/// `host_timeout` の対象プレイヤーから導出する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormationEventKind {
    /// VC の組（red / blue）を割り当てた。payload に `vc`（ラベルと両チャンネルの ID）。
    /// `matched` → `awaiting_host`。コード開示前なら割り当て直し（状態は変わらない）も受け付ける。
    VcAssigned,
    /// ホスト候補を1人指名した（subject = 候補）。`awaiting_host` の中で候補が定まる。
    HostAssigned,
    /// 候補が「部屋を作る」を押した（subject = 候補）。`awaiting_host` → `awaiting_code`。
    HostAccepted,
    /// 候補が「パスする」を押した（subject = 候補）。罰則なし。次の候補へ。
    HostDeclined,
    /// 候補が期限内に応答しなかった、またはコードを貼らなかった（subject = 候補）。次の候補へ。
    /// 仕様 §5 の `host_no_show` の記録を兼ねる。
    HostTimeout,
    /// ホストが部屋コードを投稿した（subject = ホスト、payload に部屋コード）。
    /// `awaiting_code` → `in_progress`。`in_progress` での再投稿は貼り直し（コードを上書き）。
    CodePosted,
    /// 勝敗が確定した（確定ルール、またはスタッフ裁定）。payload に結果と判定者。
    Confirmed,
    /// 報告が拮抗した。スタッフ介入待ち。
    Disputed,
    /// 報告者全員が「無効試合」で一致した。報告ゼロの `Voided` とは別の終端。
    Nullified,
    /// 試合不成立（報告ゼロで期限切れ、やり直し、スタッフ）。payload に理由。
    Voided,
    /// 解散。payload に理由（[`AbandonReason`]）。個人の特定はしない。
    Abandoned,
}

impl FormationEventKind {
    /// 全種別。DB の CHECK 制約はこの一覧と一致させる。
    pub const ALL: [Self; 11] = [
        Self::VcAssigned,
        Self::HostAssigned,
        Self::HostAccepted,
        Self::HostDeclined,
        Self::HostTimeout,
        Self::CodePosted,
        Self::Confirmed,
        Self::Disputed,
        Self::Nullified,
        Self::Voided,
        Self::Abandoned,
    ];

    /// `formation_events.kind` に入る文字列。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::VcAssigned => "vc_assigned",
            Self::HostAssigned => "host_assigned",
            Self::HostAccepted => "host_accepted",
            Self::HostDeclined => "host_declined",
            Self::HostTimeout => "host_timeout",
            Self::CodePosted => "code_posted",
            Self::Confirmed => "confirmed",
            Self::Disputed => "disputed",
            Self::Nullified => "nullified",
            Self::Voided => "voided",
            Self::Abandoned => "abandoned",
        }
    }

    /// 対象プレイヤー（`subject_player_id`）が必須の種別か。
    pub fn requires_subject(&self) -> bool {
        matches!(
            self,
            Self::HostAssigned
                | Self::HostAccepted
                | Self::HostDeclined
                | Self::HostTimeout
                | Self::CodePosted
        )
    }
}

impl fmt::Display for FormationEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FormationEventKind {
    type Err = UnknownValue;

    fn from_str(s: &str) -> Result<Self, UnknownValue> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| UnknownValue {
                what: "formation event kind",
                value: s.to_owned(),
            })
    }
}

/// `abandoned` の理由。payload は `{"reason": "<as_str>", ...}`。
///
/// 人の推測（誰が来なかったか）は入れない。ホストは正確に言えず、間違ったデータが永久に残るだけなので。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbandonReason {
    /// ホスト候補が全滅した（`awaiting_host`）。
    HostCandidatesExhausted,
    /// 部屋に人が揃わず、ホストが「不成立」を押した（`awaiting_code`）。
    NotEnoughPlayers,
    /// 空いている VC の組が無かった（`matched`）。10 人を拘束せずキューに戻す。
    VcPoolExhausted,
    /// Discord への投稿・編集がリトライしても失敗した。payload に操作名と理由。
    DiscordCallFailed,
}

impl AbandonReason {
    /// 全理由。
    pub const ALL: [Self; 4] = [
        Self::HostCandidatesExhausted,
        Self::NotEnoughPlayers,
        Self::VcPoolExhausted,
        Self::DiscordCallFailed,
    ];

    /// payload の `reason` に入る文字列。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HostCandidatesExhausted => "host_candidates_exhausted",
            Self::NotEnoughPlayers => "not_enough_players",
            Self::VcPoolExhausted => "vc_pool_exhausted",
            Self::DiscordCallFailed => "discord_call_failed",
        }
    }
}

impl fmt::Display for AbandonReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AbandonReason {
    type Err = UnknownValue;

    fn from_str(s: &str) -> Result<Self, UnknownValue> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == s)
            .ok_or_else(|| UnknownValue {
                what: "abandon reason",
                value: s.to_owned(),
            })
    }
}

/// 状態機械が読む payload。DB の `payload` jsonb から Phase 3 で取り出す。
///
/// 状態遷移に関与しない項目（VC の割当、判定結果、`voided` の理由など）は
/// ここに含めない。それらは DB の payload にはあるが、状態機械は読まない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EventPayload {
    /// 状態機械が読む項目は無い。
    #[default]
    Empty,
    /// `code_posted`: 部屋コード（`{"room_code": "..."}`）。
    RoomCode(String),
    /// `abandoned`: 解散理由（`{"reason": "..."}`）。
    Abandon(AbandonReason),
}

/// 状態機械が必要とする分だけを持つイベント。DB 行からの変換は Phase 3 で行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormationEvent {
    /// 種別。
    pub kind: FormationEventKind,
    /// このイベントが対象とするプレイヤー（`subject_player_id`）。
    /// `host_*` と `code_posted` ではホスト（候補）。システム起因でも対象者を入れる。それ以外は `None`。
    pub subject: Option<PlayerId>,
    /// 発生時刻。順序決定には使わない（順序は挿入順）。期限計算に使う。
    pub occurred_at: Timestamp,
    /// 冪等キー（formation 内で一意）。重複の検出とエラー報告に使う。
    pub idempotency_key: String,
    /// 状態機械が読む payload。
    pub payload: EventPayload,
}

/// 1イベントを適用して次の状態を返す。純粋関数。**INSERT 前の検証**に使う。
///
/// - 不正な遷移、対象プレイヤーの不足・不一致、payload の不一致は `Err`。panic しない。
/// - 重複の吸収はしない（DB の unique 制約と [`derive_state`] の責務）。
/// - 同じ種別の再適用は訂正として扱う: `code_posted` は部屋コードを上書きし、
///   `host_assigned`（同じ候補）や `host_accepted` の再送、終端イベントの再送は状態を変えない。
pub fn apply(
    state: &FormationState,
    event: &FormationEvent,
) -> Result<FormationState, TransitionError> {
    use FormationEventKind as K;
    use FormationState as S;

    check_payload(event)?;

    let next = match event.kind {
        K::VcAssigned => match state {
            S::Matched => S::AwaitingHost {
                pending: None,
                tried: 0,
            },
            // VC の割り当て直し（コード開示前）。状態は変わらない
            S::AwaitingHost { .. } | S::AwaitingCode { .. } => state.clone(),
            _ => return Err(invalid(state, event)),
        },
        K::HostAssigned => {
            let host = require_subject(event)?;
            match state {
                S::AwaitingHost {
                    pending: None,
                    tried,
                } => S::AwaitingHost {
                    pending: Some(host),
                    tried: tried.saturating_add(1),
                },
                // 同じ候補への再打診（通知の出し直し）。状態は変わらない
                S::AwaitingHost {
                    pending: Some(p), ..
                } if *p == host => state.clone(),
                S::AwaitingHost {
                    pending: Some(p), ..
                } => return Err(mismatch(event, *p, host)),
                _ => return Err(invalid(state, event)),
            }
        }
        K::HostAccepted => {
            let host = require_subject(event)?;
            match state {
                S::AwaitingHost {
                    pending: Some(p),
                    tried,
                } if *p == host => S::AwaitingCode {
                    host,
                    tried: *tried,
                },
                S::AwaitingHost {
                    pending: Some(p), ..
                } => return Err(mismatch(event, *p, host)),
                // 承諾の再送。状態は変わらない
                S::AwaitingCode { host: h, .. } if *h == host => state.clone(),
                S::AwaitingCode { host: h, .. } => return Err(mismatch(event, *h, host)),
                _ => return Err(invalid(state, event)),
            }
        }
        K::HostDeclined => {
            let host = require_subject(event)?;
            match state {
                S::AwaitingHost {
                    pending: Some(p),
                    tried,
                } if *p == host => S::AwaitingHost {
                    pending: None,
                    tried: *tried,
                },
                S::AwaitingHost {
                    pending: Some(p), ..
                } => return Err(mismatch(event, *p, host)),
                _ => return Err(invalid(state, event)),
            }
        }
        K::HostTimeout => {
            let host = require_subject(event)?;
            match state {
                S::AwaitingHost {
                    pending: Some(p),
                    tried,
                } if *p == host => S::AwaitingHost {
                    pending: None,
                    tried: *tried,
                },
                S::AwaitingHost {
                    pending: Some(p), ..
                } => return Err(mismatch(event, *p, host)),
                S::AwaitingCode { host: h, tried } if *h == host => S::AwaitingHost {
                    pending: None,
                    tried: *tried,
                },
                S::AwaitingCode { host: h, .. } => return Err(mismatch(event, *h, host)),
                _ => return Err(invalid(state, event)),
            }
        }
        K::CodePosted => {
            let host = require_subject(event)?;
            let room_code = require_room_code(event)?;
            match state {
                S::AwaitingCode { host: h, .. } if *h == host => S::InProgress { host, room_code },
                S::AwaitingCode { host: h, .. } => return Err(mismatch(event, *h, host)),
                // 部屋コードの貼り直し。新しいコードで上書きする
                S::InProgress { host: h, .. } if *h == host => S::InProgress { host, room_code },
                S::InProgress { host: h, .. } => return Err(mismatch(event, *h, host)),
                _ => return Err(invalid(state, event)),
            }
        }
        // **終わった試合の 3 つの終端（confirmed / nullified / voided）は互いに移れる。**
        // 運営の再裁定で「確定した後にやっぱり無効試合にする」を表すため。
        // 自動の経路（報告の評価・期限走査）は in_progress でしか積まないので、
        // 終端どうしを動かすのは運営の 1 行だけになる。`abandoned` は試合が
        // 始まる前の終わり方なので、ここには入らない
        K::Confirmed => match state {
            S::InProgress { .. } | S::Disputed | S::Confirmed | S::Nullified | S::Voided => {
                S::Confirmed
            }
            _ => return Err(invalid(state, event)),
        },
        K::Disputed => match state {
            S::InProgress { .. } | S::Disputed => S::Disputed,
            _ => return Err(invalid(state, event)),
        },
        K::Nullified => match state {
            S::InProgress { .. } | S::Disputed | S::Confirmed | S::Nullified | S::Voided => {
                S::Nullified
            }
            _ => return Err(invalid(state, event)),
        },
        K::Voided => match state {
            S::InProgress { .. } | S::Disputed | S::Confirmed | S::Nullified | S::Voided => {
                S::Voided
            }
            _ => return Err(invalid(state, event)),
        },
        K::Abandoned => match state {
            S::Matched | S::AwaitingHost { .. } | S::AwaitingCode { .. } | S::Abandoned => {
                S::Abandoned
            }
            _ => return Err(invalid(state, event)),
        },
    };
    Ok(next)
}

/// payload の形が種別に合っているか。`code_posted` は部屋コード、`abandoned` は理由、それ以外は空。
fn check_payload(event: &FormationEvent) -> Result<(), TransitionError> {
    use EventPayload as P;
    use FormationEventKind as K;

    let ok = match (event.kind, &event.payload) {
        (K::CodePosted, P::RoomCode(_)) | (K::Abandoned, P::Abandon(_)) => true,
        (K::CodePosted | K::Abandoned, _) => false,
        (_, P::Empty) => true,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(payload_mismatch(event))
    }
}

fn require_room_code(event: &FormationEvent) -> Result<String, TransitionError> {
    match &event.payload {
        EventPayload::RoomCode(code) => Ok(code.clone()),
        _ => Err(payload_mismatch(event)),
    }
}

fn require_subject(event: &FormationEvent) -> Result<PlayerId, TransitionError> {
    event
        .subject
        .ok_or_else(|| TransitionError::MissingSubject {
            event: event.kind,
            idempotency_key: event.idempotency_key.clone(),
        })
}

fn invalid(state: &FormationState, event: &FormationEvent) -> TransitionError {
    TransitionError::InvalidTransition {
        state: state.clone(),
        event: event.kind,
        subject: event.subject,
        idempotency_key: event.idempotency_key.clone(),
    }
}

fn mismatch(event: &FormationEvent, expected: PlayerId, actual: PlayerId) -> TransitionError {
    TransitionError::SubjectMismatch {
        event: event.kind,
        expected,
        actual,
        idempotency_key: event.idempotency_key.clone(),
    }
}

fn payload_mismatch(event: &FormationEvent) -> TransitionError {
    TransitionError::PayloadMismatch {
        event: event.kind,
        idempotency_key: event.idempotency_key.clone(),
    }
}

/// 永続化済みの列を読むときに見つかった、適用できなかったイベント。
///
/// 追記専用なので不正な行は消せない。読み出しを止めるのではなく、スキップして
/// 呼び出し側に知らせる（ログとスタッフ通知に回す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anomaly {
    /// 列の中での位置（0 始まり）。
    pub index: usize,
    /// スキップしたイベントの種別。
    pub kind: FormationEventKind,
    /// スキップしたイベントの冪等キー。
    pub idempotency_key: String,
    /// 原因。
    pub cause: AnomalyCause,
}

/// [`Anomaly`] の原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnomalyCause {
    /// 同一 `idempotency_key` のイベントが既に適用されている。
    /// DB の unique 制約を通り抜けたということなので、呼び出し側のバグ。
    DuplicateKey,
    /// 現在の状態では受け付けられない。INSERT 前の検証をすり抜けた競合か、手動 INSERT。
    Rejected(TransitionError),
}

impl fmt::Display for Anomaly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "event #{} {} ({}): {}",
            self.index, self.kind, self.idempotency_key, self.cause
        )
    }
}

impl fmt::Display for AnomalyCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKey => f.write_str("duplicate idempotency key (already applied)"),
            Self::Rejected(error) => fmt::Display::fmt(error, f),
        }
    }
}

/// 導出結果と、期限計算・監視に必要な付随情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// 現在状態。
    pub state: FormationState,
    /// 現在状態に入った時刻（状態を変えた最後のイベントの `occurred_at`）。
    /// 訂正で状態のデータが変わった場合（部屋コードの貼り直し）も更新される。
    /// イベントが無い、または状態を変えたイベントが無ければ `None`（`formations.created_at` を使う）。
    pub since: Option<Timestamp>,
    /// 適用できずスキップしたイベント。空でなければログとスタッフ通知に回す。
    pub anomalies: Vec<Anomaly>,
}

/// 永続化済みのイベント列（挿入順）から現在状態を導出する。**絶対に失敗しない。**
///
/// イベントが無ければ `Matched`。適用できないイベントはスキップして [`Anomaly`] に積む。
/// 各イベントを一度ずつ [`apply`] するだけなので、イベント数に対して線形。
pub fn derive_state(events: &[FormationEvent]) -> (FormationState, Vec<Anomaly>) {
    let snapshot = derive_snapshot(events);
    (snapshot.state, snapshot.anomalies)
}

/// [`derive_state`] のイテレータ版。イベントを一度だけ走査する。
pub fn derive_state_iter<'a, I>(events: I) -> (FormationState, Vec<Anomaly>)
where
    I: IntoIterator<Item = &'a FormationEvent>,
{
    let snapshot = derive_snapshot_iter(events);
    (snapshot.state, snapshot.anomalies)
}

/// [`derive_state`] に加えて、現在状態に入った時刻を返す。失敗しない。線形。
pub fn derive_snapshot(events: &[FormationEvent]) -> Snapshot {
    derive_snapshot_iter(events)
}

/// [`derive_snapshot`] のイテレータ版。イベントを一度だけ走査する。
pub fn derive_snapshot_iter<'a, I>(events: I) -> Snapshot
where
    I: IntoIterator<Item = &'a FormationEvent>,
{
    let mut snapshot = Snapshot {
        state: FormationState::Matched,
        since: None,
        anomalies: Vec::new(),
    };
    let mut seen: HashSet<&'a str> = HashSet::new();

    for (index, event) in events.into_iter().enumerate() {
        if !seen.insert(event.idempotency_key.as_str()) {
            snapshot.anomalies.push(Anomaly {
                index,
                kind: event.kind,
                idempotency_key: event.idempotency_key.clone(),
                cause: AnomalyCause::DuplicateKey,
            });
            continue;
        }
        match apply(&snapshot.state, event) {
            Ok(next) => {
                if next != snapshot.state {
                    snapshot.since = Some(event.occurred_at);
                }
                snapshot.state = next;
            }
            Err(error) => snapshot.anomalies.push(Anomaly {
                index,
                kind: event.kind,
                idempotency_key: event.idempotency_key.clone(),
                cause: AnomalyCause::Rejected(error),
            }),
        }
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};
    use uuid::Uuid;

    use super::FormationEventKind as K;
    use super::FormationState as S;
    use super::*;

    fn pid(n: u128) -> PlayerId {
        PlayerId(Uuid::from_u128(n))
    }

    fn at(seq: i64) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seq)
    }

    fn ev(seq: i64, kind: K, subject: Option<PlayerId>) -> FormationEvent {
        FormationEvent {
            kind,
            subject,
            occurred_at: at(seq),
            idempotency_key: format!("evt-{seq}"),
            payload: EventPayload::Empty,
        }
    }

    fn code(seq: i64, host: PlayerId, room_code: &str) -> FormationEvent {
        FormationEvent {
            payload: EventPayload::RoomCode(room_code.to_owned()),
            ..ev(seq, K::CodePosted, Some(host))
        }
    }

    fn abandon(seq: i64, reason: AbandonReason) -> FormationEvent {
        FormationEvent {
            payload: EventPayload::Abandon(reason),
            ..ev(seq, K::Abandoned, None)
        }
    }

    fn awaiting_host(pending: Option<PlayerId>, tried: u32) -> S {
        S::AwaitingHost { pending, tried }
    }

    fn in_progress(host: PlayerId, room_code: &str) -> S {
        S::InProgress {
            host,
            room_code: room_code.to_owned(),
        }
    }

    #[test]
    fn kind_strings_round_trip_and_reject_unknown() {
        for kind in K::ALL {
            assert_eq!(kind.as_str().parse::<K>().unwrap(), kind);
        }
        assert_eq!(
            "match_started".parse::<K>(),
            Err(UnknownValue {
                what: "formation event kind",
                value: "match_started".into()
            })
        );
    }

    #[test]
    fn abandon_reason_strings_round_trip_and_reject_unknown() {
        for reason in AbandonReason::ALL {
            assert_eq!(reason.as_str().parse::<AbandonReason>().unwrap(), reason);
        }
        assert!("no_show".parse::<AbandonReason>().is_err());
    }

    #[test]
    fn no_events_means_matched() {
        assert_eq!(derive_state(&[]), (S::Matched, vec![]));
        assert_eq!(
            derive_snapshot(&[]),
            Snapshot {
                state: S::Matched,
                since: None,
                anomalies: vec![]
            }
        );
    }

    #[test]
    fn invalid_transitions_return_err_without_panicking() {
        let a = pid(1);
        let cases: Vec<(S, FormationEvent)> = vec![
            (S::Matched, code(0, a, "1234")),
            (S::Matched, ev(0, K::HostAccepted, Some(a))),
            (S::Matched, ev(0, K::Confirmed, None)),
            (awaiting_host(None, 0), ev(0, K::Confirmed, None)),
            (awaiting_host(None, 0), ev(0, K::HostTimeout, Some(a))),
            (awaiting_host(None, 1), ev(0, K::HostDeclined, Some(a))),
            (
                S::AwaitingCode { host: a, tried: 1 },
                ev(0, K::Voided, None),
            ),
            (
                S::AwaitingCode { host: a, tried: 1 },
                ev(0, K::HostDeclined, Some(a)),
            ),
            (
                in_progress(a, "1234"),
                abandon(0, AbandonReason::NotEnoughPlayers),
            ),
            (in_progress(a, "1234"), ev(0, K::VcAssigned, None)),
            (in_progress(a, "1234"), ev(0, K::HostAssigned, Some(a))),
            (S::Disputed, ev(0, K::VcAssigned, None)),
            // 終わった試合の 3 つの終端どうしは再裁定で移れる（別のテスト）。
            // 係争へ戻す・解散へ落とすのは今も不正
            (S::Confirmed, ev(0, K::Disputed, None)),
            (S::Confirmed, abandon(0, AbandonReason::NotEnoughPlayers)),
            (S::Voided, ev(0, K::Disputed, None)),
            (S::Abandoned, ev(0, K::Confirmed, None)),
            (S::Abandoned, ev(0, K::HostAssigned, Some(a))),
            (S::Abandoned, ev(0, K::VcAssigned, None)),
        ];
        for (state, event) in cases {
            let result = apply(&state, &event);
            assert!(
                matches!(result, Err(TransitionError::InvalidTransition { .. })),
                "{state:?} + {} => {result:?}",
                event.kind
            );
        }
    }

    #[test]
    fn host_events_require_a_subject() {
        let state = awaiting_host(None, 0);
        assert_eq!(
            apply(&state, &ev(0, K::HostAssigned, None)),
            Err(TransitionError::MissingSubject {
                event: K::HostAssigned,
                idempotency_key: "evt-0".into()
            })
        );
        // subject 必須の種別だけが MissingSubject になる（他は Ok か InvalidTransition）。
        // payload は種別に合わせておき、欠けているのが subject だけになるようにする
        for kind in K::ALL {
            let payload = match kind {
                K::CodePosted => EventPayload::RoomCode("1234".into()),
                K::Abandoned => EventPayload::Abandon(AbandonReason::NotEnoughPlayers),
                _ => EventPayload::Empty,
            };
            let event = FormationEvent {
                payload,
                ..ev(0, kind, None)
            };
            let result = apply(&S::Matched, &event);
            let missing = matches!(result, Err(TransitionError::MissingSubject { .. }));
            assert_eq!(missing, kind.requires_subject(), "{kind}: {result:?}");
        }
    }

    #[test]
    fn payload_must_match_the_kind() {
        let a = pid(1);
        let code_state = S::AwaitingCode { host: a, tried: 1 };
        // code_posted に部屋コードが無い
        assert!(matches!(
            apply(&code_state, &ev(0, K::CodePosted, Some(a))),
            Err(TransitionError::PayloadMismatch { .. })
        ));
        // abandoned に理由が無い
        assert!(matches!(
            apply(&S::Matched, &ev(0, K::Abandoned, None)),
            Err(TransitionError::PayloadMismatch { .. })
        ));
        // 空であるべき種別に部屋コードが付いている
        let stray = FormationEvent {
            payload: EventPayload::RoomCode("1234".into()),
            ..ev(0, K::HostAssigned, Some(a))
        };
        assert!(matches!(
            apply(&awaiting_host(None, 0), &stray),
            Err(TransitionError::PayloadMismatch { .. })
        ));
    }

    #[test]
    fn subject_must_match_the_current_host() {
        let (a, b) = (pid(1), pid(2));
        let pending_a = awaiting_host(Some(a), 1);
        assert_eq!(
            apply(&pending_a, &ev(0, K::HostAccepted, Some(b))),
            Err(TransitionError::SubjectMismatch {
                event: K::HostAccepted,
                expected: a,
                actual: b,
                idempotency_key: "evt-0".into()
            })
        );
        // 別の候補を打診中に、無関係な候補を指名するのも不一致
        assert!(matches!(
            apply(&pending_a, &ev(0, K::HostAssigned, Some(b))),
            Err(TransitionError::SubjectMismatch { .. })
        ));
        let code_a = S::AwaitingCode { host: a, tried: 1 };
        assert!(matches!(
            apply(&code_a, &code(0, b, "1234")),
            Err(TransitionError::SubjectMismatch { expected, actual, .. }) if expected == a && actual == b
        ));
        assert!(matches!(
            apply(&in_progress(a, "1234"), &code(0, b, "5678")),
            Err(TransitionError::SubjectMismatch { .. })
        ));
    }

    #[test]
    fn stale_host_events_are_rejected_by_validation_and_skipped_by_derive() {
        let (a, b, c) = (pid(1), pid(2), pid(3));
        let events = vec![
            ev(0, K::VcAssigned, None),
            ev(1, K::HostAssigned, Some(a)),
            ev(2, K::HostTimeout, Some(a)),
            ev(3, K::HostAssigned, Some(b)),
        ];
        let (state, anomalies) = derive_state(&events);
        assert_eq!(state, awaiting_host(Some(b), 2));
        assert!(anomalies.is_empty());

        // A の遅れたタイマー／辞退は検証で弾く（永続化されない）
        for stale in [
            ev(4, K::HostTimeout, Some(a)),
            ev(4, K::HostDeclined, Some(a)),
            ev(4, K::HostTimeout, Some(c)),
        ] {
            assert!(
                matches!(
                    apply(&state, &stale),
                    Err(TransitionError::SubjectMismatch { expected, .. }) if expected == b
                ),
                "{}",
                stale.kind
            );
        }

        // 検証をすり抜けて永続化されていても、導出は止まらずスキップする
        let mut corrupted = events.clone();
        corrupted.push(ev(4, K::HostTimeout, Some(a)));
        let (state, anomalies) = derive_state(&corrupted);
        assert_eq!(state, awaiting_host(Some(b), 2));
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].index, 4);
        assert!(matches!(
            anomalies[0].cause,
            AnomalyCause::Rejected(TransitionError::SubjectMismatch { .. })
        ));
    }

    #[test]
    fn re_notifying_the_same_pending_host_keeps_the_state() {
        let a = pid(1);
        let pending = awaiting_host(Some(a), 1);
        assert_eq!(
            apply(&pending, &ev(9, K::HostAssigned, Some(a))).unwrap(),
            pending
        );
        let accepted = S::AwaitingCode { host: a, tried: 1 };
        assert_eq!(
            apply(&accepted, &ev(9, K::HostAccepted, Some(a))).unwrap(),
            accepted
        );
    }

    #[test]
    fn vc_reassignment_is_accepted_before_code_disclosure() {
        let a = pid(1);
        let pending = awaiting_host(Some(a), 1);
        assert_eq!(
            apply(&pending, &ev(9, K::VcAssigned, None)).unwrap(),
            pending
        );
        let code_state = S::AwaitingCode { host: a, tried: 1 };
        assert_eq!(
            apply(&code_state, &ev(9, K::VcAssigned, None)).unwrap(),
            code_state
        );
        assert!(apply(&in_progress(a, "1234"), &ev(9, K::VcAssigned, None)).is_err());
    }

    #[test]
    fn code_timeout_returns_to_awaiting_host_and_keeps_the_count() {
        let a = pid(1);
        let (state, anomalies) = derive_state(&[
            ev(0, K::VcAssigned, None),
            ev(1, K::HostAssigned, Some(a)),
            ev(2, K::HostAccepted, Some(a)),
            ev(3, K::HostTimeout, Some(a)),
        ]);
        assert_eq!(state, awaiting_host(None, 1));
        assert!(anomalies.is_empty());
    }

    #[test]
    fn disputed_is_resolved_by_staff_into_confirmed_or_voided() {
        let a = pid(1);
        let (disputed, anomalies) = derive_state(&[
            ev(0, K::VcAssigned, None),
            ev(1, K::HostAssigned, Some(a)),
            ev(2, K::HostAccepted, Some(a)),
            code(3, a, "1234"),
            ev(4, K::Disputed, None),
        ]);
        assert_eq!(disputed, S::Disputed);
        assert!(anomalies.is_empty());
        assert!(!disputed.is_terminal());
        assert!(disputed.requires_reports());
        assert_eq!(
            apply(&disputed, &ev(5, K::Confirmed, None)).unwrap(),
            S::Confirmed
        );
        assert_eq!(
            apply(&disputed, &ev(5, K::Voided, None)).unwrap(),
            S::Voided
        );
    }

    /// **終わった試合の 3 つの終端は運営の再裁定で互いに移れる。**
    /// 「確定した後にやっぱり無効試合にする」を 1 行の追記で表すため。
    #[test]
    fn a_finished_match_can_be_readjudicated_between_the_three_terminals() {
        let finished = [
            (K::Confirmed, S::Confirmed),
            (K::Nullified, S::Nullified),
            (K::Voided, S::Voided),
        ];
        for (_, from) in &finished {
            assert!(from.is_terminal());
            for (kind, to) in &finished {
                assert_eq!(
                    apply(from, &ev(9, *kind, None)).unwrap(),
                    *to,
                    "{from} -> {kind}"
                );
            }
            // 係争へ戻すことはできない（裁定されるまで結果が無い状態には戻らない）
            assert!(apply(from, &ev(9, K::Disputed, None)).is_err(), "{from}");
            // 解散へも戻れない（試合が始まる前の終わり方）
            assert!(apply(from, &ev(9, K::Abandoned, None)).is_err(), "{from}");
        }
        // 解散した試合に結果は積めない
        for (kind, _) in &finished {
            assert!(apply(&S::Abandoned, &ev(9, *kind, None)).is_err(), "{kind}");
        }
    }

    #[test]
    fn snapshot_since_follows_state_changes_including_corrections() {
        let a = pid(1);
        let snapshot = derive_snapshot(&[
            ev(0, K::VcAssigned, None),
            ev(1, K::HostAssigned, Some(a)),
            ev(7, K::HostAssigned, Some(a)), // 再打診: 状態は変わらないので since も変わらない
        ]);
        assert_eq!(snapshot.state, awaiting_host(Some(a), 1));
        assert_eq!(snapshot.since, Some(at(1)));
        assert!(snapshot.anomalies.is_empty());

        let snapshot = derive_snapshot(&[
            ev(0, K::VcAssigned, None),
            ev(1, K::HostAssigned, Some(a)),
            ev(2, K::HostAccepted, Some(a)),
            code(3, a, "1111"),
            code(8, a, "2222"), // 貼り直し: 状態が変わるので since も進む
        ]);
        assert_eq!(snapshot.state, in_progress(a, "2222"));
        assert_eq!(snapshot.since, Some(at(8)));
    }

    #[test]
    fn anomalies_are_displayable() {
        let (_, anomalies) = derive_state(&[
            ev(0, K::VcAssigned, None),
            ev(0, K::VcAssigned, None),
            ev(1, K::Confirmed, None),
        ]);
        assert_eq!(anomalies.len(), 2);
        assert!(
            anomalies[0]
                .to_string()
                .contains("duplicate idempotency key")
        );
        assert!(anomalies[1].to_string().contains("invalid transition"));
    }

    #[test]
    fn state_names_follow_the_spec() {
        let a = pid(1);
        let named = [
            (S::Matched, "matched"),
            (awaiting_host(None, 0), "awaiting_host"),
            (S::AwaitingCode { host: a, tried: 1 }, "awaiting_code"),
            (in_progress(a, "1234"), "in_progress"),
            (S::Confirmed, "confirmed"),
            (S::Disputed, "disputed"),
            (S::Nullified, "nullified"),
            (S::Voided, "voided"),
            (S::Abandoned, "abandoned"),
        ];
        for (state, name) in named {
            assert_eq!(state.to_string(), name);
        }
    }
}
