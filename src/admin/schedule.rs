//! 「水金土日 21〜2 時」のような週次の枠を運営が設定できるようにする。
//!
//! **手動の停止が常に勝つ。**障害で緊急停止したのに 21 時になったら勝手に再開する、
//! という事故を起こさないため。効き方は [`MatchmakerStop`] に書いてある。
//!
//! 時刻は **JST 固定**。運営も参加者も日本時間で話すので、UTC に直して考えさせない。
//! 保存するのも「JST の 0 時からの分」で、タイムゾーンを跨いだ解釈が入らない。

use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc};

/// JST。Discord の描画（`discord::render`）と同じ扱い。
const JST_OFFSET_SECONDS: i32 = 9 * 3600;

/// 週次の稼働枠。**`current_matchmaker_schedule` の 1 行に対応する。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchmakerSchedule {
    /// スケジュールを使うか。`false` なら手動の停止・再開だけが効く。
    pub enabled: bool,
    /// 稼働する曜日のビットマスク。**bit 0 = 月曜 … bit 6 = 日曜。**
    pub days_mask: i32,
    /// 稼働開始（JST の 0 時からの分）。
    pub start_minute: i32,
    /// 稼働終了（JST の 0 時からの分）。**`start` より小さければ日をまたぐ。**
    pub end_minute: i32,
}

/// 稼働枠の既定値: **水金土日 21:00〜翌 2:00（JST）。**
///
/// `current_matchmaker_schedule` が「1 行も無いとき」に返す値と同じ。
/// 実際の開催枠なので、何も設定していない状態でそのまま動く。
/// **SQL 側とここがずれていないことは統合テストで実測する**
/// （`tests/admin_ops.rs` の既定値テスト）。
impl Default for MatchmakerSchedule {
    fn default() -> Self {
        Self {
            enabled: true,
            // bit 0 = 月曜。水(2)+金(4)+土(5)+日(6) = 4+16+32+64
            days_mask: 0b111_0100,
            start_minute: 21 * 60,
            end_minute: 2 * 60,
        }
    }
}

impl MatchmakerSchedule {
    /// いま稼働時間内か。**純粋関数**（`now` を渡す側が時計を持つ）。
    ///
    /// 日をまたぐ枠（21:00 → 翌 2:00）は、**開始側の曜日**で判定する。
    /// 水曜 21 時に開いた枠は木曜 2 時まで開いており、木曜が `days_mask` に
    /// 入っていなくても閉じない。「水曜の夜」と言ったときに人が思う範囲に合わせる。
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            // スケジュールを使わないなら、時間で止めることはしない
            return true;
        }
        if self.days_mask == 0 {
            // 曜日が 1 つも選ばれていない = 常に閉じている
            return false;
        }
        let jst = now.with_timezone(&FixedOffset::east_opt(JST_OFFSET_SECONDS).expect("valid"));
        let minute = jst.hour() as i32 * 60 + jst.minute() as i32;
        // 月曜 = 0 … 日曜 = 6
        let today = jst.weekday().num_days_from_monday() as i32;

        if self.start_minute < self.end_minute {
            // 同じ日のうちに閉じる枠
            return self.day_selected(today)
                && minute >= self.start_minute
                && minute < self.end_minute;
        }
        // 日をまたぐ枠。今日の開始以降か、**昨日**開いた枠の続きか
        if self.day_selected(today) && minute >= self.start_minute {
            return true;
        }
        let yesterday = (today + 6) % 7;
        self.day_selected(yesterday) && minute < self.end_minute
    }

    fn day_selected(&self, day: i32) -> bool {
        self.days_mask & (1 << day) != 0
    }

    /// 画面に出す曜日の並び（月〜日）。
    pub fn day_labels(&self) -> Vec<&'static str> {
        const NAMES: [&str; 7] = ["月", "火", "水", "木", "金", "土", "日"];
        (0..7)
            .filter(|day| self.day_selected(*day))
            .map(|day| NAMES[day as usize])
            .collect()
    }

    /// 画面に出す時刻（`21:00`）。
    pub fn start_text(&self) -> String {
        minute_text(self.start_minute)
    }

    /// 画面に出す時刻（`02:00`）。日をまたぐかは呼ぶ側が書く。
    pub fn end_text(&self) -> String {
        minute_text(self.end_minute)
    }

    /// 枠が日をまたぐか。
    pub fn crosses_midnight(&self) -> bool {
        self.start_minute >= self.end_minute
    }
}

fn minute_text(minute: i32) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// マッチメイカが止まっている理由。**手動が先に来る。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchmakerStop {
    /// 運営が手で止めた。**スケジュールより優先される。**
    Manual {
        /// 止めた運営。
        actor_discord_user_id: i64,
        /// 止めた時刻。
        since: DateTime<Utc>,
        /// 理由（書いていなければ `None`）。
        reason: Option<String>,
    },
    /// スケジュールの稼働時間外。
    Scheduled,
}

/// スケジュール変更の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleOutcome {
    /// 追記した。
    Changed {
        /// `matchmaker_schedule_changes.id`。
        id: i64,
    },
    /// 同じ冪等キーで既に追記済み（no-op）。
    Duplicate,
}

/// `HH:MM` を「0 時からの分」にする。**純粋関数。**
pub fn parse_time(raw: &str) -> Option<i32> {
    let raw = raw.trim();
    let (hour, minute) = raw.split_once(':')?;
    let hour: i32 = hour.parse().ok()?;
    let minute: i32 = minute.parse().ok()?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    Some(hour * 60 + minute)
}

/// 選ばれた曜日（`mon` `wed` …）をビットマスクにする。
pub fn parse_days(values: &[String]) -> i32 {
    const KEYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    values
        .iter()
        .filter_map(|value| KEYS.iter().position(|key| *key == value.trim()))
        .fold(0, |mask, day| mask | (1 << day))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 水金土日 21:00〜翌 2:00。**指示された例そのもの。**
    fn nightly() -> MatchmakerSchedule {
        MatchmakerSchedule {
            enabled: true,
            days_mask: parse_days(&[
                "wed".to_owned(),
                "fri".to_owned(),
                "sat".to_owned(),
                "sun".to_owned(),
            ]),
            start_minute: parse_time("21:00").unwrap(),
            end_minute: parse_time("02:00").unwrap(),
        }
    }

    /// JST の日時を UTC にして渡す。
    fn jst(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        FixedOffset::east_opt(JST_OFFSET_SECONDS)
            .unwrap()
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// **「水金土日 21-2 時」がそのまま書ける。**
    #[test]
    fn the_example_schedule_opens_and_closes_where_it_should() {
        let schedule = nightly();
        // 2026-09-16 は水曜
        assert_eq!(
            jst(2026, 9, 16, 12, 0)
                .with_timezone(&FixedOffset::east_opt(JST_OFFSET_SECONDS).unwrap())
                .weekday(),
            chrono::Weekday::Wed
        );

        // 水曜 20:59 は閉、21:00 で開く
        assert!(!schedule.is_open(jst(2026, 9, 16, 20, 59)));
        assert!(schedule.is_open(jst(2026, 9, 16, 21, 0)));
        assert!(schedule.is_open(jst(2026, 9, 16, 23, 59)));
        // **日をまたいで木曜 1:59 まで開いている**（木曜は days_mask に無い）
        assert!(schedule.is_open(jst(2026, 9, 17, 0, 0)));
        assert!(schedule.is_open(jst(2026, 9, 17, 1, 59)));
        // 2:00 ちょうどで閉じる
        assert!(!schedule.is_open(jst(2026, 9, 17, 2, 0)));
        // 木曜の夜は開かない
        assert!(!schedule.is_open(jst(2026, 9, 17, 21, 0)));
        // 金曜（9/18）は開く
        assert!(schedule.is_open(jst(2026, 9, 18, 22, 0)));
        // 月曜（9/14）の未明は**日曜 21 時に開いた枠の続き**なので開いている
        assert!(schedule.is_open(jst(2026, 9, 14, 0, 0)), "日曜の枠の続き");
        assert!(schedule.is_open(jst(2026, 9, 14, 1, 59)));
        // 2 時に閉じたあとは、月曜のあいだずっと閉じたまま
        for hour in 2..24 {
            assert!(
                !schedule.is_open(jst(2026, 9, 14, hour, 0)),
                "月曜 {hour} 時"
            );
        }
    }

    /// **日曜 21 時に開いた枠は月曜 2 時まで続く。**週をまたぐ境界。
    #[test]
    fn a_window_opened_on_sunday_runs_into_monday() {
        let schedule = nightly();
        // 2026-09-20 は日曜
        assert!(schedule.is_open(jst(2026, 9, 20, 21, 0)));
        assert!(schedule.is_open(jst(2026, 9, 21, 1, 30)), "月曜の未明");
        assert!(!schedule.is_open(jst(2026, 9, 21, 2, 0)));
    }

    /// 日をまたがない枠（13:00〜17:00）。
    #[test]
    fn a_daytime_window_does_not_wrap() {
        let schedule = MatchmakerSchedule {
            enabled: true,
            days_mask: parse_days(&["sat".to_owned()]),
            start_minute: parse_time("13:00").unwrap(),
            end_minute: parse_time("17:00").unwrap(),
        };
        assert!(!schedule.crosses_midnight());
        // 2026-09-19 は土曜
        assert!(!schedule.is_open(jst(2026, 9, 19, 12, 59)));
        assert!(schedule.is_open(jst(2026, 9, 19, 13, 0)));
        assert!(schedule.is_open(jst(2026, 9, 19, 16, 59)));
        assert!(!schedule.is_open(jst(2026, 9, 19, 17, 0)));
        // 翌日の未明には食い込まない
        assert!(!schedule.is_open(jst(2026, 9, 20, 1, 0)));
    }

    /// **無効なら常に開いている。**これまでの挙動（手動だけ）に戻る。
    #[test]
    fn a_disabled_schedule_never_closes_anything() {
        let schedule = MatchmakerSchedule {
            enabled: false,
            ..nightly()
        };
        for hour in 0..24 {
            assert!(schedule.is_open(jst(2026, 9, 14, hour, 0)));
        }
        // **既定は「使う」設定。**無効化はここで明示したときだけ
        assert!(MatchmakerSchedule::default().enabled);
    }

    /// **既定は水金土日 21:00〜翌 2:00。**実際の開催枠をそのまま入れてある。
    ///
    /// SQL 側（`current_matchmaker_schedule` の coalesce）と一致していることは
    /// `tests/admin_ops.rs` の統合テストが本物の DB で実測する。
    #[test]
    fn the_default_is_the_nightly_window() {
        let defaults = MatchmakerSchedule::default();
        assert_eq!(defaults, nightly());
        assert!(defaults.enabled);
        assert_eq!(defaults.day_labels(), vec!["水", "金", "土", "日"]);
        assert_eq!(defaults.start_text(), "21:00");
        assert_eq!(defaults.end_text(), "02:00");
        assert!(defaults.crosses_midnight());
        // 月曜の昼は閉じている（枠の外）
        assert!(!defaults.is_open(jst(2026, 9, 14, 12, 0)));
        // 金曜の夜は開いている
        assert!(defaults.is_open(jst(2026, 9, 11, 21, 0)));
    }

    /// 曜日を 1 つも選ばなければ、有効でも常に閉じている。
    #[test]
    fn no_days_means_always_closed() {
        let schedule = MatchmakerSchedule {
            enabled: true,
            days_mask: 0,
            ..nightly()
        };
        for hour in 0..24 {
            assert!(!schedule.is_open(jst(2026, 9, 16, hour, 0)));
        }
    }

    #[test]
    fn times_and_days_round_trip_through_the_form() {
        assert_eq!(parse_time("21:00"), Some(1260));
        assert_eq!(parse_time("02:00"), Some(120));
        assert_eq!(parse_time("00:00"), Some(0));
        assert_eq!(parse_time(" 23:59 "), Some(1439));
        for bad in ["", "24:00", "21:60", "21", "9時", "-1:00", "1:2:3"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }

        let mask = parse_days(&[
            "wed".to_owned(),
            "fri".to_owned(),
            "sat".to_owned(),
            "sun".to_owned(),
        ]);
        assert_eq!(mask, 0b111_0100, "水金土日 = bit 2,4,5,6");
        assert_eq!(mask, 116);
        let schedule = MatchmakerSchedule {
            days_mask: mask,
            ..MatchmakerSchedule::default()
        };
        assert_eq!(schedule.day_labels(), vec!["水", "金", "土", "日"]);
        // 知らない値は落とす
        assert_eq!(parse_days(&["mon".to_owned(), "zzz".to_owned()]), 0b1);
    }

    #[test]
    fn the_times_are_shown_with_two_digits() {
        let schedule = nightly();
        assert_eq!(schedule.start_text(), "21:00");
        assert_eq!(schedule.end_text(), "02:00");
        assert!(schedule.crosses_midnight());
    }
}
