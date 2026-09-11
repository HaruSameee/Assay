//! Phase 6 report evaluation. This module is server-internal and is never exposed by HTTP.

use super::model::{Outcome, ReportDecision, ReportEvidence};

/// 全員が報告し、その全員が一致した。
pub(crate) const BASIS_ALL_REPORTED: &str = "all_players_reported";
/// 受付期限が過ぎた時点で、報告した人の中で一致していた。
pub(crate) const BASIS_WINDOW_ELAPSED: &str = "report_window_elapsed";
/// 閾値以上の人数が同じ結果で一致した（早期確定）。
pub(crate) const BASIS_MAJORITY: &str = "majority_threshold";

/// 判定と、その根拠。根拠は `formation_events` の payload にそのまま載る。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Confirmation {
    /// 確定・無効試合・係争・保留のいずれか。
    pub decision: ReportDecision,
    /// 何を根拠に決めたか。`Pending` / `Disputed` のときは `None`。
    pub basis: Option<&'static str>,
}

impl Confirmation {
    fn pending() -> Self {
        Self {
            decision: ReportDecision::Pending,
            basis: None,
        }
    }

    fn disputed() -> Self {
        Self {
            decision: ReportDecision::Disputed,
            basis: None,
        }
    }

    fn settled(outcome: Outcome, basis: &'static str) -> Self {
        let decision = match outcome {
            Outcome::Nullified => ReportDecision::Nullified,
            outcome => ReportDecision::Confirmed(outcome),
        };
        Self {
            decision,
            basis: Some(basis),
        }
    }
}

/// 最新報告だけで確定状態を決める。
///
/// 経路は 2 つある。**どちらも「各プレイヤーの最新の報告」だけを見る**（訂正は反映される）。
///
/// 1. **全員一致**（従来）: unanimous かつ (all_reported または report_window_elapsed)。
///    この条件は変えていない。とくに**期限切れ時の判定は従来のまま**で、
///    「報告した人の中での全員一致」で確定・不一致なら係争になる。
/// 2. **早期確定**（追加）: 同じ結果に majority_threshold 人以上が一致したら即確定する。
///    勝敗と無効試合を区別しない——非対称にする理由が無い。
///
/// 1 を先に見るのは、既存の経路の根拠（all_players_reported / report_window_elapsed）を
/// 保つためである。全員一致は必ず閾値も満たすので、どちらを先に見ても結論は変わらない。
///
/// 早期確定は期限の前後を問わず効く。期限切れ時に閾値へ届いていなければ、
/// 従来どおり 1 の判定に落ちる。
pub(crate) fn confirmation_from_latest_reports(
    evidence: &ReportEvidence,
    report_window_elapsed: bool,
    majority_threshold: usize,
) -> Confirmation {
    let latest = &evidence.latest_for_confirmation;
    if latest.is_empty() {
        return Confirmation::pending();
    }
    let unanimous = latest.iter().all(|vote| vote.outcome == latest[0].outcome);
    let all_reported = latest.len() == evidence.participant_count;

    if unanimous && (all_reported || report_window_elapsed) {
        let basis = if all_reported {
            BASIS_ALL_REPORTED
        } else {
            BASIS_WINDOW_ELAPSED
        };
        return Confirmation::settled(latest[0].outcome, basis);
    }
    if let Some(outcome) = leading_outcome(evidence, majority_threshold) {
        return Confirmation::settled(outcome, BASIS_MAJORITY);
    }
    if report_window_elapsed {
        Confirmation::disputed()
    } else {
        Confirmation::pending()
    }
}

/// 閾値に届いた結果があればそれを返す。
///
/// **閾値は過半数（6 以上）を強制してある**ので、同時に 2 つの結果が届くことはない
/// （6 × 2 = 12 > 10）。5 以下を許すと 5 対 5 で両方が届き、ここが返す結果が
/// 報告の並び順に依存してしまう。検証は
/// [`crate::lifecycle::LifecycleConfig::parse`] 側。
fn leading_outcome(evidence: &ReportEvidence, majority_threshold: usize) -> Option<Outcome> {
    let latest = &evidence.latest_for_confirmation;
    latest.iter().map(|vote| vote.outcome).find(|outcome| {
        latest
            .iter()
            .filter(|vote| vote.outcome == *outcome)
            .count()
            >= majority_threshold
    })
}

/// 一致率は訂正後の最新値ではなく、各プレイヤーの最初の報告だけで算出する。
pub(crate) fn first_report_agreement_counts(
    evidence: &ReportEvidence,
    final_outcome: Outcome,
) -> (usize, usize) {
    let first = &evidence.first_for_agreement_metric;
    (
        first
            .iter()
            .filter(|vote| vote.outcome == final_outcome)
            .count(),
        first.len(),
    )
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::lifecycle::{DEFAULT_REPORT_MAJORITY_THRESHOLD, PlayerId};
    use crate::persistence::model::ReportVote;
    use crate::persistence::{ReportId, Team};

    const RED: Outcome = Outcome::Win(Team::Red);
    const BLUE: Outcome = Outcome::Win(Team::Blue);
    const VOID: Outcome = Outcome::Nullified;

    fn vote(id: i64, player: u128, outcome: Outcome) -> ReportVote {
        ReportVote {
            id: ReportId(id),
            player_id: PlayerId(Uuid::from_u128(player)),
            outcome,
        }
    }

    /// 10 人の試合で、渡した結果の並びだけが報告済み。
    fn evidence(outcomes: &[Outcome]) -> ReportEvidence {
        let votes: Vec<ReportVote> = outcomes
            .iter()
            .enumerate()
            .map(|(index, outcome)| vote(index as i64 + 1, index as u128 + 1, *outcome))
            .collect();
        ReportEvidence {
            latest_for_confirmation: votes.clone(),
            first_for_agreement_metric: votes,
            participant_count: 10,
        }
    }

    /// **閾値を下げると、2 つの結果が同時に閾値へ届きうる。**
    ///
    /// 参加者 10 人なので、閾値 5 以下では「赤 5・青 5」のように両方が届く。
    /// いまの `leading_outcome` は**先に見つかった方**を返すので、
    /// 結論が報告の並び順で変わってしまう。既定の 7 では起きない
    /// （7 × 2 = 14 > 10）が、設定は 2〜10 を許している。
    #[test]
    fn a_threshold_below_a_majority_would_depend_on_report_order() {
        use Outcome::Win;
        let red_first = evidence(&[
            Win(Team::Red),
            Win(Team::Red),
            Win(Team::Red),
            Win(Team::Red),
            Win(Team::Red),
            Win(Team::Blue),
            Win(Team::Blue),
            Win(Team::Blue),
            Win(Team::Blue),
            Win(Team::Blue),
        ]);
        let blue_first = evidence(&[
            Win(Team::Blue),
            Win(Team::Blue),
            Win(Team::Blue),
            Win(Team::Blue),
            Win(Team::Blue),
            Win(Team::Red),
            Win(Team::Red),
            Win(Team::Red),
            Win(Team::Red),
            Win(Team::Red),
        ]);
        // **下限が 6 なので、5 は設定できない。**仮に渡しても並び順で結論が変わる
        // ——それをここで記録しておく（設定側で弾く根拠）
        let a = confirmation_from_latest_reports(&red_first, false, 5);
        let b = confirmation_from_latest_reports(&blue_first, false, 5);
        assert_ne!(a.decision, b.decision, "5 以下が危険である根拠");

        // **許される最小の 6 では起きない**（6 × 2 = 12 > 10）
        let a = confirmation_from_latest_reports(&red_first, true, 6);
        let b = confirmation_from_latest_reports(&blue_first, true, 6);
        assert_eq!(a.decision, b.decision);
        println!("閾値 6: どちらも {:?}", a.decision);

        let a = confirmation_from_latest_reports(&red_first, true, 7);
        let b = confirmation_from_latest_reports(&blue_first, true, 7);
        assert_eq!(a.decision, b.decision);
        println!("閾値 7: どちらも {:?}", a.decision);
    }

    fn decide(outcomes: &[Outcome], elapsed: bool) -> Confirmation {
        confirmation_from_latest_reports(
            &evidence(outcomes),
            elapsed,
            DEFAULT_REPORT_MAJORITY_THRESHOLD,
        )
    }

    fn same(outcome: Outcome, count: usize) -> Vec<Outcome> {
        vec![outcome; count]
    }

    #[test]
    fn correction_changes_confirmation_but_not_first_report_metric() {
        let evidence = ReportEvidence {
            latest_for_confirmation: vec![vote(2, 1, RED), vote(3, 2, RED)],
            first_for_agreement_metric: vec![vote(1, 1, BLUE), vote(3, 2, RED)],
            participant_count: 2,
        };

        let confirmation =
            confirmation_from_latest_reports(&evidence, false, DEFAULT_REPORT_MAJORITY_THRESHOLD);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        assert_eq!(confirmation.basis, Some(BASIS_ALL_REPORTED));
        assert_eq!(first_report_agreement_counts(&evidence, RED), (1, 2));
    }

    /// 1. **7 人が同一結果 → 即確定。**期限前でも、残り 3 人を待たない。
    #[test]
    fn seven_matching_reports_settle_immediately() {
        let confirmation = decide(&same(RED, 7), false);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        assert_eq!(confirmation.basis, Some(BASIS_MAJORITY));
    }

    /// 2. **6 人では確定しない。**
    #[test]
    fn six_matching_reports_do_not_settle() {
        assert_eq!(
            decide(&same(RED, 6), false).decision,
            ReportDecision::Pending
        );
    }

    /// 3. **7 人が「無効試合」で一致 → nullified。**勝敗と非対称にしない。
    #[test]
    fn seven_nullified_reports_settle_as_nullified() {
        let confirmation = decide(&same(VOID, 7), false);
        assert_eq!(confirmation.decision, ReportDecision::Nullified);
        assert_eq!(confirmation.basis, Some(BASIS_MAJORITY));
    }

    /// 4. **4 対 3 に割れた 7 人では確定しない。**報告の頭数ではなく一致数を見る。
    #[test]
    fn a_split_of_four_and_three_does_not_settle() {
        let mut votes = same(RED, 4);
        votes.extend(same(BLUE, 3));
        assert_eq!(decide(&votes, false).decision, ReportDecision::Pending);
    }

    /// 6. **5 人が割れたまま期限切れ → 従来どおり係争。**期限切れの判定は変えていない。
    #[test]
    fn a_split_at_the_deadline_is_still_disputed() {
        let mut votes = same(RED, 3);
        votes.extend(same(BLUE, 2));
        assert_eq!(decide(&votes, true).decision, ReportDecision::Disputed);
    }

    /// 7. **8 対 2 なら 8 人側で確定する。**
    #[test]
    fn eight_against_two_settles_on_the_larger_side() {
        let mut votes = same(RED, 8);
        votes.extend(same(BLUE, 2));
        let confirmation = decide(&votes, false);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        assert_eq!(confirmation.basis, Some(BASIS_MAJORITY));
    }

    /// **期限切れ時の従来の判定が回帰していないこと。**
    ///
    /// 報告した人の中で全員一致していれば、閾値に届いていなくても確定する。
    #[test]
    fn the_deadline_still_settles_on_unanimity_below_the_threshold() {
        let confirmation = decide(&same(RED, 5), true);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        assert_eq!(confirmation.basis, Some(BASIS_WINDOW_ELAPSED));
    }

    /// **全員一致は従来の根拠のまま。**閾値も満たすが all_players_reported を保つ。
    #[test]
    fn ten_matching_reports_keep_the_existing_basis() {
        let confirmation = decide(&same(RED, 10), false);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        assert_eq!(confirmation.basis, Some(BASIS_ALL_REPORTED));
    }

    /// **本番で係争になった 4 件が、閾値 7 なら確定していたこと。**
    ///
    /// 2026-09-09 の 4 件の内訳（formation_events の latest_report_breakdown）を
    /// そのまま入れて、判定が変わることを固定する。
    #[test]
    fn the_four_production_disputes_would_have_settled() {
        for (name, red, void, blue) in [
            ("9878932e", 8usize, 2usize, 0usize),
            ("416f98df", 9, 1, 0),
            ("0b264972", 9, 1, 0),
            ("cf9510fc", 9, 0, 1),
        ] {
            let mut votes = same(RED, red);
            votes.extend(same(VOID, void));
            votes.extend(same(BLUE, blue));
            assert_eq!(votes.len(), 10, "{name}");

            // 従来の条件だけなら、期限切れで係争になっていた
            let evidence = evidence(&votes);
            let old = confirmation_from_latest_reports(&evidence, true, usize::MAX);
            assert_eq!(old.decision, ReportDecision::Disputed, "{name}");

            // 閾値 7 では即確定する（期限を待たない）
            let new = decide(&votes, false);
            assert_eq!(new.decision, ReportDecision::Confirmed(RED), "{name}");
            assert_eq!(new.basis, Some(BASIS_MAJORITY), "{name}");
            println!("{name}: red {red} / nullified {void} / blue {blue} -> 確定（従来は係争）");
        }
    }

    /// 閾値は設定で動く。その値どおりに効く。
    #[test]
    fn the_threshold_is_configurable() {
        let votes = same(RED, 3);
        let evidence = evidence(&votes);
        assert_eq!(
            confirmation_from_latest_reports(&evidence, false, 3).decision,
            ReportDecision::Confirmed(RED)
        );
        assert_eq!(
            confirmation_from_latest_reports(&evidence, false, 4).decision,
            ReportDecision::Pending
        );
    }
}
