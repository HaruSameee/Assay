//! IP のシャドウバン（運営操作）。**追記専用で、現在値は IP ごとの最新行から導出する。**
//!
//! 表立って BAN すると荒れるので、**断らずに組まれないようにする**。BAN 中の IP から
//! キューに入った行は `queue_entries.shadow_banned` が立ち、マッチメイカは候補から外す。
//! 本人の画面は「キュー中」のまま何も変わらない。他の参加者の「範囲内 N 人」にも数えない。
//!
//! - 効くのは**次の投入から**。いまキューにいる行は書き換えない（投入時点の判定の写し）
//! - IP は完全一致。スマホ回線（CGNAT）は無関係な人が同じ IP を共有するので、
//!   巻き添えが出うる。運営パネルのキュー一覧で「組まれない」印を見て気付けるようにしてある
//! - 解除は `unbanned` を 1 行積む。同じ IP に何度でも積める
//!
//! IP をどう知るかは [`crate::registration::watch`]（監視対象のアクセス記録と係争チャンネルへの通知）。

use chrono::{DateTime, Utc};

/// いま BAN 中の IP 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpBan {
    pub ip: String,
    /// BAN したときの理由。
    pub reason: String,
    /// 操作した運営（`discord:<id>`）。
    pub created_by: String,
    pub banned_at: DateTime<Utc>,
}

/// 変更ログ 1 行（新しい順で出す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpBanChange {
    pub id: i64,
    pub ip: String,
    /// `banned` / `unbanned`。
    pub kind: IpBanKind,
    pub reason: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// 追記の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpBanKind {
    Banned,
    Unbanned,
}

impl IpBanKind {
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
pub struct IpBanPreview {
    /// いま BAN 中の IP（新しい順）。
    pub current: Vec<IpBan>,
    /// いまキューにいて、投入時点で BAN 中だった行の数。
    pub shadow_banned_in_queue: usize,
    /// 変更履歴（新しい順、最大 50 件）。
    pub history: Vec<IpBanChange>,
}

/// 追記の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpBanOutcome {
    /// 追記した。
    Changed {
        /// `ip_ban_changes.id`。
        id: i64,
    },
    /// 同じ冪等キーで既に追記済み。
    Duplicate,
    /// 状態が既にそうなっている（BAN 中の IP をまた BAN、BAN していない IP を解除）。
    /// **行は積まない**（ログを無駄に増やさない）。
    Unchanged,
}

/// IP の文字列を整える。**純粋関数。**空・長すぎ・空白入りは断る。
///
/// 形式の厳密な検証（IPv4 / IPv6 として妥当か）はしない: 突き合わせは完全一致なので、
/// おかしな値を入れても誰にも当たらないだけ。ただし空白入りと 64 文字超は打ち間違いなので断る。
pub fn normalize_ip(raw: &str) -> Option<String> {
    let ip = raw.trim();
    if ip.is_empty() || ip.len() > 64 || ip.chars().any(char::is_whitespace) {
        return None;
    }
    Some(ip.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ips_are_trimmed_and_nonsense_is_refused() {
        assert_eq!(
            normalize_ip(" 203.0.113.9 "),
            Some("203.0.113.9".to_owned())
        );
        assert_eq!(normalize_ip("2001:db8::1"), Some("2001:db8::1".to_owned()));
        assert_eq!(normalize_ip(""), None);
        assert_eq!(normalize_ip("203.0.113.9 203.0.113.10"), None);
        assert_eq!(normalize_ip(&"1".repeat(65)), None);
    }

    #[test]
    fn kinds_round_trip() {
        for kind in [IpBanKind::Banned, IpBanKind::Unbanned] {
            assert_eq!(IpBanKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(IpBanKind::parse("x"), None);
    }
}
