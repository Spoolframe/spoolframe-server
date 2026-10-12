//! A bounded reader for a WebAuthn `clientDataJSON` (P7-08): just the
//! members a root signature is checked by, from a strict JSON object.
//!
//! Hand-written rather than serde_json, so the protocol crate takes no new
//! dependency (the Linux client builds it from its own lock file). It is
//! for untrusted input: the caller bounds the length first
//! ([`super::MAX_CLIENT_DATA_JSON`]), nesting is bounded here, it never
//! panics, and anything that is not strict JSON (RFC 8259) is refused.

/// The deepest an object or array may nest inside the top-level object.
const MAX_DEPTH: usize = 8;

/// What a root signature reads of a `clientDataJSON`. Other members are
/// parsed and skipped.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientData {
    pub kind: Option<String>,
    pub challenge: Option<String>,
    /// `crossOrigin`, if present: `Some(Err(()))` when it is not a boolean.
    pub cross_origin: Option<Result<bool, ()>>,
}

/// A member's value, as far as it is read.
enum Value {
    String(String),
    Bool(bool),
    Other,
}

/// `bytes` read as a JSON object, or `None` if it is not one: not UTF-8,
/// not strict JSON, nested past [`MAX_DEPTH`], trailing bytes, or a member
/// of the top-level object given twice (which one a reader takes is not to
/// be guessed).
pub(crate) fn parse(bytes: &[u8]) -> Option<ClientData> {
    std::str::from_utf8(bytes).ok()?;
    let mut reader = Reader { bytes, at: 0 };
    reader.space();
    let mut data = ClientData::default();
    let mut seen: Vec<String> = Vec::new();
    reader.object(0, &mut |key, value| {
        if seen.contains(&key) {
            return false;
        }
        match (key.as_str(), value) {
            ("type", Value::String(text)) => data.kind = Some(text),
            ("challenge", Value::String(text)) => data.challenge = Some(text),
            ("crossOrigin", Value::Bool(cross)) => data.cross_origin = Some(Ok(cross)),
            ("crossOrigin", _) => data.cross_origin = Some(Err(())),
            ("type" | "challenge", _) => return false,
            _ => {}
        }
        seen.push(key);
        true
    })?;
    reader.space();
    (reader.at == bytes.len()).then_some(data)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.next()? == byte).then_some(())
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// An object, each member given to `member` (which says whether to go
    /// on); the reader is past its `}`.
    fn object(
        &mut self,
        depth: usize,
        member: &mut dyn FnMut(String, Value) -> bool,
    ) -> Option<()> {
        self.eat(b'{')?;
        self.space();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Some(());
        }
        loop {
            self.space();
            let key = self.string()?;
            self.space();
            self.eat(b':')?;
            self.space();
            let value = self.value(depth)?;
            if !member(key, value) {
                return None;
            }
            self.space();
            match self.next()? {
                b',' => continue,
                b'}' => return Some(()),
                _ => return None,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<()> {
        self.eat(b'[')?;
        self.space();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Some(());
        }
        loop {
            self.space();
            self.value(depth)?;
            self.space();
            match self.next()? {
                b',' => continue,
                b']' => return Some(()),
                _ => return None,
            }
        }
    }

    /// A value inside an object or array at `depth`.
    fn value(&mut self, depth: usize) -> Option<Value> {
        match self.peek()? {
            b'"' => self.string().map(Value::String),
            b'{' | b'[' if depth >= MAX_DEPTH => None,
            b'{' => self
                .object(depth + 1, &mut |_, _| true)
                .map(|()| Value::Other),
            b'[' => self.array(depth + 1).map(|()| Value::Other),
            b't' => self.literal(b"true").map(|()| Value::Bool(true)),
            b'f' => self.literal(b"false").map(|()| Value::Bool(false)),
            b'n' => self.literal(b"null").map(|()| Value::Other),
            b'-' | b'0'..=b'9' => self.number().map(|()| Value::Other),
            _ => None,
        }
    }

    fn literal(&mut self, word: &[u8]) -> Option<()> {
        let end = self.at.checked_add(word.len())?;
        (self.bytes.get(self.at..end)? == word).then(|| self.at = end)
    }

    fn digits(&mut self) -> Option<()> {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        (self.at > start).then_some(())
    }

    /// `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`
    fn number(&mut self) -> Option<()> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.next()? {
            b'0' => {}
            b'1'..=b'9' => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            self.digits()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            self.digits()?;
        }
        Some(())
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut code = 0;
        for _ in 0..4 {
            code = code * 16 + char::from(self.next()?).to_digit(16)?;
        }
        Some(code)
    }

    /// A string, its escapes decoded. The input is UTF-8 (checked first),
    /// and `"` and `\` are ASCII, so the raw runs copied are whole
    /// characters.
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            match self.next()? {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let decoded = match self.next()? {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let high = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&high) {
                                self.eat(b'\\')?;
                                self.eat(b'u')?;
                                let low = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&low) {
                                    return None;
                                }
                                0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                            } else {
                                high
                            };
                            // A lone low surrogate is no character.
                            char::from_u32(code)?
                        }
                        _ => return None,
                    };
                    let mut buffer = [0; 4];
                    out.extend_from_slice(decoded.encode_utf8(&mut buffer).as_bytes());
                }
                0..0x20 => return None,
                byte => out.push(byte),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Option<ClientData> {
        parse(text.as_bytes())
    }

    #[test]
    fn it_reads_the_members_a_root_signature_is_checked_by() {
        let data = read(
            r#" { "type" : "webauthn.get", "challenge":"q83v", "origin":"https://x",
                 "crossOrigin":false, "other":{"a":[1,-2.5e+3,true,null,{}]}, "n":0 } "#,
        )
        .unwrap();
        assert_eq!(data.kind.as_deref(), Some("webauthn.get"));
        assert_eq!(data.challenge.as_deref(), Some("q83v"));
        assert_eq!(data.cross_origin, Some(Ok(false)));
    }

    #[test]
    fn it_decodes_escapes() {
        let data = read(r#"{"type":"webauthn.get","challenge":"a\/b😀\""}"#).unwrap();
        assert_eq!(data.kind.as_deref(), Some("webauthn.get"));
        assert_eq!(data.challenge.as_deref(), Some("a/b\u{1f600}\""));
    }

    #[test]
    fn it_refuses_what_is_not_strict_json() {
        for text in [
            "",
            "{",
            "{}x",
            "[]",
            r#"{"a":1,}"#,
            r#"{"a":01}"#,
            r#"{"a":1.}"#,
            r#"{"a":-}"#,
            r#"{"a":tru}"#,
            r#"{'a':1}"#,
            r#"{"a":"\x"}"#,
            r#"{"a":"\ud800"}"#,
            r#"{"a":"\udc00"}"#,
            "{\"a\":\"\n\"}",
            r#"{"type":"webauthn.get","type":"webauthn.get"}"#,
            r#"{"type":1}"#,
            r#"{"challenge":null}"#,
            r#"{"a":[[[[[[[[[1]]]]]]]]]}"#,
        ] {
            assert_eq!(read(text), None, "{text}");
        }
        assert_eq!(parse(b"{\"a\":\"\xff\"}"), None);
        // Eight deep is allowed.
        assert!(read(r#"{"a":[[[[[[[[1]]]]]]]]}"#).is_some());
    }

    #[test]
    fn a_crossorigin_that_is_not_a_boolean_is_marked() {
        assert_eq!(
            read(r#"{"crossOrigin":"no"}"#).unwrap().cross_origin,
            Some(Err(()))
        );
    }
}
