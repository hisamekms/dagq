//! Shared SQL for the image publisher's tokens and the digests it hands over
//! (`domain::image_publish`). The rows are written by [`token_rows`] and
//! [`publish_rows`], which both adapters run (the store contract); the
//! [`ImageStore`] entries add each write's queue event in the same fenced
//! transaction, so a row is never kept without its event. The event's
//! insert is the SQLite adapter's today: on Postgres it fails and the
//! transaction writes nothing until the Postgres event functions land.
use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, TransactionEntries, sql_params};
use crate::application::ImageStore;
use crate::domain::{
    EventKind,
    image_publish::{
        ImageDigest, Publication, PublishOutcome, Publisher, PublisherToken, resent, token_live,
    },
};
use anyhow::Result;
use serde_json::json;

/// Insert the token's row: its hash and the publisher's claims.
pub(crate) fn token_rows(tx: &impl SqlWrite, token: &PublisherToken) -> Result<()> {
    tx.execute(
        "INSERT INTO image_publishers(hash, id, repository, workflow_ref, commit_sha, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        sql_params![
            token.hash,
            token.id,
            token.publisher.repository,
            token.publisher.workflow_ref,
            token.publisher.commit,
            token.expires_at
        ],
    )?;
    Ok(())
}

/// The publisher id and claims of a live token's `hash`.
pub(crate) fn publisher(
    tx: &impl SqlRead,
    hash: &str,
    now: i64,
) -> Result<Option<(String, Publisher)>> {
    let row = tx
        .query_row(
            "SELECT id, repository, workflow_ref, commit_sha, expires_at FROM image_publishers WHERE hash = ?1",
            sql_params![hash],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Publisher {
                        repository: r.get(1)?,
                        workflow_ref: r.get(2)?,
                        commit: r.get(3)?,
                    },
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.and_then(|(id, publisher, expires_at)| {
        token_live(expires_at, now).then_some((id, publisher))
    }))
}

const DIGEST_COLUMNS: &str =
    "arch, tag, digest, repository, workflow_ref, commit_sha, published_at";

fn digest_row(r: &super::sql_exec::SqlRow<'_>) -> super::sql_exec::SqlResult<ImageDigest> {
    Ok(ImageDigest {
        arch: r.get(0)?,
        tag: r.get(1)?,
        digest: r.get(2)?,
        publisher: Publisher {
            repository: r.get(3)?,
            workflow_ref: r.get(4)?,
            commit: r.get(5)?,
        },
        published_at: r.get(6)?,
    })
}

fn latest(tx: &impl SqlRead, arch: &str) -> Result<Option<ImageDigest>> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT {DIGEST_COLUMNS} FROM image_digests WHERE arch = ?1 ORDER BY id DESC LIMIT 1"
            ),
            sql_params![arch],
            digest_row,
        )
        .optional()?)
}

/// Record `publication` by the live token `hash`: a new row, unless the
/// architecture recorded the pair before ([`resent`]).
pub(crate) fn publish_rows(
    tx: &impl SqlWrite,
    hash: &str,
    publication: &Publication,
    now: i64,
) -> Result<PublishOutcome> {
    let Some((_, publisher)) = publisher(tx, hash, now)? else {
        return Ok(PublishOutcome::Unauthenticated);
    };
    let first = tx
        .query_row(
            &format!(
                "SELECT {DIGEST_COLUMNS} FROM image_digests WHERE arch = ?1 AND tag = ?2 AND digest = ?3 ORDER BY id LIMIT 1"
            ),
            sql_params![publication.arch, publication.tag, publication.digest],
            digest_row,
        )
        .optional()?;
    if let Some(row) = resent(&first, publication) {
        return Ok(PublishOutcome::Resent(row.clone()));
    }
    let row = ImageDigest {
        arch: publication.arch.clone(),
        tag: publication.tag.clone(),
        digest: publication.digest.clone(),
        publisher,
        published_at: now,
    };
    tx.execute(
        &format!("INSERT INTO image_digests({DIGEST_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
        sql_params![
            row.arch,
            row.tag,
            row.digest,
            row.publisher.repository,
            row.publisher.workflow_ref,
            row.publisher.commit,
            row.published_at
        ],
    )?;
    Ok(PublishOutcome::Recorded(row))
}

/// Each architecture's latest row, main's digest now, by architecture.
pub(crate) fn current(tx: &impl SqlRead) -> Result<Vec<ImageDigest>> {
    let arches = tx.query_map(
        "SELECT DISTINCT arch FROM image_digests ORDER BY arch",
        sql_params![],
        |r| r.get::<_, String>(0),
    )?;
    let mut rows = Vec::new();
    for arch in arches {
        rows.extend(latest(tx, &arch)?);
    }
    Ok(rows)
}

macro_rules! image_store {
    ($store:ty $(, $session:ident : $bound:path)?) => {
        impl $(<$session: $bound>)? ImageStore for $store {
            fn issue_image_publisher(&self, token: &PublisherToken) -> Result<()> {
                let tx = self.write_entry("image_publisher_issue")?;
                token_rows(&tx, token)?;
                super::store_writes::record_queue_event_in(
                    &tx,
                    EventKind::ImagePublisherTokenIssued,
                    &json!({"publisher": token.id, "publisher_claims": token.publisher,
                        "expires_at": token.expires_at}),
                )?;
                tx.commit()?;
                Ok(())
            }
            fn image_publisher(&self, hash: &str, now: i64) -> Result<Option<String>> {
                let tx = self.read_entry("image_publisher")?;
                Ok(publisher(&tx, hash, now)?.map(|(id, _)| id))
            }
            fn publish_image(
                &self,
                hash: &str,
                publication: &Publication,
                now: i64,
            ) -> Result<PublishOutcome> {
                let tx = self.write_entry("image_publish")?;
                let outcome = publish_rows(&tx, hash, publication, now)?;
                if let PublishOutcome::Recorded(row) = &outcome {
                    super::store_writes::record_queue_event_in(
                        &tx,
                        EventKind::ImagePublished,
                        &serde_json::to_value(row)?,
                    )?;
                    tx.commit()?;
                }
                Ok(outcome)
            }
            fn current_images(&self) -> Result<Vec<ImageDigest>> {
                let tx = self.read_entry("current_images")?;
                current(&tx)
            }
        }
    };
}
image_store!(super::sqlite::SqliteQueue);
image_store!(super::postgres::PostgresStore<S>, S: super::postgres::PostgresSession);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::image_publish::TOKEN_SECONDS;
    use crate::infrastructure::sqlite::SqliteQueue;

    fn queue() -> (SqliteQueue, SqliteQueue) {
        let uri = format!("file:/image-publish-{}?vfs=memdb", uuid::Uuid::new_v4());
        let keeper = SqliteQueue::init(&uri).unwrap();
        (keeper, SqliteQueue::open(&uri).unwrap())
    }

    fn token() -> PublisherToken {
        PublisherToken {
            id: "p1".into(),
            hash: "token-hash".into(),
            publisher: Publisher {
                repository: "org/repo".into(),
                workflow_ref: "org/repo/.github/workflows/image.yml@refs/heads/main".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
            },
            expires_at: 100 + TOKEN_SECONDS,
        }
    }

    fn publication(digest: char) -> Publication {
        Publication {
            arch: "arm64".into(),
            tag: "t1".into(),
            digest: format!("sha256:{}", digest.to_string().repeat(64)),
        }
    }

    fn count(queue: &SqliteQueue, sql: &str) -> i64 {
        queue.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn events(queue: &SqliteQueue, kind: EventKind) -> i64 {
        queue
            .conn
            .query_row(
                "SELECT count(*) FROM run_events WHERE kind = ?1",
                [kind.as_str()],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn fail_events(queue: &SqliteQueue) {
        queue
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER no_events BEFORE INSERT ON run_events \
                 BEGIN SELECT RAISE(ABORT, 'no events'); END;",
            )
            .unwrap();
    }

    #[test]
    fn the_issue_writes_the_token_and_its_event_together_or_neither() {
        let (_keeper, queue) = queue();
        fail_events(&queue);
        assert!(queue.issue_image_publisher(&token()).is_err());
        assert_eq!(count(&queue, "SELECT count(*) FROM image_publishers"), 0);
        queue
            .conn
            .execute_batch("DROP TRIGGER temp.no_events")
            .unwrap();
        queue.issue_image_publisher(&token()).unwrap();
        assert_eq!(count(&queue, "SELECT count(*) FROM image_publishers"), 1);
        assert_eq!(events(&queue, EventKind::ImagePublisherTokenIssued), 1);
        let payload: String = queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE kind = 'image_publisher_token_issued'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!payload.contains("token-hash"), "{payload}");
        assert!(payload.contains("org/repo"));
        assert_eq!(
            queue.image_publisher("token-hash", 100).unwrap(),
            Some("p1".into())
        );
        assert_eq!(
            queue
                .image_publisher("token-hash", token().expires_at)
                .unwrap(),
            None
        );
        assert_eq!(queue.image_publisher("other", 100).unwrap(), None);
    }

    #[test]
    fn a_publication_writes_its_row_and_event_together_and_a_resend_neither() {
        let (_keeper, queue) = queue();
        queue.issue_image_publisher(&token()).unwrap();
        let first = queue
            .publish_image("token-hash", &publication('a'), 101)
            .unwrap();
        assert!(matches!(first, PublishOutcome::Recorded(_)));
        let again = queue
            .publish_image("token-hash", &publication('a'), 102)
            .unwrap();
        let PublishOutcome::Resent(row) = again else {
            panic!("{again:?}")
        };
        assert_eq!(row.published_at, 101);
        assert_eq!(count(&queue, "SELECT count(*) FROM image_digests"), 1);
        assert_eq!(events(&queue, EventKind::ImagePublished), 1);
        // A, B, then A again: the first A is answered, with no row or event,
        // and B stays the arch's digest now.
        assert!(matches!(
            queue
                .publish_image("token-hash", &publication('b'), 103)
                .unwrap(),
            PublishOutcome::Recorded(_)
        ));
        assert_eq!(
            queue
                .publish_image("token-hash", &publication('a'), 104)
                .unwrap(),
            PublishOutcome::Resent(row)
        );
        assert_eq!(count(&queue, "SELECT count(*) FROM image_digests"), 2);
        assert_eq!(events(&queue, EventKind::ImagePublished), 2);
        fail_events(&queue);
        assert!(
            queue
                .publish_image("token-hash", &publication('c'), 105)
                .is_err()
        );
        assert_eq!(count(&queue, "SELECT count(*) FROM image_digests"), 2);
        assert_eq!(
            queue.current_images().unwrap()[0].digest,
            publication('b').digest
        );
        // A resend writes no event, so a store that cannot write one still
        // answers it.
        assert!(matches!(
            queue
                .publish_image("token-hash", &publication('a'), 104)
                .unwrap(),
            PublishOutcome::Resent(_)
        ));
        assert_eq!(
            queue
                .publish_image("unknown", &publication('c'), 104)
                .unwrap(),
            PublishOutcome::Unauthenticated
        );
    }
}
