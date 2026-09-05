//! VC プール。`vc1-red` … `vc<N>-blue` を起動時に名前から解決し、空いている組を LRU で確保する。
//!
//! チャンネルを作らない、消さない、権限を変更しない。読むだけ。VC のテキストチャットの
//! メッセージも消さない（記録は DB 側にある。過去のルーム ID は試合終了で無効になるので無害）。
//!
//! 占有状態は DB から導出する: 未終了の formation の `vc_assigned` payload に記録された
//! 割当が「使用中」。終端（`confirmed` / `disputed` / `voided` / `abandoned`）に達した
//! formation が `confirmed` / `disputed` / `voided` / `abandoned` に達すると割当は DB の
//! 導出結果から外れ、再起動後も自動的に解放される。確保中の状態をメモリに持たず、プール全体の
//! PostgreSQL advisory lock と `vc_assigned` の同一トランザクションで直列化する。
//!
//! 割当は LRU: 最後に使われたのが最も古い組を選ぶ。設定順（毎回 vc1 から）だと、直前の試合の
//! 参加者がまだ VC に残っているところに次の組が入る。

use std::collections::HashMap;

use serenity::model::id::ChannelId;

use crate::persistence::Team;

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
}

impl VcPool {
    /// 解決済みの組から作る。
    pub fn new(pairs: Vec<VcPair>) -> Self {
        Self { pairs }
    }

    /// プールの全組（設定順）。
    pub fn pairs(&self) -> &[VcPair] {
        &self.pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels(names: &[(&str, u64)]) -> Vec<(String, ChannelId)> {
        names
            .iter()
            .map(|(name, id)| ((*name).to_owned(), ChannelId::new(*id)))
            .collect()
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
}
