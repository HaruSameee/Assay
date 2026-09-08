//! ホストのリソース使用状況。`/proc` を直読みする。
//!
//! 設計の要点:
//!
//! - **プロセス内に前回値を持たない。**瞬間の CPU 使用率を出そうとすると `/proc/stat` の
//!   差分を保持することになり、再起動直後の 1 回目が必ず欠ける。ロードアベレージは
//!   スナップショットで 1 分 / 5 分 / 15 分の平均が取れるので、15 分サマリとそのまま対応する。
//! - **クレートを足さない。**`sysinfo` などを入れなくても `/proc` の 3 ファイルで足りる。
//! - **項目ごとに独立して失敗する。**読めなかった項目だけ `None` になり、他の項目と
//!   サマリ全体は落ちない（00 §3-6 の「壊れたデータで停止せず劣化して動き続ける」）。
//!   [`read`] はエラーを返さない。
//! - 解釈は純粋関数（[`parse_loadavg`] / [`parse_meminfo`] / [`parse_vm_rss_kib`]）に分けてある。
//!   `/proc` の無い環境（開発機の Windows）でも、本番から採った実物の文字列で検査できる。
//!
//! メモリは **`MemAvailable`** を使う。`MemFree` はページキャッシュを空きに数えないので、
//! Linux では常に小さく出て「メモリが足りない」と誤読させる。

use std::fs;

/// ロードアベレージ（1 分 / 5 分 / 15 分）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAverage {
    /// 1 分平均。
    pub one: f64,
    /// 5 分平均。
    pub five: f64,
    /// 15 分平均。
    pub fifteen: f64,
}

impl LoadAverage {
    /// コア数で割った利用率（%）。コア数が分からなければ `None`。
    pub fn per_core_percent(self, cores: Option<usize>) -> Option<(u32, u32, u32)> {
        let cores = cores.filter(|cores| *cores > 0)?;
        let percent = |value: f64| (value / cores as f64 * 100.0).round().max(0.0) as u32;
        Some((percent(self.one), percent(self.five), percent(self.fifteen)))
    }
}

/// メモリの使用状況（KiB）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryUsage {
    /// `MemTotal`。
    pub total_kib: u64,
    /// `MemAvailable`。**`MemFree` ではない。**
    pub available_kib: u64,
}

impl MemoryUsage {
    /// 使用中の量（KiB）。
    pub fn used_kib(self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    /// 使用率（%）。
    pub fn used_percent(self) -> u32 {
        if self.total_kib == 0 {
            return 0;
        }
        ((self.used_kib() as f64 / self.total_kib as f64) * 100.0).round() as u32
    }
}

/// ホストのリソース使用状況。**読めなかった項目は `None`。**
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HostResources {
    /// ロードアベレージ。
    pub load: Option<LoadAverage>,
    /// ホスト全体のメモリ。
    pub memory: Option<MemoryUsage>,
    /// このプロセスの常駐サイズ（KiB）。
    pub process_rss_kib: Option<u64>,
    /// 利用可能なコア数。ロードアベレージを %(利用率) に直すのに使う。
    pub cores: Option<usize>,
}

impl HostResources {
    /// 1 項目も読めなかったか。すべて欠けているならサマリに行を足さない。
    pub fn is_empty(&self) -> bool {
        self.load.is_none() && self.memory.is_none() && self.process_rss_kib.is_none()
    }
}

/// `/proc` を読む。**失敗しない。**読めなかった項目が `None` になるだけ。
///
/// Linux 以外（開発機）ではすべて `None` になる。呼び出し側はそれを前提にしてよい。
pub fn read() -> HostResources {
    HostResources {
        load: read_text("/proc/loadavg")
            .as_deref()
            .and_then(parse_loadavg),
        memory: read_text("/proc/meminfo")
            .as_deref()
            .and_then(parse_meminfo),
        process_rss_kib: read_text("/proc/self/status")
            .as_deref()
            .and_then(parse_vm_rss_kib),
        cores: std::thread::available_parallelism()
            .ok()
            .map(std::num::NonZeroUsize::get),
    }
}

fn read_text(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// `/proc/loadavg` の解釈。`0.13 0.09 0.09 1/178 111858` の先頭 3 つ。
///
/// 3 つ揃って有限で非負のときだけ返す。1 つでも壊れていれば `None`（部分的に嘘の値を出さない）。
pub fn parse_loadavg(raw: &str) -> Option<LoadAverage> {
    let mut fields = raw.split_whitespace();
    let mut next = || -> Option<f64> {
        let value: f64 = fields.next()?.parse().ok()?;
        (value.is_finite() && value >= 0.0).then_some(value)
    };
    Some(LoadAverage {
        one: next()?,
        five: next()?,
        fifteen: next()?,
    })
}

/// `/proc/meminfo` の解釈。`MemTotal` と `MemAvailable` の両方が要る。
///
/// 単位は kB 表記だが実体は KiB（カーネルの表記に合わせてある）。
pub fn parse_meminfo(raw: &str) -> Option<MemoryUsage> {
    let mut total = None;
    let mut available = None;
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let parsed = value
            .split_whitespace()
            .next()
            .and_then(|number| number.parse::<u64>().ok());
        match key.trim() {
            "MemTotal" => total = parsed,
            "MemAvailable" => available = parsed,
            _ => {}
        }
    }
    let total_kib = total.filter(|total| *total > 0)?;
    let available_kib = available?.min(total_kib);
    Some(MemoryUsage {
        total_kib,
        available_kib,
    })
}

/// `/proc/self/status` から `VmRSS`（KiB）。
pub fn parse_vm_rss_kib(raw: &str) -> Option<u64> {
    raw.lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|number| number.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本番（`match.aynm.dev`、2 コア / 5.3GiB）から実際に採った文字列。
    const REAL_LOADAVG: &str = "0.13 0.09 0.09 1/178 111858\n";
    const REAL_MEMINFO: &str = "MemTotal:        5549640 kB\n\
                                MemFree:         2629588 kB\n\
                                MemAvailable:    4717120 kB\n\
                                Buffers:          126072 kB\n\
                                Cached:          2218468 kB\n";
    const REAL_STATUS: &str = "Name:\tunite-matchsyst\nState:\tS (sleeping)\n\
                               VmHWM:\t   20912 kB\nVmRSS:\t   20912 kB\n";

    #[test]
    fn the_real_production_files_parse() {
        let load = parse_loadavg(REAL_LOADAVG).expect("loadavg");
        assert_eq!((load.one, load.five, load.fifteen), (0.13, 0.09, 0.09));
        // 2 コアなので利用率は半分
        assert_eq!(load.per_core_percent(Some(2)), Some((7, 5, 5)));
        assert_eq!(load.per_core_percent(None), None, "コア数不明なら出さない");

        let memory = parse_meminfo(REAL_MEMINFO).expect("meminfo");
        assert_eq!(memory.total_kib, 5_549_640);
        assert_eq!(memory.available_kib, 4_717_120, "MemFree ではない");
        assert_eq!(memory.used_kib(), 832_520);
        assert_eq!(memory.used_percent(), 15);

        assert_eq!(parse_vm_rss_kib(REAL_STATUS), Some(20_912));
    }

    #[test]
    fn a_broken_file_yields_none_instead_of_a_wrong_number() {
        for broken in ["", "\n", "0.13", "0.13 0.09", "nan 0.09 0.09", "-1 0 0"] {
            assert_eq!(parse_loadavg(broken), None, "{broken:?}");
        }
        for broken in [
            "",
            "MemTotal: 0 kB\nMemAvailable: 0 kB\n",
            "MemFree: 100 kB\n",
            "MemTotal:        5549640 kB\n",
        ] {
            assert_eq!(parse_meminfo(broken), None, "{broken:?}");
        }
        for broken in ["", "VmHWM:\t 20912 kB\n", "VmRSS:\tmuch kB\n"] {
            assert_eq!(parse_vm_rss_kib(broken), None, "{broken:?}");
        }
    }

    #[test]
    fn available_is_never_reported_as_larger_than_total() {
        // 壊れた値でも「使用量が負」にならない
        let memory = parse_meminfo("MemTotal: 100 kB\nMemAvailable: 500 kB\n").expect("parsed");
        assert_eq!(memory.available_kib, 100);
        assert_eq!(memory.used_kib(), 0);
        assert_eq!(memory.used_percent(), 0);
    }

    #[test]
    fn reading_never_panics_even_without_proc() {
        // 開発機（Windows）では全項目 None になる。落ちないことだけを見る
        let resources = read();
        assert!(resources.load.is_some() || resources.load.is_none());
        if cfg!(target_os = "linux") {
            assert!(resources.load.is_some(), "Linux なら読めるはず");
        } else {
            assert!(resources.is_empty(), "/proc が無ければ空");
        }
    }

    #[test]
    fn an_empty_reading_adds_no_lines() {
        assert!(HostResources::default().is_empty());
        assert!(
            !HostResources {
                process_rss_kib: Some(1),
                ..HostResources::default()
            }
            .is_empty(),
            "1 項目でも読めていれば出す"
        );
    }
}
