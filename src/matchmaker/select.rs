//! 10 人を選ぶアルゴリズム（07-マッチング仕様 §4-2 / §4-3）。**純粋関数**。
//!
//! Phase 2 の状態機械と同じ設計で、DB にも Discord にも触らない。時刻は引数で受ける。
//! ここがテストの主戦場になる。
//!
//! **総当たりを書かない。**キュー 40 人から 10 人を選ぶ組み合わせは 8×10⁸ 通りあり、
//! 成立しない。07 §4-3 の方針どおり、
//!
//! 1. レート降順にソートし、**幅 50 のスライディングウィンドウ**を取る
//! 2. ウィンドウ内から、各人の下方向設定を満たさない者を除く
//! 3. ロール枠への割り当てを**最小費用流**で解く（[`super::flow`]）
//! 4. 最大流が 10 に達すれば成立。達しなければ次のウィンドウへ
//!
//! で解く。ウィンドウは**その最小レートを基準**に取る。07 §3-1 の 2 条件は
//!
//! - `max(rating) − min(rating) ≤ 50`
//! - すべての i で `rating_i − min(rating) ≤ d_i`
//!
//! なので、「最小レート `m` を基準に `m ≤ rating ≤ m + 50` かつ `rating − m ≤ d`」を満たす
//! 集合を作れば、**その部分集合はどれも 2 条件を満たす**（実際の最小レートは `m` 以上なので、
//! 差はここで測った値より小さくなる）。基準を全候補について走らせるので、成立しうる組を
//! 取りこぼさない。
//!
//! チーム分けだけは成立した 10 人に対する総当たり（`C(10,5)/2 = 126` 通り）でよい。
//! 07 §4-3 のとおり、ここは十分小さい。

use std::collections::{HashMap, HashSet};

use crate::lifecycle::{PlayerId, Timestamp};
use crate::persistence::Team;
use crate::queue::model::MatchingRanges;

use super::flow::MinCostFlow;

/// 1 formation の人数。
pub const MATCH_SIZE: usize = 10;
/// 1 チームの人数。
pub const TEAM_SIZE: usize = MATCH_SIZE / 2;

/// Active directed ally blocks. A relation in either direction prevents the
/// two players from being assigned to the same team.
pub type AllyBlocks = HashMap<PlayerId, HashSet<PlayerId>>;

/// Observes selection work without changing the matching decisions.
///
/// The production observer is a no-op. Benchmarks can opt into detailed
/// counters and timings without adding clock reads to the production path.
trait SelectionObserver {
    fn begin_measurement(&self) -> Option<std::time::Instant> {
        None
    }

    fn record_anchor(&mut self) {}

    fn record_window(&mut self, _large_enough: bool, _elapsed: Option<std::time::Duration>) {}

    fn record_role_assignment(&mut self, _success: bool, _elapsed: Option<std::time::Duration>) {}

    fn record_team_mask(&mut self) {}

    fn record_block_check(&mut self, _blocked: bool, _elapsed: Option<std::time::Duration>) {}

    fn record_blocked_team_mask(&mut self) {}

    fn record_team_split(&mut self, _success: bool, _elapsed: Option<std::time::Duration>) {}
}

struct NoopSelectionObserver;

impl SelectionObserver for NoopSelectionObserver {}

/// ロールの正規値（00 §4 / Phase 1 マイグレーションの `roles`）。
///
/// 各チームにこの 5 つが 1 人ずつ入る。`roles` テーブルとずれると編成が作れなくなるので、
/// `tests/matchmaker.rs` が DB の値と突き合わせる。
pub const ROLES: [&str; 5] = [
    "top_carry",
    "top_support",
    "jungle",
    "bot_carry",
    "bot_tank",
];

/// マッチング時点で凍結するパラメータ版数。**選択の費用や範囲を変えたら上げること。**
///
/// 後から「どの版で組まれた試合か」を SQL だけで切り分けられるようにするための値で、
/// アルゴリズムの入力ではない。
///
/// | 版 | 下方向の範囲 | 上方向 |
/// |---|---|---|
/// | 1 | 95〜200（既定 95） | +95 固定 |
/// | 2 | 30〜100（既定 95） | +95 固定 |
/// | 3 | **50 固定** | **+50 固定** |
///
/// レーティングの版数は別物（[`crate::rating::RATING_PARAMS_VERSION`]。あちらは導出値の
/// パラメータなので凍結しない）。
pub const MATCHMAKING_PARAMS_VERSION: i32 = 3;

/// 予測勝率の凍結値。
///
/// 初週は固定増減方式（07 §2）で、勝率モデルを持たない。**無い数字を作らない**ため 0.5 を
/// 入れる。チーム平均レートの差は `formation_players.mu_at_match` から後で再現できるので、
/// 07 §7 の比較に必要な情報は失われない。
pub const PREDICTED_WIN_PROB: f32 = 0.5;

// ----------------------------------------------------------------------
// 費用（07 §4-3 の 4）。当日の校正対象。
// ----------------------------------------------------------------------

/// 待ち時間 1 秒あたりの費用の下がり幅。
///
/// 「費用: 待ち時間が長いほど低くする（＝待ち時間の合計を最大化する）」がこれ。流量は必ず
/// 10 なので、`(最大待ち − 自分の待ち)` の合計を最小化することは待ち時間の合計を
/// 最大化することと同じになる。
const WAIT_COST_PER_SECOND: i64 = 1;

/// 第 1 希望から 1 段下がるごとの費用。
///
/// 07 は希望順の一般的な優先を要求していない（要求しているのは待ち時間の合計とフレックス
/// 補償だけ）。ここは**同点の割り振りを決めるためだけ**の値で、1〜2 秒の待ち時間に相当する。
/// これより大きくすると「待ち時間の合計が最大」（§4-2 の 3）を侵し始める。
const PRIORITY_COST: i64 = 1;

/// フレックス補償の対象者が第 1 希望へ張る辺を、待ち時間 10 分ぶん安くする。
///
/// 07 §5 の「強く下げる」。**ハード制約にはしない**——費用を下げるだけなので、
/// この辺を使わないと 10 人揃わない場合は普通に第 2 希望で組まれる。
const FLEX_FIRST_CHOICE_BONUS: i64 = 600;

// ----------------------------------------------------------------------
// 入出力
// ----------------------------------------------------------------------

/// マッチメイカが見るキュー中の 1 人。
///
/// レートは**表示スケール**（1500 スタート）。導出値であり、ここでは受け取るだけ。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// 誰か。
    pub player_id: PlayerId,
    /// 表示スケールのレート。
    pub rating: f64,
    /// 表示スケールの σ。formation に凍結する。
    pub sigma: f64,
    /// 下方向の幅。いまは 50 固定（`clamp_down_range` を通した値が入る）。
    pub down_range: i32,
    /// キューに入った時刻。待ち時間の算出に使う。
    pub enqueued_at: Timestamp,
    /// 運営が付与した優先度（待ち時間に加算する秒数。`queue_priority_grants` の
    /// まだ効いている分の合算）。基準の順と費用にだけ効き、画面の待ち時間は変えない。
    pub priority_bonus_seconds: i64,
    /// 希望ロール。`priority` の昇順（1 が第 1 希望）。
    pub roles: Vec<RoleWish>,
    /// フレックス補償の対象か（07 §5。`formation_players` から導出済み）。
    pub flex_compensation: bool,
    /// マッチメイカが走って、この人を見て、組まなかった回数（`queue_entries.unmatched_scans`）。
    /// 1486 以下の人と、そのスキャンでいちばん上の人の[`Self::matching_rating`]にだけ効く。
    pub unmatched_scans: i32,
}

impl Candidate {
    /// **マッチング範囲の判定に使うレート。**端の取り残し対策の寄せ込み
    /// （[`super::boost`]。`bounds` はそのスキャンの候補の実レートの最小と最大）。
    /// 実レートは [`Self::rating`] のまま formation に凍結する。
    pub fn matching_rating(&self, bounds: super::boost::Bounds) -> f64 {
        super::boost::matching_rating(self.rating, self.unmatched_scans, bounds)
    }

    /// **優先度の判定に使う投入時刻。**運営の付与ぶんだけ早めた時刻で、基準の順
    /// （長く待っている人から）と最小費用流の費用がこれを見る。
    pub fn effective_enqueued_at(&self) -> Timestamp {
        self.enqueued_at - chrono::Duration::seconds(self.priority_bonus_seconds.max(0))
    }
}

/// 希望ロール 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleWish {
    /// `roles.id`。
    pub role: String,
    /// 1 が第 1 希望。
    pub priority: i16,
}

/// 成立した 10 人の編成。まだ DB には何も書かれていない。
#[derive(Debug, Clone, PartialEq)]
pub struct ProposedMatch {
    /// 参加者 10 人。チーム順・ロール順に並ぶ。
    pub players: Vec<ProposedPlayer>,
}

/// 編成の 1 人。
#[derive(Debug, Clone, PartialEq)]
pub struct ProposedPlayer {
    /// 誰か。
    pub player_id: PlayerId,
    /// チーム。
    pub team: Team,
    /// 割り当てられたロール。
    pub role: String,
    /// 割り当てられたロールの希望順位（1 が第 1 希望）。
    ///
    /// formation に保存する必要はない。成立時の運用ログで、選択が希望順位をどれだけ
    /// 満たしたかを集計するためだけに使う。
    pub assigned_preference: i16,
    /// 第 1 希望以外を割り当てられたか。次回のフレックス補償の入力になる。
    pub role_was_flexed: bool,
    /// 前回のフレックスに対する補償が今回の第 1 希望へ適用されたか。
    ///
    /// これも成立時ログだけの集計値で、プレイヤーの可変状態には持たない。
    pub flex_compensation_applied: bool,
    /// 今回の成立時点での待機時間（秒）。
    pub wait_seconds: u64,
    /// 表示スケールのレート（凍結値の元）。
    pub rating: f64,
    /// 表示スケールの σ（凍結値の元）。
    pub sigma: f64,
}

impl ProposedMatch {
    /// 参加者の ID。
    pub fn player_ids(&self) -> Vec<PlayerId> {
        self.players.iter().map(|player| player.player_id).collect()
    }

    /// チームの平均レート（表示スケール）。
    pub fn team_average(&self, team: Team) -> f64 {
        let ratings: Vec<f64> = self
            .players
            .iter()
            .filter(|player| player.team == team)
            .map(|player| player.rating)
            .collect();
        ratings.iter().sum::<f64>() / ratings.len() as f64
    }

    /// 最高レートと最低レートの差。07 §3-1 の条件 1 は 50 以下（境界を含む）。
    pub fn rating_span(&self) -> f64 {
        let ratings: Vec<f64> = self.players.iter().map(|player| player.rating).collect();
        let max = ratings.iter().copied().fold(f64::MIN, f64::max);
        let min = ratings.iter().copied().fold(f64::MAX, f64::min);
        max - min
    }
}

// ----------------------------------------------------------------------
// 基準の順と走査の方向
// ----------------------------------------------------------------------
//
// ウィンドウは「基準（最低レート）+ 上方向の幅」。**基準はいちばん長く待っている人から**
// 順に取り、その人を含むウィンドウを人数の多い順に試す（`find_one`）。レート順に
// 取っていた頃は、上と下の両方の帯に入れる中間の人が毎回どちらかの帯に先を越され、
// 1 時間以上取り残されることがあった。
//
// `ScanDirection` は**同点の並び**（同じ待ち時間の基準どうし、同じ人数のウィンドウどうし）
// を決めるだけ。以前は基準そのものの順（上から / 下から）を決めていて、中間の人の
// 取り残しを緩和するためにスキャンごとにランダムに引いていたが、待ち時間順に取れば
// 要らないので、**本番は常に上から**（2026-09-26）。`BottomUp` はテストのために残す。

/// 1 回のスキャンで基準を試す順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanDirection {
    /// 最高レートの人を基準にして下へ（07 §4-3 の元の形）。
    #[default]
    TopDown,
    /// 最低レートの人を基準にして上へ。
    BottomUp,
}

impl ScanDirection {
    /// サーバのログに出す名前。**参加者向けの経路には出さない。**
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TopDown => "top_down",
            Self::BottomUp => "bottom_up",
        }
    }
}

// ----------------------------------------------------------------------
// 本体
// ----------------------------------------------------------------------

/// キューから組めるだけ組む。DB にも Discord にも触らない。
///
/// 成立した 10 人は候補から外して探索を続ける（07 §4-2「1 回の実行で組めるだけ組む」）。
/// 返る順序は決定的（レート降順・同点は `player_id` 昇順で走査した結果）。
pub fn select_matches(candidates: &[Candidate], now: Timestamp) -> Vec<ProposedMatch> {
    let mut observer = NoopSelectionObserver;
    select_matches_inner(
        candidates,
        &AllyBlocks::new(),
        now,
        MatchingRanges::default(),
        ScanDirection::TopDown,
        &mut observer,
    )
}

/// Select matches with hard ally-only incompatibilities.
///
/// Role allocation is deliberately unchanged. If the allocated ten players
/// have no legal team split, [`find_one`] continues with the next existing
/// rating-window anchor rather than searching another role assignment.
///
/// 幅は既定値（50/50）。**運営が変えた幅で組むのは
/// [`select_matches_within`] の方**で、本番のスキャンはそちらを通る。
pub fn select_matches_with_ally_blocks(
    candidates: &[Candidate],
    ally_blocks: &AllyBlocks,
    now: Timestamp,
) -> Vec<ProposedMatch> {
    select_matches_within(candidates, ally_blocks, now, MatchingRanges::default())
}

/// 幅を明示して組む。**本番のスキャンはこれを通る。**
///
/// `ranges` はマッチメイカが 1 スキャンにつき 1 回だけ DB から読んだもの
/// （`current_matching_ranges`）。ここで読み直さないのは、1 回の選抜の途中で
/// 基準がずれないようにするため。
///
/// 下方向は候補ごとの `down_range` に丸め込み済みなので、ここで見るのは
/// `ranges.up` だけである。
pub fn select_matches_within(
    candidates: &[Candidate],
    ally_blocks: &AllyBlocks,
    now: Timestamp,
    ranges: MatchingRanges,
) -> Vec<ProposedMatch> {
    select_matches_scanning(candidates, ally_blocks, now, ranges, ScanDirection::TopDown)
}

/// 幅と走査の方向を明示して組む。**本番のスキャンはこれを通る。**
///
/// 方向で変わるのは基準を試す順だけで、ウィンドウの条件（07 §3-1）も
/// ロール割り当ても同じ。どの組が**先に**確定するかが変わる。
pub fn select_matches_scanning(
    candidates: &[Candidate],
    ally_blocks: &AllyBlocks,
    now: Timestamp,
    ranges: MatchingRanges,
    direction: ScanDirection,
) -> Vec<ProposedMatch> {
    let mut observer = NoopSelectionObserver;
    select_matches_inner(
        candidates,
        ally_blocks,
        now,
        ranges,
        direction,
        &mut observer,
    )
}

fn select_matches_inner<O: SelectionObserver>(
    candidates: &[Candidate],
    ally_blocks: &AllyBlocks,
    now: Timestamp,
    ranges: MatchingRanges,
    direction: ScanDirection,
    observer: &mut O,
) -> Vec<ProposedMatch> {
    let mut pool: Vec<&Candidate> = candidates.iter().collect();
    let bounds = super::boost::Bounds::of(candidates.iter().map(|candidate| &candidate.rating));
    let mut matches = Vec::new();
    // **2 周する。1 周目は実レート、2 周目は寄せ込み後の判定用レート**（`super::boost`）。
    // 寄せ込みは端の人を中央へ動かすが、動かした先で組めず、動かす前なら組めた、という
    // 取りこぼしを作ってはいけない（実レートで組めるなら常にそれを優先する）。
    // 2 周目は 1 周目で組めなかった人だけを、寄せ込んだレートで見直す
    let passes: [&dyn Fn(&Candidate) -> f64; 2] = [&|candidate| candidate.rating, &|candidate| {
        candidate.matching_rating(bounds)
    }];
    for rating_of in passes {
        // 決定的にするため、レート降順・同点は player_id 昇順で固定する
        pool.sort_by(|a, b| {
            rating_of(b)
                .partial_cmp(&rating_of(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.player_id.0.cmp(&b.player_id.0))
        });
        while pool.len() >= MATCH_SIZE {
            let Some((taken, proposal)) = find_one(
                &pool,
                rating_of,
                ally_blocks,
                now,
                ranges,
                direction,
                observer,
            ) else {
                break;
            };
            // 成立した 10 人を候補から外す（添字の大きい方から消す）
            let mut taken = taken;
            taken.sort_unstable();
            for index in taken.into_iter().rev() {
                pool.remove(index);
            }
            matches.push(proposal);
        }
    }
    matches
}

/// **いちばん長く待っている人から順に**、その人を含む幅 50 のウィンドウを試して、
/// 最初に成立した 1 組を返す。
///
/// 以前は基準をレート順（上から / 下から）に取っていた。それだと「上の帯にも下の帯にも
/// 10 人いて、自分はどちらにも入れる」中間の人が、毎回どちらかの帯に先を越されて
/// 1 時間以上取り残されることがあった（走査方向をランダムにしても、その人の帯が
/// 最初に試されるとは限らない）。待ち時間の長い順に基準を取れば、**その人を含む
/// 組み合わせが 1 つでも成立するなら、その人より長く待っている人がいない限り組まれる。**
///
/// 基準の人 1 人につき、その人を含むウィンドウ（下端は本人か、本人の下 50 以内の誰か）を
/// **人数の多い順**に試す（同数なら `direction` の側から）。ウィンドウの中の 10 人は
/// 最小費用流が待ち時間の長い順に選ぶ（[`assign_roles`]）ので、基準の人は
/// ロールの都合が付く限り入る。
///
/// **比較は両端を含む（`<=`）。**差ちょうど 50 は同じウィンドウに入る。
/// `rating_of` はウィンドウの判定に使うレート（実レートか、寄せ込み後の判定用レート）。
/// `direction` は同点（同じ待ち時間・同じ人数）の並びを決めるだけで、どちらでも
/// 成立する組み合わせの集合は変わらない。
fn find_one<O: SelectionObserver>(
    pool: &[&Candidate],
    rating_of: &dyn Fn(&Candidate) -> f64,
    ally_blocks: &AllyBlocks,
    now: Timestamp,
    ranges: MatchingRanges,
    direction: ScanDirection,
    observer: &mut O,
) -> Option<(Vec<usize>, ProposedMatch)> {
    let span = f64::from(ranges.up);
    // 基準: 待ち時間の長い順。同点は `direction` の側（上から / 下から）
    let mut anchors: Vec<usize> = (0..pool.len()).collect();
    anchors.sort_by(|&a, &b| {
        pool[a]
            .effective_enqueued_at()
            .cmp(&pool[b].effective_enqueued_at())
            .then_with(|| {
                let by_rating = rating_of(pool[b])
                    .partial_cmp(&rating_of(pool[a]))
                    .unwrap_or(std::cmp::Ordering::Equal);
                match direction {
                    ScanDirection::TopDown => by_rating,
                    ScanDirection::BottomUp => by_rating.reverse(),
                }
            })
            .then_with(|| pool[a].player_id.0.cmp(&pool[b].player_id.0))
    });
    for anchor in anchors {
        observer.record_anchor();
        let mine = rating_of(pool[anchor]);
        // この人を含むウィンドウの下端の候補: 本人と、本人の下 span 以内の人
        let mut floors: Vec<f64> = pool
            .iter()
            .map(|candidate| rating_of(candidate))
            .filter(|rating| *rating <= mine && mine - *rating <= span)
            .collect();
        floors.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        floors.dedup();
        let mut windows: Vec<Vec<usize>> = floors
            .into_iter()
            .map(|floor| {
                (0..pool.len())
                    .filter(|&i| {
                        let above = rating_of(pool[i]) - floor;
                        above >= 0.0 && above <= span && above <= f64::from(pool[i].down_range)
                    })
                    .collect()
            })
            .collect();
        // 人数の多い順。同数なら direction の側（上から: 下端が高い方、下から: 低い方）
        windows.sort_by(|a, b| {
            b.len().cmp(&a.len()).then_with(|| {
                let floor_a = rating_of(pool[a[0]]);
                let floor_b = rating_of(pool[b[0]]);
                let low_first = floor_a
                    .partial_cmp(&floor_b)
                    .unwrap_or(std::cmp::Ordering::Equal);
                match direction {
                    ScanDirection::TopDown => low_first.reverse(),
                    ScanDirection::BottomUp => low_first,
                }
            })
        });
        for window in windows {
            let window_started = observer.begin_measurement();
            observer.record_window(
                window.len() >= MATCH_SIZE,
                window_started.map(|started| started.elapsed()),
            );
            if window.len() < MATCH_SIZE {
                continue;
            }
            let role_started = observer.begin_measurement();
            let assignment = assign_roles(pool, &window, now);
            observer.record_role_assignment(
                assignment.is_some(),
                role_started.map(|started| started.elapsed()),
            );
            if let Some(assignment) = assignment {
                let split_started = observer.begin_measurement();
                let proposal = split_teams(pool, &assignment, ally_blocks, now, observer);
                observer.record_team_split(
                    proposal.is_some(),
                    split_started.map(|started| started.elapsed()),
                );
                if let Some(proposal) = proposal {
                    let taken = assignment.iter().map(|slot| slot.pool_index).collect();
                    return Some((taken, proposal));
                }
            }
        }
    }
    None
}

/// ロール枠への割り当て 1 件。
#[derive(Debug, Clone, Copy)]
struct Slot {
    /// `pool` の添字。
    pool_index: usize,
    /// [`ROLES`] の添字。
    role_index: usize,
    /// 割り当てられたロールの希望順（1 が第 1 希望）。
    priority: i16,
}

/// ウィンドウ内から 10 人をロール枠へ割り当てる。埋まらなければ `None`。
///
/// 枠は「5 ロール × 各 2 人」。07 §4-3 は右側を 10 枠（5 ロール × 2 チーム）と書いているが、
/// **どちらのチームに入るかは §4-2 の 4 で後から決める**ので、同じロールの 2 枠は区別が無い。
/// 容量 2 のロール頂点 1 個に畳んでも、流量も費用も同じグラフになる。
fn assign_roles(pool: &[&Candidate], window: &[usize], now: Timestamp) -> Option<Vec<Slot>> {
    let waits: Vec<i64> = window
        .iter()
        .map(|&i| (now - pool[i].effective_enqueued_at()).num_seconds().max(0))
        .collect();
    let longest = waits.iter().copied().max().unwrap_or(0);

    let source = 0;
    let player_node = |slot: usize| 1 + slot;
    let role_node = |role: usize| 1 + window.len() + role;
    let sink = 1 + window.len() + ROLES.len();

    let mut flow = MinCostFlow::new(sink + 1);
    let mut edges = Vec::new();
    for (slot, &i) in window.iter().enumerate() {
        flow.add_edge(source, player_node(slot), 1, 0);
        for wish in &pool[i].roles {
            let Some(role_index) = ROLES.iter().position(|role| *role == wish.role) else {
                // roles テーブルに無いロールは DB の FK が拒否するので通常は来ない。
                // 来たら黙って無視する（マッチメイカを止める理由にはならない）
                continue;
            };
            let cost = edge_cost(pool[i], wish, waits[slot], longest);
            let edge = flow.add_edge(player_node(slot), role_node(role_index), 1, cost);
            edges.push((edge, slot, role_index, wish.priority));
        }
    }
    for role in 0..ROLES.len() {
        flow.add_edge(role_node(role), sink, (MATCH_SIZE / ROLES.len()) as i64, 0);
    }

    let (filled, _cost) = flow.run(source, sink, MATCH_SIZE as i64);
    if filled != MATCH_SIZE as i64 {
        return None;
    }
    Some(
        edges
            .into_iter()
            .filter(|(edge, ..)| flow.used(*edge))
            .map(|(_, slot, role_index, priority)| Slot {
                pool_index: window[slot],
                role_index,
                priority,
            })
            .collect(),
    )
}

/// 1 本の「プレイヤー → ロール」の辺の費用。
///
/// 全ての辺に [`FLEX_FIRST_CHOICE_BONUS`] を下駄として履かせてあるので、値は必ず 0 以上に
/// なる。流量 1 につきこの種類の辺をちょうど 1 本通るため、下駄は最小解を変えない。
fn edge_cost(candidate: &Candidate, wish: &RoleWish, wait: i64, longest: i64) -> i64 {
    let waiting = WAIT_COST_PER_SECOND * (longest - wait);
    let preference = PRIORITY_COST * i64::from(wish.priority - 1);
    let flex = if candidate.flex_compensation && wish.priority == 1 {
        FLEX_FIRST_CHOICE_BONUS
    } else {
        0
    };
    FLEX_FIRST_CHOICE_BONUS + waiting + preference - flex
}

/// チーム平均レートの差が最小になる分け方を選ぶ（07 §4-2 の 4）。
///
/// 成立した 10 人に対する総当たり。`C(10,5)/2 = 126` 通りを、先頭のプレイヤーを red に
/// 固定して数え上げる。各チームに 5 ロールが 1 人ずつ入る分け方だけを残す。
fn split_teams<O: SelectionObserver>(
    pool: &[&Candidate],
    assignment: &[Slot],
    ally_blocks: &AllyBlocks,
    now: Timestamp,
    observer: &mut O,
) -> Option<ProposedMatch> {
    let ratings: Vec<f64> = assignment
        .iter()
        .map(|slot| pool[slot.pool_index].rating)
        .collect();

    let mut best: Option<(f64, u32)> = None;
    for mask in 0u32..(1 << MATCH_SIZE) {
        // 先頭を red に固定して対称な分け方を 1 回だけ数える（252 / 2 = 126 通り）
        if mask & 1 == 0 || mask.count_ones() as usize != TEAM_SIZE {
            continue;
        }
        let mut red_roles = 0u32;
        let mut red_sum = 0.0;
        let mut blue_sum = 0.0;
        for (index, slot) in assignment.iter().enumerate() {
            if mask & (1 << index) != 0 {
                red_roles |= 1 << slot.role_index;
                red_sum += ratings[index];
            } else {
                blue_sum += ratings[index];
            }
        }
        // 各チームに 5 ロールが 1 人ずつ（red が 5 種類なら blue も自動的に 5 種類）
        if red_roles.count_ones() as usize != ROLES.len() {
            continue;
        }
        observer.record_team_mask();
        let block_started = observer.begin_measurement();
        let blocked = has_same_team_block(pool, assignment, mask, ally_blocks);
        observer.record_block_check(blocked, block_started.map(|started| started.elapsed()));
        if blocked {
            observer.record_blocked_team_mask();
            continue;
        }
        let difference = ((red_sum - blue_sum) / TEAM_SIZE as f64).abs();
        if best.is_none_or(|(current, _)| difference < current) {
            best = Some((difference, mask));
        }
    }
    // 割り当ては 5 ロール × 各 2 人なので、条件を満たす分け方は必ず存在する
    let (_, mask) = best?;

    let mut players: Vec<ProposedPlayer> = assignment
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let candidate = pool[slot.pool_index];
            ProposedPlayer {
                player_id: candidate.player_id,
                team: if mask & (1 << index) != 0 {
                    Team::Red
                } else {
                    Team::Blue
                },
                role: ROLES[slot.role_index].to_owned(),
                assigned_preference: slot.priority,
                // 第 1 希望以外を割り当てられたか。次回のフレックス補償の入力
                role_was_flexed: slot.priority != 1,
                flex_compensation_applied: candidate.flex_compensation && slot.priority == 1,
                wait_seconds: u64::try_from((now - candidate.enqueued_at).num_seconds())
                    .unwrap_or(0),
                rating: candidate.rating,
                sigma: candidate.sigma,
            }
        })
        .collect();
    players.sort_by_key(|player| {
        (
            player.team != Team::Red,
            ROLES.iter().position(|role| *role == player.role),
        )
    });
    Some(ProposedMatch { players })
}

fn has_same_team_block(
    pool: &[&Candidate],
    assignment: &[Slot],
    mask: u32,
    ally_blocks: &AllyBlocks,
) -> bool {
    for (left_index, left_slot) in assignment.iter().enumerate() {
        let left_player = pool[left_slot.pool_index].player_id;
        for (right_index, right_slot) in assignment.iter().enumerate() {
            let left_is_red = mask & (1 << left_index) != 0;
            let right_is_red = mask & (1 << right_index) != 0;
            if left_index == right_index || left_is_red != right_is_red {
                continue;
            }
            let right_player = pool[right_slot.pool_index].player_id;
            if ally_blocks
                .get(&left_player)
                .is_some_and(|blocked| blocked.contains(&right_player))
            {
                return true;
            }
        }
    }
    false
}

/// 割り当て済みのロールごとの人数（テストと不変条件の確認に使う）。
pub fn role_counts(proposal: &ProposedMatch) -> HashMap<(&str, Team), usize> {
    let mut counts = HashMap::new();
    for player in &proposal.players {
        *counts
            .entry((player.role.as_str(), player.team))
            .or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use chrono::{TimeDelta, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::queue::model::{DEFAULT_DOWN_RANGE, DEFAULT_UP_RANGE};

    #[derive(Debug, Default)]
    struct SelectionDiagnostics {
        anchors: usize,
        large_enough_windows: usize,
        role_allocations: usize,
        role_allocation_failures: usize,
        team_splits: usize,
        failed_team_splits: usize,
        legal_role_masks: usize,
        block_checks: usize,
        block_rejected_masks: usize,
        window_time: Duration,
        role_time: Duration,
        block_check_time: Duration,
        split_time: Duration,
    }

    impl SelectionObserver for SelectionDiagnostics {
        fn begin_measurement(&self) -> Option<Instant> {
            Some(Instant::now())
        }

        fn record_anchor(&mut self) {
            self.anchors += 1;
        }

        fn record_window(&mut self, large_enough: bool, elapsed: Option<Duration>) {
            self.large_enough_windows += usize::from(large_enough);
            self.window_time += elapsed.unwrap_or_default();
        }

        fn record_role_assignment(&mut self, success: bool, elapsed: Option<Duration>) {
            self.role_allocations += 1;
            self.role_allocation_failures += usize::from(!success);
            self.role_time += elapsed.unwrap_or_default();
        }

        fn record_team_mask(&mut self) {
            self.legal_role_masks += 1;
        }

        fn record_block_check(&mut self, _blocked: bool, elapsed: Option<Duration>) {
            self.block_checks += 1;
            self.block_check_time += elapsed.unwrap_or_default();
        }

        fn record_blocked_team_mask(&mut self) {
            self.block_rejected_masks += 1;
        }

        fn record_team_split(&mut self, success: bool, elapsed: Option<Duration>) {
            self.team_splits += 1;
            self.failed_team_splits += usize::from(!success);
            self.split_time += elapsed.unwrap_or_default();
        }
    }

    /// 基準時刻。すべての待ち時間はここからの逆算。
    fn now() -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 30, 0).unwrap()
    }

    /// 1 人分。`waited` 秒だけ前からキューにいる。希望は渡した順に第 1・第 2・第 3 希望。
    fn who(n: u128, rating: f64, down_range: i32, waited: i64, roles: &[&str]) -> Candidate {
        Candidate {
            player_id: PlayerId(Uuid::from_u128(n)),
            rating,
            sigma: 500.0,
            down_range,
            enqueued_at: now() - TimeDelta::seconds(waited),
            priority_bonus_seconds: 0,
            roles: roles
                .iter()
                .enumerate()
                .map(|(index, role)| RoleWish {
                    role: (*role).to_owned(),
                    priority: index as i16 + 1,
                })
                .collect(),
            flex_compensation: false,
            unmatched_scans: 0,
        }
    }

    /// ロールがちょうど 2 人ずつ埋まる 10 人。全員同じレート・同じ待ち時間。
    fn full_roster(first_id: u128, rating: f64) -> Vec<Candidate> {
        (0..MATCH_SIZE)
            .map(|index| {
                who(
                    first_id + index as u128,
                    rating,
                    DEFAULT_DOWN_RANGE,
                    600,
                    &[ROLES[index % ROLES.len()]],
                )
            })
            .collect()
    }

    fn select_with_diagnostics(
        roster: &[Candidate],
        blocks: &AllyBlocks,
    ) -> (Vec<ProposedMatch>, SelectionDiagnostics) {
        let mut diagnostics = SelectionDiagnostics::default();
        let matches = select_matches_inner(
            roster,
            blocks,
            now(),
            MatchingRanges::default(),
            ScanDirection::TopDown,
            &mut diagnostics,
        );
        (matches, diagnostics)
    }

    fn ids(proposal: &ProposedMatch) -> Vec<u128> {
        let mut ids: Vec<u128> = proposal
            .player_ids()
            .into_iter()
            .map(|player| player.0.as_u128())
            .collect();
        ids.sort_unstable();
        ids
    }

    fn role_of(proposal: &ProposedMatch, n: u128) -> &ProposedPlayer {
        proposal
            .players
            .iter()
            .find(|player| player.player_id.0 == Uuid::from_u128(n))
            .expect("player is in the match")
    }

    /// 編成が 07 §3-1 と §4-2 の形を満たすこと。どのテストからも呼ぶ。
    fn assert_well_formed(proposal: &ProposedMatch) {
        assert_eq!(proposal.players.len(), MATCH_SIZE);
        for team in Team::ALL {
            let roles: Vec<&str> = proposal
                .players
                .iter()
                .filter(|player| player.team == team)
                .map(|player| player.role.as_str())
                .collect();
            assert_eq!(roles.len(), TEAM_SIZE, "{team} has five players");
            let mut sorted = roles.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                ROLES.len(),
                "{team} has each role once: {roles:?}"
            );
        }
        assert!(
            proposal.rating_span() <= f64::from(DEFAULT_UP_RANGE),
            "span {} exceeds the fixed +95 window",
            proposal.rating_span()
        );
        for role in ROLES {
            for team in Team::ALL {
                assert_eq!(role_counts(proposal).get(&(role, team)), Some(&1));
            }
        }
    }

    // ------------------------------------------------------------------
    // 走査の方向
    // ------------------------------------------------------------------

    /// 5 人ずつ 3 段（1610 / 1575 / 1550）。幅 50 なので上 2 段か下 2 段のどちらかしか
    /// 組めない。**上からは上 2 段、下からは下 2 段が先に確定する。**
    fn three_tiers() -> Vec<Candidate> {
        let mut roster = Vec::new();
        for (tier, rating) in [(100u128, 1610.0), (200, 1575.0), (300, 1550.0)] {
            for (index, role) in ROLES.iter().enumerate() {
                roster.push(who(
                    tier + index as u128,
                    rating,
                    DEFAULT_DOWN_RANGE,
                    600,
                    &[*role],
                ));
            }
        }
        roster
    }

    /// **同じキューでも、方向で先に組まれる帯が変わる。**上からだと最下段が残り、
    /// 下からだと最上段が残る。残った 5 人はどちらでも組めない（幅 50 に 10 人いない）。
    #[test]
    fn the_scan_direction_decides_which_band_forms_first() {
        let roster = three_tiers();
        let top_down = select_matches_scanning(
            &roster,
            &AllyBlocks::new(),
            now(),
            MatchingRanges::default(),
            ScanDirection::TopDown,
        );
        assert_eq!(top_down.len(), 1);
        assert_well_formed(&top_down[0]);
        let mut expected: Vec<u128> = (100..105).chain(200..205).collect();
        expected.sort_unstable();
        assert_eq!(ids(&top_down[0]), expected, "上からは 1610 と 1575 の帯");

        let bottom_up = select_matches_scanning(
            &roster,
            &AllyBlocks::new(),
            now(),
            MatchingRanges::default(),
            ScanDirection::BottomUp,
        );
        assert_eq!(bottom_up.len(), 1);
        assert_well_formed(&bottom_up[0]);
        let mut expected: Vec<u128> = (200..205).chain(300..305).collect();
        expected.sort_unstable();
        assert_eq!(ids(&bottom_up[0]), expected, "下からは 1575 と 1550 の帯");

        // 既定の入口は今までどおり上から
        assert_eq!(ids(&select_matches(&roster, now())[0]), ids(&top_down[0]));
    }

    /// **方向が変わっても、組の条件は変わらない。**幅・各人の下方向設定・ロールは同じ。
    #[test]
    fn a_bottom_up_scan_respects_the_same_window_rules() {
        // 最下段の 1 人が「下方向 10」に絞っている → 下からでも彼を基準にした窓は
        // 上に 50 まで見るので影響しない。逆に、1575 の帯の 1 人が下方向 10 なら、
        // 1550 を基準にした窓（差 25）には入れない
        let mut roster = three_tiers();
        roster[5].down_range = 10; // id 200（1575 帯の 1 人目）
        let bottom_up = select_matches_scanning(
            &roster,
            &AllyBlocks::new(),
            now(),
            MatchingRanges::default(),
            ScanDirection::BottomUp,
        );
        // 1550 を基準にした窓は 9 人（id 200 が抜ける）で組めない。次の基準（1575 帯）
        // では上 2 段が組める
        assert_eq!(bottom_up.len(), 1);
        assert_well_formed(&bottom_up[0]);
        let mut expected: Vec<u128> = (100..105).chain(200..205).collect();
        expected.sort_unstable();
        assert_eq!(ids(&bottom_up[0]), expected);
    }

    // ------------------------------------------------------------------
    // 成立の基本
    // ------------------------------------------------------------------

    #[test]
    fn exactly_ten_players_form_one_match() {
        let matches = select_matches(&full_roster(1, 1500.0), now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        assert_eq!(ids(&matches[0]), (1..=10).collect::<Vec<u128>>());
        assert!(
            matches[0]
                .players
                .iter()
                .all(|player| !player.role_was_flexed)
        );
    }

    #[test]
    fn an_ally_block_keeps_the_pair_on_opposing_teams() {
        let roster = full_roster(1, 1500.0);
        let mut blocks = AllyBlocks::new();
        blocks
            .entry(PlayerId(Uuid::from_u128(1)))
            .or_default()
            .insert(PlayerId(Uuid::from_u128(2)));

        let matches = select_matches_with_ally_blocks(&roster, &blocks, now());
        assert_eq!(matches.len(), 1);
        let proposal = &matches[0];
        assert_ne!(
            role_of(proposal, 1).team,
            role_of(proposal, 2).team,
            "an outgoing block only prevents teammates"
        );
    }

    #[test]
    fn a_block_between_same_role_players_does_not_reject_the_formation() {
        let roster = full_roster(1, 1500.0);
        let mut blocks = AllyBlocks::new();
        // Players 1 and 6 have the same fixed role and must already be opponents.
        blocks
            .entry(PlayerId(Uuid::from_u128(1)))
            .or_default()
            .insert(PlayerId(Uuid::from_u128(6)));

        let matches = select_matches_with_ally_blocks(&roster, &blocks, now());
        assert_eq!(matches.len(), 1);
        assert_ne!(role_of(&matches[0], 1).team, role_of(&matches[0], 6).team);
    }

    #[test]
    fn a_blocked_candidate_set_falls_through_to_the_next_window() {
        let mut roster = full_roster(1, 1551.0);
        roster.extend(full_roster(11, 1500.0));
        let mut blocks = AllyBlocks::new();
        let blocked = blocks.entry(PlayerId(Uuid::from_u128(1))).or_default();
        for player in 2..=10 {
            blocked.insert(PlayerId(Uuid::from_u128(player)));
        }

        let matches = select_matches_with_ally_blocks(&roster, &blocks, now());
        assert_eq!(matches.len(), 1);
        assert_eq!(ids(&matches[0]), (11..=20).collect::<Vec<u128>>());
        assert_well_formed(&matches[0]);
    }

    #[test]
    fn a_candidate_set_without_a_legal_team_split_is_rejected() {
        let roster = full_roster(1, 1500.0);
        let mut blocks = AllyBlocks::new();
        let blocked = blocks.entry(PlayerId(Uuid::from_u128(1))).or_default();
        for player in 2..=10 {
            blocked.insert(PlayerId(Uuid::from_u128(player)));
        }

        assert!(select_matches_with_ally_blocks(&roster, &blocks, now()).is_empty());
    }

    #[test]
    #[ignore = "benchmark; run cargo test --lib ally_block_benchmark -- --ignored --nocapture"]
    fn ally_block_benchmark_covers_realistic_queue_sizes_and_diagnoses_the_worst_case() {
        for queued in [30, 50, 100] {
            let roster = benchmark_roster(queued);
            let (baseline_formations, baseline_elapsed) =
                benchmark_case(&roster, &AllyBlocks::new());
            println!(
                "ally-block benchmark: queued={queued} condition=none formations={baseline_formations} avg_ms={:.3}",
                baseline_elapsed.as_secs_f64() * 1000.0,
            );

            for (label, active_percent, slots_per_active_player) in
                [("all-1", 100, 1), ("30pct-2", 30, 2)]
            {
                let blocks = benchmark_blocks(queued, active_percent, slots_per_active_player);
                let (formations, elapsed) = benchmark_case(&roster, &blocks);
                println!(
                    "ally-block benchmark: queued={queued} condition={label} active_blockers={} slots_per_active_player={slots_per_active_player} formations={formations} avg_ms={:.3}",
                    queued * active_percent / 100,
                    elapsed.as_secs_f64() * 1000.0,
                );
            }
        }

        // Keep the original all-player, two-slot case as a worst-case reference.
        let roster = benchmark_roster(300);
        let blocks = benchmark_blocks(300, 100, 2);
        let (baseline_formations, baseline_elapsed) = benchmark_case(&roster, &AllyBlocks::new());
        let (formations, elapsed) = benchmark_case(&roster, &blocks);
        let diagnostic_started = Instant::now();
        let (diagnostic_matches, diagnostics) = select_with_diagnostics(&roster, &blocks);
        let diagnostic_elapsed = diagnostic_started.elapsed();

        println!(
            "ally-block benchmark: queued=300 condition=all-2 formations={formations} avg_ms={:.3} baseline_formations={baseline_formations} baseline_avg_ms={:.3}",
            elapsed.as_secs_f64() * 1000.0,
            baseline_elapsed.as_secs_f64() * 1000.0,
        );
        println!(
            "ally-block diagnostics: queued=300 condition=all-2 formations={} total_ms={:.3} anchors={} large_windows={} role_allocations={} role_failures={} team_splits={} failed_team_splits={} legal_role_masks={} block_checks={} block_rejected_masks={} window_ms={:.3} role_ms={:.3} block_check_ms={:.3} split_ms={:.3}",
            diagnostic_matches.len(),
            diagnostic_elapsed.as_secs_f64() * 1000.0,
            diagnostics.anchors,
            diagnostics.large_enough_windows,
            diagnostics.role_allocations,
            diagnostics.role_allocation_failures,
            diagnostics.team_splits,
            diagnostics.failed_team_splits,
            diagnostics.legal_role_masks,
            diagnostics.block_checks,
            diagnostics.block_rejected_masks,
            diagnostics.window_time.as_secs_f64() * 1000.0,
            diagnostics.role_time.as_secs_f64() * 1000.0,
            diagnostics.block_check_time.as_secs_f64() * 1000.0,
            diagnostics.split_time.as_secs_f64() * 1000.0,
        );
    }

    fn benchmark_roster(queued: usize) -> Vec<Candidate> {
        (1..=queued as u128)
            .map(|id| {
                who(
                    id,
                    1500.0,
                    DEFAULT_DOWN_RANGE,
                    600,
                    &[ROLES[(id as usize - 1) % ROLES.len()]],
                )
            })
            .collect()
    }

    fn benchmark_blocks(
        queued: usize,
        active_percent: usize,
        slots_per_active_player: usize,
    ) -> AllyBlocks {
        let active_players = queued * active_percent / 100;
        let mut sources: Vec<u128> = (1..=queued as u128).collect();
        let mut seed = 0x4d595df4d0f33173_u64;
        if active_players != queued {
            for index in (1..sources.len()).rev() {
                seed = next_benchmark_random(&mut seed);
                sources.swap(index, (seed as usize) % (index + 1));
            }
        }

        let mut blocks = AllyBlocks::new();
        for source in sources.into_iter().take(active_players) {
            let blocked = blocks.entry(PlayerId(Uuid::from_u128(source))).or_default();
            while blocked.len() < slots_per_active_player {
                seed = next_benchmark_random(&mut seed);
                let target = 1 + u128::from(seed % queued as u64);
                if target != source {
                    blocked.insert(PlayerId(Uuid::from_u128(target)));
                }
            }
        }
        blocks
    }

    fn next_benchmark_random(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *seed
    }

    fn benchmark_case(roster: &[Candidate], blocks: &AllyBlocks) -> (usize, Duration) {
        const RUNS: u32 = 5;
        let mut formations = None;
        let started = Instant::now();
        for _ in 0..RUNS {
            let result = select_matches_with_ally_blocks(roster, blocks, now());
            if let Some(expected) = formations {
                assert_eq!(result.len(), expected, "benchmark must be deterministic");
            } else {
                formations = Some(result.len());
            }
        }
        (formations.unwrap_or_default(), started.elapsed() / RUNS)
    }

    #[test]
    fn fewer_than_ten_players_form_nothing() {
        let mut roster = full_roster(1, 1500.0);
        roster.pop();
        assert!(select_matches(&roster, now()).is_empty());
        assert!(select_matches(&[], now()).is_empty());
    }

    #[test]
    fn the_result_is_deterministic() {
        let roster = full_roster(1, 1500.0);
        assert_eq!(
            select_matches(&roster, now()),
            select_matches(&roster, now())
        );
        let mut shuffled = roster.clone();
        shuffled.reverse();
        assert_eq!(
            select_matches(&roster, now()),
            select_matches(&shuffled, now())
        );
    }

    // ------------------------------------------------------------------
    // 待ち時間（07 §4-2 の 3）
    // ------------------------------------------------------------------

    /// 11 人目がいても、待ち時間の合計が最大になる 10 人が選ばれる。
    #[test]
    fn the_least_patient_eleventh_player_is_left_out() {
        let mut roster = full_roster(1, 1500.0);
        // jungle 希望で 10 秒しか待っていない 11 人目。既存の jungle 2 人は 600 秒待っている
        roster.push(who(11, 1500.0, DEFAULT_DOWN_RANGE, 10, &["jungle"]));
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        assert_eq!(
            ids(&matches[0]),
            (1..=10).collect::<Vec<u128>>(),
            "the player who waited 10s must lose to the ones who waited 600s"
        );
    }

    /// 逆向き: 長く待っている 11 人目が入り、待ちの短い人が落ちる。
    #[test]
    fn a_long_waiting_newcomer_displaces_a_fresh_player() {
        let mut roster = full_roster(1, 1500.0);
        // full_roster は index % 5 でロールを配るので、3 番は jungle
        roster[2] = who(3, 1500.0, DEFAULT_DOWN_RANGE, 5, &["jungle"]);
        roster.push(who(11, 1500.0, DEFAULT_DOWN_RANGE, 3600, &["jungle"]));
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        let chosen = ids(&matches[0]);
        assert!(chosen.contains(&11), "{chosen:?}");
        assert!(!chosen.contains(&3), "{chosen:?}");
    }

    // ------------------------------------------------------------------
    // レート範囲（07 §3-1）
    // ------------------------------------------------------------------

    /// **境界の実測（1-2）。**差ちょうど 50 は同じ組に入る。51 は入らない。
    ///
    /// ウィンドウの条件は `above <= span`（`find_one`）なので、**両端を含む**。
    #[test]
    fn exactly_fifty_apart_share_a_match_but_fifty_one_does_not() {
        let build = |high: f64| {
            let mut roster: Vec<Candidate> = full_roster(1, 1500.0);
            for (index, candidate) in roster.iter_mut().enumerate() {
                if index % 2 == 0 {
                    candidate.rating = high;
                }
            }
            roster
        };
        assert!(
            select_matches(&build(1551.0), now()).is_empty(),
            "51 apart is outside the fixed 50 window"
        );
        let matches = select_matches(&build(1550.0), now());
        assert_eq!(matches.len(), 1, "50 apart is inside the window (<=)");
        assert_eq!(matches[0].rating_span(), 50.0);
    }

    /// 51 下の人とは組まされない。範囲は全員同じなので、条件 1 と条件 2 が一致する。
    #[test]
    fn a_player_is_never_matched_fifty_one_below() {
        let mut roster = full_roster(1, 1549.0);
        // 1 番だけ 51 上
        roster[0] = who(1, 1600.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        assert!(select_matches(&roster, now()).is_empty());

        // 50 下なら組める
        for candidate in roster.iter_mut().skip(1) {
            candidate.rating = 1550.0;
        }
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].rating_span(), 50.0);
    }

    /// **1486 以下の人は取り残された回数 x 14 だけ判定用レートが上がり、上の帯と組める。**
    /// 凍結されるレートは実レートのまま（`ProposedPlayer::rating`）。
    #[test]
    fn a_stranded_low_rated_player_is_lifted_into_the_window_above() {
        let mut roster = full_roster(1, 1560.0);
        // 1 番だけ 1486（74 下）。そのままでは 51 以上離れていて組めない
        roster[0] = who(1, 1486.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        assert!(select_matches(&roster, now()).is_empty());

        // 取り残し 1 回（1500、差 60）でもまだ 50 より遠い
        roster[0].unmatched_scans = 1;
        assert_eq!(
            roster[0].matching_rating(super::super::boost::Bounds {
                bottom: 1486.0,
                top: 1560.0
            }),
            1500.0
        );
        assert!(select_matches(&roster, now()).is_empty());

        // 2 回（1514）で差 46 になり組める
        roster[0].unmatched_scans = 2;
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        let lifted = matches[0]
            .players
            .iter()
            .find(|player| player.player_id == PlayerId(Uuid::from_u128(1)))
            .expect("the lifted player is in the match");
        assert_eq!(lifted.rating, 1486.0, "凍結するのは実レート");
        assert_eq!(
            matches[0].rating_span(),
            74.0,
            "実レートの幅は 50 を超えてよい"
        );

        // 1487 の人は何回取り残されても上がらない
        roster[0] = who(1, 1487.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        roster[0].unmatched_scans = 10;
        assert_eq!(
            roster[0].matching_rating(super::super::boost::Bounds {
                bottom: 1487.0,
                top: 1560.0
            }),
            1487.0
        );
        assert!(select_matches(&roster, now()).is_empty());
    }

    /// **いちばん上の人は取り残された回数 x 14 だけ判定用レートが下がり、下の帯と組める。**
    /// 前の逆。凍結されるレートは実レートのまま。
    #[test]
    fn a_stranded_top_rated_player_is_pulled_down_into_the_window_below() {
        let mut roster = full_roster(1, 1600.0);
        // 1 番だけ 1674（74 上）。そのままでは組めない
        roster[0] = who(1, 1674.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        assert!(select_matches(&roster, now()).is_empty());

        // 1 回（1660、差 60）でもまだ遠い
        roster[0].unmatched_scans = 1;
        assert_eq!(
            roster[0].matching_rating(super::super::boost::Bounds {
                bottom: 1600.0,
                top: 1674.0
            }),
            1660.0
        );
        assert!(select_matches(&roster, now()).is_empty());

        // 2 回（1646、差 46）で組める
        roster[0].unmatched_scans = 2;
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        let pulled = matches[0]
            .players
            .iter()
            .find(|player| player.player_id == PlayerId(Uuid::from_u128(1)))
            .expect("the top player is in the match");
        assert_eq!(pulled.rating, 1674.0, "凍結するのは実レート");
        assert_eq!(matches[0].rating_span(), 74.0);

        // 2 番目（1673）は、上に 1 人いる限り何回取り残されても動かない
        roster[0] = who(1, 1680.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        roster[1] = who(2, 1673.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[1]]);
        roster[1].unmatched_scans = 10;
        assert_eq!(
            roster[1].matching_rating(super::super::boost::Bounds {
                bottom: 1600.0,
                top: 1680.0
            }),
            1673.0
        );
        assert!(select_matches(&roster, now()).is_empty());
    }

    /// **寄せ込みで組めなくなっても、実レートで組めるなら組む。**
    ///
    /// 1650 の首位が 8 回取り残されて判定用 1550 になっていても、実レートの
    /// [1600, 1650] に 10 人いれば 1 周目（実レート）で成立する。
    #[test]
    fn a_pulled_down_top_player_still_matches_at_the_real_rating() {
        let mut roster = full_roster(1, 1600.0);
        roster[0] = who(1, 1650.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        roster[0].unmatched_scans = 8;
        let bounds = super::super::boost::Bounds {
            bottom: 1600.0,
            top: 1650.0,
        };
        assert_eq!(
            roster[0].matching_rating(bounds),
            1550.0,
            "寄せ込み後は 1550（帯の外）"
        );
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1, "実レートでは [1600, 1650] に 10 人いる");
        assert_eq!(matches[0].rating_span(), 50.0);
    }

    /// **1600 以上の首位は 1550 まで、1600 未満の首位は 65 まで**下がる。
    #[test]
    fn the_top_drop_stops_at_1550_or_sixty_five() {
        // 1720 の首位は 1550 まで下がり、1500〜1550 の帯と組める
        let mut roster = full_roster(1, 1500.0);
        roster[0] = who(1, 1720.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        roster[0].unmatched_scans = 20;
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1, "1720 → 1550 で 1500 の帯に入る");
        // 1549 の帯とは組めない（1550 より下へは行かない。差 51 以上なら帯の外）
        let mut roster = full_roster(1, 1499.0);
        roster[0] = who(1, 1720.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        roster[0].unmatched_scans = 50;
        assert!(select_matches(&roster, now()).is_empty(), "1550 で止まる");
        // 1590 の首位は 65 まで（1525）。1470〜1475 の帯（差 50〜55）には届かない…
        let mut roster = full_roster(1, 1470.0);
        roster[0] = who(1, 1590.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        roster[0].unmatched_scans = 20;
        assert!(
            select_matches(&roster, now()).is_empty(),
            "1525 と 1470 は差 55"
        );
        // …1475 なら差 50 で組める
        let mut roster = full_roster(1, 1475.0);
        roster[0] = who(1, 1590.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        roster[0].unmatched_scans = 20;
        assert_eq!(select_matches(&roster, now()).len(), 1);
    }

    /// **1430 以下は、取り残されていなくても 1430 として帯に入る。**
    #[test]
    fn a_player_below_1430_joins_the_1430_band_from_the_start() {
        // 1350 の人 1 人 + 1470 の 9 人。実レートでは差 120 で組めないが、1430 として見れば差 40
        let mut roster = full_roster(1, 1470.0);
        roster[0] = who(1, 1350.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        assert_eq!(roster[0].unmatched_scans, 0);
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1, "取り残し 0 回でも 1430 から");
        let lifted = matches[0]
            .players
            .iter()
            .find(|player| player.player_id == PlayerId(Uuid::from_u128(1)))
            .unwrap();
        assert_eq!(lifted.rating, 1350.0, "凍結するのは実レート");
        // 1481 の帯（差 51）には、取り残し 0 回では届かない
        let mut roster = full_roster(1, 1481.0);
        roster[0] = who(1, 1350.0, DEFAULT_DOWN_RANGE, 600, &[ROLES[0]]);
        assert!(select_matches(&roster, now()).is_empty());
        // 1 回取り残されれば 1444 で届く
        roster[0].unmatched_scans = 1;
        assert_eq!(select_matches(&roster, now()).len(), 1);
    }

    /// **いちばん長く待っている人が、成立しうる限り最初の組に入る。**
    ///
    /// 上の帯（1640〜1649）に 10 人、下の帯（1560〜1569）に 10 人、あいだの 1600 に
    /// 67 分待っている人が 1 人。1600 はどちらの帯とも組めるが、レート順に基準を取ると
    /// 上から / 下からのどちらでも先に純粋な帯が成立し、1600 は取り残され続ける。
    /// 待ち時間順に基準を取れば、1600 を含む組が最初に成立する。
    #[test]
    fn the_longest_waiting_player_is_matched_first_even_between_two_bands() {
        let mut roster = Vec::new();
        for n in 0..10u128 {
            roster.push(who(
                100 + n,
                1640.0 + n as f64,
                DEFAULT_DOWN_RANGE,
                600,
                &[ROLES[n as usize % ROLES.len()]],
            ));
            roster.push(who(
                200 + n,
                1560.0 + n as f64,
                DEFAULT_DOWN_RANGE,
                600,
                &[ROLES[n as usize % ROLES.len()]],
            ));
        }
        roster.push(who(1, 1600.0, DEFAULT_DOWN_RANGE, 67 * 60, &[ROLES[1]]));
        for direction in [ScanDirection::TopDown, ScanDirection::BottomUp] {
            let matches = select_matches_scanning(
                &roster,
                &AllyBlocks::default(),
                now(),
                MatchingRanges::default(),
                direction,
            );
            assert_eq!(matches.len(), 2, "{direction:?}");
            let first: Vec<PlayerId> = matches[0].players.iter().map(|p| p.player_id).collect();
            assert!(
                first.contains(&PlayerId(Uuid::from_u128(1))),
                "{direction:?}: 67 分待った 1600 が最初の組に入る: {first:?}"
            );
        }
    }

    /// **運営の優先度は待ち時間への加算として効く。**5 分しか待っていない人でも、
    /// 70 分の加算があれば 10 分待ちの人より先に基準になり、費用でも先に選ばれる。
    #[test]
    fn a_priority_grant_counts_as_extra_waiting_time() {
        let mut roster = full_roster(1, 1600.0);
        for candidate in roster.iter_mut() {
            candidate.enqueued_at = now() - TimeDelta::seconds(600);
        }
        // 11 人目: 5 分待ち + 70 分の加算。ロールは 1 番と同じ第 1 希望だけ
        let mut granted = who(99, 1600.0, DEFAULT_DOWN_RANGE, 300, &[ROLES[0]]);
        granted.priority_bonus_seconds = 70 * 60;
        roster.push(granted);
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        let chosen: Vec<PlayerId> = matches[0].players.iter().map(|p| p.player_id).collect();
        assert!(
            chosen.contains(&PlayerId(Uuid::from_u128(99))),
            "加算込みで最長の待ちなので入る: {chosen:?}"
        );
        // 加算が無ければ、後から来た人は 10 人の外に残る
        roster[10].priority_bonus_seconds = 0;
        let matches = select_matches(&roster, now());
        let chosen: Vec<PlayerId> = matches[0].players.iter().map(|p| p.player_id).collect();
        assert!(
            !chosen.contains(&PlayerId(Uuid::from_u128(99))),
            "{chosen:?}"
        );
    }

    /// 決定的な擬似乱数（乱数クレートを持ち込まない。LCG で十分）。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// **不変条件を、でたらめなキュー 2000 通りで確かめる。**
    ///
    /// レート・待ち時間・希望ロール（0〜3 個）・取り残し回数・運営の加算・味方ブロックを
    /// ランダムに作り、成立した組すべてについて:
    /// - 10 人が相異なり、複数の組に同じ人がいない
    /// - 各ロールちょうど 2 人、各チームに 5 ロールが 1 人ずつ、割り当ては本人の希望のどれか
    /// - 実レートか判定用レートのどちらかで max − min ≤ 50（2 周のどちらかで成立している）
    /// - 同じチームにブロックの組がいない
    /// - 同じ入力からは同じ出力（決定的）
    #[test]
    fn random_queues_never_violate_the_invariants() {
        let mut rng = Lcg(20_260_920);
        let mut formed_total = 0usize;
        for round in 0..2000u32 {
            let count = 1 + rng.below(40) as usize;
            let mut roster: Vec<Candidate> = (0..count)
                .map(|n| {
                    let wishes = rng.below(4) as usize;
                    let mut roles: Vec<&str> = Vec::new();
                    while roles.len() < wishes {
                        let role = ROLES[rng.below(5) as usize];
                        if !roles.contains(&role) {
                            roles.push(role);
                        }
                    }
                    let mut candidate = who(
                        n as u128 + 1,
                        1400.0 + rng.below(300) as f64,
                        DEFAULT_DOWN_RANGE,
                        rng.below(3600) as i64,
                        &roles,
                    );
                    candidate.unmatched_scans = rng.below(12) as i32;
                    candidate.priority_bonus_seconds = if rng.below(8) == 0 {
                        rng.below(180) as i64 * 60
                    } else {
                        0
                    };
                    candidate
                })
                .collect();
            // ブロックを何組か
            let mut blocks = AllyBlocks::new();
            for _ in 0..rng.below(4) {
                let a = PlayerId(Uuid::from_u128(rng.below(count as u64) as u128 + 1));
                let b = PlayerId(Uuid::from_u128(rng.below(count as u64) as u128 + 1));
                if a != b {
                    blocks.entry(a).or_default().insert(b);
                }
            }
            let direction = if rng.below(2) == 0 {
                ScanDirection::TopDown
            } else {
                ScanDirection::BottomUp
            };
            let bounds = super::super::boost::Bounds::of(roster.iter().map(|c| &c.rating));
            let run = |roster: &[Candidate]| {
                select_matches_scanning(
                    roster,
                    &blocks,
                    now(),
                    MatchingRanges::default(),
                    direction,
                )
            };
            let matches = run(&roster);
            assert_eq!(matches, run(&roster), "round {round}: 決定的でない");
            formed_total += matches.len();

            let mut seen: HashSet<PlayerId> = HashSet::new();
            for proposal in &matches {
                assert_eq!(proposal.players.len(), MATCH_SIZE, "round {round}");
                let mut red_roles: HashSet<&str> = HashSet::new();
                let mut blue_roles: HashSet<&str> = HashSet::new();
                let mut ids: Vec<PlayerId> = Vec::new();
                for player in &proposal.players {
                    assert!(
                        seen.insert(player.player_id),
                        "round {round}: 二重に組まれた"
                    );
                    ids.push(player.player_id);
                    let candidate = roster
                        .iter()
                        .find(|c| c.player_id == player.player_id)
                        .expect("候補にいる人");
                    assert!(
                        candidate.roles.iter().any(|w| w.role == player.role),
                        "round {round}: 希望に無いロール {}",
                        player.role
                    );
                    let set = match player.team {
                        Team::Red => &mut red_roles,
                        Team::Blue => &mut blue_roles,
                    };
                    assert!(
                        set.insert(player.role.as_str()),
                        "round {round}: 同じチームに同じロール"
                    );
                }
                assert_eq!(red_roles.len(), ROLES.len(), "round {round}");
                assert_eq!(blue_roles.len(), ROLES.len(), "round {round}");
                // 幅 50: 実レートか判定用レートのどちらかで収まっている
                let group: Vec<&Candidate> = ids
                    .iter()
                    .map(|id| roster.iter().find(|c| c.player_id == *id).unwrap())
                    .collect();
                let span_of = |f: &dyn Fn(&Candidate) -> f64| {
                    let max = group.iter().map(|c| f(c)).fold(f64::MIN, f64::max);
                    let min = group.iter().map(|c| f(c)).fold(f64::MAX, f64::min);
                    max - min
                };
                let real = span_of(&|c| c.rating);
                let shifted = span_of(&|c| c.matching_rating(bounds));
                assert!(
                    real <= 50.0 + 1e-9 || shifted <= 50.0 + 1e-9,
                    "round {round}: 幅 50 に収まっていない（実 {real}, 判定 {shifted}）"
                );
                // 同じチームにブロックの組がいない
                for a in &proposal.players {
                    for b in &proposal.players {
                        if a.team == b.team
                            && blocks
                                .get(&a.player_id)
                                .is_some_and(|set| set.contains(&b.player_id))
                        {
                            panic!("round {round}: ブロックの組が同じチーム");
                        }
                    }
                }
            }
            // 使い回しの検査用: roster は変えていない
            roster.clear();
        }
        assert!(
            formed_total > 200,
            "成立が少なすぎる（検査が空回り）: {formed_total}"
        );
    }

    /// **seed の差 100 で 1500 群と 1600 群が分離する。**今回の変更の狙い。
    #[test]
    fn the_fifteen_hundred_and_sixteen_hundred_pools_do_not_mix() {
        let mut roster = full_roster(1, 1500.0);
        for candidate in roster.iter_mut().take(5) {
            candidate.rating = 1600.0;
        }
        assert!(select_matches(&roster, now()).is_empty());
    }

    /// 中間の 1550 は、1500 群とも 1600 群とも組める（どちらも差ちょうど 50）。
    #[test]
    fn fifteen_fifty_bridges_both_pools_at_exactly_the_boundary() {
        // 1550 と 1600 の混成
        let mut upper = full_roster(1, 1550.0);
        for candidate in upper.iter_mut().take(5) {
            candidate.rating = 1600.0;
        }
        let matches = select_matches(&upper, now());
        assert_eq!(matches.len(), 1, "1550 と 1600 は差ちょうど 50");
        assert_well_formed(&matches[0]);
        assert_eq!(matches[0].rating_span(), 50.0);

        // 1550 と 1500 の混成
        let mut lower = full_roster(1, 1550.0);
        for candidate in lower.iter_mut().take(5) {
            candidate.rating = 1500.0;
        }
        let matches = select_matches(&lower, now());
        assert_eq!(matches.len(), 1, "1550 と 1500 も差ちょうど 50");
        assert_eq!(matches[0].rating_span(), 50.0);
    }

    // ------------------------------------------------------------------
    // ロール構成
    // ------------------------------------------------------------------

    /// 希望が偏っていて枠が埋まらないときは空を返す。
    #[test]
    fn a_lopsided_role_distribution_produces_nothing() {
        // 全員 jungle だけ。jungle は 2 枠しかない
        let all_jungle: Vec<Candidate> = (1..=12)
            .map(|n| who(n, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]))
            .collect();
        assert!(select_matches(&all_jungle, now()).is_empty());

        // 1 ロールだけ供給が消えても成立しない
        let mut roster = full_roster(1, 1500.0);
        roster[4] = who(5, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]);
        roster[9] = who(10, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]);
        assert!(select_matches(&roster, now()).is_empty());
    }

    /// 第 2 希望で枠を埋められるなら成立する。埋めた人は `role_was_flexed`。
    #[test]
    fn a_second_choice_fills_the_gap_and_is_recorded_as_flexed() {
        let mut roster = full_roster(1, 1500.0);
        roster[9] = who(10, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle", "bot_tank"]);
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        let filled = role_of(&matches[0], 10);
        assert_eq!(filled.role, "bot_tank");
        assert!(filled.role_was_flexed, "second choice is a flex");
    }

    // ------------------------------------------------------------------
    // フレックス補償（07 §5）
    // ------------------------------------------------------------------

    /// 対象者は争っている第 1 希望を取る。
    #[test]
    fn a_flex_compensated_player_wins_the_contested_first_choice() {
        let contested = |flex_target: u128| -> Vec<Candidate> {
            let mut roster = vec![
                who(1, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle", "bot_tank"]),
                who(2, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle", "bot_tank"]),
                who(3, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle", "bot_tank"]),
                who(4, 1500.0, DEFAULT_DOWN_RANGE, 600, &["bot_tank"]),
                who(5, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_carry"]),
                who(6, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_carry"]),
                who(7, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_support"]),
                who(8, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_support"]),
                who(9, 1500.0, DEFAULT_DOWN_RANGE, 600, &["bot_carry"]),
                who(10, 1500.0, DEFAULT_DOWN_RANGE, 600, &["bot_carry"]),
            ];
            for candidate in roster.iter_mut() {
                candidate.flex_compensation = candidate.player_id.0 == Uuid::from_u128(flex_target);
            }
            roster
        };

        for target in [1u128, 2, 3] {
            let matches = select_matches(&contested(target), now());
            assert_eq!(matches.len(), 1);
            assert_well_formed(&matches[0]);
            let compensated = role_of(&matches[0], target);
            assert_eq!(
                compensated.role, "jungle",
                "the compensated player takes the first choice"
            );
            assert!(!compensated.role_was_flexed);
            // 誰か 1 人は bot_tank に回る（補償は費用であって席の保証ではない）
            assert_eq!(
                matches[0]
                    .players
                    .iter()
                    .filter(|player| player.role_was_flexed)
                    .count(),
                1
            );
        }
    }

    /// 成立しないくらいなら第 2 希望で組む。**ハード制約にしない。**
    #[test]
    fn flex_compensation_never_blocks_a_match() {
        let mut roster = vec![
            who(1, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]),
            who(2, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]),
            who(
                3,
                1500.0,
                DEFAULT_DOWN_RANGE,
                600,
                &["top_carry", "bot_tank"],
            ),
            who(4, 1500.0, DEFAULT_DOWN_RANGE, 600, &["bot_tank"]),
            who(5, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_carry"]),
            who(6, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_carry"]),
            who(7, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_support"]),
            who(8, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_support"]),
            who(9, 1500.0, DEFAULT_DOWN_RANGE, 600, &["bot_carry"]),
            who(10, 1500.0, DEFAULT_DOWN_RANGE, 600, &["bot_carry"]),
        ];
        // 3 番は補償対象だが、第 1 希望の top_carry は 5・6 番で埋まっている
        roster[2].flex_compensation = true;
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1, "the match must still form");
        assert_well_formed(&matches[0]);
        let compensated = role_of(&matches[0], 3);
        assert_eq!(compensated.role, "bot_tank", "second choice is taken");
        assert!(compensated.role_was_flexed);
    }

    // ------------------------------------------------------------------
    // 1 回の実行で組めるだけ組む（07 §4-2）
    // ------------------------------------------------------------------

    #[test]
    fn twenty_players_form_two_matches_in_one_run() {
        let mut roster = full_roster(1, 1500.0);
        roster.extend(full_roster(11, 1500.0));
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 2);
        for proposal in &matches {
            assert_well_formed(proposal);
        }
        // 同じ人が 2 つの組に入らない
        let mut everyone: Vec<u128> = matches.iter().flat_map(ids).collect();
        everyone.sort_unstable();
        assert_eq!(everyone, (1..=20).collect::<Vec<u128>>());
    }

    #[test]
    fn nineteen_players_form_only_one_match() {
        let mut roster = full_roster(1, 1500.0);
        roster.extend(full_roster(11, 1500.0));
        roster.pop();
        assert_eq!(select_matches(&roster, now()).len(), 1);
    }

    // ------------------------------------------------------------------
    // チーム分け（07 §4-2 の 4）
    // ------------------------------------------------------------------

    /// 各ロールの 2 人の高低を交互にすると、差 0 の分け方が存在する。それが選ばれること。
    #[test]
    fn teams_are_split_to_minimise_the_average_rating_gap() {
        let roster = vec![
            who(1, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_carry"]),
            who(2, 1540.0, DEFAULT_DOWN_RANGE, 600, &["top_carry"]),
            who(3, 1540.0, DEFAULT_DOWN_RANGE, 600, &["top_support"]),
            who(4, 1500.0, DEFAULT_DOWN_RANGE, 600, &["top_support"]),
            who(5, 1500.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]),
            who(6, 1540.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]),
            who(7, 1540.0, DEFAULT_DOWN_RANGE, 600, &["bot_carry"]),
            who(8, 1500.0, DEFAULT_DOWN_RANGE, 600, &["bot_carry"]),
            who(9, 1520.0, DEFAULT_DOWN_RANGE, 600, &["bot_tank"]),
            who(10, 1520.0, DEFAULT_DOWN_RANGE, 600, &["bot_tank"]),
        ];
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        let gap = (matches[0].team_average(Team::Red) - matches[0].team_average(Team::Blue)).abs();
        assert!(
            gap < 1e-9,
            "an even split exists, so it must be found: {gap}"
        );
    }

    /// 差 0 が作れない配置では、作れる中で最小のものになる。
    ///
    /// 各ロールが 1500 と 1510 の 2 人。各チームは各ロールから 1 人ずつなので、
    /// 片方が受け取る 1510 の人数 `k` に対して平均差は `|5 − 2k| × 2`。最小は `k = 2` の 2。
    #[test]
    fn an_uneven_roster_lands_on_the_smallest_reachable_gap() {
        let roster: Vec<Candidate> = (0..MATCH_SIZE)
            .map(|index| {
                who(
                    index as u128 + 1,
                    if index < ROLES.len() { 1500.0 } else { 1510.0 },
                    95,
                    600,
                    &[ROLES[index % ROLES.len()]],
                )
            })
            .collect();
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        assert_well_formed(&matches[0]);
        let gap = (matches[0].team_average(Team::Red) - matches[0].team_average(Team::Blue)).abs();
        assert!((gap - 2.0).abs() < 1e-9, "gap = {gap}");
    }

    #[test]
    fn an_unbalanceable_roster_still_produces_the_smallest_gap() {
        // 1 人だけ 50 高い（範囲の上限ちょうど）。どう割っても差は出るが、最小のものを選ぶ
        let mut roster = full_roster(1, 1500.0);
        roster[0].rating = 1550.0;
        let matches = select_matches(&roster, now());
        assert_eq!(matches.len(), 1);
        let gap = (matches[0].team_average(Team::Red) - matches[0].team_average(Team::Blue)).abs();
        assert!((gap - 10.0).abs() < 1e-9, "gap = {gap}");
    }

    /// **1-4 の実測。**上のウィンドウで組めなかった中間層が、同じ周期で下のウィンドウでも
    /// 候補として評価されること。
    ///
    /// 再現手順どおり 1600×6 / 1550×6 / 1500×10 を並べ、上層（1600+1550=12人）は
    /// ロール構成が埋まらないようにする。`find_one` は anchor を高い方から下ろすので、
    /// 1550 を基準にしたウィンドウが失敗しても、次の 1500 基準のウィンドウで
    /// 1550 が再び候補に入る。**スキップされない。**
    #[test]
    fn the_middle_layer_is_still_a_candidate_in_the_lower_window() {
        let mut pool: Vec<Candidate> = Vec::new();
        // 1600 が 6 人。全員 jungle しか希望しない（jungle は 2 枠）
        for n in 1..=6u128 {
            pool.push(who(n, 1600.0, DEFAULT_DOWN_RANGE, 600, &["jungle"]));
        }
        // 1550 が 6 人。全員 bot_tank だけ
        for n in 7..=12u128 {
            pool.push(who(n, 1550.0, DEFAULT_DOWN_RANGE, 600, &["bot_tank"]));
        }
        // 1500 が 10 人。bot_tank だけ供給が無い（top_carry が 4 人）
        let lower_roles = [
            "top_carry",
            "top_carry",
            "top_carry",
            "top_carry",
            "top_support",
            "top_support",
            "jungle",
            "jungle",
            "bot_carry",
            "bot_carry",
        ];
        for (index, role) in lower_roles.iter().enumerate() {
            pool.push(who(
                13 + index as u128,
                1500.0,
                DEFAULT_DOWN_RANGE,
                600,
                &[role],
            ));
        }

        let matches = select_matches(&pool, now());
        assert_eq!(matches.len(), 1, "1550 + 1500 で 1 組成立する");
        let proposal = &matches[0];
        assert_well_formed(proposal);

        // 幅は 50（1500〜1550）。1600 は 1 人も入らない
        assert_eq!(proposal.rating_span(), 50.0);
        let ratings: Vec<f64> = proposal
            .players
            .iter()
            .map(|player| player.rating)
            .collect();
        assert_eq!(
            ratings.iter().filter(|rating| **rating == 1600.0).count(),
            0,
            "1600 は窓の外"
        );
        assert_eq!(
            ratings.iter().filter(|rating| **rating == 1550.0).count(),
            2,
            "bot_tank の 2 枠を中間層が埋める"
        );
        assert_eq!(
            ratings.iter().filter(|rating| **rating == 1500.0).count(),
            8
        );

        // 上のウィンドウ（1550 基準の 12 人）は実際に埋まらないことも確かめる
        let upper: Vec<Candidate> = pool
            .iter()
            .filter(|candidate| candidate.rating >= 1550.0)
            .cloned()
            .collect();
        assert_eq!(upper.len(), 12);
        assert!(
            select_matches(&upper, now()).is_empty(),
            "上層だけではロール構成が埋まらない"
        );
    }
}
