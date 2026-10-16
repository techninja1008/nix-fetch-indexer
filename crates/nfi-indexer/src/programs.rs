//! Programs indexer: programs.sqlite from the nixexprs tarball.

use anyhow::{Context, Result};
use nfi_interchange::{Bitset, Program};

/// Read the `Programs(name, system, package)` table from a programs.sqlite file.
pub fn read_programs_db(data: &[u8], release: usize) -> Result<Vec<Program>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("programs.sqlite");
    std::fs::write(&path, data)?;
    let conn = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening programs.sqlite")?;
    let mut stmt =
        conn.prepare("SELECT name, system, package FROM Programs ORDER BY name, system, package")?;
    let bits = Bitset::single(release);
    let rows = stmt
        .query_map([], |r| {
            Ok(Program {
                name: r.get(0)?,
                system: r.get(1)?,
                attr: r.get(2)?,
                releases: bits.clone(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
pub(crate) fn make_programs_db(rows: &[(&str, &str, &str)]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE Programs (name text not null, system text not null, package text not null, primary key (name, system, package));",
    )
    .unwrap();
    for (n, s, p) in rows {
        conn.execute("INSERT INTO Programs VALUES (?1, ?2, ?3)", [n, s, p])
            .unwrap();
    }
    drop(conn);
    std::fs::read(path).unwrap()
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads() {
        let db = super::make_programs_db(&[
            ("ls", "x86_64-linux", "coreutils"),
            ("hello", "x86_64-linux", "hello"),
        ]);
        let p = super::read_programs_db(&db, 3).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "hello");
        assert!(p[0].releases.get(3));
    }
}
