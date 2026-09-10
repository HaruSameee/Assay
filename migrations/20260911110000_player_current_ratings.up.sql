-- 現在レートの合算を**1 か所に集約する**。
--
-- これまで「1500 + Σ調整 + Σ試合増減」という同じ式が 8 か所に散っていた
-- （キューの範囲判定・マッチメイカの凍結値・マッチ画面の名簿・管理パネル・
--   公開プロフィール・ランキング・キューのプレビュー・Rust の 1 人分の経路）。
-- 導出の入口が増えるたびに 8 か所すべてを直す必要があり、1 つ忘れると
-- 「ランキングだけ反映されない」形の事故になる。
--
-- 集約の前に 8 か所が同じ値を返すことを実測済み（tests/rating.rs の
-- `every_rating_path_agrees`）。**このマイグレーションで値は変わらない。**
--
-- 不変条件:
--   * **レーティングは導出値**（docs/02 不変条件 2）。現在値のテーブルは作らない。
--     このビューも実体を持たず、読むたびに追記ログから計算し直される。
--   * **recompute が唯一の計算経路**（同 3）。シーズン境界も増減幅も、
--     このビューが引く先（current_season / current_rating_params）に集約済み。
--   * 1 試合もしていない人も**必ず 1 行返す**。`players` を主にした左外部結合なので、
--     `player_match_rating_deltas` に行が無くても既定値が出る（INNER JOIN にしない）。

create view player_current_ratings as
select
    p.id                                                     as player_id,
    -- 初期値は一律 1500（07 §2-1）。1600 は rating_adjustments への +100 の追記であって、
    -- 第二の初期値ではない。Rust 側の rating::DEFAULT_DISPLAY_MU と同じ値
    1500.0::double precision
        + coalesce(adjustments.delta_mu, 0::double precision)
        + coalesce(matches.delta, 0::double precision)        as rating,
    -- σ は画面に出さない。マッチメイカの凍結値と Rust の導出だけが読む。
    -- Rust 側の rating::DEFAULT_DISPLAY_SIGMA と同じ値
    500.0::double precision
        + coalesce(adjustments.delta_sigma, 0::double precision) as sigma,
    -- 確定した試合の数。ランキングの掲載条件（5 試合以上）と
    -- 公開プロフィールの「確定した試合」がこの 1 か所を読む
    coalesce(matches.played, 0::bigint)                      as matches
from players p
left join lateral (
    select sum(a.delta_mu) as delta_mu, sum(a.delta_sigma) as delta_sigma
    from season_rating_adjustments a
    where a.player_id = p.id
) adjustments on true
left join (
    select d.player_id, sum(d.delta) as delta, count(*)::bigint as played
    from player_match_rating_deltas d
    group by d.player_id
) matches on matches.player_id = p.id;

comment on view player_current_ratings is
    '現在レート・σ・確定した試合数を 1 か所で合算する唯一のビュー。1500 + Σ season_rating_adjustments + Σ player_match_rating_deltas。導出値であり実体を持たない。レートを読む経路はすべてここを引くこと（式を各モジュールの SQL に書かない）。1 試合もしていない人も 1 行返る';
