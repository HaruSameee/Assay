//! VC プール。`vc1-red` … `vc<N>-blue` を起動時に名前から解決し、空いている組を LRU で確保する。
//!
//! チャンネルを作らない、消さない、権限を変更しない。読むだけ。VC のテキストチャットの
//! メッセージも消さない（記録は DB 側にある。過去のルーム ID は試合終了で無効になるので無害）。
//!
//! 占有状態は DB から導出する: 未終了の formation の `vc_assigned` payload に記録された
//! 割当が「使用中」。終端（`confirmed` / `disputed` / `voided` / `abandoned`）に達した
//! formation は [`crate::persistence::FormationRepository::list_unfinished`] に出てこないので、
//! 割当は自動的に解放される（再起動後も同じ）。メモリ上に持つのは、確保してから
//! `vc_assigned` を追記するまでの間（DB にまだ無い割当）だけ。
//!
//! 割当は LRU: 最後に使われたのが最も古い組を選ぶ。設定順（毎回 vc1 から）だと、直前の試合の
//! 参加者がまだ VC に残っているところに次の組が入る。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serenity::model::id::ChannelId;

use crate::lifecycle::Timestamp;
use crate::persistence::{Team, UnfinishedFormation};

use super::payload::DiscordRecord;

/// VC の組。`red` は `vc<label>-red`、`blue` は `vc<label>-blue`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcPair {
    /// ラベル（`vc<N>` の `N`）。プール内で一意。
    pub label: String,
    /// red チームの VC。
    pub red: ChannelId,
    /// blue チームの VC。
    pub blue: ChannelId,
}

impl VcPair {
    /// チームの VC。
    pub fn channel(&self, team: Team) -> ChannelId {
        match team {
            Team::Red => self.red,
            Team::Blue => self.blue,
        }
    }

    /// チャンネル ID からチーム。この組のものでなければ `None`。
    pub fn team_of(&self, channel: ChannelId) -> Option<Team> {
        Team::ALL
            .into_iter()
            .find(|team| self.channel(*team) == channel)
    }

    /// チームの VC の名前（設定上の名前）。
    pub fn channel_name(&self, team: Team) -> String {
        channel_name(&self.label, team)
    }
}

/// VC の名前の規約: `vc<label>-<team>`。
pub fn channel_name(label: &str, team: Team) -> String {
    format!("vc{label}-{team}")
}

/// 名前解決のエラー。1 本でも見つからない、または同名が 2 本以上あれば起動を失敗させる。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// 個数が 0。
    #[error("DISCORD_VC_POOL_SIZE must be at least 1")]
    EmptyPool,
    /// 見つからない名前。
    #[error("voice channels not found: {0:?}")]
    Missing(Vec<String>),
    /// 同名が 2 本以上ある名前（Discord はチャンネル名の重複を許す）。
    #[error("voice channel names are ambiguous (more than one channel): {0:?}")]
    Duplicated(Vec<String>),
}

/// ギルドのボイスチャンネル一覧（名前, ID）から、`vc1` … `vc<size>` の組を解決する。
///
/// 欠落と重複は全部集めてから返す（1 回の起動失敗で全部直せるように）。
pub fn resolve(size: usize, channels: &[(String, ChannelId)]) -> Result<Vec<VcPair>, ResolveError> {
    if size == 0 {
        return Err(ResolveError::EmptyPool);
    }
    let mut by_name: HashMap<&str, Vec<ChannelId>> = HashMap::new();
    for (name, id) in channels {
        by_name.entry(name.as_str()).or_default().push(*id);
    }
    let mut missing = Vec::new();
    let mut duplicated = Vec::new();
    let mut pairs = Vec::with_capacity(size);
    for n in 1..=size {
        let label = n.to_string();
        let mut ids = [None, None];
        for (slot, team) in Team::ALL.into_iter().enumerate() {
            let name = channel_name(&label, team);
            match by_name.get(name.as_str()).map(Vec::as_slice) {
                None | Some([]) => missing.push(name),
                Some([id]) => ids[slot] = Some(*id),
                Some(_) => duplicated.push(name),
            }
        }
        if let [Some(red), Some(blue)] = ids {
            pairs.push(VcPair { label, red, blue });
        }
    }
    if !duplicated.is_empty() {
        return Err(ResolveError::Duplicated(duplicated));
    }
    if !missing.is_empty() {
        return Err(ResolveError::Missing(missing));
    }
    Ok(pairs)
}

/// VC の組のプール。
#[derive(Debug)]
pub struct VcPool {
    pairs: Vec<VcPair>,
    /// 確保済みだが、まだ `vc_assigned` として DB に記録されていないラベル。
    in_flight: Mutex<HashSet<String>>,
}

/// 確保した VC の組。`vc_assigned` を追記したら [`Reservation::commit`] する。
///
/// 追記せずに drop したら確保は取り消される。
#[must_use = "commit after the assignment is recorded, or drop to give it back"]
pub struct Reservation<'a> {
    pool: &'a VcPool,
    pair: VcPair,
}

impl VcPool {
    /// 解決済みの組から作る。
    pub fn new(pairs: Vec<VcPair>) -> Self {
        Self {
            pairs,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// プールの全組（設定順）。
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

    /// 空いている組を LRU で 1 つ確保する。無ければ `None`。
    ///
    /// - `occupied`: [`VcPool::occupied_labels`] で DB から導出した占有状態
    /// - `last_used`: ラベルごとの最終使用時刻（[`crate::persistence::FormationRepository::vc_last_used`]）。
    ///   一度も使われていない組が最優先、次に最終使用が最も古い組。同時刻は設定順
    ///
    /// それに加えて、このプロセスで確保中（未記録）の組も避ける。
    pub fn reserve(
        &self,
        occupied: &HashSet<String>,
        last_used: &HashMap<String, Timestamp>,
    ) -> Option<Reservation<'_>> {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        let pair = self
            .pairs
            .iter()
            .filter(|pair| !occupied.contains(&pair.label) && !in_flight.contains(&pair.label))
            // Option は None < Some。min_by_key は同点で最初の要素を返すので、設定順が同点の決着になる
            .min_by_key(|pair| last_used.get(&pair.label).copied())?
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

    /// `vc_assigned` に記録済み。以後の占有状態は DB から導出される。
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
    use chrono::{TimeDelta, TimeZone, Utc};

    use super::*;

    fn channels(names: &[(&str, u64)]) -> Vec<(String, ChannelId)> {
        names
            .iter()
            .map(|(name, id)| ((*name).to_owned(), ChannelId::new(*id)))
            .collect()
    }

    fn pool(count: usize) -> VcPool {
        VcPool::new(
            (1..=count)
                .map(|n| VcPair {
                    label: n.to_string(),
                    red: ChannelId::new(n as u64 * 10),
                    blue: ChannelId::new(n as u64 * 10 + 1),
                })
                .collect(),
        )
    }

    fn at(seq: i64) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::minutes(seq)
    }

    #[test]
    fn resolve_finds_every_pair_by_name() {
        let pairs = resolve(
            2,
            &channels(&[
                ("general", 1),
                ("vc1-red", 11),
                ("vc1-blue", 12),
                ("vc2-blue", 22),
                ("vc2-red", 21),
                ("vc3-red", 31),
            ]),
        )
        .unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].label, "1");
        assert_eq!(pairs[0].red, ChannelId::new(11));
        assert_eq!(pairs[0].blue, ChannelId::new(12));
        assert_eq!(pairs[1].channel(Team::Red), ChannelId::new(21));
        assert_eq!(pairs[1].channel_name(Team::Blue), "vc2-blue");
        assert_eq!(pairs[1].team_of(ChannelId::new(22)), Some(Team::Blue));
        assert_eq!(pairs[1].team_of(ChannelId::new(99)), None);
    }

    #[test]
    fn resolve_fails_when_a_channel_is_missing() {
        let error = resolve(
            2,
            &channels(&[("vc1-red", 11), ("vc1-blue", 12), ("vc2-red", 21)]),
        )
        .unwrap_err();
        assert_eq!(error, ResolveError::Missing(vec!["vc2-blue".into()]));
    }

    #[test]
    fn resolve_fails_when_a_name_is_duplicated() {
        let error = resolve(
            1,
            &channels(&[("vc1-red", 11), ("vc1-red", 13), ("vc1-blue", 12)]),
        )
        .unwrap_err();
        assert_eq!(error, ResolveError::Duplicated(vec!["vc1-red".into()]));
        assert_eq!(resolve(0, &channels(&[])), Err(ResolveError::EmptyPool));
    }

    #[test]
    fn reserve_prefers_never_used_then_least_recently_used() {
        let pool = pool(3);
        let none = HashSet::new();
        let mut last_used = HashMap::new();
        last_used.insert("1".to_owned(), at(10));
        last_used.insert("3".to_owned(), at(5));
        // 2 は未使用なので最優先
        let first = pool.reserve(&none, &last_used).unwrap();
        assert_eq!(first.pair().label, "2");
        // 次は最終使用が最も古い 3
        let second = pool.reserve(&none, &last_used).unwrap();
        assert_eq!(second.pair().label, "3");
        drop(first);
        drop(second);
        // 全部使用済みなら最も古いものから。同時刻は設定順
        last_used.insert("2".to_owned(), at(5));
        assert_eq!(pool.reserve(&none, &last_used).unwrap().pair().label, "2");
    }

    #[test]
    fn reserve_skips_occupied_and_in_flight_pairs() {
        let pool = pool(2);
        let occupied: HashSet<String> = ["1".to_owned()].into_iter().collect();
        let first = pool.reserve(&occupied, &HashMap::new()).unwrap();
        assert_eq!(first.pair().label, "2");
        assert!(
            pool.reserve(&occupied, &HashMap::new()).is_none(),
            "pool exhausted"
        );
        drop(first);
        assert_eq!(
            pool.reserve(&occupied, &HashMap::new())
                .unwrap()
                .pair()
                .label,
            "2"
        );
    }

    #[test]
    fn dropping_or_committing_a_reservation_clears_the_in_flight_mark() {
        let pool = pool(2);
        let none = HashSet::new();
        {
            let _a = pool.reserve(&none, &HashMap::new()).unwrap();
            let _b = pool.reserve(&none, &HashMap::new()).unwrap();
            assert_eq!(pool.in_flight_count(), 2);
            assert!(pool.reserve(&none, &HashMap::new()).is_none());
        }
        assert_eq!(pool.in_flight_count(), 0);
        let reservation = pool.reserve(&none, &HashMap::new()).unwrap();
        let label = reservation.pair().label.clone();
        reservation.commit();
        assert_eq!(pool.in_flight_count(), 0);
        // DB 側で占有されていれば避けられる
        let occupied: HashSet<String> = [label].into_iter().collect();
        assert_eq!(
            pool.reserve(&occupied, &HashMap::new())
                .unwrap()
                .pair()
                .label,
            "2"
        );
    }
}
