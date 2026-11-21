//! Shared test helpers: a capturing writer and a parser for the flat JSON objects the log
//! layer writes (the crate may not depend on a JSON library, so tests carry their own).
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, missing_docs)]

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
pub struct Buf(Arc<Mutex<Vec<u8>>>);

impl Buf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    pub fn lines(&self) -> Vec<Line> {
        self.text().lines().map(parse_line).collect()
    }
}

impl io::Write for Buf {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

/// A parsed log line: key -> raw scalar (strings unquoted and unescaped, others as written).
pub type Line = BTreeMap<String, String>;

pub fn parse_line(line: &str) -> Line {
    let b: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut out = Line::new();
    assert_eq!(b[i], '{', "line is not an object: {line}");
    i += 1;
    loop {
        if b[i] == '}' {
            break;
        }
        if b[i] == ',' {
            i += 1;
        }
        let key = string(&b, &mut i);
        assert_eq!(b[i], ':', "expected ':' in {line}");
        i += 1;
        let value = if b[i] == '"' {
            string(&b, &mut i)
        } else {
            let start = i;
            while b[i] != ',' && b[i] != '}' {
                i += 1;
            }
            b[start..i].iter().collect()
        };
        out.insert(key, value);
    }
    assert_eq!(i, b.len() - 1, "trailing data in {line}");
    out
}

fn string(b: &[char], i: &mut usize) -> String {
    assert_eq!(b[*i], '"');
    *i += 1;
    let mut s = String::new();
    while b[*i] != '"' {
        if b[*i] == '\\' {
            *i += 1;
            match b[*i] {
                'n' => s.push('\n'),
                'r' => s.push('\r'),
                't' => s.push('\t'),
                'u' => {
                    let hex: String = b[*i + 1..*i + 5].iter().collect();
                    s.push(char::from_u32(u32::from_str_radix(&hex, 16).unwrap()).unwrap());
                    *i += 4;
                }
                c => s.push(c),
            }
        } else {
            s.push(b[*i]);
        }
        *i += 1;
    }
    *i += 1;
    s
}
