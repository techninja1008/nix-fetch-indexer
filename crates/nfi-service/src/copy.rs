//! Helpers for bulk loading via `COPY ... FROM STDIN` (text format).

use sqlx::PgConnection;

/// Accumulates rows in PostgreSQL COPY text format.
#[derive(Default)]
pub struct CopyBuf {
    buf: Vec<u8>,
    first: bool,
    pub rows: usize,
}

pub enum Field<'a> {
    Str(&'a str),
    Int(i64),
    Bool(bool),
    Null,
}

impl<'a> From<&'a str> for Field<'a> {
    fn from(s: &'a str) -> Self {
        Field::Str(s)
    }
}
impl<'a> From<&'a String> for Field<'a> {
    fn from(s: &'a String) -> Self {
        Field::Str(s)
    }
}
impl<'a> From<Option<&'a str>> for Field<'a> {
    fn from(s: Option<&'a str>) -> Self {
        s.map_or(Field::Null, Field::Str)
    }
}
impl<'a> From<&'a Option<String>> for Field<'a> {
    fn from(s: &'a Option<String>) -> Self {
        s.as_deref().map_or(Field::Null, Field::Str)
    }
}
impl From<i64> for Field<'_> {
    fn from(v: i64) -> Self {
        Field::Int(v)
    }
}
impl From<i32> for Field<'_> {
    fn from(v: i32) -> Self {
        Field::Int(v as i64)
    }
}
impl From<u32> for Field<'_> {
    fn from(v: u32) -> Self {
        Field::Int(v as i64)
    }
}
impl From<Option<u64>> for Field<'_> {
    fn from(v: Option<u64>) -> Self {
        v.map_or(Field::Null, |v| Field::Int(v as i64))
    }
}
impl From<bool> for Field<'_> {
    fn from(v: bool) -> Self {
        Field::Bool(v)
    }
}

impl CopyBuf {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn field<'a>(&mut self, f: impl Into<Field<'a>>) -> &mut Self {
        if self.first {
            self.buf.push(b'\t');
        }
        self.first = true;
        match f.into() {
            Field::Null => self.buf.extend_from_slice(b"\\N"),
            Field::Int(v) => self.buf.extend_from_slice(v.to_string().as_bytes()),
            Field::Bool(v) => self.buf.push(if v { b't' } else { b'f' }),
            Field::Str(s) => {
                for &b in s.as_bytes() {
                    match b {
                        b'\\' => self.buf.extend_from_slice(b"\\\\"),
                        b'\t' => self.buf.extend_from_slice(b"\\t"),
                        b'\n' => self.buf.extend_from_slice(b"\\n"),
                        b'\r' => self.buf.extend_from_slice(b"\\r"),
                        0 => {}
                        _ => self.buf.push(b),
                    }
                }
            }
        }
        self
    }

    pub fn end_row(&mut self) {
        self.buf.push(b'\n');
        self.first = false;
        self.rows += 1;
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Run `COPY <table_and_columns> FROM STDIN` with the buffered rows.
pub async fn copy_in(conn: &mut PgConnection, target: &str, data: CopyBuf) -> sqlx::Result<u64> {
    let mut copy = conn.copy_in_raw(&format!("COPY {target} FROM STDIN")).await?;
    let bytes = data.into_bytes();
    for chunk in bytes.chunks(4 << 20) {
        copy.send(chunk).await?;
    }
    copy.finish().await
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes() {
        let mut c = super::CopyBuf::new();
        c.field("a\tb\\c\nd").field(None::<&str>).field(5i64).field(true);
        c.end_row();
        assert_eq!(c.into_bytes(), b"a\\tb\\\\c\\nd\t\\N\t5\tt\n");
    }
}
