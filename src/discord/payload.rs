//! Discord 側の副作用を `formation_events.payload` に記録する形と、その読み出し。
//!
//! 副作用の記録が正: スレッド ID・メッセージ ID・VC 割当は必ずここに保存する。
//! 再起動後は Discord 側を検索し直さず、ログから [`DiscordRecord`] を導出して続きを操作する。
//! Discord の 64bit ID は JSON では文字列で持つ（`DISCORD_THREAD_ID_KEY` の慣例に合わせる）。

use serde_json::{Map, Value, json};
use serenity::model::id::{ChannelId, MessageId};

use crate::persistence::{DISCORD_THREAD_ID_KEY, FormationLog, ROOM_CODE_KEY};

use super::vc_pool::VcPair;

/// `thread_created`: VC 割当（`{"label": "01", "red_channel_id": "...", "blue_channel_id": "..."}`）。
pub const VC_KEY: &str = "vc";
/// [`VC_KEY`] の中のラベル。
pub const VC_LABEL_KEY: &str = "label";
/// [`VC_KEY`] の中の red チームの VC。
pub const VC_RED_KEY: &str = "red_channel_id";
/// [`VC_KEY`] の中の blue チームの VC。
pub const VC_BLUE_KEY: &str = "blue_channel_id";
/// `thread_created`: 割り当て表のメッセージ ID。投稿に失敗していれば `null`。
pub const ASSIGNMENT_MESSAGE_ID_KEY: &str = "assignment_message_id";
/// `host_assigned`: ホスト打診メッセージの ID。候補が変わってもこのメッセージを編集する。
pub const HOST_PROMPT_MESSAGE_ID_KEY: &str = "host_prompt_message_id";
/// `code_posted`: ルーム ID を開示したメッセージの ID。貼り直しはこのメッセージを編集する。
pub const CODE_MESSAGE_ID_KEY: &str = "code_message_id";
/// `host_*` / `code_posted`: 対象プレイヤーの Discord ユーザー ID（監査用。状態機械は読まない）。
pub const DISCORD_USER_ID_KEY: &str = "discord_user_id";

/// ログから導出した、Discord 側の現在の対応物。
///
/// 最新の `thread_created` 以降のイベントだけを見る（スレッドを作り直したら、
/// 古いスレッドのメッセージ ID は捨てる）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscordRecord {
    /// プライベートスレッド。
    pub thread_id: Option<ChannelId>,
    /// VC 割当。
    pub vc: Option<VcPair>,
    /// 割り当て表のメッセージ。
    pub assignment_message_id: Option<MessageId>,
    /// ホスト打診メッセージ（状態表示を兼ねる）。
    pub host_prompt_message_id: Option<MessageId>,
    /// ルーム ID を開示したメッセージ。
    pub code_message_id: Option<MessageId>,
}

impl DiscordRecord {
    /// イベント列（id 順）から導出する。失敗しない。読めない値は無いものとして扱う。
    pub fn from_log(log: &FormationLog) -> Self {
        use crate::lifecycle::FormationEventKind as K;

        let mut record = Self::default();
        for stored in &log.events {
            let payload = &stored.payload;
            match stored.event.kind {
                K::ThreadCreated => {
                    if let Some(thread) = id_field(payload, DISCORD_THREAD_ID_KEY) {
                        record = Self {
                            thread_id: Some(ChannelId::new(thread)),
                            vc: vc_from_json(payload.get(VC_KEY)),
                            assignment_message_id: id_field(payload, ASSIGNMENT_MESSAGE_ID_KEY)
                                .map(MessageId::new),
                            host_prompt_message_id: None,
                            code_message_id: None,
                        };
                    }
                }
                K::HostAssigned => {
                    if let Some(id) = id_field(payload, HOST_PROMPT_MESSAGE_ID_KEY) {
                        record.host_prompt_message_id = Some(MessageId::new(id));
                    }
                }
                K::CodePosted => {
                    if let Some(id) = id_field(payload, CODE_MESSAGE_ID_KEY) {
                        record.code_message_id = Some(MessageId::new(id));
                    }
                }
                _ => {}
            }
        }
        record
    }
}

/// 文字列で入っている Discord ID を読む。0 や数値でないものは `None`。
fn id_field(payload: &Value, key: &str) -> Option<u64> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|id| *id != 0)
}

fn id_string(id: u64) -> Value {
    Value::String(id.to_string())
}

/// VC 割当を JSON にする。
pub fn vc_to_json(pair: &VcPair) -> Value {
    json!({
        VC_LABEL_KEY: pair.label,
        VC_RED_KEY: pair.red.get().to_string(),
        VC_BLUE_KEY: pair.blue.get().to_string(),
    })
}

/// JSON から VC 割当を読む。形が合わなければ `None`。
pub fn vc_from_json(value: Option<&Value>) -> Option<VcPair> {
    let value = value?;
    let label = value.get(VC_LABEL_KEY)?.as_str()?;
    let red = id_field(value, VC_RED_KEY)?;
    let blue = id_field(value, VC_BLUE_KEY)?;
    Some(VcPair {
        label: label.to_owned(),
        red: ChannelId::new(red),
        blue: ChannelId::new(blue),
    })
}

/// `thread_created` の payload。
pub fn thread_created(
    thread: ChannelId,
    vc: &VcPair,
    assignment_message_id: Option<MessageId>,
) -> Value {
    let mut map = Map::new();
    map.insert(DISCORD_THREAD_ID_KEY.into(), id_string(thread.get()));
    map.insert(VC_KEY.into(), vc_to_json(vc));
    map.insert(
        ASSIGNMENT_MESSAGE_ID_KEY.into(),
        assignment_message_id.map_or(Value::Null, |id| id_string(id.get())),
    );
    Value::Object(map)
}

/// `host_assigned` の payload。
pub fn host_assigned(prompt_message_id: MessageId, discord_user_id: i64) -> Value {
    json!({
        HOST_PROMPT_MESSAGE_ID_KEY: prompt_message_id.get().to_string(),
        DISCORD_USER_ID_KEY: discord_user_id.to_string(),
    })
}

/// `host_accepted` / `host_declined` の payload。
pub fn host_response(discord_user_id: i64) -> Value {
    json!({ DISCORD_USER_ID_KEY: discord_user_id.to_string() })
}

/// `code_posted` の payload。状態機械が読む `room_code` を含む。
pub fn code_posted(room_code: &str, code_message_id: MessageId, discord_user_id: i64) -> Value {
    json!({
        ROOM_CODE_KEY: room_code,
        CODE_MESSAGE_ID_KEY: code_message_id.get().to_string(),
        DISCORD_USER_ID_KEY: discord_user_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::lifecycle::{FormationEvent, FormationEventKind as K, FormationId, PlayerId};
    use crate::persistence::{EventId, StoredEvent, decode_payload};

    use super::*;

    fn stored(seq: i64, kind: K, payload: Value) -> StoredEvent {
        StoredEvent {
            id: EventId(seq),
            event: FormationEvent {
                kind,
                subject: Some(PlayerId(Uuid::from_u128(1))),
                occurred_at: Utc::now(),
                idempotency_key: format!("k{seq}"),
                payload: decode_payload(kind, &payload),
            },
            payload,
        }
    }

    fn log(events: Vec<StoredEvent>) -> FormationLog {
        FormationLog {
            formation_id: FormationId(Uuid::from_u128(0xf0)),
            created_at: Utc::now(),
            events,
            unreadable: Vec::new(),
        }
    }

    fn pair() -> VcPair {
        VcPair {
            label: "01".into(),
            red: ChannelId::new(11),
            blue: ChannelId::new(12),
        }
    }

    #[test]
    fn record_is_derived_from_the_latest_thread_and_its_messages() {
        let events = vec![
            stored(
                1,
                K::ThreadCreated,
                thread_created(ChannelId::new(100), &pair(), Some(MessageId::new(101))),
            ),
            stored(2, K::HostAssigned, host_assigned(MessageId::new(102), 1001)),
            stored(3, K::HostAccepted, host_response(1001)),
            stored(
                4,
                K::CodePosted,
                code_posted("1234", MessageId::new(103), 1001),
            ),
        ];
        let record = DiscordRecord::from_log(&log(events));
        assert_eq!(
            record,
            DiscordRecord {
                thread_id: Some(ChannelId::new(100)),
                vc: Some(pair()),
                assignment_message_id: Some(MessageId::new(101)),
                host_prompt_message_id: Some(MessageId::new(102)),
                code_message_id: Some(MessageId::new(103)),
            }
        );
    }

    #[test]
    fn a_recreated_thread_forgets_the_old_messages() {
        let events = vec![
            stored(
                1,
                K::ThreadCreated,
                thread_created(ChannelId::new(100), &pair(), None),
            ),
            stored(2, K::HostAssigned, host_assigned(MessageId::new(102), 1001)),
            stored(
                3,
                K::ThreadCreated,
                thread_created(ChannelId::new(200), &pair(), Some(MessageId::new(201))),
            ),
        ];
        let record = DiscordRecord::from_log(&log(events));
        assert_eq!(record.thread_id, Some(ChannelId::new(200)));
        assert_eq!(record.assignment_message_id, Some(MessageId::new(201)));
        assert_eq!(record.host_prompt_message_id, None);
    }

    #[test]
    fn unreadable_ids_are_ignored_not_fatal() {
        let events = vec![stored(
            1,
            K::ThreadCreated,
            json!({ DISCORD_THREAD_ID_KEY: 123, VC_KEY: "01" }),
        )];
        assert_eq!(
            DiscordRecord::from_log(&log(events)),
            DiscordRecord::default()
        );
        let events = vec![stored(
            1,
            K::ThreadCreated,
            json!({ DISCORD_THREAD_ID_KEY: "123", VC_KEY: { VC_LABEL_KEY: "01" } }),
        )];
        let record = DiscordRecord::from_log(&log(events));
        assert_eq!(record.thread_id, Some(ChannelId::new(123)));
        assert_eq!(record.vc, None);
    }

    #[test]
    fn code_posted_payload_keeps_the_room_code_readable_by_the_state_machine() {
        let payload = code_posted("9999", MessageId::new(5), 1);
        assert_eq!(
            decode_payload(K::CodePosted, &payload),
            crate::lifecycle::EventPayload::RoomCode("9999".into())
        );
    }
}
