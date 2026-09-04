//! VC プール。事前に作られた VC の組を設定から読み、空いている組を確保する。
//!
//! チャンネルを作らない、消さない、権限を変更しない。読むだけ。
//!
//! 占有状態は DB から導出する: 未終了の formation の `thread_created` payload に記録された
//! 割当が「使用中」。終端（`confirmed` / `disputed` / `voided` / `abandoned`）に達した
//! formation は [`crate::persistence::FormationRepository::list_unfinished`] に出てこないので、
//! 割当は自動的に解放される（再起動後も同じ）。メモリ上に持つのは、確保してから
//! `thread_created` を追記するまでの間（DB にまだ無い割当）だけ。

use std::collections::HashSet;
use std::sync::Mutex;

use serenity::model::id::ChannelId;

use crate::persistence::UnfinishedFormation;

use super::payload::DiscordRecord;

/// VC の組。`red` は `VC-<label>-red`、`blue` は `VC-<label>-blue`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcPair {
    /// 表示用のラベル（例: `01`）。プール内で一意。
    pub label: String,
    /// red チームの VC。
    pub red: ChannelId,
    /// blue チームの VC。
    pub blue: ChannelId,
}

/// VC の組のプール。
#[derive(Debug)]
pub struct VcPool {
    pairs: Vec<VcPair>,
    /// 確保済みだが、まだ `thread_created` として DB に記録されていないラベル。
    in_flight: Mutex<HashSet<String>>,
}

/// 確保した VC の組。`thread_created` を追記したら [`Reservation::commit`] する。
///
/// 追記せずに drop したら（スレッド作成失敗など）確保は取り消される。
#[must_use = "commit after the assignment is recorded, or drop to give it back"]
pub struct Reservation<'a> {
    pool: &'a VcPool,
    pair: VcPair,
}

impl VcPool {
    /// 設定の組から作る。
    pub fn new(pairs: Vec<VcPair>) -> Self {
        Self {
            pairs,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// プールの全組。
    pub fn pairs(&self) -> &[VcPair] {
        &self.pairs
    }

    /// 未終了の formation が使っているラベル（DB から導出した占有状態）。
    pub fn occupied_labels(unfinished: &[UnfinishedFormation]) -> HashSet<String> {
        unfinished
            .iter()
            .filter_map(|formation| DiscordRecord::from_log(&formation.log).vc)
            .map(|pair| pair.label)
            .collect()
    }

    /// 空いている組を 1 つ確保する。無ければ `None`。
    ///
    /// `occupied` は [`VcPool::occupied_labels`] で DB から導出したもの。
    /// それに加えて、このプロセスで確保中（未記録）の組も避ける。
    pub fn reserve(&self, occupied: &HashSet<String>) -> Option<Reservation<'_>> {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        let pair = self
            .pairs
            .iter()
            .find(|pair| !occupied.contains(&pair.label) && !in_flight.contains(&pair.label))?
            .clone();
        in_flight.insert(pair.label.clone());
        Some(Reservation { pool: self, pair })
    }

    /// 終端に達した formation の割当を解放する。
    ///
    /// 占有状態は DB から導出するので、ここでやることは確保中の組を手放すことだけ。
    /// 記録済みの割当は、formation が終端に達した時点で自動的に空きになる。
    pub fn release(&self, label: &str) {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        in_flight.remove(label);
    }

    #[cfg(test)]
    fn in_flight_count(&self) -> usize {
        self.in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl Reservation<'_> {
    /// 確保した組。
    pub fn pair(&self) -> &VcPair {
        &self.pair
    }

    /// `thread_created` に記録済み。以後の占有状態は DB から導出される。
    pub fn commit(self) {
        // Drop がメモリ上の確保を手放す。DB に記録済みなので、次の確保は DB 側で避けられる。
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.pool.release(&self.pair.label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> VcPool {
        VcPool::new(
            (1..=2)
                .map(|n| VcPair {
                    label: format!("0{n}"),
                    red: ChannelId::new(n * 10),
                    blue: ChannelId::new(n * 10 + 1),
                })
                .collect(),
        )
    }

    #[test]
    fn reserve_skips_occupied_and_in_flight_pairs() {
        let pool = pool();
        let occupied: HashSet<String> = ["01".to_owned()].into_iter().collect();
        let first = pool.reserve(&occupied).unwrap();
        assert_eq!(first.pair().label, "02");
        assert!(pool.reserve(&occupied).is_none(), "pool exhausted");
        drop(first);
        assert_eq!(pool.reserve(&occupied).unwrap().pair().label, "02");
    }

    #[test]
    fn dropping_a_reservation_gives_the_pair_back() {
        let pool = pool();
        let none = HashSet::new();
        {
            let _a = pool.reserve(&none).unwrap();
            let _b = pool.reserve(&none).unwrap();
            assert_eq!(pool.in_flight_count(), 2);
            assert!(pool.reserve(&none).is_none());
        }
        assert_eq!(pool.in_flight_count(), 0);
    }

    #[test]
    fn commit_hands_the_pair_over_to_the_database() {
        let pool = pool();
        let none = HashSet::new();
        let reservation = pool.reserve(&none).unwrap();
        let label = reservation.pair().label.clone();
        reservation.commit();
        assert_eq!(pool.in_flight_count(), 0);
        // DB 側で占有されていれば避けられる
        let occupied: HashSet<String> = [label].into_iter().collect();
        assert_eq!(pool.reserve(&occupied).unwrap().pair().label, "02");
    }
}
