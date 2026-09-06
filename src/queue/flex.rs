//! フレックス補償の導出（07-マッチング仕様 §5）。純粋関数。
//!
//! 第1希望以外のロールを割り当てられたプレイヤーには、**次の1回だけ**第1希望を優先する。
//!
//! **対象かどうかをプレイヤーの可変列に持たない。**`formation_players.role_was_flexed` に
//! 「第1希望以外を割り当てられたか」が既に記録されているので、そこから導出する。
//! 可変列に置くと、プロセス再起動や打ち消しレコードとの整合が取れなくなる（07 §5）。
//!
//! 累積しない。見るのは**直近のマッチ1件だけ**なので、次にマッチが成立した時点で
//! 判定の入力が入れ替わり、第1希望が取れたかどうかに関わらずフラグは消える。
//!
//! 使うのはマッチメイカ（次のタスク）。ここでは導出そのものだけを置く。

use crate::lifecycle::{FormationId, Timestamp};

/// `formation_players` に記録された割り当て1件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleAssignment {
    /// どの formation か。同時刻の tiebreak にも使う。
    pub formation_id: FormationId,
    /// `formations.created_at`（マッチが成立した時刻）。
    pub matched_at: Timestamp,
    /// `formation_players.role_was_flexed`。第1希望以外を割り当てられたか。
    pub role_was_flexed: bool,
}

/// 履歴から**直近のマッチ**を選ぶ。
///
/// `matched_at` が同一時刻で並ぶことがある（マッチメイカは1回の実行で複数の formation を
/// 作る。07 §4-1）。順序が結果を変えるので、`formation_id` で決定的に tiebreak する。
pub fn latest_assignment(history: &[RoleAssignment]) -> Option<&RoleAssignment> {
    history
        .iter()
        .max_by_key(|assignment| (assignment.matched_at, assignment.formation_id.0))
}

/// このプレイヤーがフレックス補償の対象か。
///
/// 直近のマッチで第1希望以外を割り当てられていれば対象。履歴が無ければ対象外。
pub fn flex_compensation_owed(history: &[RoleAssignment]) -> bool {
    latest_assignment(history).is_some_and(|assignment| assignment.role_was_flexed)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;

    fn at(minutes: i64) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::minutes(minutes)
    }

    fn assignment(id: u128, minutes: i64, flexed: bool) -> RoleAssignment {
        RoleAssignment {
            formation_id: FormationId(Uuid::from_u128(id)),
            matched_at: at(minutes),
            role_was_flexed: flexed,
        }
    }

    #[test]
    fn a_player_who_never_matched_is_not_owed_anything() {
        assert!(!flex_compensation_owed(&[]));
        assert_eq!(latest_assignment(&[]), None);
    }

    #[test]
    fn being_flexed_in_the_latest_match_makes_you_a_target() {
        assert!(flex_compensation_owed(&[assignment(1, 0, true)]));
        assert!(!flex_compensation_owed(&[assignment(1, 0, false)]));
    }

    #[test]
    fn the_compensation_disappears_at_the_next_match_however_it_went() {
        // 第1希望が取れた次のマッチで消える
        let got_first_choice = [assignment(1, 0, true), assignment(2, 3, false)];
        assert!(!flex_compensation_owed(&got_first_choice));

        // 取れなくても「前回の分」は消える。対象であることは新しい割り当てから来ている
        let flexed_again = [assignment(1, 0, true), assignment(2, 3, true)];
        assert!(flex_compensation_owed(&flexed_again));
        // 累積しない: 戻り値は真偽値ひとつで、回数を持たない
        let flexed_three_times = [
            assignment(1, 0, true),
            assignment(2, 3, true),
            assignment(3, 6, true),
        ];
        assert_eq!(
            flex_compensation_owed(&flexed_three_times),
            flex_compensation_owed(&[assignment(3, 6, true)])
        );
    }

    #[test]
    fn the_order_of_the_input_does_not_matter() {
        let ascending = [assignment(1, 0, true), assignment(2, 3, false)];
        let descending = [assignment(2, 3, false), assignment(1, 0, true)];
        assert!(!flex_compensation_owed(&ascending));
        assert!(!flex_compensation_owed(&descending));
        assert_eq!(
            latest_assignment(&ascending).unwrap().formation_id,
            latest_assignment(&descending).unwrap().formation_id
        );
    }

    /// 同一実行で作られた formation は `created_at` が並びうる。順序が結果を変えるので、
    /// tiebreak が決定的でなければならない（07 §7 が確認を求めているのと同じ問題）。
    #[test]
    fn simultaneous_matches_break_ties_deterministically() {
        let low = assignment(1, 5, true);
        let high = assignment(2, 5, false);
        assert_eq!(
            latest_assignment(&[low, high]).unwrap().formation_id,
            high.formation_id
        );
        assert_eq!(
            latest_assignment(&[high, low]).unwrap().formation_id,
            high.formation_id
        );
        assert!(!flex_compensation_owed(&[low, high]));
        assert!(!flex_compensation_owed(&[high, low]));
    }
}
