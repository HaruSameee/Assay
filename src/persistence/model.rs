//! 永続化層の入出力型。

use serde_json::{Map, Value};

use crate::lifecycle::{
    AbandonReason, Anomaly, FormationEvent, FormationEventKind, FormationId, FormationState,
    PlayerId, Snapshot, Timestamp, UnknownValue, derive_snapshot_iter,
};

use super::codec::{
    ABANDON_REASON_KEY, ROOM_CODE_KEY, VC_BLUE_CHANNEL_ID_KEY, VC_KEY, VC_LABEL_KEY,
    VC_RED_CHANNEL_ID_KEY, decode_payload,
};

/// `formation_events.id`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(pub i64);

/// `match_reports.id`。
///
/// [`super::FormationRepository::append_report`] が返すのはこれだけ。確定状態・係争の有無・
/// フラグは返さない（報告者が反応を見てから訂正で指標をすり抜けるのを防ぐ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReportId(pub i64);

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::fmt::Display for ReportId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// チーム（`formation_players.team`: `red` または `blue`）。DB の値がそのまま名前。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Team {
    /// team = 'red'
    Red,
    /// team = 'blue'
    Blue,
}

impl Team {
    /// 全チーム。
    pub const ALL: [Self; 2] = [Self::Red, Self::Blue];

    /// DB の値（表示名を兼ねる）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Blue => "blue",
        }
    }
}

impl std::fmt::Display for Team {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Team {
    type Err = UnknownValue;

    fn from_str(s: &str) -> Result<Self, UnknownValue> {
        Self::ALL
            .into_iter()
            .find(|team| team.as_str() == s)
            .ok_or_else(|| UnknownValue {
                what: "team",
                value: s.to_owned(),
            })
    }
}

/// 報告の結果（`match_reports.outcome`）。引き分けは NULL ではなく [`Outcome::Draw`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// どちらかのチームの勝ち。
    Win(Team),
    /// 引き分け。
    Draw,
}

impl Outcome {
    /// DB の値（`red` / `blue` / `draw`）。勝ったチームは `formation_players.team` と同じ値なので、
    /// 勝者側のプレイヤーは `where fp.team = mr.outcome` で引ける。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Win(team) => team.as_str(),
            Self::Draw => "draw",
        }
    }
}

/// マッチメイカが確定した formation。INSERT 後は不変。
#[derive(Debug, Clone, PartialEq)]
pub struct NewFormation {
    /// formation ID（アプリが採番する）。
    pub id: FormationId,
    /// 確定時刻。
    pub created_at: Timestamp,
    /// マッチング時点のパラメータ版数（凍結値）。
    pub matchmaking_params_version: i32,
    /// 予測勝率（凍結値）。
    pub predicted_win_prob: f32,
    /// 参加者。10人、各チーム5人。
    pub players: Vec<NewFormationPlayer>,
}

/// formation の参加者 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct NewFormationPlayer {
    /// プレイヤー ID。
    pub player_id: PlayerId,
    /// チーム。
    pub team: Team,
    /// ロール（`roles.id`）。
    pub role: String,
    /// フレックスで埋められたか。
    pub role_was_flexed: bool,
    /// マッチング時点の μ。
    pub mu_at_match: f64,
    /// マッチング時点の σ。
    pub sigma_at_match: f64,
}

/// 追記するイベント。
///
/// `payload` は DB に入れる生の JSON。状態機械が読む項目（部屋コード、解散理由）は
/// その中から [`decode_payload`] で取り出す。読まない項目（VC の割当など）もそのまま残る。
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    /// 種別。
    pub kind: FormationEventKind,
    /// このイベントが対象とするプレイヤー（`subject_player_id`）。
    pub subject: Option<PlayerId>,
    /// 発生時刻。
    pub occurred_at: Timestamp,
    /// 冪等キー（formation 内で一意）。
    pub idempotency_key: String,
    /// 生の payload。
    pub payload: Value,
}

/// DB 上で確保する VC の組。
///
/// Discord の型を永続化層へ持ち込まないため、チャンネル ID は snowflake の数値で表す。
/// `vc_assigned` の payload には文字列として保存する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcAssignment {
    /// プール内で一意なラベル。
    pub label: String,
    /// red 側 VC のチャンネル ID。
    pub red_channel_id: u64,
    /// blue 側 VC のチャンネル ID。
    pub blue_channel_id: u64,
}

impl NewEvent {
    /// payload が空のイベント。
    pub fn new(
        kind: FormationEventKind,
        subject: Option<PlayerId>,
        occurred_at: Timestamp,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            subject,
            occurred_at,
            idempotency_key: idempotency_key.into(),
            payload: Value::Object(Map::new()),
        }
    }

    /// payload を差し替える。
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    /// `vc_assigned`。VC の組（ラベルと red / blue のチャンネル ID）。ID は JSON では文字列で持つ。
    pub fn vc_assigned(
        label: &str,
        red_channel_id: u64,
        blue_channel_id: u64,
        occurred_at: Timestamp,
        idempotency_key: impl Into<String>,
    ) -> Self {
        let mut vc = Map::new();
        vc.insert(VC_LABEL_KEY.to_owned(), Value::String(label.to_owned()));
        vc.insert(
            VC_RED_CHANNEL_ID_KEY.to_owned(),
            Value::String(red_channel_id.to_string()),
        );
        vc.insert(
            VC_BLUE_CHANNEL_ID_KEY.to_owned(),
            Value::String(blue_channel_id.to_string()),
        );
        Self::new(
            FormationEventKind::VcAssigned,
            None,
            occurred_at,
            idempotency_key,
        )
        .with_payload(object(VC_KEY, Value::Object(vc)))
    }

    /// `code_posted`（subject = ホスト）。
    pub fn code_posted(
        host: PlayerId,
        room_code: &str,
        occurred_at: Timestamp,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self::new(
            FormationEventKind::CodePosted,
            Some(host),
            occurred_at,
            idempotency_key,
        )
        .with_payload(object(ROOM_CODE_KEY, Value::String(room_code.to_owned())))
    }

    /// `abandoned`。
    pub fn abandoned(
        reason: AbandonReason,
        occurred_at: Timestamp,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self::new(
            FormationEventKind::Abandoned,
            None,
            occurred_at,
            idempotency_key,
        )
        .with_payload(object(
            ABANDON_REASON_KEY,
            Value::String(reason.as_str().to_owned()),
        ))
    }

    /// 状態機械に渡す形。
    pub fn to_core(&self) -> FormationEvent {
        FormationEvent {
            kind: self.kind,
            subject: self.subject,
            occurred_at: self.occurred_at,
            idempotency_key: self.idempotency_key.clone(),
            payload: decode_payload(self.kind, &self.payload),
        }
    }
}

fn object(key: &str, value: Value) -> Value {
    let mut map = Map::new();
    map.insert(key.to_owned(), value);
    Value::Object(map)
}

/// 追記する報告。
#[derive(Debug, Clone, PartialEq)]
pub struct NewReport {
    /// 対象の formation。
    pub formation_id: FormationId,
    /// 報告者（当該 formation の参加者でなければ複合 FK で弾かれる）。
    pub player_id: PlayerId,
    /// 報告時刻。
    pub reported_at: Timestamp,
    /// 報告者が主張する結果。
    pub outcome: Outcome,
    /// 報告された生データ。受け取ったものをそのまま入れる（パースに失敗しても捨てない）。
    pub raw_result: Value,
    /// 打ち消す以前の報告（同じプレイヤー・同じ formation のものに限る）。
    pub supersedes_id: Option<ReportId>,
}

/// 永続化済みのイベント（DB の id 付き）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    /// `formation_events.id`。
    pub id: EventId,
    /// 状態機械に渡す形。
    pub event: FormationEvent,
    /// DB に入っている生の payload。状態機械が読まない項目（Discord のチャンネル ID・
    /// メッセージ ID・VC 割当など）はここから読む。副作用の記録の正はこの列。
    pub payload: Value,
}

/// formation の参加者 1 人分の編成（`formation_players` に `players` / `roles` を結合したもの）。
///
/// Discord アダプタが割り当て表を作り、インタラクションの押下者を照合するのに使う。
#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    /// プレイヤー ID。
    pub player_id: PlayerId,
    /// `players.discord_user_id`。
    pub discord_user_id: i64,
    /// チーム。
    pub team: Team,
    /// ロール（`roles.id`）。
    pub role: String,
    /// ロールの表示名（`roles.display_name`）。
    pub role_display_name: String,
    /// ロールの表示順（`roles.sort_order`）。
    pub role_sort_order: i32,
    /// マッチング時点の μ。
    pub mu_at_match: f64,
}

/// 読めなかった行。捨てずに呼び出し側へ返す（ログとスタッフ通知に回す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableRow {
    /// `formation_events.id`。
    pub id: EventId,
    /// 行の kind 文字列。
    pub kind: String,
    /// 行の冪等キー。
    pub idempotency_key: String,
    /// 読めなかった理由。
    pub reason: String,
}

/// formation のヘッダとイベント列（id 順）。
#[derive(Debug, Clone, PartialEq)]
pub struct FormationLog {
    /// formation ID。
    pub formation_id: FormationId,
    /// `formations.created_at`。
    pub created_at: Timestamp,
    /// 読めたイベント（id 順）。
    pub events: Vec<StoredEvent>,
    /// 読めなかった行。
    pub unreadable: Vec<UnreadableRow>,
}

impl FormationLog {
    /// 現在状態を導出する。失敗しない。
    pub fn snapshot(&self) -> Snapshot {
        derive_snapshot_iter(self.events.iter().map(|stored| &stored.event))
    }

    /// 期限計算の起点。状態を変えたイベントが無ければ `formations.created_at`。
    pub fn since(&self, snapshot: &Snapshot) -> Timestamp {
        snapshot.since.unwrap_or(self.created_at)
    }
}

/// 追記の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendOutcome {
    /// 追記した（または既に存在した）イベントの id。
    pub event_id: EventId,
    /// 追記後の状態。
    pub state: FormationState,
    /// 同じ冪等キーのイベントが既にあり、何も書かなかった。
    pub already_recorded: bool,
    /// 追記前の導出で見つかった異常。空でなければログとスタッフ通知に回す。
    pub anomalies: Vec<Anomaly>,
}

/// 未終了の formation（起動時のリカバリ用）。
#[derive(Debug, Clone, PartialEq)]
pub struct UnfinishedFormation {
    /// イベント列。
    pub log: FormationLog,
    /// 導出した現在状態。
    pub snapshot: Snapshot,
}
