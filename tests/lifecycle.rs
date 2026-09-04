//! Phase 2 の必須テスト。DB にも Discord にも依存しない。

use std::time::{Duration, Instant};

use chrono::{TimeDelta, TimeZone, Utc};
use unite_matchsystem::lifecycle::{
    AbandonReason, AnomalyCause, Deadline, EventPayload, Formation, FormationEvent,
    FormationEventKind as K, FormationId, FormationPlayer, FormationState as S, HighestRating,
    HostAction, HostSelector, LifecycleConfig, PlayerId, Timestamp, TransitionError, apply,
    derive_snapshot, derive_state, derive_state_iter, next_deadline, next_host_action,
};
use uuid::Uuid;

fn pid(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn at(seq: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seq)
}

fn ev(seq: i64, kind: K, subject: Option<PlayerId>) -> FormationEvent {
    FormationEvent {
        kind,
        subject,
        occurred_at: at(seq),
        idempotency_key: format!("evt-{seq}"),
        payload: EventPayload::Empty,
    }
}

fn code(seq: i64, host: PlayerId, room_code: &str) -> FormationEvent {
    FormationEvent {
        payload: EventPayload::RoomCode(room_code.to_owned()),
        ..ev(seq, K::CodePosted, Some(host))
    }
}

fn abandon(seq: i64, reason: AbandonReason) -> FormationEvent {
    FormationEvent {
        payload: EventPayload::Abandon(reason),
        ..ev(seq, K::Abandoned, None)
    }
}

fn in_progress(host: PlayerId, room_code: &str) -> S {
    S::InProgress {
        host,
        room_code: room_code.to_owned(),
    }
}

/// 10人の formation。player 1 が最高レート。
fn ten_players() -> Formation {
    Formation {
        id: FormationId(Uuid::from_u128(0xf0)),
        players: (1..=10)
            .map(|i| FormationPlayer {
                player_id: pid(i),
                mu_at_match: 30.0 - i as f64,
                last_hosted_at: None,
            })
            .collect(),
    }
}

/// matched → confirmed の正常系イベント列。
fn happy_path(host: PlayerId) -> Vec<FormationEvent> {
    vec![
        ev(0, K::ThreadCreated, None),
        ev(1, K::HostAssigned, Some(host)),
        ev(2, K::HostAccepted, Some(host)),
        code(3, host, "1234"),
        ev(4, K::Confirmed, None),
    ]
}

#[test]
fn happy_path_matched_to_confirmed() {
    let host = pid(1);
    let events = happy_path(host);
    let expected = [
        S::AwaitingHost {
            pending: None,
            tried: 0,
        },
        S::AwaitingHost {
            pending: Some(host),
            tried: 1,
        },
        S::AwaitingCode { host, tried: 1 },
        in_progress(host, "1234"),
        S::Confirmed,
    ];

    let mut state = S::Matched;
    for (event, want) in events.iter().zip(expected) {
        state = apply(&state, event).unwrap();
        assert_eq!(state, want, "after {}", event.kind);
    }

    let (derived, anomalies) = derive_state(&events);
    assert_eq!(derived, S::Confirmed);
    assert!(anomalies.is_empty());
    assert!(S::Confirmed.is_terminal());
    assert!(S::Confirmed.requires_reports());
}

#[test]
fn three_consecutive_host_timeouts_end_in_abandoned() {
    let config = LifecycleConfig::default();
    let candidates = HighestRating::from_config(&config).candidates(&ten_players());
    assert_eq!(candidates, vec![pid(1), pid(2), pid(3)]);

    // アプリ側のループを模す: 指名 → 期限切れ → 次の候補、を候補が尽きるまで
    let mut events = vec![ev(0, K::ThreadCreated, None)];
    let mut seq = 1;
    let mut timeouts = 0;
    loop {
        let (state, anomalies) = derive_state(&events);
        assert!(anomalies.is_empty());
        match next_host_action(&state, &config) {
            Some(HostAction::Assign { candidate_index }) => {
                let host = candidates[candidate_index];
                events.push(ev(seq, K::HostAssigned, Some(host)));

                // 承諾期限は設定値（90秒）から出る
                let snapshot = derive_snapshot(&events);
                let (deadline, due) =
                    next_deadline(&snapshot.state, snapshot.since.unwrap(), None, &config).unwrap();
                assert_eq!(deadline, Deadline::HostAccept);
                assert_eq!(due, at(seq) + TimeDelta::seconds(90));

                events.push(ev(seq + 1, K::HostTimeout, Some(host)));
                seq += 2;
                timeouts += 1;
            }
            Some(HostAction::Abandon) => {
                assert_eq!(
                    state,
                    S::AwaitingHost {
                        pending: None,
                        tried: 3,
                    }
                );
                events.push(abandon(seq, AbandonReason::HostCandidatesExhausted));
                break;
            }
            None => panic!("unexpected state {state:?}"),
        }
    }

    assert_eq!(timeouts, 3);
    let (final_state, anomalies) = derive_state(&events);
    assert_eq!(final_state, S::Abandoned);
    assert!(anomalies.is_empty());
    assert!(final_state.is_terminal());
    assert!(
        !final_state.requires_reports(),
        "abandoned は報告ゲートを立てない"
    );
    assert_eq!(next_deadline(&final_state, at(99), None, &config), None);
}

#[test]
fn voided_after_code_posted_does_not_raise_the_report_gate() {
    let host = pid(1);
    let until_code = &happy_path(host)[..4];
    let (state, anomalies) = derive_state(until_code);
    assert_eq!(state, in_progress(host, "1234"));
    assert!(anomalies.is_empty());
    assert!(state.requires_reports(), "コード開示後は報告義務がある");

    let voided = apply(&state, &ev(4, K::Voided, None)).unwrap();
    assert_eq!(voided, S::Voided);
    assert!(!voided.requires_reports(), "voided は報告ゲートを立てない");
    assert!(voided.is_terminal());
    assert_eq!(
        next_deadline(&voided, at(4), None, &LifecycleConfig::default()),
        None
    );

    // voided の後に勝敗を確定させることはできない
    assert!(apply(&voided, &ev(5, K::Confirmed, None)).is_err());
    assert!(apply(&voided, &ev(5, K::Disputed, None)).is_err());

    // コード開示前の状態にも報告義務は無い
    for state in [
        S::Matched,
        S::AwaitingHost {
            pending: Some(host),
            tried: 1,
        },
        S::AwaitingCode { host, tried: 1 },
        S::Abandoned,
    ] {
        assert!(!state.requires_reports(), "{state:?}");
    }
}

#[test]
fn duplicate_idempotency_keys_are_no_ops_recorded_as_anomalies() {
    let (a, b, c) = (pid(1), pid(2), pid(3));
    let scenarios: Vec<Vec<FormationEvent>> = vec![
        happy_path(a),
        // ホストが次々と脱落し、最後は解散
        vec![
            ev(0, K::ThreadCreated, None),
            ev(1, K::HostAssigned, Some(a)),
            ev(2, K::HostTimeout, Some(a)),
            ev(3, K::HostAssigned, Some(b)),
            ev(4, K::HostDeclined, Some(b)),
            ev(5, K::HostAssigned, Some(c)),
            ev(6, K::HostAccepted, Some(c)),
            ev(7, K::HostTimeout, Some(c)),
            abandon(8, AbandonReason::HostCandidatesExhausted),
        ],
        // 係争になり、スタッフが無効化
        vec![
            ev(0, K::ThreadCreated, None),
            ev(1, K::HostAssigned, Some(a)),
            ev(2, K::HostAccepted, Some(a)),
            code(3, a, "1234"),
            ev(4, K::Disputed, None),
            ev(5, K::Voided, None),
        ],
    ];

    for events in scenarios {
        let (once, clean) = derive_state(&events);
        assert!(clean.is_empty());

        // 全イベントを二重にしても状態は同じ。2回目はすべて no-op として Anomaly に記録される
        let doubled: Vec<FormationEvent> =
            events.iter().flat_map(|e| [e.clone(), e.clone()]).collect();
        let (twice, anomalies) = derive_state(&doubled);
        assert_eq!(twice, once);
        assert_eq!(anomalies.len(), events.len());
        for (i, anomaly) in anomalies.iter().enumerate() {
            assert_eq!(anomaly.index, 2 * i + 1);
            assert_eq!(anomaly.kind, events[i].kind);
            assert_eq!(anomaly.idempotency_key, events[i].idempotency_key);
            assert_eq!(anomaly.cause, AnomalyCause::DuplicateKey);
        }
    }
}

#[test]
fn derive_state_never_fails_on_a_corrupted_log() {
    let (a, b) = (pid(1), pid(2));
    let events = vec![
        ev(0, K::ThreadCreated, None),
        ev(1, K::Confirmed, None), // #1 不正遷移（まだ試合前）
        ev(2, K::HostAssigned, Some(a)),
        ev(3, K::HostAccepted, Some(b)), // #3 対象不一致
        ev(4, K::HostAccepted, None),    // #4 対象なし
        ev(5, K::HostAccepted, Some(a)),
        ev(6, K::CodePosted, Some(a)), // #6 payload 不一致（部屋コードが無い）
        code(7, a, "1234"),
        FormationEvent {
            idempotency_key: "evt-7".into(), // #8 同一キー（unique 制約を通り抜けた想定）
            ..ev(8, K::Voided, None)
        },
        ev(9, K::Confirmed, None),
    ];

    // 検証用の apply は各不正イベントを Err にする。
    // ただし apply 単体は同一キーの重複（#8）を見抜けないので、素朴に適用し続けると
    // #8 の voided が通り、その後の #9 confirmed が弾かれる。重複排除が DB の unique 制約と
    // derive_state の責務である理由がこれ
    let mut state = S::Matched;
    let mut rejected = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match apply(&state, event) {
            Ok(next) => state = next,
            Err(_) => rejected.push(index),
        }
    }
    assert_eq!(rejected, vec![1, 3, 4, 6, 9]);

    // 導出は止まらず、不正なイベントをスキップして状態と Anomaly を返す
    let (state, anomalies) = derive_state(&events);
    assert_eq!(state, S::Confirmed);
    let indexes: Vec<usize> = anomalies.iter().map(|a| a.index).collect();
    assert_eq!(indexes, vec![1, 3, 4, 6, 8]);
    assert!(matches!(
        anomalies[0].cause,
        AnomalyCause::Rejected(TransitionError::InvalidTransition { .. })
    ));
    assert!(matches!(
        anomalies[1].cause,
        AnomalyCause::Rejected(TransitionError::SubjectMismatch { .. })
    ));
    assert!(matches!(
        anomalies[2].cause,
        AnomalyCause::Rejected(TransitionError::MissingSubject { .. })
    ));
    assert!(matches!(
        anomalies[3].cause,
        AnomalyCause::Rejected(TransitionError::PayloadMismatch { .. })
    ));
    assert_eq!(anomalies[4].cause, AnomalyCause::DuplicateKey);
    assert_eq!(anomalies[4].kind, K::Voided);

    let snapshot = derive_snapshot(&events);
    assert_eq!(snapshot.state, S::Confirmed);
    assert_eq!(snapshot.since, Some(at(9)));
}

#[test]
fn reposted_room_code_overwrites_the_previous_one() {
    let (a, b) = (pid(1), pid(2));
    let events = vec![
        ev(0, K::ThreadCreated, None),
        ev(1, K::HostAssigned, Some(a)),
        ev(2, K::HostAccepted, Some(a)),
        code(3, a, "111111"),
        code(4, a, "222222"), // 違うキー、違うコード＝貼り直し
    ];
    let snapshot = derive_snapshot(&events);
    assert_eq!(snapshot.state, in_progress(a, "222222"));
    assert!(snapshot.anomalies.is_empty());
    assert_eq!(snapshot.since, Some(at(4)));

    // 貼り直せるのはホスト本人だけ
    assert!(matches!(
        apply(&snapshot.state, &code(5, b, "333333")),
        Err(TransitionError::SubjectMismatch { .. })
    ));
}

#[test]
fn matched_can_be_abandoned_when_thread_creation_fails() {
    let events = [abandon(0, AbandonReason::ThreadCreationFailed)];
    let (state, anomalies) = derive_state(&events);
    assert_eq!(state, S::Abandoned);
    assert!(anomalies.is_empty());
    assert!(state.is_terminal());
    assert!(!state.requires_reports());

    // 理由の無い abandoned は検証で弾く
    assert!(matches!(
        apply(&S::Matched, &ev(0, K::Abandoned, None)),
        Err(TransitionError::PayloadMismatch { .. })
    ));
    // 解散後は何も積めない
    assert!(apply(&state, &ev(1, K::ThreadCreated, None)).is_err());
}

/// スレッド作成後、候補を打診してはタイムアウトする、を `cycles` 回繰り返すイベント列。
/// 状態機械自体は回数を制限しない（上限は設定と `next_host_action` の責務）。
fn host_churn(cycles: usize) -> Vec<FormationEvent> {
    let mut events = Vec::with_capacity(cycles * 2 + 1);
    events.push(ev(0, K::ThreadCreated, None));
    for i in 0..cycles {
        let host = pid((i % 2) as u128 + 1);
        let seq = (2 * i + 1) as i64;
        events.push(ev(seq, K::HostAssigned, Some(host)));
        events.push(ev(seq + 1, K::HostTimeout, Some(host)));
    }
    events
}

#[test]
fn derive_state_visits_each_event_exactly_once() {
    // 構造的な保証: イテレータの消費回数 == イベント数（一度しか走査しない）
    let events = host_churn(10_000);
    let mut visited = 0usize;
    let (state, anomalies) = derive_state_iter(events.iter().inspect(|_| visited += 1));
    assert_eq!(visited, events.len());
    assert!(anomalies.is_empty());
    assert_eq!(
        state,
        S::AwaitingHost {
            pending: None,
            tried: 10_000,
        }
    );
}

fn fastest_of<R>(runs: usize, mut f: impl FnMut() -> R) -> Duration {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(f());
            start.elapsed()
        })
        .min()
        .unwrap()
}

/// 実測。保証ではなく目安であり、CI では走らせない（`cargo test -- --ignored` で手動実行）。
/// 実際の formation は 15 イベント程度なので、この規模は現実とは無関係。閾値は締めない。
#[test]
#[ignore = "timing measurement, not a guarantee; run manually with --ignored"]
fn derive_state_scaling_measurement() {
    let small = host_churn(20_000);
    let large = host_churn(200_000);
    let t_small = fastest_of(3, || derive_state(&small));
    let t_large = fastest_of(3, || derive_state(&large));
    assert!(
        t_large <= t_small * 40 + Duration::from_millis(20),
        "small={t_small:?} large={t_large:?}"
    );
}
