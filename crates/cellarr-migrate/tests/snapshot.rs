//! A snapshot of a live database is consistent and leaves the source untouched.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::Row;

#[tokio::test]
async fn a_snapshot_is_taken_while_the_app_holds_a_write_and_sees_only_committed_rows() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("lidarr.db");
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", src.display()))
        .unwrap()
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    // The "app": its own pool, in WAL mode like Lidarr.
    let app = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE Artists (Id INTEGER PRIMARY KEY, Name TEXT)")
        .execute(&app)
        .await
        .unwrap();
    for i in 0..500 {
        sqlx::query("INSERT INTO Artists (Name) VALUES (?1)")
            .bind(format!("artist {i}"))
            .execute(&app)
            .await
            .unwrap();
    }
    // The app is mid-write while the snapshot is taken.
    let mut tx = app.begin().await.unwrap();
    sqlx::query("INSERT INTO Artists (Name) VALUES ('uncommitted')")
        .execute(&mut *tx)
        .await
        .unwrap();

    let dest = dir.path().join("snap.db");
    cellarr_migrate::snapshot(&src.to_string_lossy(), &dest.to_string_lossy())
        .await
        .expect("snapshot while the app holds a write transaction");

    // The app's write goes through afterwards: no lock was left behind.
    tx.commit().await.unwrap();
    let live: i64 = sqlx::query("SELECT count(*) AS n FROM Artists")
        .fetch_one(&app)
        .await
        .unwrap()
        .get("n");
    assert_eq!(live, 501);

    let snap = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&format!("sqlite://{}", dest.display()))
                .unwrap()
                .read_only(true),
        )
        .await
        .unwrap();
    let n: i64 = sqlx::query("SELECT count(*) AS n FROM Artists")
        .fetch_one(&snap)
        .await
        .unwrap()
        .get("n");
    assert_eq!(n, 500, "the snapshot holds exactly the committed rows");

    // It refuses to overwrite.
    assert!(
        cellarr_migrate::snapshot(&src.to_string_lossy(), &dest.to_string_lossy())
            .await
            .is_err()
    );
}
