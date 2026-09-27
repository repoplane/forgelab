//! JSON exactly as Go's `encoding/json` `MarshalIndent(v, "", "  ")` writes it.
//!
//! serde_json's pretty printer already lays out objects and arrays the same way. What differs
//! is string escaping: Go's encoder is HTML-safe, so `<`, `>` and `&` become `<`,
//! `>` and `&`, the line separators U+2028 and U+2029 are escaped, and the control
//! characters backspace and form feed are written as `\u0008` and `\u000c` rather than `\b`
//! and `\f`. A lock is committed and compared byte for byte, so it has to match.

use std::io;

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter, PrettyFormatter};

pub struct GoFormatter<'a> {
    inner: PrettyFormatter<'a>,
}

impl Default for GoFormatter<'_> {
    fn default() -> Self {
        GoFormatter {
            inner: PrettyFormatter::new(),
        }
    }
}

impl Formatter for GoFormatter<'_> {
    fn begin_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.inner.begin_array(w)
    }
    fn end_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.inner.end_array(w)
    }
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.inner.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.inner.end_array_value(w)
    }
    fn begin_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.inner.begin_object(w)
    }
    fn end_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.inner.end_object(w)
    }
    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.inner.begin_object_key(w, first)
    }
    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.inner.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.inner.end_object_value(w)
    }

    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            let esc = match c {
                '<' => "\\u003c",
                '>' => "\\u003e",
                '&' => "\\u0026",
                '\u{2028}' => "\\u2028",
                '\u{2029}' => "\\u2029",
                _ => continue,
            };
            w.write_all(&fragment.as_bytes()[start..i])?;
            w.write_all(esc.as_bytes())?;
            start = i + c.len_utf8();
        }
        w.write_all(&fragment.as_bytes()[start..])
    }

    fn write_char_escape<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        esc: CharEscape,
    ) -> io::Result<()> {
        match esc {
            CharEscape::Backspace => w.write_all(b"\\u0008"),
            CharEscape::FormFeed => w.write_all(b"\\u000c"),
            other => self.inner.write_char_escape(w, other),
        }
    }
}

/// Serialises `v` the way Go's `json.MarshalIndent(v, "", "  ")` does, without the trailing
/// newline.
pub fn to_vec_pretty<T: Serialize + ?Sized>(v: &T) -> serde_json::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, GoFormatter::default());
    v.serialize(&mut ser)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_go() {
        let got = to_vec_pretty(&serde_json::json!({"s": "a<b>&c\u{2028}\u{8}\u{c}\n"})).unwrap();
        assert_eq!(
            String::from_utf8(got).unwrap(),
            "{\n  \"s\": \"a\\u003cb\\u003e\\u0026c\\u2028\\u0008\\u000c\\n\"\n}"
        );
    }

    #[test]
    fn empty_array_is_two_brackets() {
        let got = to_vec_pretty(&serde_json::json!({"t": []})).unwrap();
        assert_eq!(String::from_utf8(got).unwrap(), "{\n  \"t\": []\n}");
    }
}
