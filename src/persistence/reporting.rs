//! Phase 6 report evaluation. This module is server-internal and is never exposed by HTTP.

use super::model::{Outcome, ReportDecision, ReportEvidence};

/// 最新報告だけで確定状態を決める。全員報告済みで一致なら即時、それ以外は期限まで待つ。
pub(crate) fn confirmation_from_latest_reports(
    evidence: &ReportEvidence,
    report_window_elapsed: bool,
) -> ReportDecision {
    let latest = &evidence.latest_for_confirmation;
    if latest.is_empty() {
        return ReportDecision::Pending;
    }
    let unanimous = latest.iter().all(|vote| vote.outcome == latest[0].outcome);
    let all_reported = latest.len() == evidence.participant_count;

    if unanimous && (all_reported || report_window_elapsed) {
        return match latest[0].outcome {
            Outcome::Nullified => ReportDecision::Nullified,
            outcome => ReportDecision::Confirmed(outcome),
        };
    }
    if report_window_elapsed {
        ReportDecision::Disputed
    } else {
        ReportDecision::Pending
    }
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
    use crate::lifecycle::PlayerId;
    use crate::persistence::model::ReportVote;
    use crate::persistence::{ReportId, Team};

    fn vote(id: i64, player: u128, outcome: Outcome) -> ReportVote {
        ReportVote {
            id: ReportId(id),
            player_id: PlayerId(Uuid::from_u128(player)),
            outcome,
        }
    }

    #[test]
    fn correction_changes_confirmation_but_not_first_report_metric() {
        let red = Outcome::Win(Team::Red);
        let blue = Outcome::Win(Team::Blue);
        let evidence = ReportEvidence {
            latest_for_confirmation: vec![vote(2, 1, red), vote(3, 2, red)],
            first_for_agreement_metric: vec![vote(1, 1, blue), vote(3, 2, red)],
            participant_count: 2,
        };

        assert_eq!(
            confirmation_from_latest_reports(&evidence, false),
            ReportDecision::Confirmed(red)
        );
        assert_eq!(first_report_agreement_counts(&evidence, red), (1, 2));
    }
}
