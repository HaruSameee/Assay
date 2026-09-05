//! DB 行と状態機械の型の変換、および payload の JSON キー。

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::lifecycle::{EventPayload, FormationEvent, FormationEventKind, PlayerId};

use super::model::{EventId, StoredEvent, UnreadableRow};

/// `code_posted` の payload キー（部屋コード、文字列）。
pub const ROOM_CODE_KEY: &str = "room_code";
/// `abandoned` の payload キー（[`crate::lifecycle::AbandonReason`] の文字列）。
pub const ABANDON_REASON_KEY: &str = "reason";
/// `vc_assigned` の payload キー: VC の組（`{"label": "...", "red_channel_id": "...", "blue_channel_id": "..."}`）。
pub const VC_KEY: &str = "vc";
/// [`VC_KEY`] の中: 表示用ラベル（設定の名前 `vc<N>-red` / `vc<N>-blue` の `<N>`）。
pub const VC_LABEL_KEY: &str = "label";
/// [`VC_KEY`] の中: red チームの VC のチャンネル ID。値は文字列（64bit の Discord ID を JSON 数値にしない）。
pub const VC_RED_CHANNEL_ID_KEY: &str = "red_channel_id";
/// [`VC_KEY`] の中: blue チームの VC のチャンネル ID。値は文字列。
pub const VC_BLUE_CHANNEL_ID_KEY: &str = "blue_channel_id";

/// 生の payload から状態機械が読む部分を取り出す。失敗しない。
///
/// 形が合わなければ [`EventPayload::Empty`] になり、[`crate::lifecycle::apply`] が
/// `PayloadMismatch` で弾く（追記前なら拒否、永続化済みなら `Anomaly`）。
pub fn decode_payload(kind: FormationEventKind, payload: &Value) -> EventPayload {
    match kind {
        FormationEventKind::CodePosted => payload
            .get(ROOM_CODE_KEY)
            .and_then(Value::as_str)
            .map(|code| EventPayload::RoomCode(code.to_owned()))
            .unwrap_or_default(),
        FormationEventKind::Abandoned => payload
            .get(ABANDON_REASON_KEY)
            .and_then(Value::as_str)
            .and_then(|reason| reason.parse().ok())
            .map(EventPayload::Abandon)
            .unwrap_or_default(),
        _ => EventPayload::Empty,
    }
}

/// `formation_events` の行（`query_as!` の受け皿）。
#[derive(Debug, Clone)]
pub(crate) struct EventRow {
    pub id: i64,
    pub formation_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub kind: String,
    pub subject_player_id: Option<Uuid>,
    pub payload: Value,
    pub idempotency_key: String,
}

/// 行を状態機械のイベントに変換する。種別が未知なら [`UnreadableRow`]（捨てない）。
pub(crate) fn decode_row(row: EventRow) -> Result<StoredEvent, UnreadableRow> {
    let kind: FormationEventKind = match row.kind.parse() {
        Ok(kind) => kind,
        Err(error) => {
            return Err(UnreadableRow {
                id: EventId(row.id),
                kind: row.kind,
                idempotency_key: row.idempotency_key,
                reason: error.to_string(),
            });
        }
    };
    let payload = decode_payload(kind, &row.payload);
    Ok(StoredEvent {
        id: EventId(row.id),
        event: FormationEvent {
            kind,
            subject: row.subject_player_id.map(PlayerId),
            occurred_at: row.occurred_at,
            idempotency_key: row.idempotency_key,
            payload,
        },
        payload: row.payload,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::lifecycle::AbandonReason;

    #[test]
    fn room_code_is_decoded_only_for_code_posted() {
        let payload = json!({ ROOM_CODE_KEY: "123456" });
        assert_eq!(
            decode_payload(FormationEventKind::CodePosted, &payload),
            EventPayload::RoomCode("123456".into())
        );
        assert_eq!(
            decode_payload(FormationEventKind::HostAssigned, &payload),
            EventPayload::Empty
        );
        // 形が合わなければ Empty（apply が PayloadMismatch にする）
        assert_eq!(
            decode_payload(FormationEventKind::CodePosted, &json!({})),
            EventPayload::Empty
        );
        assert_eq!(
            decode_payload(
                FormationEventKind::CodePosted,
                &json!({ ROOM_CODE_KEY: 123 })
            ),
            EventPayload::Empty
        );
    }

    #[test]
    fn abandon_reason_is_decoded_from_its_string() {
        assert_eq!(
            decode_payload(
                FormationEventKind::Abandoned,
                &json!({ ABANDON_REASON_KEY: "not_enough_players" })
            ),
            EventPayload::Abandon(AbandonReason::NotEnoughPlayers)
        );
        assert_eq!(
            decode_payload(
                FormationEventKind::Abandoned,
                &json!({ ABANDON_REASON_KEY: "no_show" })
            ),
            EventPayload::Empty
        );
    }

    #[test]
    fn unknown_kind_becomes_an_unreadable_row() {
        let row = EventRow {
            id: 7,
            formation_id: Uuid::nil(),
            occurred_at: Utc::now(),
            kind: "match_started".into(),
            subject_player_id: None,
            payload: json!({}),
            idempotency_key: "k".into(),
        };
        let unreadable = decode_row(row).unwrap_err();
        assert_eq!(unreadable.id, EventId(7));
        assert_eq!(unreadable.kind, "match_started");
        assert!(unreadable.reason.contains("unknown formation event kind"));
    }
}
