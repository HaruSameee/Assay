//! ホストの選び方（仕様 §2）。`HostSelector` trait で差し替え可能。

use super::config::LifecycleConfig;
use super::ids::{FormationId, PlayerId, Timestamp};

/// ホスト選定に必要な最小限の formation ビュー。
///
/// DB 行からの変換は Phase 3 で行う。ここでは選定に使う値だけを持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct Formation {
    /// formation ID。
    pub id: FormationId,
    /// 参加者（通常 10 人）。並び順は同点時の優先順になる。
    pub players: Vec<FormationPlayer>,
}

/// ホスト選定に使う参加者の情報。
#[derive(Debug, Clone, PartialEq)]
pub struct FormationPlayer {
    /// プレイヤー ID。
    pub player_id: PlayerId,
    /// マッチング時点の μ（`formation_players.mu_at_match`）。
    pub mu_at_match: f64,
    /// 直近にホストをした時刻。一度もしていなければ `None`。
    ///
    /// 列は作らない。`formation_events` の `host_accepted`（`subject_player_id` = 本人）の
    /// 最新 `occurred_at` から呼び出し側が導出して渡す。ログと食い違いうる可変フィールドを増やさないため。
    /// 全員 `None` なら [`LeastRecentlyHosted`] は [`HighestRating`] と同じ順になる。
    pub last_hosted_at: Option<Timestamp>,
}

/// ホスト候補の選び方。差し替え可能。
pub trait HostSelector {
    /// 優先順に並べたホスト候補を返す。先頭から順に打診する。
    ///
    /// 返す人数は候補上限（[`LifecycleConfig::max_host_candidates`]）まで。
    fn candidates(&self, formation: &Formation) -> Vec<PlayerId>;
}

impl<T: HostSelector + ?Sized> HostSelector for Box<T> {
    fn candidates(&self, formation: &Formation) -> Vec<PlayerId> {
        (**self).candidates(formation)
    }
}

/// レートが高い順（仕様 §2 の現在の決定。デフォルト）。
///
/// 同点は formation 内の並び順。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighestRating {
    /// 返す候補の上限。
    pub max_candidates: usize,
}

impl HighestRating {
    /// 設定の候補上限で作る。
    pub fn from_config(config: &LifecycleConfig) -> Self {
        Self {
            max_candidates: config.max_host_candidates as usize,
        }
    }
}

impl Default for HighestRating {
    fn default() -> Self {
        Self::from_config(&LifecycleConfig::default())
    }
}

impl HostSelector for HighestRating {
    fn candidates(&self, formation: &Formation) -> Vec<PlayerId> {
        let mut players: Vec<&FormationPlayer> = formation.players.iter().collect();
        players.sort_by(|a, b| b.mu_at_match.total_cmp(&a.mu_at_match));
        players
            .into_iter()
            .take(self.max_candidates)
            .map(|p| p.player_id)
            .collect()
    }
}

/// 直近ホスト時刻が最も古い順（仕様 §2 の代案）。
///
/// 一度もホストしていない人が最優先。同点はレート降順、それでも同点なら formation 内の並び順。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeastRecentlyHosted {
    /// 返す候補の上限。
    pub max_candidates: usize,
}

impl LeastRecentlyHosted {
    /// 設定の候補上限で作る。
    pub fn from_config(config: &LifecycleConfig) -> Self {
        Self {
            max_candidates: config.max_host_candidates as usize,
        }
    }
}

impl Default for LeastRecentlyHosted {
    fn default() -> Self {
        Self::from_config(&LifecycleConfig::default())
    }
}

impl HostSelector for LeastRecentlyHosted {
    fn candidates(&self, formation: &Formation) -> Vec<PlayerId> {
        let mut players: Vec<&FormationPlayer> = formation.players.iter().collect();
        // Option の順序は None < Some なので、未経験者が先頭に来る
        players.sort_by(|a, b| {
            a.last_hosted_at
                .cmp(&b.last_hosted_at)
                .then_with(|| b.mu_at_match.total_cmp(&a.mu_at_match))
        });
        players
            .into_iter()
            .take(self.max_candidates)
            .map(|p| p.player_id)
            .collect()
    }
}

/// デフォルトのホスト選定（仕様 §2「決着のさせ方」: 初週は `HighestRating`）。
pub type DefaultHostSelector = HighestRating;

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;

    fn pid(n: u128) -> PlayerId {
        PlayerId(Uuid::from_u128(n))
    }

    fn at(seq: i64) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seq)
    }

    fn formation(players: Vec<(u128, f64, Option<Timestamp>)>) -> Formation {
        Formation {
            id: FormationId(Uuid::from_u128(0xf0)),
            players: players
                .into_iter()
                .map(|(id, mu, last_hosted_at)| FormationPlayer {
                    player_id: pid(id),
                    mu_at_match: mu,
                    last_hosted_at,
                })
                .collect(),
        }
    }

    #[test]
    fn highest_rating_returns_top_three_by_mu() {
        let f = formation((1..=10).map(|i| (i, i as f64, None)).collect());
        assert_eq!(
            HighestRating::default().candidates(&f),
            vec![pid(10), pid(9), pid(8)]
        );
    }

    #[test]
    fn highest_rating_ties_keep_formation_order() {
        let f = formation(vec![
            (1, 25.0, None),
            (2, 25.0, None),
            (3, 25.0, None),
            (4, 25.0, None),
        ]);
        assert_eq!(
            HighestRating::default().candidates(&f),
            vec![pid(1), pid(2), pid(3)]
        );
    }

    #[test]
    fn candidate_limit_comes_from_config() {
        let f = formation((1..=10).map(|i| (i, i as f64, None)).collect());
        let config = LifecycleConfig {
            max_host_candidates: 5,
            ..LifecycleConfig::default()
        };
        assert_eq!(HighestRating::from_config(&config).candidates(&f).len(), 5);
        assert_eq!(
            LeastRecentlyHosted::from_config(&config)
                .candidates(&f)
                .len(),
            5
        );
        let two = formation(vec![(1, 1.0, None), (2, 2.0, None)]);
        assert_eq!(
            HighestRating::default().candidates(&two),
            vec![pid(2), pid(1)]
        );
        assert!(
            HighestRating::default()
                .candidates(&formation(vec![]))
                .is_empty()
        );
    }

    #[test]
    fn least_recently_hosted_prefers_never_hosted_then_oldest_then_mu() {
        let f = formation(vec![
            (1, 20.0, Some(at(100))), // A: 最近ホストした
            (2, 10.0, None),          // B: 未経験、低レート
            (3, 30.0, None),          // C: 未経験、高レート
            (4, 40.0, Some(at(50))),  // D: 少し前にホストした
        ]);
        assert_eq!(
            LeastRecentlyHosted::default().candidates(&f),
            vec![pid(3), pid(2), pid(4)]
        );
    }

    #[test]
    fn least_recently_hosted_without_data_equals_highest_rating() {
        let f = formation((1..=10).map(|i| (i, (i * 7 % 10) as f64, None)).collect());
        assert_eq!(
            LeastRecentlyHosted::default().candidates(&f),
            HighestRating::default().candidates(&f)
        );
    }

    #[test]
    fn selectors_can_be_boxed() {
        let f = formation((1..=10).map(|i| (i, i as f64, None)).collect());
        let boxed: Box<dyn HostSelector> = Box::new(DefaultHostSelector::default());
        assert_eq!(boxed.candidates(&f).len(), 3);
    }
}
