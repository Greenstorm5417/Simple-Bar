//! Minimal JSON reader for Mango IPC and the kernel script. Strings without
//! escapes borrow from the input, so a typical Mango event parses with only
//! the Vec allocations for its arrays and objects.

use std::borrow::Cow;

/// A parsed JSON value. Objects keep their fields in source order.
#[derive(Debug)]
pub enum Json<'a> {
    /// `null`
    Null,
    /// `true` or `false`
    Bool(bool),
    /// Any number.
    Num(f64),
    /// A string; borrowed from the input unless it contained escapes.
    Str(Cow<'a, str>),
    /// An array.
    Arr(Vec<Self>),
    /// An object.
    Obj(Vec<(Cow<'a, str>, Self)>),
}

impl Json<'_> {
    /// The value of an object field.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The boolean, if this is one.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The number, if this is a non-negative integer.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Num(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
            _ => None,
        }
    }

    /// The items, if this is an array.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Arr(items) => Some(items),
            _ => None,
        }
    }
}

/// Parse a complete JSON document. Returns `None` on any syntax error or
/// trailing input.
#[must_use]
pub fn parse(text: &str) -> Option<Json<'_>> {
    let mut p = Parser { s: text, i: 0 };
    let value = p.value(0)?;
    p.ws();
    (p.i == text.len()).then_some(value)
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: u32) -> Option<Json<'a>> {
        if depth > 64 {
            return None;
        }
        self.ws();
        match self.peek()? {
            b'{' => {
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.peek()? == b'}' {
                    self.i += 1;
                    return Some(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.ws();
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.i += 1;
                    let value = self.value(depth + 1)?;
                    fields.push((key, value));
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Some(Json::Obj(fields));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek()? == b']' {
                    self.i += 1;
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Some(Json::Arr(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' => self.eat("true").then_some(Json::Bool(true)),
            b'f' => self.eat("false").then_some(Json::Bool(false)),
            b'n' => self.eat("null").then_some(Json::Null),
            _ => {
                let start = self.i;
                while let Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') = self.peek() {
                    self.i += 1;
                }
                self.s[start..self.i].parse().ok().map(Json::Num)
            }
        }
    }

    fn string(&mut self) -> Option<Cow<'a, str>> {
        if self.peek()? != b'"' {
            return None;
        }
        self.i += 1;
        let start = self.i;
        let bytes = self.s.as_bytes();
        loop {
            match *bytes.get(self.i)? {
                b'"' => {
                    let out = &self.s[start..self.i];
                    self.i += 1;
                    return Some(Cow::Borrowed(out));
                }
                b'\\' => break,
                _ => self.i += 1,
            }
        }
        let mut out = String::from(&self.s[start..self.i]);
        loop {
            let b = *bytes.get(self.i)?;
            match b {
                b'"' => {
                    self.i += 1;
                    return Some(Cow::Owned(out));
                }
                b'\\' => {
                    self.i += 1;
                    let esc = *bytes.get(self.i)?;
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&hi) && self.eat("\\u") {
                                let lo = self.hex4()?;
                                0x10000 + ((hi - 0xd800) << 10) + (lo.wrapping_sub(0xdc00) & 0x3ff)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Copy one whole UTF-8 sequence.
                    let ch = self.s[self.i..].chars().next()?;
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.s.get(self.i..self.i + 4)?;
        self.i += 4;
        u32::from_str_radix(digits, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_escapes() {
        let v = parse(r#"{"a":[1,true,null,{"b":"x\"yé😀\n"}],"c":-2.5e1}"#).unwrap();
        let arr = v.get("a").unwrap().as_array().unwrap();
        assert_eq!(arr[0].as_u64(), Some(1));
        assert_eq!(arr[1].as_bool(), Some(true));
        assert!(matches!(arr[2], Json::Null));
        assert_eq!(
            arr[3].get("b").unwrap().as_str(),
            Some("x\"y\u{e9}\u{1f600}\n")
        );
        assert!(matches!(v.get("c"), Some(Json::Num(n)) if n.to_bits() == (-25.0_f64).to_bits()));
    }

    #[test]
    fn plain_strings_borrow() {
        let v = parse(r#"{"k":"plain"}"#).unwrap();
        assert!(matches!(
            v.get("k"),
            Some(Json::Str(Cow::Borrowed("plain")))
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("{\"a\":}").is_none());
        assert!(parse("[1,2").is_none());
        assert!(parse("{} x").is_none());
    }
}
