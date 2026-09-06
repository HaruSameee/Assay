//! Discord 側の副作用と、その失敗を `formation_events.payload` に記録する形と、その読み出し。
//!
//! 副作用の記録が正: VC 割当と投稿したメッセージの ID は必ずここに保存する。
//! 再起動後は Discord 側を検索し直さず、ログから [`DiscordRecord`] を導出して続きを操作する。
//! Discord 呼び出しの失敗も stdout ではなくここに残す（「なぜこの試合が流れたか」を後から説明できるように）。
//! Discord の 64bit ID は JSON では文字列で持つ（`VC_KEY` の慣例に合わせる）。

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use serenity::model::id::{ChannelId, MessageId};

use crate::lifecycle::{AbandonReason, Timestamp};
use crate::persistence::{
    ABANDON_REASON_KEY, FormationLog, ROOM_CODE_KEY, Team, VC_BLUE_CHANNEL_ID_KEY, VC_KEY,
    VC_LABEL_KEY, VC_RED_CHANNEL_ID_KEY,
};

use super::vc_pool::VcPair;

/// `vc_assigned`: 割り当て表のメッセージ ID（`{"red": "...", "blue": "..."}`）。投稿できなかった側は無い。
pub const ASSIGNMENT_MESSAGE_IDS_KEY: &str = "assignment_message_ids";
/// `host_assigned`: ホスト打診メッセージを投稿したチャンネル（ホスト候補の側の VC）。
pub const HOST_PROMPT_CHANNEL_ID_KEY: &str = "host_prompt_channel_id";
/// `host_assigned`: ホスト打診メッセージの ID。同じ側の候補に変わったらこのメッセージを編集する。
pub const HOST_PROMPT_MESSAGE_ID_KEY: &str = "host_prompt_message_id";
/// `code_posted`: ルーム ID を開示したメッセージの ID（`{"red": "...", "blue": "..."}`）。貼り直しは編集する。
pub const CODE_MESSAGE_IDS_KEY: &str = "code_message_ids";
/// `code_posted`: 報告ボタンを置いたメッセージの ID（red / blue）。
pub const REPORT_MESSAGE_IDS_KEY: &str = "report_message_ids";
/// `disputed`: 運営専用通知のチャンネル ID。
pub const DISPUTE_CHANNEL_ID_KEY: &str = "dispute_channel_id";
/// `disputed`: 運営専用通知のメッセージ ID。
pub const DISPUTE_MESSAGE_ID_KEY: &str = "dispute_message_id";
/// `host_*` / `code_posted`: 対象プレイヤーの Discord ユーザー ID（監査用。状態機械は読まない）。
pub const DISCORD_USER_ID_KEY: &str = "discord_user_id";
/// どのイベントにも付きうる: Discord 呼び出しの失敗の一覧（[`DiscordFailure`] の配列）。無ければ省略。
pub const DISCORD_FAILURES_KEY: &str = "discord_failures";
/// [`DiscordFailure`] の中: 操作名。
pub const OPERATION_KEY: &str = "operation";
/// [`DiscordFailure`] の中: エラー文字列。
pub const ERROR_KEY: &str = "error";
/// [`DiscordFailure`] の中: 試行回数。
pub const ATTEMPTS_KEY: &str = "attempts";
/// 時間駆動の遷移で使う理由。`abandoned` と同じキー名だが、種別ごとに語彙は異なる。
pub const TIMEOUT_REASON_KEY: &str = "reason";
/// 期限判定に使ったイベント由来の時刻。監査用に RFC 3339 文字列で残す。
pub const DEADLINE_AT_KEY: &str = "deadline_at";

/// Discord 呼び出しの失敗。リトライを使い切った後の最後のエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordFailure {
    /// 操作名（`send:assignment:red`、`edit:host_prompt`、`respond:modal` など）。
    pub operation: String,
    /// エラー文字列。
    pub error: String,
    /// 試行回数。
    pub attempts: u32,
}

impl DiscordFailure {
    fn to_json(&self) -> Value {
        json!({
            OPERATION_KEY: self.operation,
            ERROR_KEY: self.error,
            ATTEMPTS_KEY: self.attempts,
        })
    }
}

/// ログから導出した、Discord 側の現在の対応物。
///
/// 最新の `vc_assigned` 以降のイベントだけを見る（割り当て直したら、古い VC のメッセージ ID は捨てる）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscordRecord {
    /// VC 割当。
    pub vc: Option<VcPair>,
    /// 割り当て表のメッセージ（チームごと）。
    pub assignment_message_ids: HashMap<Team, MessageId>,
    /// ホスト打診メッセージ（状態表示を兼ねる）: 投稿先のチャンネルと ID。
    pub host_prompt: Option<(ChannelId, MessageId)>,
    /// ルーム ID を開示したメッセージ（チームごと）。
    pub code_message_ids: HashMap<Team, MessageId>,
    /// 報告ボタンのメッセージ（チームごと）。
    pub report_message_ids: HashMap<Team, MessageId>,
    /// 係争の運営通知。
    pub dispute_notification: Option<(ChannelId, MessageId)>,
}

impl DiscordRecord {
    /// イベント列（id 順）から導出する。失敗しない。読めない値は無いものとして扱う。
    pub fn from_log(log: &FormationLog) -> Self {
        use crate::lifecycle::FormationEventKind as K;

        let mut record = Self::default();
        for stored in &log.events {
            let payload = &stored.payload;
            match stored.event.kind {
                K::VcAssigned => {
                    if let Some(vc) = vc_from_json(payload.get(VC_KEY)) {
                        record = Self {
                            vc: Some(vc),
                            assignment_message_ids: per_team_ids(
                                payload.get(ASSIGNMENT_MESSAGE_IDS_KEY),
                            ),
                            host_prompt: None,
                            code_message_ids: HashMap::new(),
                            report_message_ids: HashMap::new(),
                            dispute_notification: None,
                        };
                    }
                }
                K::HostAssigned => {
                    if let (Some(channel), Some(message)) = (
                        id_field(payload, HOST_PROMPT_CHANNEL_ID_KEY),
                        id_field(payload, HOST_PROMPT_MESSAGE_ID_KEY),
                    ) {
                        record.host_prompt =
                            Some((ChannelId::new(channel), MessageId::new(message)));
                    }
                }
                K::CodePosted => {
                    let ids = per_team_ids(payload.get(CODE_MESSAGE_IDS_KEY));
                    if !ids.is_empty() {
                        record.code_message_ids = ids;
                    }
                    let report_ids = per_team_ids(payload.get(REPORT_MESSAGE_IDS_KEY));
                    if !report_ids.is_empty() {
                        record.report_message_ids = report_ids;
                    }
                }
                K::Disputed => {
                    if let (Some(channel), Some(message)) = (
                        id_field(payload, DISPUTE_CHANNEL_ID_KEY),
                        id_field(payload, DISPUTE_MESSAGE_ID_KEY),
                    ) {
                        record.dispute_notification =
                            Some((ChannelId::new(channel), MessageId::new(message)));
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

/// `{"red": "...", "blue": "..."}` を読む。
fn per_team_ids(value: Option<&Value>) -> HashMap<Team, MessageId> {
    let Some(value) = value else {
        return HashMap::new();
    };
    Team::ALL
        .into_iter()
        .filter_map(|team| id_field(value, team.as_str()).map(|id| (team, MessageId::new(id))))
        .collect()
}

/// `{"red": "...", "blue": "..."}` を作る。無い側は省く。
fn per_team_json(ids: &HashMap<Team, MessageId>) -> Value {
    let mut map = Map::new();
    for team in Team::ALL {
        if let Some(id) = ids.get(&team) {
            map.insert(team.as_str().to_owned(), id_string(id.get()));
        }
    }
    Value::Object(map)
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
        VC_RED_CHANNEL_ID_KEY: pair.red.get().to_string(),
        VC_BLUE_CHANNEL_ID_KEY: pair.blue.get().to_string(),
    })
}

/// JSON から VC 割当を読む。形が合わなければ `None`。
pub fn vc_from_json(value: Option<&Value>) -> Option<VcPair> {
    let value = value?;
    let label = value.get(VC_LABEL_KEY)?.as_str()?;
    let red = id_field(value, VC_RED_CHANNEL_ID_KEY)?;
    let blue = id_field(value, VC_BLUE_CHANNEL_ID_KEY)?;
    Some(VcPair {
        label: label.to_owned(),
        red: ChannelId::new(red),
        blue: ChannelId::new(blue),
    })
}

/// `vc_assigned` の payload。
pub fn vc_assigned(
    vc: &VcPair,
    assignment_message_ids: &HashMap<Team, MessageId>,
    failures: &[DiscordFailure],
) -> Value {
    let mut map = Map::new();
    map.insert(VC_KEY.into(), vc_to_json(vc));
    map.insert(
        ASSIGNMENT_MESSAGE_IDS_KEY.into(),
        per_team_json(assignment_message_ids),
    );
    with_failures(map, failures)
}

/// `host_assigned` の payload。
pub fn host_assigned(
    prompt_channel: ChannelId,
    prompt_message_id: MessageId,
    discord_user_id: i64,
    failures: &[DiscordFailure],
) -> Value {
    let mut map = Map::new();
    map.insert(
        HOST_PROMPT_CHANNEL_ID_KEY.into(),
        id_string(prompt_channel.get()),
    );
    map.insert(
        HOST_PROMPT_MESSAGE_ID_KEY.into(),
        id_string(prompt_message_id.get()),
    );
    map.insert(
        DISCORD_USER_ID_KEY.into(),
        Value::String(discord_user_id.to_string()),
    );
    with_failures(map, failures)
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
    code_message_ids: &HashMap<Team, MessageId>,
    report_message_ids: &HashMap<Team, MessageId>,
    discord_user_id: i64,
    failures: &[DiscordFailure],
) -> Value {
    let mut map = Map::new();
    map.insert(ROOM_CODE_KEY.into(), Value::String(room_code.to_owned()));
    map.insert(CODE_MESSAGE_IDS_KEY.into(), per_team_json(code_message_ids));
    map.insert(
        REPORT_MESSAGE_IDS_KEY.into(),
        per_team_json(report_message_ids),
    );
    map.insert(
        DISCORD_USER_ID_KEY.into(),
        Value::String(discord_user_id.to_string()),
    );
    with_failures(map, failures)
}

/// `disputed` の運営通知を記録する payload。失敗時は ID 無しでエラーだけを残す。
pub fn dispute_notification(
    channel: ChannelId,
    message: Option<MessageId>,
    failures: &[DiscordFailure],
) -> Value {
    let mut map = Map::new();
    map.insert(DISPUTE_CHANNEL_ID_KEY.into(), id_string(channel.get()));
    if let Some(message) = message {
        map.insert(DISPUTE_MESSAGE_ID_KEY.into(), id_string(message.get()));
    }
    with_failures(map, failures)
}

/// `abandoned` の payload。状態機械が読む `reason` に、失敗の一覧を添える。
pub fn abandoned(reason: AbandonReason, failures: &[DiscordFailure]) -> Value {
    let mut map = Map::new();
    map.insert(
        ABANDON_REASON_KEY.into(),
        Value::String(reason.as_str().to_owned()),
    );
    with_failures(map, failures)
}

/// 時間駆動の遷移の payload。状態機械はこの値を読まないが、なぜ遷移したかを後から集計できる。
pub fn timeout(reason: &str, deadline_at: Timestamp) -> Value {
    json!({
        TIMEOUT_REASON_KEY: reason,
        DEADLINE_AT_KEY: deadline_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::lifecycle::{
        EventPayload, FormationEvent, FormationEventKind as K, FormationId, PlayerId,
    };
    use crate::persistence::{EventId, NewEvent, StoredEvent, decode_payload};

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
            label: "3".into(),
            red: ChannelId::new(31),
            blue: ChannelId::new(32),
        }
    }

    fn ids(red: u64, blue: u64) -> HashMap<Team, MessageId> {
        [
            (Team::Red, MessageId::new(red)),
            (Team::Blue, MessageId::new(blue)),
        ]
        .into_iter()
        .collect()
    }

    fn failure(operation: &str) -> DiscordFailure {
        DiscordFailure {
            operation: operation.into(),
            error: "boom".into(),
            attempts: 3,
        }
    }

    #[test]
    fn record_is_derived_from_the_latest_assignment_and_its_messages() {
        let events = vec![
            stored(1, K::VcAssigned, vc_assigned(&pair(), &ids(101, 102), &[])),
            stored(
                2,
                K::HostAssigned,
                host_assigned(ChannelId::new(31), MessageId::new(103), 1001, &[]),
            ),
            stored(3, K::HostAccepted, host_response(1001, &[])),
            stored(
                4,
                K::CodePosted,
                code_posted("1234", &ids(104, 105), &ids(106, 107), 1001, &[]),
            ),
        ];
        let record = DiscordRecord::from_log(&log(events));
        assert_eq!(
            record,
            DiscordRecord {
                vc: Some(pair()),
                assignment_message_ids: ids(101, 102),
                host_prompt: Some((ChannelId::new(31), MessageId::new(103))),
                code_message_ids: ids(104, 105),
                report_message_ids: ids(106, 107),
                dispute_notification: None,
            }
        );
    }

    #[test]
    fn a_reassigned_vc_forgets_the_old_messages() {
        let mut other = pair();
        other.label = "4".into();
        let events = vec![
            stored(1, K::VcAssigned, vc_assigned(&pair(), &ids(101, 102), &[])),
            stored(
                2,
                K::HostAssigned,
                host_assigned(ChannelId::new(31), MessageId::new(103), 1001, &[]),
            ),
            stored(3, K::VcAssigned, vc_assigned(&other, &ids(201, 202), &[])),
        ];
        let record = DiscordRecord::from_log(&log(events));
        assert_eq!(record.vc.unwrap().label, "4");
        assert_eq!(record.assignment_message_ids, ids(201, 202));
        assert_eq!(record.host_prompt, None);
    }

    #[test]
    fn the_persistence_helper_and_the_adapter_agree_on_the_vc_payload() {
        // Phase 3 のヘルパで積んだ vc_assigned も読める（VC の JSON の形が同じ）
        let event = NewEvent::vc_assigned("3", 31, 32, Utc::now(), "k");
        let events = vec![stored(1, K::VcAssigned, event.payload)];
        let record = DiscordRecord::from_log(&log(events));
        assert_eq!(record.vc, Some(pair()));
        assert!(record.assignment_message_ids.is_empty());
    }

    #[test]
    fn unreadable_ids_are_ignored_not_fatal() {
        let events = vec![stored(1, K::VcAssigned, json!({ VC_KEY: "3" }))];
        assert_eq!(
            DiscordRecord::from_log(&log(events)),
            DiscordRecord::default()
        );
        let events = vec![stored(
            1,
            K::VcAssigned,
            json!({ VC_KEY: { VC_LABEL_KEY: "3", VC_RED_CHANNEL_ID_KEY: "31", VC_BLUE_CHANNEL_ID_KEY: "32" },
                    ASSIGNMENT_MESSAGE_IDS_KEY: { "red": 5, "blue": "0" } }),
        )];
        let record = DiscordRecord::from_log(&log(events));
        assert_eq!(record.vc, Some(pair()));
        assert!(record.assignment_message_ids.is_empty());
    }

    #[test]
    fn code_posted_payload_keeps_the_room_code_readable_by_the_state_machine() {
        let payload = code_posted("9999", &ids(5, 6), &ids(7, 8), 1, &[]);
        assert_eq!(
            decode_payload(K::CodePosted, &payload),
            EventPayload::RoomCode("9999".into())
        );
        assert_eq!(payload[CODE_MESSAGE_IDS_KEY]["red"], "5");
        assert_eq!(payload[REPORT_MESSAGE_IDS_KEY]["blue"], "8");
        assert!(payload.get(DISCORD_FAILURES_KEY).is_none());
    }

    #[test]
    fn failures_are_recorded_and_the_reason_stays_readable() {
        let failures = [
            failure("send:assignment:red"),
            failure("send:abandoned_notice:blue"),
        ];
        let payload = abandoned(AbandonReason::DiscordCallFailed, &failures);
        assert_eq!(
            decode_payload(K::Abandoned, &payload),
            EventPayload::Abandon(AbandonReason::DiscordCallFailed)
        );
        let recorded = payload[DISCORD_FAILURES_KEY].as_array().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0][OPERATION_KEY], "send:assignment:red");
        assert_eq!(recorded[0][ATTEMPTS_KEY], 3);

        let payload = abandoned(AbandonReason::VcPoolExhausted, &[]);
        assert_eq!(
            decode_payload(K::Abandoned, &payload),
            EventPayload::Abandon(AbandonReason::VcPoolExhausted)
        );
        assert!(payload.get(DISCORD_FAILURES_KEY).is_none());

        let payload = host_response(1001, &failures[..1]);
        assert_eq!(
            payload[DISCORD_FAILURES_KEY][0][OPERATION_KEY],
            "send:assignment:red"
        );
    }
}
