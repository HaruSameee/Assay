//! 最小費用最大流（07-マッチング仕様 §4-3 の 4）。純粋。DB も Discord も知らない。
//!
//! プレイヤーをロール枠へ割り当てるためだけに使う。総当たりを書かないための道具であり、
//! ここに競技プログラミング以上の一般性は要らない（頂点は最大でも「候補人数 + 7」）。
//!
//! 実装は SPFA（キュー版 Bellman-Ford）による逐次最短路法。費用に負値が来ないよう、
//! 呼び出し側が下駄を履かせてから渡すこと（[`super::select`] がそうしている）。全ての
//! フローが「プレイヤー → ロール」の辺をちょうど 1 本通るので、その辺すべてに同じ定数を
//! 足しても最小解は変わらない。
//!
//! 逐次最短路法は「流量を 1 ずつ増やしながら、そのつど最小費用の増加路を選ぶ」ので、
//! **各流量における最小費用**を通る。したがって最大流に達した時点の費用が
//! 「最大流のうち費用が最小のもの」になる（＝10 人組めるなら、組める中で費用最小の 10 人）。

use std::collections::VecDeque;

/// 残余グラフの辺。
#[derive(Debug, Clone, Copy)]
struct Edge {
    /// 行き先。
    to: usize,
    /// 逆辺の位置（`graph[to]` の添字）。
    reverse: usize,
    /// 残り容量。
    capacity: i64,
    /// 単位流量あたりの費用。
    cost: i64,
}

/// 張った辺の位置。流量が乗ったかを後から読むために返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeRef {
    from: usize,
    index: usize,
}

/// 最小費用流のグラフ。
#[derive(Debug)]
pub struct MinCostFlow {
    graph: Vec<Vec<Edge>>,
}

impl MinCostFlow {
    /// 頂点数を決めて空のグラフを作る。
    pub fn new(nodes: usize) -> Self {
        Self {
            graph: vec![Vec::new(); nodes],
        }
    }

    /// 有向辺を張る（逆辺は容量 0・費用 `-cost` で自動的に張られる）。
    pub fn add_edge(&mut self, from: usize, to: usize, capacity: i64, cost: i64) -> EdgeRef {
        let forward = Edge {
            to,
            reverse: self.graph[to].len(),
            capacity,
            cost,
        };
        let backward = Edge {
            to: from,
            reverse: self.graph[from].len(),
            capacity: 0,
            cost: -cost,
        };
        let index = self.graph[from].len();
        self.graph[from].push(forward);
        self.graph[to].push(backward);
        EdgeRef { from, index }
    }

    /// その辺に流量が乗ったか（容量 1 の辺なら「使われたか」と同じ）。
    pub fn used(&self, edge: EdgeRef) -> bool {
        self.graph[edge.from][edge.index].capacity == 0
    }

    /// `source` から `sink` へ最大 `limit` まで流す。返すのは (流量, 費用)。
    ///
    /// 増加路が無くなるか `limit` に達したら止める。
    pub fn run(&mut self, source: usize, sink: usize, limit: i64) -> (i64, i64) {
        let nodes = self.graph.len();
        let mut flow = 0;
        let mut total_cost = 0;

        while flow < limit {
            // SPFA で最小費用の増加路を探す
            let mut distance = vec![i64::MAX; nodes];
            let mut previous: Vec<Option<(usize, usize)>> = vec![None; nodes];
            let mut queued = vec![false; nodes];
            let mut queue = VecDeque::new();
            distance[source] = 0;
            queue.push_back(source);
            queued[source] = true;

            while let Some(node) = queue.pop_front() {
                queued[node] = false;
                let here = distance[node];
                for index in 0..self.graph[node].len() {
                    let edge = self.graph[node][index];
                    if edge.capacity <= 0 {
                        continue;
                    }
                    let next = here + edge.cost;
                    if next < distance[edge.to] {
                        distance[edge.to] = next;
                        previous[edge.to] = Some((node, index));
                        if !queued[edge.to] {
                            queued[edge.to] = true;
                            queue.push_back(edge.to);
                        }
                    }
                }
            }
            if distance[sink] == i64::MAX {
                break;
            }

            // 増加路の隘路を求めて流す
            let mut bottleneck = limit - flow;
            let mut node = sink;
            while let Some((from, index)) = previous[node] {
                bottleneck = bottleneck.min(self.graph[from][index].capacity);
                node = from;
            }
            let mut node = sink;
            while let Some((from, index)) = previous[node] {
                self.graph[from][index].capacity -= bottleneck;
                let reverse = self.graph[from][index].reverse;
                self.graph[node][reverse].capacity += bottleneck;
                node = from;
            }
            flow += bottleneck;
            total_cost += bottleneck * distance[sink];
        }
        (flow, total_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2 人 → 2 枠。安い組み合わせが選ばれる。
    #[test]
    fn it_picks_the_cheaper_assignment() {
        // 0: source, 1-2: players, 3-4: slots, 5: sink
        let mut flow = MinCostFlow::new(6);
        flow.add_edge(0, 1, 1, 0);
        flow.add_edge(0, 2, 1, 0);
        let cheap = flow.add_edge(1, 3, 1, 1);
        let expensive = flow.add_edge(1, 4, 1, 9);
        flow.add_edge(2, 3, 1, 5);
        let second = flow.add_edge(2, 4, 1, 2);
        flow.add_edge(3, 5, 1, 0);
        flow.add_edge(4, 5, 1, 0);

        // 1→3 (1) と 2→4 (2) で 3。逆の組み合わせ（9 + 5 = 14）より安い
        assert_eq!(flow.run(0, 5, 2), (2, 3));
        assert!(flow.used(cheap));
        assert!(!flow.used(expensive));
        assert!(flow.used(second));
    }

    /// 容量 2 の枠に 2 人入り、一番高い 1 人があぶれる。
    #[test]
    fn a_slot_with_capacity_two_takes_the_two_cheapest() {
        // 0: source, 1-3: players, 4: slot, 5: sink
        let mut flow = MinCostFlow::new(6);
        for player in 1..=3usize {
            flow.add_edge(0, player, 1, 0);
            flow.add_edge(player, 4, 1, player as i64);
        }
        flow.add_edge(4, 5, 2, 0);
        assert_eq!(flow.run(0, 5, 3), (2, 3), "players 1 and 2 fill the slot");
    }

    /// 流せなければ流量が上限に届かない。
    #[test]
    fn the_flow_stops_when_no_augmenting_path_remains() {
        let mut flow = MinCostFlow::new(4);
        flow.add_edge(0, 1, 1, 0);
        flow.add_edge(1, 2, 1, 0);
        flow.add_edge(2, 3, 1, 0);
        assert_eq!(flow.run(0, 3, 5), (1, 0));
    }

    /// 逐次最短路法は各流量で最小費用を通る。安い辺を先に使っても、逆辺を戻して
    /// 最大流の最小費用に到達する（貪欲では 1→3 を取って 2 があぶれる）。
    #[test]
    fn it_reroutes_through_reverse_edges_to_stay_optimal() {
        // 0: source, 1-2: players, 3-4: slots, 5: sink
        let mut flow = MinCostFlow::new(6);
        flow.add_edge(0, 1, 1, 0);
        flow.add_edge(0, 2, 1, 0);
        // 1 は両方に行けるが 3 が安い。2 は 3 にしか行けない
        flow.add_edge(1, 3, 1, 1);
        flow.add_edge(1, 4, 1, 4);
        flow.add_edge(2, 3, 1, 2);
        flow.add_edge(3, 5, 1, 0);
        flow.add_edge(4, 5, 1, 0);
        // 1→4 (4) と 2→3 (2) の 6 が最大流の最小費用
        assert_eq!(flow.run(0, 5, 2), (2, 6));
    }
}
