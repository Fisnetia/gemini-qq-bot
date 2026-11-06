//! Tests moved out verbatim (std-split); assertions unchanged.

use super::*;

const PARTNER: &str = "20001";
const OTHER: &str = "20002";
const OWNER: &str = "10001";

fn exchange(damper: &Damper, at: i64, sender: &str, addressed: bool) {
    damper.note_reply("g", at, sender, addressed);
}

fn say(
    damper: &Damper,
    now: i64,
    id: i64,
    sender: &str,
    at: i64,
    addressed: bool,
) -> (Verdict, Vec<Event>) {
    damper.observe(
        "g",
        now,
        &[Observation {
            id,
            sender,
            at,
            addressed,
            from_owner: sender == OWNER,
        }],
    )
}

#[test]
fn the_streak_climbs_through_the_levels_and_holds_at_each_one() {
    let damper = Damper::new();
    for round in 1..=L1_STREAK {
        assert_eq!(
            damper.tier("g"),
            Tier::from_streak(round - 1),
            "before round {round}"
        );
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    assert_eq!(damper.tier("g"), Tier::L1);

    for round in L1_STREAK + 1..=L2_STREAK {
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    assert_eq!(damper.tier("g"), Tier::L2);
    for round in L2_STREAK + 1..=L3_STREAK {
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    assert_eq!(damper.tier("g"), Tier::L3);

    let (verdict, _) = say(&damper, 1_020, 1, PARTNER, 1_015, false);
    assert_eq!(verdict.tier, Tier::L3);
    assert!(verdict.addressed_only);
    // Only the partner is held; anyone else in the room is unaffected.
    assert!(verdict.hold(PARTNER, false, 999, 99).is_some());
    assert!(verdict.hold(PARTNER, true, 999, 99).is_none());
    assert!(verdict.hold(OTHER, false, 999, 99).is_none());
}

#[test]
fn a_third_party_ends_the_episode_and_clears_the_streak() {
    let damper = Damper::new();
    for round in 1..=L1_STREAK {
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    assert_eq!(damper.tier("g"), Tier::L1);
    let (verdict, events) = say(&damper, 1_010, 1, OTHER, 1_009, false);
    assert_eq!(verdict.tier, Tier::L0);
    assert_eq!(verdict.streak, 0);
    assert!(events.contains(&Event::Reset {
        reason: ResetReason::ThirdParty
    }));
    // The count restarts from whoever made the bot answer again.
    exchange(&damper, 1_020, PARTNER, false);
    assert_eq!(damper.tier("g"), Tier::L0);
    let (verdict, _) = say(&damper, 1_021, 2, PARTNER, 1_020, false);
    assert_eq!(verdict.streak, 1);
}

#[test]
fn ten_quiet_minutes_end_the_episode() {
    let damper = Damper::new();
    for round in 1..=L1_STREAK {
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    assert_eq!(damper.tier("g"), Tier::L1);
    // Nothing at all arrives, and the clock walks past a whole window.
    let (verdict, events) = damper.observe("g", 1_006 + WINDOW_SECONDS, &[]);
    assert_eq!(verdict.tier, Tier::L0);
    assert!(events.contains(&Event::Reset {
        reason: ResetReason::Silence
    }));
}

#[test]
fn an_addressed_round_neither_extends_nor_breaks_the_streak() {
    let damper = Damper::new();
    for round in 1..=L1_STREAK - 1 {
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    exchange(&damper, 1_100, PARTNER, true);
    let (verdict, _) = say(&damper, 1_101, 1, PARTNER, 1_100, true);
    assert_eq!(verdict.streak, L1_STREAK - 1, "an @ is not a round");
    assert_eq!(verdict.tier, Tier::L0);
    exchange(&damper, 1_102, PARTNER, false);
    assert_eq!(damper.tier("g"), Tier::L1);
}

#[test]
fn the_soft_cap_holds_one_sender_and_spares_the_others() {
    let damper = Damper::new();
    // The cap needs message volume as well as rounds, so the fixture gives this
    // sender the volume a real flood carries (see `CAP_MIN_SENDER_MESSAGES`), plus
    // enough neutral chatter that the window is not 100% one person.
    for index in 0..CAP_MIN_SENDER_MESSAGES {
        say(&damper, 1_080, 100 + index as i64, OTHER, 1_050, false);
    }
    for index in 0..10 {
        say(&damper, 1_080, 200 + index as i64, PARTNER, 1_060, false);
    }
    // The rounds land after the volume: `Event::Cap` is announced on the tick that
    // first sees both counts at once, and never again for the same sender. Feeding
    // the messages first makes the observation below that crossing tick.
    for round in 0..SOFT_CAP_ROUNDS {
        exchange(&damper, 1_000 + round as i64, OTHER, false);
    }
    let (verdict, events) = say(&damper, 1_100, 1, OTHER, 1_090, false);
    assert!(events.contains(&Event::Cap {
        sender: OTHER.to_string()
    }));
    assert!(verdict.hold(OTHER, false, 999, 99).is_some());
    // Addressed still passes, and so does anyone else.
    assert!(verdict.hold(OTHER, true, 0, 1).is_none());
    assert!(verdict.hold(PARTNER, false, 999, 99).is_none());
}

#[test]
fn the_soft_cap_needs_message_volume_not_just_rounds() {
    let damper = Damper::new();
    for round in 0..SOFT_CAP_ROUNDS {
        exchange(&damper, 1_000 + round as i64, PARTNER, false);
    }
    // Eight replies to ten messages: a one-to-one chat in a quiet hour. The bot has
    // answered a lot, but nobody is flooding, so nothing is capped.
    for index in 0..10 {
        say(&damper, 1_010, 100 + index as i64, PARTNER, 1_000, false);
    }
    let (verdict, events) = damper.observe("g", 1_011, &[]);
    assert!(
        !events.iter().any(|e| matches!(e, Event::Cap { .. })),
        "events: {events:?}"
    );
    assert!(verdict.capped.is_empty(), "verdict: {verdict:?}");
    assert!(
        verdict.hold(PARTNER, false, 999, 99).is_none(),
        "verdict: {verdict:?}"
    );
}

#[test]
fn the_soft_cap_outlives_an_episode_reset_and_expires_with_the_window() {
    let damper = Damper::new();
    for round in 0..SOFT_CAP_ROUNDS {
        exchange(&damper, 1_000 + round as i64, PARTNER, false);
    }
    // Volume: the partner has to fill the window before a cap can apply.
    for index in 0..CAP_MIN_SENDER_MESSAGES {
        say(&damper, 1_005, 100 + index as i64, PARTNER, 990, false);
    }
    // The third party's chatter lands as one batch, so the reset and the widened
    // window happen in the same tick. Fed one message at a time, the first of them
    // would reset the tier and be immediately re-raised by a window that is still
    // ~94% the partner — and the tier never comes back down on its own.
    let later = 1_000 + WINDOW_SECONDS + 10;
    let chatter: Vec<Observation<'_>> = (0..18)
        .map(|index| Observation {
            id: 300 + index,
            sender: OTHER,
            at: 1_006,
            addressed: false,
            from_owner: false,
        })
        .collect();
    // Someone else opens a new episode: the streak clears, the cap does not.
    let (verdict, _) = damper.observe("g", 1_009, &chatter);
    assert_eq!(verdict.tier, Tier::L0, "verdict: {verdict:?}");
    assert!(verdict.capped.iter().any(|s| s == PARTNER));
    // Once those rounds slide out of the window the cap expires too.
    let (verdict, _) = damper.observe("g", later, &[]);
    assert!(verdict.capped.is_empty());
}

#[test]
fn a_normal_multi_person_chat_never_damps() {
    let damper = Damper::new();
    let mut now = 1_000;
    let crowd = [PARTNER, OTHER, "20003", "20004", "20005"];
    for round in 0..35 {
        let sender = crowd[round % crowd.len()];
        now += 5;
        say(&damper, now, round as i64 * 2, sender, now - 1, false);
        exchange(&damper, now, sender, false);
    }
    let (verdict, _) = damper.observe("g", now + 1, &[]);
    assert_eq!(verdict.tier, Tier::L0, "verdict: {verdict:?}");
    // Seven rounds each: a lively room, but nobody is over the soft cap.
    assert!(verdict.capped.is_empty(), "verdict: {verdict:?}");
    assert!(verdict.concentration < 0.5);
}

#[test]
fn a_concentrated_window_opens_l1_without_a_streak() {
    let damper = Damper::new();
    // Four rounds to different people as a warm-up, then a burst of one person.
    for round in 0..L1_BOT_ROUNDS {
        exchange(
            &damper,
            1_000 + round as i64,
            ["a", "b", "c", "d"][round],
            false,
        );
    }
    let mut id = 0;
    for _ in 0..L1_WINDOW_MESSAGES {
        id += 1;
        say(&damper, 1_100, id, PARTNER, 1_050, false);
    }
    let (verdict, _) = damper.observe("g", 1_101, &[]);
    assert_eq!(verdict.tier, Tier::L1, "verdict: {verdict:?}");
    assert!(verdict.concentration >= 0.8);
}

#[test]
fn the_prompt_hint_appears_at_l1_and_at_sixty_percent_concentration() {
    let damper = Damper::new();
    assert!(damper.hint("g").is_none());
    for round in 1..=L1_STREAK {
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    damper.observe("g", 1_100, &[]);
    let hint = damper.hint("g").expect("hint at L1");
    assert!(hint.contains("L1"), "hint: {hint}");
    assert!(hint.contains("集中度"), "hint: {hint}");
}

#[test]
fn rows_are_counted_once_however_often_the_tick_sees_them() {
    let damper = Damper::new();
    let observation = Observation {
        id: 7,
        sender: PARTNER,
        at: 1_000,
        addressed: false,
        from_owner: false,
    };
    for _ in 0..5 {
        let (verdict, _) = damper.observe("g", 1_001, std::slice::from_ref(&observation));
        assert_eq!(verdict.window_messages, 1);
    }
}

/// `Tier` is compared with `>` throughout the state machine, so the derived order
/// is load-bearing, not cosmetic.
#[test]
fn tiers_are_strictly_ordered_and_level_matches_the_order() {
    assert!(Tier::L3 > Tier::L2);
    assert!(Tier::L2 > Tier::L1);
    assert!(Tier::L1 > Tier::L0);
    assert_eq!(Tier::default(), Tier::L0);
    assert_eq!(Tier::L0.level(), 0);
    assert_eq!(Tier::L1.level(), 1);
    assert_eq!(Tier::L2.level(), 2);
    assert_eq!(Tier::L3.level(), 3);
}

#[test]
fn from_streak_changes_level_exactly_at_the_documented_counts() {
    let table = [
        (0, Tier::L0),
        (1, Tier::L0),
        (L1_STREAK - 1, Tier::L0),
        (L1_STREAK, Tier::L1),
        (L1_STREAK + 1, Tier::L1),
        (L2_STREAK - 1, Tier::L1),
        (L2_STREAK, Tier::L2),
        (L3_STREAK - 1, Tier::L2),
        (L3_STREAK, Tier::L3),
        (u32::MAX, Tier::L3),
    ];
    for (streak, expected) in table {
        assert_eq!(Tier::from_streak(streak), expected, "streak={streak}");
    }
}

/// The held window grows with the level, but never shrinks: a group that reached
/// `L2` must not get the `L0` treatment back by accident.
#[test]
fn quiet_windows_and_pending_floors_are_monotonic_in_the_level() {
    let tiers = [Tier::L0, Tier::L1, Tier::L2, Tier::L3];
    let quiets: Vec<i64> = tiers.iter().map(|t| t.quiet_seconds()).collect();
    let pendings: Vec<usize> = tiers.iter().map(|t| t.min_pending()).collect();
    assert_eq!(
        quiets,
        vec![0, L1_QUIET_SECONDS, L2_QUIET_SECONDS, L2_QUIET_SECONDS]
    );
    assert_eq!(pendings, vec![1, 1, L2_MIN_PENDING, L2_MIN_PENDING]);
    assert!(quiets.windows(2).all(|w| w[0] <= w[1]));
    assert!(pendings.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn reset_reasons_have_stable_audit_slugs() {
    assert_eq!(ResetReason::ThirdParty.as_str(), "third_party");
    assert_eq!(ResetReason::Owner.as_str(), "owner");
    assert_eq!(ResetReason::Silence.as_str(), "silence");
}

/// The neutral verdict is what a poisoned lock returns, so it must let every batch
/// through: fail open, never fail closed.
#[test]
fn the_neutral_verdict_never_holds_anything() {
    let neutral = Verdict::neutral();
    assert_eq!(neutral.tier, Tier::L0);
    assert_eq!(neutral.hold_seconds, 0);
    assert_eq!(neutral.min_pending, 1);
    assert!(!neutral.addressed_only);
    assert!(neutral.dominant.is_none());
    assert!(neutral.capped.is_empty());
    assert_eq!(neutral.hold("anyone", false, 0, 1), None);
    assert_eq!(neutral.hold("anyone", false, 10_000, 999), None);
}

#[test]
fn an_addressed_batch_is_never_held_whatever_the_level() {
    let verdict = Verdict {
        tier: Tier::L3,
        dominant: Some(PARTNER.into()),
        addressed_only: true,
        capped: vec![PARTNER.into()],
        hold_seconds: L2_QUIET_SECONDS,
        min_pending: L2_MIN_PENDING,
        ..Verdict::neutral()
    };
    // Addressed wins over every hold, including two hard ones.
    assert_eq!(verdict.hold(PARTNER, true, 0, 0), None);
    assert!(verdict.hold(PARTNER, false, 0, 0).is_some());
}

#[test]
fn the_l3_hold_is_reported_before_the_soft_cap() {
    // A partner who reached L3 by streak has also crossed the cap. Reporting "cap"
    // would hide the real reason, so the tier wins.
    let verdict = Verdict {
        tier: Tier::L3,
        dominant: Some(PARTNER.into()),
        addressed_only: true,
        capped: vec![PARTNER.into(), OTHER.into()],
        hold_seconds: L2_QUIET_SECONDS,
        min_pending: L2_MIN_PENDING,
        ..Verdict::neutral()
    };
    let hold = verdict.hold(PARTNER, false, 0, 0).expect("应拦住");
    assert_eq!(hold.reason, "l3");
    assert!(hold.hard);
    // A capped sender who is NOT the dominant one still reports the cap.
    let hold = verdict.hold(OTHER, false, 0, 0).expect("应拦住");
    assert_eq!(hold.reason, "cap");
    assert!(hold.hard);
}

#[test]
fn the_soft_cap_outranks_the_quiet_window() {
    let verdict = Verdict {
        tier: Tier::L2,
        capped: vec![PARTNER.into()],
        hold_seconds: L2_QUIET_SECONDS,
        min_pending: L2_MIN_PENDING,
        ..Verdict::neutral()
    };
    let hold = verdict.hold(PARTNER, false, 1, 1).expect("应拦住");
    assert_eq!(hold.reason, "cap");
    assert!(hold.hard, "额度不应被批次封顶冲开");
}

#[test]
fn quiet_and_thin_holds_are_soft_and_named_after_their_level() {
    let l1 = Verdict {
        tier: Tier::L1,
        hold_seconds: L1_QUIET_SECONDS,
        min_pending: 1,
        ..Verdict::neutral()
    };
    let hold = l1
        .hold(PARTNER, false, L1_QUIET_SECONDS - 1, 5)
        .expect("应拦住");
    assert_eq!(hold.reason, "l1_quiet");
    assert!(!hold.hard, "静默期必须能被封顶冲开");
    // Exactly at the window the batch is allowed through.
    assert_eq!(l1.hold(PARTNER, false, L1_QUIET_SECONDS, 1), None);

    let l2 = Verdict {
        tier: Tier::L2,
        hold_seconds: L2_QUIET_SECONDS,
        min_pending: L2_MIN_PENDING,
        ..Verdict::neutral()
    };
    let hold = l2.hold(PARTNER, false, 0, L2_MIN_PENDING).expect("应拦住");
    assert_eq!(hold.reason, "l2_quiet");
    // Past the quiet window but short of the pending floor: "thin", also soft.
    let hold = l2
        .hold(PARTNER, false, L2_QUIET_SECONDS, L2_MIN_PENDING - 1)
        .expect("应拦住");
    assert_eq!(hold.reason, "thin");
    assert!(!hold.hard);
    assert_eq!(
        l2.hold(PARTNER, false, L2_QUIET_SECONDS, L2_MIN_PENDING),
        None
    );
}

#[test]
fn an_unknown_group_is_undamped_and_has_no_hint() {
    let damper = Damper::new();
    assert_eq!(damper.tier("never-seen"), Tier::L0);
    assert!(damper.hint("never-seen").is_none());
    // Observing an empty batch must not invent a level either.
    let (verdict, events) = damper.observe("never-seen", 1_000, &[]);
    assert_eq!(verdict.tier, Tier::L0);
    assert!(events.is_empty());
    assert_eq!(verdict.window_messages, 0);
    assert_eq!(verdict.concentration, 0.0);
}

#[test]
fn messages_older_than_the_window_leave_it() {
    let damper = Damper::new();
    let old = Observation {
        id: 1,
        sender: PARTNER,
        at: 1_000,
        addressed: false,
        from_owner: false,
    };
    let fresh = Observation {
        id: 2,
        sender: OTHER,
        at: 1_700,
        addressed: false,
        from_owner: false,
    };
    damper.observe("g", 1_000, std::slice::from_ref(&old));
    // WINDOW_SECONDS later the old row is gone and only the fresh one counts.
    let (verdict, _) = damper.observe("g", 1_000 + WINDOW_SECONDS, std::slice::from_ref(&fresh));
    assert_eq!(verdict.window_messages, 1);
    assert_eq!(verdict.dominant.as_deref(), Some(OTHER));
}

#[test]
fn the_owner_ends_an_episode_without_joining_the_window_count_as_a_partner() {
    let damper = Damper::new();
    for round in 1..=L1_STREAK {
        exchange(&damper, 1_000 + i64::from(round), PARTNER, false);
    }
    assert_eq!(damper.tier("g"), Tier::L1);
    let (verdict, events) = say(&damper, 1_010, 1, OWNER, 1_010, true);
    assert_eq!(verdict.tier, Tier::L0);
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Reset {
                reason: ResetReason::Owner
            }
        )),
        "events: {events:?}"
    );
    assert_eq!(damper.tier("g"), Tier::L0);
    // The level is gone, but the concentration that caused it is still in the
    // window, so the hint degrades to a pure structure note at L0 and zero rounds
    // instead of vanishing.
    let hint = damper
        .hint("g")
        .expect("窗口没清空，提示应当退化成 L0 版本");
    assert!(hint.contains("L0"), "hint: {hint}");
    assert!(hint.contains(" 0 轮"), "hint: {hint}");
}
