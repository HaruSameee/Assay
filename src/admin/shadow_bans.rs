//! シャドウバン（運営操作）。**追記専用で、現在値は対象ごとの最新行から導出する。**
//!
//! 表立って BAN すると荒れるので、**断らずに組まれないようにする**。対象は 2 種類:
//!
//! - **IP**: その IP からキューに入った行
//! - **Discord ユーザー ID**: そのアカウントでキューに入った行（IP を変えても効く）
//!
//! どちらかに当たる投入は `queue_entries.shadow_banned` が立ち、マッチメイカは候補から外す。
//! 本人の画面は「キュー中」のまま何も変わらない。他の参加者の「同じ試合に入れる人」にも数えない。
//!
//! - 効くのは**次の投入から**。いまキューにいる行は書き換えない（投入時点の判定の写し）
//! - IP は完全一致。スマホ回線（CGNAT）は無関係な人が同じ IP を共有するので、
//!   巻き添えが出うる。運営パネルのキュー一覧で「組まれない」印を見て気付けるようにしてある
//! - 解除は `unbanned` を 1 行積む。同じ対象に何度でも積める
//!
//! IP をどう知るかは [`crate::registration::watch`]（監視対象のアクセス記録と係争チャンネルへの通知）。

use chrono::{DateTime, Utc};

/// BAN の対象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BanSubject {
    /// 接続元の IP（文字列のまま。IPv4 / IPv6 どちらも）。
    Ip(String),
    /// Discord ユーザー ID。
    DiscordUser(i64),
}

impl BanSubject {
    /// DB の `subject_kind`。
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Ip(_) => "ip",
            Self::DiscordUser(_) => "discord_user",
        }
    }

    /// DB の `subject`。
    pub fn value(&self) -> String {
        match self {
            Self::Ip(ip) => ip.clone(),
            Self::DiscordUser(id) => id.to_string(),
        }
    }

    /// 画面の言葉。
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::Ip(_) => "IP",
            Self::DiscordUser(_) => "Discord ID",
        }
    }

    /// DB の行から戻す。知らない種別は `None`。
    pub fn from_row(kind: &str, subject: &str) -> Option<Self> {
        match kind {
            "ip" => Some(Self::Ip(subject.to_owned())),
            "discord_user" => subject.parse().ok().map(Self::DiscordUser),
            _ => None,
        }
    }

    /// 画面から来た 1 つの文字列を対象にする。**純粋関数。**
    ///
    /// **数字だけなら Discord ユーザー ID、それ以外は IP。** Discord の ID（snowflake）は
    /// 17〜20 桁の数字で、IPv4 は必ず `.`、IPv6 は必ず `:` を含むので取り違えない。
    /// 空・64 文字超・空白入りは打ち間違いなので断る。IP の形式の厳密な検証はしない:
    /// 突き合わせは完全一致なので、おかしな値を入れても誰にも当たらないだけ。
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > 64 || raw.chars().any(char::is_whitespace) {
            return None;
        }
        if raw.chars().all(|c| c.is_ascii_digit()) {
            return raw.parse::<i64>().ok().map(Self::DiscordUser);
        }
        Some(Self::Ip(raw.to_owned()))
    }
}

/// いま BAN 中の対象 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowBan {
    pub subject: BanSubject,
    /// BAN したときの理由。
    pub reason: String,
    /// 操作した運営（`discord:<id>`）。
    pub created_by: String,
    pub banned_at: DateTime<Utc>,
}

/// 変更ログ 1 行（新しい順で出す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowBanChange {
    pub id: i64,
    pub subject: BanSubject,
    /// `banned` / `unbanned`。
    pub kind: ShadowBanKind,
    pub reason: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// 追記の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowBanKind {
    Banned,
    Unbanned,
}

impl ShadowBanKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Banned => "banned",
            Self::Unbanned => "unbanned",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "banned" => Some(Self::Banned),
            "unbanned" => Some(Self::Unbanned),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Banned => "BAN",
            Self::Unbanned => "解除",
        }
    }
}

/// 画面に出す値。**すべて導出値。**
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShadowBanPreview {
    /// いま BAN 中の対象（新しい順）。
    pub current: Vec<ShadowBan>,
    /// いまキューにいて、投入時点で BAN 中だった行の数。
    pub shadow_banned_in_queue: usize,
    /// 変更履歴（新しい順、最大 50 件）。
    pub history: Vec<ShadowBanChange>,
}

/// 追記の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowBanOutcome {
    /// 追記した。
    Changed {
        /// `shadow_ban_changes.id`。
        id: i64,
    },
    /// 同じ冪等キーで既に追記済み。
    Duplicate,
    /// 状態が既にそうなっている（BAN 中の対象をまた BAN、BAN していない対象を解除）。
    /// **行は積まない**（ログを無駄に増やさない）。
    Unchanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **数字だけなら Discord ID、それ以外は IP。**空白・長すぎは断る。
    #[test]
    fn subjects_are_told_apart_by_shape() {
        assert_eq!(
            BanSubject::parse(" 203.0.113.9 "),
            Some(BanSubject::Ip("203.0.113.9".to_owned()))
        );
        assert_eq!(
            BanSubject::parse("2001:db8::1"),
            Some(BanSubject::Ip("2001:db8::1".to_owned()))
        );
        assert_eq!(
            BanSubject::parse("900000000000000004"),
            Some(BanSubject::DiscordUser(900_000_000_000_000_004))
        );
        assert_eq!(BanSubject::parse(""), None);
        assert_eq!(BanSubject::parse("203.0.113.9 203.0.113.10"), None);
        assert_eq!(BanSubject::parse(&"1".repeat(65)), None);
        // i64 に入らない数字列は断る
        assert_eq!(BanSubject::parse(&"9".repeat(30)), None);
    }

    #[test]
    fn subjects_round_trip_through_the_row_shape() {
        for subject in [
            BanSubject::Ip("198.51.100.7".to_owned()),
            BanSubject::DiscordUser(42),
        ] {
            assert_eq!(
                BanSubject::from_row(subject.kind_str(), &subject.value()),
                Some(subject)
            );
        }
        assert_eq!(BanSubject::from_row("x", "1"), None);
        assert_eq!(BanSubject::from_row("discord_user", "abc"), None);
    }

    #[test]
    fn kinds_round_trip() {
        for kind in [ShadowBanKind::Banned, ShadowBanKind::Unbanned] {
            assert_eq!(ShadowBanKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ShadowBanKind::parse("x"), None);
    }
}
