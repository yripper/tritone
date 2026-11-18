use super::*;
use crate::store::OwnMessageRow;

const NOW: i64 = 1_800_000_000_000;

fn row(id: u64, at: i64) -> OpportunityRow {
    OpportunityRow {
        message_id: Id(id),
        channel_id: Id(100),
        guild_id: Id(10),
        rule: "question".into(),
        created_at: at,
        status: OpportunityStatus::Open,
        updated_at: at,
    }
}

#[test]
fn first_rule_wins_and_open_rows_list_newest_first() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.add_opportunity(&row(1, NOW - 10)).unwrap());
    assert!(!s.add_opportunity(&OpportunityRow { rule: "my_thread".into(), ..row(1, NOW - 10) }).unwrap());
    s.add_opportunity(&row(2, NOW - 5)).unwrap();
    s.add_opportunity(&row(3, NOW - 100)).unwrap();
    let open: Vec<Id> = s.open_opportunities(NOW - 50).unwrap().iter().map(|r| r.message_id).collect();
    assert_eq!(open, [Id(2), Id(1)]);
    assert_eq!(s.opportunity(Id(1)).unwrap().unwrap().rule, "question");
}

#[test]
fn dismissed_and_answered_stick() {
    let s = Store::open_in_memory().unwrap();
    s.add_opportunity(&row(1, NOW)).unwrap();
    s.add_opportunity(&row(2, NOW)).unwrap();
    assert!(s.set_opportunity_status(Id(1), OpportunityStatus::Dismissed, NOW + 1).unwrap());
    assert!(!s.set_opportunity_status(Id(9), OpportunityStatus::Dismissed, NOW).unwrap());
    assert!(s.opportunity_answered(Id(2), NOW + 2).unwrap());
    assert!(!s.opportunity_answered(Id(1), NOW + 2).unwrap(), "only open ones");
    assert!(!s.add_opportunity(&row(1, NOW)).unwrap(), "a dismissed one never reopens");
    assert!(s.open_opportunities(0).unwrap().is_empty());
    assert_eq!(s.opportunity(Id(2)).unwrap().unwrap().status, OpportunityStatus::Answered);
}

#[test]
fn wrote_in_and_own_message_use_both_logs() {
    let s = Store::open_in_memory().unwrap();
    let own = OwnMessageRow { message_id: Id(5), channel_id: Id(150), guild_id: Some(Id(10)), created_at: NOW };
    s.record_own_messages(&[own], NOW).unwrap();
    assert!(s.wrote_in(Id(150)).unwrap() && !s.wrote_in(Id(151)).unwrap());
    assert!(s.is_own_message(Id(5)).unwrap() && !s.is_own_message(Id(6)).unwrap());
}
