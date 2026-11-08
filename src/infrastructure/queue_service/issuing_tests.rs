//! SQLite rejects a second BEGIN, so these cases exercise issuing's reads
//! through its already-open write transaction.
use super::*;
use crate::domain::queue_service::{HeldToken, IssueHeld, JobState};

use crate::domain::session_step::TargetKind;

#[test]
fn issuing_reads_all_held_facts_and_job_records_in_its_write_transaction() {
    struct At;
    impl crate::application::Clock for At {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        }
        fn monotonic(&self) -> Instant {
            panic!("issuing and its fixture do not use the monotonic clock")
        }
    }
    let generators = crate::application::Generators {
        clock: Arc::new(At),
        ids: Arc::new(crate::infrastructure::clock::UuidGenerator),
    };
    let lease = LeaseToken::new("owner");
    let (_dir, kept, backend) =
        super::supervisor_tests::claimed_in_memdb_with(&lease, "issuing-reads", generators);
    let created: String = kept
        .conn
        .query_row("SELECT created_at FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(created, "2027-01-15T08:00:00.000Z");
    kept.conn
        .execute_batch(
            "INSERT INTO members(issuer,subject,role,on_call,created_at)
             VALUES ('issuer','subject','member',0,1800000000);
         INSERT INTO supervisors(token,pid,parallel,heartbeat_at) VALUES ('owner',1,1,1800000000);
         INSERT INTO planners(id,origin,created_at) VALUES (1,'runtime',1800000000);
         INSERT INTO proposals(id,status,owner_origin,submitted_at,created_at,updated_at)
             VALUES (1,'submitted','runtime','t','t','t');
         INSERT INTO plan_reviews(id,proposal_id,attempt,supervisor_token,started_at,finished_at)
             VALUES (1,1,1,'owner',1800000000,1800000001);",
        )
        .unwrap();
    let mut service = backend.open(&ActorContext::user()).unwrap();
    let target = StepTarget::of(TargetKind::Planner, 1);
    let held = [
        IssueHeld::Lease {
            run: RunId::new(super::supervisor_tests::RUN).unwrap(),
            lease: lease.clone(),
        },
        IssueHeld::OpenPlanner {
            planner: 1,
            lease: lease.clone(),
        },
        IssueHeld::Registered(lease.clone()),
        IssueHeld::OpenTarget {
            target,
            registration: Some(lease.clone()),
        },
    ];
    let job = HeldToken {
        principal: Principal::of(&ActorContext::instance(ActorRole::PlanReviewJob, "1:1")),
        issued_by: None,
        issued_at: 1800000000,
    };
    for held in held {
        let mut called = false;
        let value = service.issuing(&held, 1800000000000, &mut |facts| {
            called = true;
            assert!(facts.member(&crate::domain::oidc::OidcIdentity {
                issuer: "issuer".into(), subject: "subject".into(),
            })?.is_some());
            assert!(facts.registered(&lease));
            assert_eq!(facts.job(&job, None), JobState::Ended);
            assert!(matches!(facts.target_start(&StepTarget::of(TargetKind::PlanReview, 1))?,
                Some(crate::domain::queue_service::TargetStart::Supervised { supervisor: Some(token) }) if token == lease));
            Ok(json!({"issued": true}))
        }).unwrap();
        assert!(called);
        assert_eq!(value, json!({"issued": true}));
    }

    // Refusal decisions are covered by the domain's not_held tests. One
    // missing run here checks the adapter boundary: even the fencing renewal
    // rolls back, and the continuation is never called.
    let refused = IssueHeld::Lease {
        run: RunId::new("22222222-2222-4222-8222-222222222222").unwrap(),
        lease,
    };
    kept.conn
        .execute("UPDATE write_leader SET lease_until_ms=123", [])
        .unwrap();
    let error = service
        .issuing(&refused, 1800000000000, &mut |_| {
            panic!("refused issue called its continuation")
        })
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::application::queue_service::NotHeld>()
            .is_some(),
        "{error:#}"
    );
    let until: i64 = kept
        .conn
        .query_row("SELECT lease_until_ms FROM write_leader", [], |r| r.get(0))
        .unwrap();
    assert_eq!(until, 123);
}
