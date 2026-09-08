//! シーズン境界（導出の起点）の型。**純粋**。DB も HTTP も知らない。
//!
//! **これは「レートのリセット」ではない。**過去のログは 1 行も消さない。境界を 1 行
//! 追記すると、導出（レート・試合数・勝率・ゲート）がその時刻から読み始めるようになる。
//! docs/02 の不変条件 1〜3 はすべて維持される。
//!
//! - 不変条件 1（試合ログが唯一の正・追記専用）— 何も消さない。境界も追記
//! - 不変条件 2（レーティングは導出値）— 現在値のテーブルは無いまま。境界を含めて再構築できる
//! - 不変条件 3（recompute が唯一の計算経路）— 境界は recompute の入力が変わるだけ
//!
//! 境界を適用する条件は SQL の `current_season` ビュー 1 か所にあり、
//! 導出の入口 3 本（`player_match_rating_deltas` / `season_rating_adjustments` /
//! `formations_without_report_from`）がそれを引く。Rust 側に条件式は無い。

use chrono::{DateTime, Utc};

/// 追記済みの境界 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonBoundary {
    pub id: i64,
    /// この時刻以降に**作られた** formation と、この時刻以降に追記された調整だけを導出に入れる。
    pub effective_at: DateTime<Utc>,
    /// 運営が付けたシーズン名。
    pub label: String,
    /// 操作した運営（`discord:<id>`）。
    pub created_by: String,
    /// 追記した時刻。
    pub created_at: DateTime<Utc>,
}

/// 実行前に数えて画面に出す値。**すべて導出値**で、どこにも保存しない。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeasonPreview {
    /// 進行中の formation 数（matched / awaiting_host / awaiting_code / in_progress）。
    /// **1 件でもあれば実行を拒否する。**
    pub active_formations: usize,
    /// 未報告の試合数（コード開示済みで、まだ報告していない人がいる formation）。
    pub pending_reports: usize,
    /// 集計から外れる試合数（いま導出に入っている確定済みの formation）。
    pub matches_leaving: usize,
    /// 影響を受けるプレイヤー数（外れる試合に参加していた人）。
    pub players_affected: usize,
    /// いま効いている境界。まだ 1 本も無ければ `None`。
    pub current: Option<SeasonBoundary>,
}

impl SeasonPreview {
    /// 実行してよいか。**進行中の formation が 1 件でもあれば拒否する。**
    ///
    /// 進行中のまま境界を入れると、その試合は集計から外れるのに進行だけ続き、
    /// 報告しても何にも反映されない状態になる。
    pub fn can_start(&self) -> bool {
        self.active_formations == 0
    }
}

/// 境界を追記した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeasonOutcome {
    /// 追記した。
    Started {
        /// 追記された行の id。
        id: i64,
    },
    /// 同一 `idempotency_key` の行が既にある（no-op）。**状態ベースの判定はしない。**
    Duplicate,
    /// 進行中の formation があるので実行しなかった。
    Blocked {
        /// 進行中の件数。画面に出して止める。
        active_formations: usize,
    },
}

/// ラベルの検証。空・空白だけは拒否し、長さも上限を切る。
pub const MAX_SEASON_LABEL_LENGTH: usize = 60;

/// 入力されたラベルを正規化する。使えない値なら `None`。
pub fn normalize_label(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_SEASON_LABEL_LENGTH {
        return None;
    }
    // 改行はラベルに要らない（1 行で画面と操作ログに出す）
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    Some(trimmed.to_owned())
}

/// 2 段目の確認。**同じラベルを手で入力させる。**
///
/// クリック 2 回だと誤操作がそのまま通るので、打ち直しを要求する。
/// 比較は正規化した文字列どうしで行う（前後の空白だけの違いで弾かない）。
pub fn confirmation_matches(label: &str, typed: &str) -> bool {
    match (normalize_label(label), normalize_label(typed)) {
        (Some(label), Some(typed)) => label == typed,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unusable_label_is_refused() {
        assert_eq!(normalize_label(" 2026 秋 "), Some("2026 秋".to_owned()));
        for invalid in ["", "   ", "\n", "a\nb", "a\rb"] {
            assert_eq!(normalize_label(invalid), None, "{invalid:?}");
        }
        let too_long = "あ".repeat(MAX_SEASON_LABEL_LENGTH + 1);
        assert_eq!(normalize_label(&too_long), None);
        let just_right = "あ".repeat(MAX_SEASON_LABEL_LENGTH);
        assert_eq!(normalize_label(&just_right), Some(just_right.clone()));
    }

    #[test]
    fn the_second_stage_requires_the_same_label_typed_again() {
        assert!(confirmation_matches("2026 秋", "2026 秋"));
        assert!(
            confirmation_matches("2026 秋", "  2026 秋  "),
            "前後の空白は無視"
        );
        assert!(
            !confirmation_matches("2026 秋", "2026秋"),
            "空白の有無は別物"
        );
        assert!(!confirmation_matches("2026 秋", ""));
        assert!(!confirmation_matches("", ""), "空ラベルでは通さない");
    }

    #[test]
    fn an_active_formation_blocks_the_start() {
        let clean = SeasonPreview {
            matches_leaving: 3,
            players_affected: 13,
            ..SeasonPreview::default()
        };
        assert!(clean.can_start());
        let running = SeasonPreview {
            active_formations: 1,
            ..clean.clone()
        };
        assert!(!running.can_start(), "1 件でも進行中なら拒否");
    }
}
