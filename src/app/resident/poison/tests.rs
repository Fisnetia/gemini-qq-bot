use super::PromptRejections;

#[test]
fn first_rejection_never_drops() {
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[1, 2]));
}

#[test]
fn second_rejection_of_same_batch_drops() {
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[1, 2]));
    assert!(t.note("g", &[1, 2]));
}

#[test]
fn second_rejection_of_grown_batch_drops() {
    // New messages merged into the retry batch do not launder the
    // refused material: the old ids are still in there.
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[1, 2]));
    assert!(t.note("g", &[1, 2, 3]));
}

#[test]
fn second_rejection_of_shrunk_batch_drops() {
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[1, 2]));
    assert!(t.note("g", &[2]));
}

#[test]
fn disjoint_batch_restarts_the_count() {
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[1, 2]));
    assert!(!t.note("g", &[3, 4]));
    assert!(t.note("g", &[4, 5]));
}

#[test]
fn drop_consumes_the_record() {
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[1]));
    assert!(t.note("g", &[1]));
    assert!(!t.note("g", &[1]));
}

#[test]
fn clear_restarts_the_count() {
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[1]));
    t.clear("g");
    assert!(!t.note("g", &[1]));
}

#[test]
fn empty_batch_is_ignored() {
    let mut t = PromptRejections::new();
    assert!(!t.note("g", &[]));
    assert!(!t.note("g", &[1]));
}

#[test]
fn groups_are_independent() {
    let mut t = PromptRejections::new();
    assert!(!t.note("a", &[1]));
    assert!(!t.note("b", &[1]));
    assert!(t.note("a", &[1]));
    assert!(!t.note("b", &[2]));
}
