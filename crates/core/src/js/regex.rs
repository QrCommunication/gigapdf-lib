//! A small backtracking regular-expression engine (zero-dependency).
//!
//! Supports the JavaScript `RegExp` core: literals, `.`, character classes
//! `[...]` (ranges, `\d\w\s` and negations), anchors `^`/`$`, word boundaries
//! `\b`/`\B`, groups `(...)` / non-capturing `(?:...)`, alternation `|`,
//! quantifiers `* + ? {n} {n,} {n,m}` (greedy and lazy `?`), backreferences
//! `\1`, and the flags `i` (ignore-case), `m` (multiline), `s` (dotAll). The
//! `g`/`y` flags are handled by the caller via `lastIndex`.
//!
//! Matching is backtracking via continuation-passing, which keeps capture-group
//! semantics straightforward.

/// A compiled regular expression.
#[derive(Debug)]
pub struct Regex {
    root: Node,
    pub group_count: usize,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dotall: bool,
    pub global: bool,
    pub sticky: bool,
}

/// A successful match: the overall span plus capture-group spans (char indices).
#[derive(Debug, Clone)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    /// `groups[k]` is the span of capture group `k` (1-based), if it matched.
    pub groups: Vec<Option<(usize, usize)>>,
}

#[derive(Debug)]
enum Node {
    Empty,
    Char(char),
    AnyChar,
    Class { negated: bool, items: Vec<ClassItem> },
    Start,
    End,
    WordBoundary(bool), // true = \b, false = \B
    Group { index: Option<usize>, node: Box<Node> },
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat { node: Box<Node>, min: usize, max: Option<usize>, greedy: bool },
    Backref(usize),
}

#[derive(Debug)]
enum ClassItem {
    Single(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

struct Caps {
    groups: Vec<Option<(usize, usize)>>,
    end: Option<usize>,
}

impl Regex {
    /// Compile `pattern` with `flags`. Returns `Err` on a malformed pattern.
    pub fn new(pattern: &str, flags: &str) -> Result<Regex, String> {
        let mut p = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
            group_count: 0,
        };
        let root = p.parse_alt()?;
        if p.pos != p.chars.len() {
            return Err("unexpected character in pattern".to_string());
        }
        Ok(Regex {
            root,
            group_count: p.group_count,
            ignore_case: flags.contains('i'),
            multiline: flags.contains('m'),
            dotall: flags.contains('s'),
            global: flags.contains('g'),
            sticky: flags.contains('y'),
        })
    }

    /// Try to match starting at or after `start` (char index). With the sticky
    /// flag the match must begin exactly at `start`.
    pub fn exec(&self, input: &[char], start: usize) -> Option<Match> {
        let last = if self.sticky { start } else { input.len() };
        for i in start..=last {
            let mut caps = Caps {
                groups: vec![None; self.group_count + 1],
                end: None,
            };
            let matched = self.m(&self.root, input, i, &mut caps, &|end, c| {
                c.end = Some(end);
                true
            });
            if matched {
                return Some(Match {
                    start: i,
                    end: caps.end.unwrap_or(i),
                    groups: caps.groups[1..].to_vec(),
                });
            }
            if self.sticky {
                break;
            }
        }
        None
    }

    #[allow(clippy::only_used_in_recursion)]
    fn m(
        &self,
        node: &Node,
        s: &[char],
        pos: usize,
        caps: &mut Caps,
        cont: &dyn Fn(usize, &mut Caps) -> bool,
    ) -> bool {
        match node {
            Node::Empty => cont(pos, caps),
            Node::Char(c) => {
                if pos < s.len() && self.char_eq(s[pos], *c) {
                    cont(pos + 1, caps)
                } else {
                    false
                }
            }
            Node::AnyChar => {
                if pos < s.len() && (self.dotall || s[pos] != '\n') {
                    cont(pos + 1, caps)
                } else {
                    false
                }
            }
            Node::Class { negated, items } => {
                if pos < s.len() && self.class_match(s[pos], items) != *negated {
                    cont(pos + 1, caps)
                } else {
                    false
                }
            }
            Node::Start => {
                let ok = pos == 0 || (self.multiline && s[pos - 1] == '\n');
                if ok {
                    cont(pos, caps)
                } else {
                    false
                }
            }
            Node::End => {
                let ok = pos == s.len() || (self.multiline && s[pos] == '\n');
                if ok {
                    cont(pos, caps)
                } else {
                    false
                }
            }
            Node::WordBoundary(want) => {
                let before = pos > 0 && is_word(s[pos - 1]);
                let after = pos < s.len() && is_word(s[pos]);
                if (before != after) == *want {
                    cont(pos, caps)
                } else {
                    false
                }
            }
            Node::Concat(nodes) => self.m_seq(nodes, 0, s, pos, caps, cont),
            Node::Alt(branches) => {
                for b in branches {
                    if self.m(b, s, pos, caps, cont) {
                        return true;
                    }
                }
                false
            }
            Node::Group { index, node } => match index {
                Some(gi) => {
                    let gi = *gi;
                    let start = pos;
                    self.m(node, s, pos, caps, &|end, c| {
                        let saved = c.groups[gi];
                        c.groups[gi] = Some((start, end));
                        if cont(end, c) {
                            true
                        } else {
                            c.groups[gi] = saved;
                            false
                        }
                    })
                }
                None => self.m(node, s, pos, caps, cont),
            },
            Node::Repeat { node, min, max, greedy } => {
                self.m_repeat(node, *min, *max, *greedy, 0, s, pos, caps, cont)
            }
            Node::Backref(idx) => {
                let span = caps.groups.get(*idx).copied().flatten();
                match span {
                    None => cont(pos, caps), // unset group matches empty
                    Some((gs, ge)) => {
                        let len = ge - gs;
                        if pos + len <= s.len()
                            && (0..len).all(|k| self.char_eq(s[pos + k], s[gs + k]))
                        {
                            cont(pos + len, caps)
                        } else {
                            false
                        }
                    }
                }
            }
        }
    }

    fn m_seq(
        &self,
        nodes: &[Node],
        i: usize,
        s: &[char],
        pos: usize,
        caps: &mut Caps,
        cont: &dyn Fn(usize, &mut Caps) -> bool,
    ) -> bool {
        if i >= nodes.len() {
            return cont(pos, caps);
        }
        self.m(&nodes[i], s, pos, caps, &|p, c| self.m_seq(nodes, i + 1, s, p, c, cont))
    }

    #[allow(clippy::too_many_arguments)]
    fn m_repeat(
        &self,
        node: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        count: usize,
        s: &[char],
        pos: usize,
        caps: &mut Caps,
        cont: &dyn Fn(usize, &mut Caps) -> bool,
    ) -> bool {
        let can_more = max.is_none_or(|m| count < m);
        let more = |c: &mut Caps| -> bool {
            if !can_more {
                return false;
            }
            self.m(node, s, pos, c, &|p, c2| {
                // Stop zero-width repetition once the minimum is satisfied.
                if p == pos && count >= min {
                    return false;
                }
                self.m_repeat(node, min, max, greedy, count + 1, s, p, c2, cont)
            })
        };
        let done = |c: &mut Caps| -> bool {
            if count >= min {
                cont(pos, c)
            } else {
                false
            }
        };
        if greedy {
            more(caps) || done(caps)
        } else {
            done(caps) || more(caps)
        }
    }

    fn char_eq(&self, a: char, b: char) -> bool {
        if self.ignore_case {
            a.eq_ignore_ascii_case(&b) || a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    }

    fn class_match(&self, ch: char, items: &[ClassItem]) -> bool {
        for it in items {
            let hit = match it {
                ClassItem::Single(c) => self.char_eq(ch, *c),
                ClassItem::Range(a, b) => {
                    (*a..=*b).contains(&ch)
                        || (self.ignore_case
                            && {
                                let l = ch.to_ascii_lowercase();
                                let u = ch.to_ascii_uppercase();
                                (*a..=*b).contains(&l) || (*a..=*b).contains(&u)
                            })
                }
                ClassItem::Digit => ch.is_ascii_digit(),
                ClassItem::NotDigit => !ch.is_ascii_digit(),
                ClassItem::Word => is_word(ch),
                ClassItem::NotWord => !is_word(ch),
                ClassItem::Space => ch.is_whitespace(),
                ClassItem::NotSpace => !ch.is_whitespace(),
            };
            if hit {
                return true;
            }
        }
        false
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---- parser ----------------------------------------------------------------

struct Parser {
    chars: Vec<char>,
    pos: usize,
    group_count: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn parse_alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.bump();
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.parse_repeat()?);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Node::Concat(items)),
        }
    }

    fn parse_repeat(&mut self) -> Result<Node, String> {
        let atom = self.parse_atom()?;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.bump();
                (0, None)
            }
            Some('+') => {
                self.bump();
                (1, None)
            }
            Some('?') => {
                self.bump();
                (0, Some(1))
            }
            Some('{') => match self.try_parse_brace() {
                Some(mm) => mm,
                None => return Ok(atom), // `{` not a quantifier → literal handled below
            },
            _ => return Ok(atom),
        };
        let greedy = if self.peek() == Some('?') {
            self.bump();
            false
        } else {
            true
        };
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    fn try_parse_brace(&mut self) -> Option<(usize, Option<usize>)> {
        let save = self.pos;
        self.bump(); // {
        let min = self.parse_int();
        let result = match self.peek() {
            Some('}') if min.is_some() => {
                self.bump();
                Some((min.unwrap(), Some(min.unwrap())))
            }
            Some(',') => {
                self.bump();
                let max = self.parse_int();
                if self.peek() == Some('}') {
                    self.bump();
                    Some((min.unwrap_or(0), max))
                } else {
                    None
                }
            }
            _ => None,
        };
        if result.is_none() {
            self.pos = save; // not a valid quantifier; treat `{` literally
        }
        result
    }

    fn parse_int(&mut self) -> Option<usize> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            None
        } else {
            self.chars[start..self.pos]
                .iter()
                .collect::<String>()
                .parse()
                .ok()
        }
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        match self.peek() {
            Some('(') => {
                self.bump();
                let index = if self.peek() == Some('?') {
                    self.bump();
                    // (?: ...) non-capturing; (?= / (?! lookarounds are not
                    // supported — consume the marker and treat as non-capturing.
                    self.bump();
                    None
                } else {
                    self.group_count += 1;
                    Some(self.group_count)
                };
                let inner = self.parse_alt()?;
                if self.peek() != Some(')') {
                    return Err("missing )".to_string());
                }
                self.bump();
                Ok(Node::Group {
                    index,
                    node: Box::new(inner),
                })
            }
            Some('[') => self.parse_class(),
            Some('.') => {
                self.bump();
                Ok(Node::AnyChar)
            }
            Some('^') => {
                self.bump();
                Ok(Node::Start)
            }
            Some('$') => {
                self.bump();
                Ok(Node::End)
            }
            Some('\\') => {
                self.bump();
                self.parse_escape()
            }
            Some(c) => {
                self.bump();
                Ok(Node::Char(c))
            }
            None => Ok(Node::Empty),
        }
    }

    fn parse_escape(&mut self) -> Result<Node, String> {
        let c = self.bump().ok_or("trailing backslash")?;
        Ok(match c {
            'd' => Node::Class { negated: false, items: vec![ClassItem::Digit] },
            'D' => Node::Class { negated: false, items: vec![ClassItem::NotDigit] },
            'w' => Node::Class { negated: false, items: vec![ClassItem::Word] },
            'W' => Node::Class { negated: false, items: vec![ClassItem::NotWord] },
            's' => Node::Class { negated: false, items: vec![ClassItem::Space] },
            'S' => Node::Class { negated: false, items: vec![ClassItem::NotSpace] },
            'b' => Node::WordBoundary(true),
            'B' => Node::WordBoundary(false),
            'n' => Node::Char('\n'),
            'r' => Node::Char('\r'),
            't' => Node::Char('\t'),
            'f' => Node::Char('\u{C}'),
            'v' => Node::Char('\u{B}'),
            '0' => Node::Char('\0'),
            'u' => Node::Char(self.parse_unicode().unwrap_or('u')),
            'x' => Node::Char(self.parse_hex(2).unwrap_or('x')),
            '1'..='9' => {
                let mut n = c.to_digit(10).unwrap() as usize;
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    n = n * 10 + self.bump().unwrap().to_digit(10).unwrap() as usize;
                }
                Node::Backref(n)
            }
            other => Node::Char(other),
        })
    }

    fn parse_unicode(&mut self) -> Option<char> {
        if self.peek() == Some('{') {
            self.bump();
            let mut v = 0u32;
            while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                v = v * 16 + d;
                self.bump();
            }
            if self.peek() == Some('}') {
                self.bump();
            }
            char::from_u32(v)
        } else {
            self.parse_hex(4)
        }
    }

    fn parse_hex(&mut self, n: usize) -> Option<char> {
        let mut v = 0u32;
        for _ in 0..n {
            let d = self.peek().and_then(|c| c.to_digit(16))?;
            v = v * 16 + d;
            self.bump();
        }
        char::from_u32(v)
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        self.bump(); // [
        let negated = if self.peek() == Some('^') {
            self.bump();
            true
        } else {
            false
        };
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == ']' {
                break;
            }
            let lo = self.class_char()?;
            // Range?
            if let ClassChar::Single(a) = lo {
                if self.peek() == Some('-') && self.chars.get(self.pos + 1) != Some(&']') {
                    self.bump(); // -
                    if let ClassChar::Single(b) = self.class_char()? {
                        items.push(ClassItem::Range(a, b));
                        continue;
                    } else {
                        items.push(ClassItem::Single(a));
                        items.push(ClassItem::Single('-'));
                        continue;
                    }
                }
            }
            match lo {
                ClassChar::Single(c) => items.push(ClassItem::Single(c)),
                ClassChar::Set(set) => items.push(set),
            }
        }
        if self.peek() == Some(']') {
            self.bump();
        }
        Ok(Node::Class { negated, items })
    }

    fn class_char(&mut self) -> Result<ClassChar, String> {
        let c = self.bump().ok_or("unterminated class")?;
        if c == '\\' {
            let e = self.bump().ok_or("trailing backslash in class")?;
            return Ok(match e {
                'd' => ClassChar::Set(ClassItem::Digit),
                'D' => ClassChar::Set(ClassItem::NotDigit),
                'w' => ClassChar::Set(ClassItem::Word),
                'W' => ClassChar::Set(ClassItem::NotWord),
                's' => ClassChar::Set(ClassItem::Space),
                'S' => ClassChar::Set(ClassItem::NotSpace),
                'n' => ClassChar::Single('\n'),
                'r' => ClassChar::Single('\r'),
                't' => ClassChar::Single('\t'),
                'f' => ClassChar::Single('\u{C}'),
                'v' => ClassChar::Single('\u{B}'),
                'b' => ClassChar::Single('\u{8}'),
                '0' => ClassChar::Single('\0'),
                'x' => ClassChar::Single(self.parse_hex(2).unwrap_or('x')),
                'u' => ClassChar::Single(self.parse_unicode().unwrap_or('u')),
                other => ClassChar::Single(other),
            });
        }
        Ok(ClassChar::Single(c))
    }
}

enum ClassChar {
    Single(char),
    Set(ClassItem),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pat: &str, flags: &str, input: &str) -> bool {
        let re = Regex::new(pat, flags).unwrap();
        let chars: Vec<char> = input.chars().collect();
        re.exec(&chars, 0).is_some()
    }

    fn first_match(pat: &str, flags: &str, input: &str) -> Option<String> {
        let re = Regex::new(pat, flags).unwrap();
        let chars: Vec<char> = input.chars().collect();
        re.exec(&chars, 0).map(|m| chars[m.start..m.end].iter().collect())
    }

    #[test]
    fn literals_and_dot() {
        assert!(matches("abc", "", "xxabcyy"));
        assert!(!matches("abc", "", "abx"));
        assert_eq!(first_match("a.c", "", "zabcz").as_deref(), Some("abc"));
    }

    #[test]
    fn quantifiers() {
        assert_eq!(first_match("a+", "", "baaab").as_deref(), Some("aaa"));
        assert_eq!(first_match("a*", "", "bbb").as_deref(), Some(""));
        assert_eq!(first_match("ab?c", "", "ac").as_deref(), Some("ac"));
        assert_eq!(first_match("a{2,3}", "", "aaaa").as_deref(), Some("aaa"));
        assert_eq!(first_match("a+?", "", "aaa").as_deref(), Some("a")); // lazy
    }

    #[test]
    fn classes_and_escapes() {
        assert_eq!(first_match("[0-9]+", "", "abc123def").as_deref(), Some("123"));
        assert_eq!(first_match("\\d+", "", "x42y").as_deref(), Some("42"));
        assert_eq!(first_match("[^a-z]+", "", "abc123").as_deref(), Some("123"));
        assert!(matches("\\w+@\\w+", "", "a@b"));
    }

    #[test]
    fn anchors_and_boundaries() {
        assert!(matches("^abc$", "", "abc"));
        assert!(!matches("^abc$", "", "xabc"));
        assert_eq!(first_match("\\bword\\b", "", "a word here").as_deref(), Some("word"));
    }

    #[test]
    fn groups_alternation_backref() {
        assert!(matches("(cat|dog)", "", "I have a dog"));
        assert_eq!(first_match("(ab)+", "", "ababab").as_deref(), Some("ababab"));
        assert!(matches("(.)\\1", "", "aa")); // backreference
        assert!(!matches("(.)\\1", "", "ab"));
    }

    #[test]
    fn ignore_case_and_multiline() {
        assert!(matches("hello", "i", "HELLO"));
        assert_eq!(first_match("^b", "m", "a\nbc").as_deref(), Some("b"));
    }

    #[test]
    fn capture_groups_recorded() {
        let re = Regex::new("(\\d+)-(\\d+)", "").unwrap();
        let chars: Vec<char> = "12-34".chars().collect();
        let m = re.exec(&chars, 0).unwrap();
        assert_eq!(m.groups[0], Some((0, 2)));
        assert_eq!(m.groups[1], Some((3, 5)));
    }
}
