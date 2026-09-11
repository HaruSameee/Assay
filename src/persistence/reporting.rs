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

/// 最新報告だけで確定状態を決める。**各プレイヤーの最新の報告**だけを見る（訂正は反映される）。
///
/// **結果を決められるのは、同じ結果に majority_threshold 人以上（既定 6 = 過半数）が
/// 一致したときだけ。**それより少ない人数では、何人が一致していても、期限が過ぎても
/// 確定しない。
///
/// 以前は「期限切れ時に、報告した人の中で全員一致していれば確定」も許していた。
/// これだと **1 人が試合開始直後に誤って（または故意に）報告し、15 分後の期限で
/// その 1 票だけで確定する**——本番で実際に起きた（2026-09-12 の match-c4389f1e:
/// 開始 1 分後の 1 票「青の勝ち」で確定し、その後の 8 票「赤の勝ち」は評価されなかった）。
/// 少数の報告での確定は、どれだけ待っても認めない。
///
/// 1. **全員一致**: 全員が報告し、全員が同じ結果。根拠は `all_players_reported`。
///    （10 人の一致は必ず閾値も満たすので、2 と結論は変わらない。根拠を分けているだけ）
/// 2. **過半数**: 同じ結果に閾値以上が一致したら、期限の前後を問わず即確定する。
///    勝敗と無効試合を区別しない——非対称にする理由が無い。
/// 3. どの結果も閾値に届かないまま、**全員が報告した**か**期限が過ぎた**ら係争。
///    全員が報告していれば、待っても届かないので待たない。
///    それ以外は保留（次の報告で判定し直す）。
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

    if unanimous && all_reported {
        return Confirmation::settled(latest[0].outcome, BASIS_ALL_REPORTED);
    }
    if let Some(outcome) = leading_outcome(evidence, majority_threshold) {
        return Confirmation::settled(outcome, BASIS_MAJORITY);
    }
    if all_reported || report_window_elapsed {
        Confirmation::disputed()
    } else {
        Confirmation::pending()
    }
}

/// 係争中の試合に報告が増えたときの判定。**過半数に届いたときだけ動かす。**
///
/// 係争になった後も、未報告だった人は報告できる（報告ゲートが残っている）。
/// 「5 人が赤・5 人が未報告」で期限切れ → 係争になった試合に 6 人目が赤で報告
/// したら、運営を待たずに確定してよい——確定の条件（閾値）はいつ満たしても同じ。
/// 届いていなければ何も積まない（係争のまま。`disputed` を重ねて積まない）。
pub(crate) fn confirmation_from_latest_reports_while_disputed(
    evidence: &ReportEvidence,
    majority_threshold: usize,
) -> Confirmation {
    match leading_outcome(evidence, majority_threshold) {
        Some(outcome) => Confirmation::settled(outcome, BASIS_MAJORITY),
        None => Confirmation::pending(),
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

    /// 1. **6 人が同一結果 → 即確定。**期限前でも、残り 4 人を待たない。
    #[test]
    fn six_matching_reports_settle_immediately() {
        assert_eq!(DEFAULT_REPORT_MAJORITY_THRESHOLD, 6, "既定は過半数");
        let confirmation = decide(&same(RED, 6), false);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        assert_eq!(confirmation.basis, Some(BASIS_MAJORITY));
    }

    /// 2. **5 人では確定しない。**
    #[test]
    fn five_matching_reports_do_not_settle() {
        assert_eq!(
            decide(&same(RED, 5), false).decision,
            ReportDecision::Pending
        );
    }

    /// 3. **6 人が「無効試合」で一致 → nullified。**勝敗と非対称にしない。
    #[test]
    fn six_nullified_reports_settle_as_nullified() {
        let confirmation = decide(&same(VOID, 6), false);
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

    /// 5. **赤 5 人が赤、青 5 人のうち 1 人が赤・4 人が青 → 6 対 4 で赤の勝ち。**
    ///
    /// 指示された例そのもの。チームの内訳は関係なく、一致した数だけを数える。
    #[test]
    fn six_against_four_settles_on_the_six() {
        let mut votes = same(RED, 5); // 赤チーム 5 人
        votes.push(RED); // 青チームの 1 人
        votes.extend(same(BLUE, 4)); // 青チームの 4 人
        let confirmation = decide(&votes, false);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        assert_eq!(confirmation.basis, Some(BASIS_MAJORITY));
        // 全員が報告しても割れたまま（5 対 5）なら、期限を待たずに係争
        let mut split = same(RED, 5);
        split.extend(same(BLUE, 5));
        assert_eq!(decide(&split, false).decision, ReportDecision::Disputed);
    }

    /// 6. **5 人が割れたまま期限切れ → 係争。**
    #[test]
    fn a_split_at_the_deadline_is_disputed() {
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

    /// **期限が過ぎても、閾値に届かない人数では確定しない。**
    ///
    /// 以前は「報告した人の中で全員一致」なら 1 人でも確定していた。本番で
    /// 開始 1 分後の 1 票が 15 分後に確定し、その後の 8 票が無視された
    /// （match-c4389f1e）。何人一致していようと、閾値未満なら係争に落とす。
    #[test]
    fn the_deadline_never_settles_below_the_threshold() {
        for count in 1..DEFAULT_REPORT_MAJORITY_THRESHOLD {
            let confirmation = decide(&same(BLUE, count), true);
            assert_eq!(
                confirmation.decision,
                ReportDecision::Disputed,
                "{count} 人の一致で確定してはいけない"
            );
            assert_eq!(confirmation.basis, None);
        }
        // 閾値ちょうどなら期限の前後を問わず確定
        let confirmation = decide(&same(BLUE, DEFAULT_REPORT_MAJORITY_THRESHOLD), true);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(BLUE));
    }

    /// **本番で起きた事故そのもの。**開始 1 分後の 1 票「青」、期限切れ、その後 8 票「赤」。
    /// 新しい判定では、1 票の時点では係争にも確定にもならず（期限前は保留、期限後は係争）、
    /// 9 票そろえば 8 票の赤で確定する。
    #[test]
    fn match_c4389f1e_would_have_settled_on_the_eight_red_reports() {
        // 期限前、1 票だけ → 保留
        assert_eq!(
            decide(&same(BLUE, 1), false).decision,
            ReportDecision::Pending
        );
        // 期限後、1 票だけ → 係争（**確定しない**）
        assert_eq!(
            decide(&same(BLUE, 1), true).decision,
            ReportDecision::Disputed
        );
        // その後の 8 票（赤）が届いた形: 青 2 / 赤 8
        let mut votes = same(BLUE, 2);
        votes.extend(same(RED, 8));
        let confirmation = decide(&votes, true);
        assert_eq!(confirmation.decision, ReportDecision::Confirmed(RED));
        // 係争になった後に届いた場合も、閾値に届けば確定する
        let late = confirmation_from_latest_reports_while_disputed(
            &evidence(&votes),
            DEFAULT_REPORT_MAJORITY_THRESHOLD,
        );
        assert_eq!(late.decision, ReportDecision::Confirmed(RED));
        assert_eq!(late.basis, Some(BASIS_MAJORITY));
        // 届いていなければ係争のまま（何も積まない）
        let still = confirmation_from_latest_reports_while_disputed(
            &evidence(&same(RED, 5)),
            DEFAULT_REPORT_MAJORITY_THRESHOLD,
        );
        assert_eq!(still.decision, ReportDecision::Pending);
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
