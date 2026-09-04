//! Discord 側の副作用と、その失敗を `formation_events.payload` に記録する形と、その読み出し。
//!
//! 副作用の記録が正: スレッド ID・メッセージ ID・VC 割当は必ずここに保存する。
//! 再起動後は Discord 側を検索し直さず、ログから [`DiscordRecord`] を導出して続きを操作する。
//! Discord 呼び出しの失敗も stdout ではなくここに残す（「なぜこの試合が流れたか」を後から説明できるように）。
//! Discord の 64bit ID は JSON では文字列で持つ（`DISCORD_THREAD_ID_KEY` の慣例に合わせる）。

use serde_json::{Map, Value, json};
use serenity::model::id::{ChannelId, MessageId};

use crate::lifecycle::{AbandonReason, PlayerId};
use crate::persistence::{ABANDON_REASON_KEY, DISCORD_THREAD_ID_KEY, FormationLog, ROOM_CODE_KEY};

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
/// どのイベントにも付きうる: Discord 呼び出しの失敗の一覧（[`DiscordFailure`] の配列）。無ければ省略。
pub const DISCORD_FAILURES_KEY: &str = "discord_failures";
/// `abandoned`（`member_add_failed`）: スレッドに追加できなかった参加者の一覧。
pub const FAILED_PLAYERS_KEY: &str = "players";
/// [`DiscordFailure`] の中: 操作名。
pub const OPERATION_KEY: &str = "operation";
/// [`DiscordFailure`] の中: エラー文字列。
pub const ERROR_KEY: &str = "error";
/// [`DiscordFailure`] の中: 試行回数。
pub const ATTEMPTS_KEY: &str = "attempts";
/// [`DiscordFailure`] の中: 対象プレイヤー（あれば）。
pub const PLAYER_ID_KEY: &str = "player_id";

/// Discord 呼び出しの失敗。リトライを使い切った後の最後のエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordFailure {
    /// 操作名（`create_private_thread`、`add_thread_member`、`send:assignment` など）。
    pub operation: String,
    /// エラー文字列。
    pub error: String,
    /// 試行回数。
    pub attempts: u32,
    /// 対象プレイヤー（スレッドへの追加など、人に紐づく操作のとき）。
    pub player: Option<(PlayerId, i64)>,
}

impl DiscordFailure {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert(OPERATION_KEY.into(), Value::String(self.operation.clone()));
        map.insert(ERROR_KEY.into(), Value::String(self.error.clone()));
        map.insert(ATTEMPTS_KEY.into(), Value::from(self.attempts));
        if let Some((player, discord_user_id)) = self.player {
            map.insert(PLAYER_ID_KEY.into(), Value::String(player.to_string()));
            map.insert(
                DISCORD_USER_ID_KEY.into(),
                Value::String(discord_user_id.to_string()),
            );
        }
        Value::Object(map)
    }
}

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

/// 失敗があれば `discord_failures` を足す。無ければ何も足さない。
fn with_failures(mut payload: Map<String, Value>, failures: &[DiscordFailure]) -> Value {
    if !failures.is_empty() {
        payload.insert(
            DISCORD_FAILURES_KEY.into(),
            Value::Array(failures.iter().map(DiscordFailure::to_json).collect()),
        );
    }
    Value::Object(payload)
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
    failures: &[DiscordFailure],
) -> Value {
    let mut map = Map::new();
    map.insert(DISCORD_THREAD_ID_KEY.into(), id_string(thread.get()));
    map.insert(VC_KEY.into(), vc_to_json(vc));
    map.insert(
        ASSIGNMENT_MESSAGE_ID_KEY.into(),
        assignment_message_id.map_or(Value::Null, |id| id_string(id.get())),
    );
    with_failures(map, failures)
}

/// `host_assigned` の payload。
pub fn host_assigned(prompt_message_id: MessageId, discord_user_id: i64) -> Value {
    json!({
        HOST_PROMPT_MESSAGE_ID_KEY: prompt_message_id.get().to_string(),
        DISCORD_USER_ID_KEY: discord_user_id.to_string(),
    })
}

/// `host_accepted` / `host_declined` の payload。
pub fn host_response(discord_user_id: i64, failures: &[DiscordFailure]) -> Value {
    let mut map = Map::new();
    map.insert(
        DISCORD_USER_ID_KEY.into(),
        Value::String(discord_user_id.to_string()),
    );
    with_failures(map, failures)
}

/// `code_posted` の payload。状態機械が読む `room_code` を含む。
pub fn code_posted(
    room_code: &str,
    code_message_id: MessageId,
    discord_user_id: i64,
    failures: &[DiscordFailure],
) -> Value {
    let mut map = Map::new();
    map.insert(ROOM_CODE_KEY.into(), Value::String(room_code.to_owned()));
    map.insert(CODE_MESSAGE_ID_KEY.into(), id_string(code_message_id.get()));
    map.insert(
        DISCORD_USER_ID_KEY.into(),
        Value::String(discord_user_id.to_string()),
    );
    with_failures(map, failures)
}

/// `abandoned` の payload。状態機械が読む `reason` に、失敗の一覧を添える。
///
/// `member_add_failed` では、対象プレイヤー付きの失敗を `players` にも並べる。
pub fn abandoned(reason: AbandonReason, failures: &[DiscordFailure]) -> Value {
    let mut map = Map::new();
    map.insert(
        ABANDON_REASON_KEY.into(),
        Value::String(reason.as_str().to_owned()),
    );
    if reason == AbandonReason::MemberAddFailed {
        let players: Vec<Value> = failures
            .iter()
            .filter(|failure| failure.player.is_some())
            .map(DiscordFailure::to_json)
            .collect();
        map.insert(FAILED_PLAYERS_KEY.into(), Value::Array(players));
    }
    with_failures(map, failures)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::lifecycle::{
        EventPayload, FormationEvent, FormationEventKind as K, FormationId, PlayerId,
    };
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

    fn failure(operation: &str, player: Option<(PlayerId, i64)>) -> DiscordFailure {
        DiscordFailure {
            operation: operation.into(),
            error: "boom".into(),
            attempts: 3,
            player,
        }
    }

    #[test]
    fn record_is_derived_from_the_latest_thread_and_its_messages() {
        let events = vec![
            stored(
                1,
                K::ThreadCreated,
                thread_created(ChannelId::new(100), &pair(), Some(MessageId::new(101)), &[]),
            ),
            stored(2, K::HostAssigned, host_assigned(MessageId::new(102), 1001)),
            stored(3, K::HostAccepted, host_response(1001, &[])),
            stored(
                4,
                K::CodePosted,
                code_posted("1234", MessageId::new(103), 1001, &[]),
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
                thread_created(ChannelId::new(100), &pair(), None, &[]),
            ),
            stored(2, K::HostAssigned, host_assigned(MessageId::new(102), 1001)),
            stored(
                3,
                K::ThreadCreated,
                thread_created(ChannelId::new(200), &pair(), Some(MessageId::new(201)), &[]),
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
        let payload = code_posted("9999", MessageId::new(5), 1, &[]);
        assert_eq!(
            decode_payload(K::CodePosted, &payload),
            EventPayload::RoomCode("9999".into())
        );
        assert!(payload.get(DISCORD_FAILURES_KEY).is_none());
    }

    #[test]
    fn failures_are_recorded_and_the_reason_stays_readable() {
        let player = PlayerId(Uuid::from_u128(7));
        let failures = [
            failure("add_thread_member", Some((player, 1007))),
            failure("send:assignment", None),
        ];
        let payload = abandoned(AbandonReason::MemberAddFailed, &failures);
        assert_eq!(
            decode_payload(K::Abandoned, &payload),
            EventPayload::Abandon(AbandonReason::MemberAddFailed)
        );
        assert_eq!(payload[DISCORD_FAILURES_KEY].as_array().unwrap().len(), 2);
        let players = payload[FAILED_PLAYERS_KEY].as_array().unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0][PLAYER_ID_KEY], player.to_string());
        assert_eq!(players[0][DISCORD_USER_ID_KEY], "1007");
        assert_eq!(players[0][ATTEMPTS_KEY], 3);

        let payload = abandoned(AbandonReason::VcPoolExhausted, &[]);
        assert_eq!(
            decode_payload(K::Abandoned, &payload),
            EventPayload::Abandon(AbandonReason::VcPoolExhausted)
        );
        assert!(payload.get(FAILED_PLAYERS_KEY).is_none());

        let payload = host_response(1001, &failures[1..]);
        assert_eq!(
            payload[DISCORD_FAILURES_KEY][0][OPERATION_KEY],
            "send:assignment"
        );
    }
}
