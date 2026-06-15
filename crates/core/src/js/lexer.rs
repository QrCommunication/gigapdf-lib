//! A zero-dependency JavaScript lexer (scanner).
//!
//! Turns source text into a [`Token`] stream covering the full ES2021 lexical
//! grammar: identifiers and reserved words, numeric literals (decimal, hex,
//! octal, binary, legacy octal, floats, exponents, `_` separators, `BigInt`
//! `n`), strings with the complete escape set, **template literals** with
//! nested `${…}` substitutions, **regular-expression literals** (disambiguated
//! from division by lexical context), every punctuator/operator, and comments.
//!
//! Each token carries a `newline_before` flag so the parser can apply Automatic
//! Semicolon Insertion. Pure `std`, no allocations beyond the token vector and
//! per-literal cooked strings.

use super::token::{expects_expression_after, Punct, Tok, Token};

/// A lexing error with position.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    /// Human-readable message.
    pub msg: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub col: u32,
}

impl core::fmt::Display for LexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} (line {}, col {})", self.msg, self.line, self.col)
    }
}

/// What a `}` on the brace stack closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Brace {
    /// A normal `{` (block, object, function body).
    Normal,
    /// A `${` opening a template substitution — its `}` resumes template text.
    Template,
}

/// The scanner.
#[derive(Debug)]
pub struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: u32,
    col: u32,
    brace_stack: Vec<Brace>,
}

/// Tokenize `src` into a vector terminated by [`Tok::Eof`].
pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(src).run()
}

impl Lexer {
    /// Build a lexer over `src`.
    pub fn new(src: &str) -> Self {
        Lexer {
            chars: src.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
            brace_stack: Vec::new(),
        }
    }

    /// Run to completion, returning all tokens (including the final `Eof`).
    pub fn run(&mut self) -> Result<Vec<Token>, LexError> {
        // Optional hashbang on the very first line.
        if self.chars.first() == Some(&'#') && self.chars.get(1) == Some(&'!') {
            while let Some(c) = self.peek(0) {
                if is_line_term(c) {
                    break;
                }
                self.bump();
            }
        }

        let mut out = Vec::new();
        let mut regex_allowed = true;
        loop {
            let newline_before = self.skip_trivia();
            let line = self.line;
            let col = self.col;
            let tok = self.scan(regex_allowed)?;
            regex_allowed = regex_allowed_after(&tok);
            let text = token_text(&tok);
            let is_eof = matches!(tok, Tok::Eof);
            out.push(Token {
                tok,
                text,
                line,
                col,
                newline_before,
            });
            if is_eof {
                break;
            }
        }
        Ok(out)
    }

    // ---- character cursor -------------------------------------------------

    /// Peek the char `n` positions ahead without consuming.
    fn peek(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    /// Consume one char, normalizing CR / CRLF line terminators to `\n` and
    /// tracking line/column.
    fn bump(&mut self) -> Option<char> {
        let c = *self.chars.get(self.pos)?;
        self.pos += 1;
        if c == '\r' {
            if self.chars.get(self.pos) == Some(&'\n') {
                self.pos += 1;
            }
            self.line += 1;
            self.col = 1;
            return Some('\n');
        }
        if c == '\n' || c == '\u{2028}' || c == '\u{2029}' {
            self.line += 1;
            self.col = 1;
            return Some(c);
        }
        self.col += 1;
        Some(c)
    }

    fn err(&self, msg: impl Into<String>) -> LexError {
        LexError {
            msg: msg.into(),
            line: self.line,
            col: self.col,
        }
    }

    // ---- trivia -----------------------------------------------------------

    /// Skip whitespace and comments. Returns `true` if a line terminator was
    /// crossed (for ASI).
    fn skip_trivia(&mut self) -> bool {
        let mut newline = false;
        loop {
            match self.peek(0) {
                Some(c) if is_line_term(c) => {
                    newline = true;
                    self.bump();
                }
                Some(c) if is_ws(c) => {
                    self.bump();
                }
                Some('/') if self.peek(1) == Some('/') => {
                    self.bump();
                    self.bump();
                    while let Some(c) = self.peek(0) {
                        if is_line_term(c) {
                            break;
                        }
                        self.bump();
                    }
                }
                Some('/') if self.peek(1) == Some('*') => {
                    self.bump();
                    self.bump();
                    loop {
                        match self.peek(0) {
                            None => break,
                            Some('*') if self.peek(1) == Some('/') => {
                                self.bump();
                                self.bump();
                                break;
                            }
                            Some(c) => {
                                if is_line_term(c) {
                                    newline = true;
                                }
                                self.bump();
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        newline
    }

    // ---- main dispatch ----------------------------------------------------

    fn scan(&mut self, regex_allowed: bool) -> Result<Tok, LexError> {
        let c = match self.peek(0) {
            None => return Ok(Tok::Eof),
            Some(c) => c,
        };

        match c {
            '`' => {
                self.bump();
                self.scan_template(false)
            }
            '}' => {
                // Close a template substitution, or emit a plain `}`.
                if self.brace_stack.last() == Some(&Brace::Template) {
                    self.brace_stack.pop();
                    self.bump();
                    self.scan_template(true)
                } else {
                    if self.brace_stack.last() == Some(&Brace::Normal) {
                        self.brace_stack.pop();
                    }
                    self.bump();
                    Ok(Tok::Punct(Punct::RBrace))
                }
            }
            '\'' | '"' => self.scan_string(c),
            '0'..='9' => self.scan_number(),
            '.' if self.peek(1).is_some_and(|d| d.is_ascii_digit()) => self.scan_number(),
            '/' if regex_allowed => self.scan_regex(),
            _ if is_id_start(c) => Ok(self.scan_ident()),
            _ => self.scan_punct(),
        }
    }

    // ---- identifiers ------------------------------------------------------

    fn scan_ident(&mut self) -> Tok {
        let mut s = String::new();
        while let Some(c) = self.peek(0) {
            if is_id_continue(c) {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        Tok::Ident(s)
    }

    // ---- numbers ----------------------------------------------------------

    fn scan_number(&mut self) -> Result<Tok, LexError> {
        let start = self.pos;

        // Radix-prefixed integers.
        if self.peek(0) == Some('0') {
            if let Some(p) = self.peek(1) {
                let radix = match p {
                    'x' | 'X' => Some(16),
                    'o' | 'O' => Some(8),
                    'b' | 'B' => Some(2),
                    _ => None,
                };
                if let Some(radix) = radix {
                    self.bump();
                    self.bump();
                    let digits = self.take_digits(radix);
                    if digits.is_empty() {
                        return Err(self.err("missing digits after numeric base prefix"));
                    }
                    if self.peek(0) == Some('n') {
                        self.bump();
                        return Ok(Tok::BigInt(to_decimal_string(&digits, radix)));
                    }
                    return Ok(Tok::Num(parse_radix(&digits, radix)));
                }
                // Legacy octal: 0 followed by octal digits (and no 8/9/./e).
                if ('0'..='7').contains(&p) {
                    let save = self.pos;
                    self.bump(); // consume the leading 0
                    let digits = self.take_digits(8);
                    // If the next char makes it a float/decimal, fall back.
                    let next = self.peek(0);
                    let is_legacy = !matches!(next, Some('.') | Some('e') | Some('E') | Some('8') | Some('9'));
                    if is_legacy && !digits.is_empty() {
                        return Ok(Tok::Num(parse_radix(&digits, 8)));
                    }
                    // Not legacy octal — rewind and scan as decimal.
                    self.pos = save;
                }
            }
        }

        // Decimal: integer [ . fraction ] [ e exponent ].
        self.take_digits(10);
        let mut is_float = false;
        if self.peek(0) == Some('.') {
            is_float = true;
            self.bump();
            self.take_digits(10);
        }
        if matches!(self.peek(0), Some('e') | Some('E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(0), Some('+') | Some('-')) {
                self.bump();
            }
            let exp = self.take_digits(10);
            if exp.is_empty() {
                return Err(self.err("missing exponent digits"));
            }
        }

        let raw: String = self.chars[start..self.pos].iter().collect();
        if !is_float && self.peek(0) == Some('n') {
            self.bump();
            return Ok(Tok::BigInt(raw.replace('_', "")));
        }

        let cleaned = raw.replace('_', "");
        match cleaned.parse::<f64>() {
            Ok(v) => Ok(Tok::Num(v)),
            Err(_) => Err(self.err(format!("invalid number literal `{raw}`"))),
        }
    }

    /// Collect a run of digits of the given radix, allowing `_` separators
    /// between digits. Returns the digits with separators stripped.
    fn take_digits(&mut self, radix: u32) -> String {
        let mut s = String::new();
        let mut last_was_digit = false;
        while let Some(c) = self.peek(0) {
            if c == '_' {
                if !last_was_digit {
                    break;
                }
                self.bump();
                last_was_digit = false;
                continue;
            }
            if c.is_digit(radix) {
                s.push(c);
                self.bump();
                last_was_digit = true;
            } else {
                break;
            }
        }
        s
    }

    // ---- strings ----------------------------------------------------------

    fn scan_string(&mut self, quote: char) -> Result<Tok, LexError> {
        self.bump(); // opening quote
        let mut s = String::new();
        loop {
            match self.peek(0) {
                None => return Err(self.err("unterminated string literal")),
                Some(c) if c == quote => {
                    self.bump();
                    return Ok(Tok::Str(s));
                }
                Some('\\') => {
                    self.bump();
                    if let Some(ch) = self.read_escape()? {
                        s.push(ch);
                    }
                }
                Some(c) if is_line_term(c) => {
                    return Err(self.err("unterminated string literal"));
                }
                Some(c) => {
                    s.push(c);
                    self.bump();
                }
            }
        }
    }

    // ---- templates --------------------------------------------------------

    /// Scan template text starting right after a `` ` `` (head/no-sub) or after
    /// a `}` that closed a `${` (middle/tail). Stops at `${` (pushing a
    /// `Template` brace and returning Head/Middle) or at the closing `` ` ``
    /// (returning NoSub/Tail).
    fn scan_template(&mut self, continuation: bool) -> Result<Tok, LexError> {
        let mut s = String::new();
        loop {
            match self.peek(0) {
                None => return Err(self.err("unterminated template literal")),
                Some('`') => {
                    self.bump();
                    return Ok(if continuation {
                        Tok::TemplateTail(s)
                    } else {
                        Tok::TemplateNoSub(s)
                    });
                }
                Some('$') if self.peek(1) == Some('{') => {
                    self.bump();
                    self.bump();
                    self.brace_stack.push(Brace::Template);
                    return Ok(if continuation {
                        Tok::TemplateMiddle(s)
                    } else {
                        Tok::TemplateHead(s)
                    });
                }
                Some('\\') => {
                    self.bump();
                    if let Some(ch) = self.read_escape()? {
                        s.push(ch);
                    }
                }
                Some(c) => {
                    // Raw line terminators are allowed inside templates.
                    s.push(if c == '\r' { '\n' } else { c });
                    self.bump();
                }
            }
        }
    }

    // ---- escapes ----------------------------------------------------------

    /// Read the body of an escape sequence (the `\` is already consumed).
    /// Returns `None` for a line-continuation (which contributes nothing).
    fn read_escape(&mut self) -> Result<Option<char>, LexError> {
        let c = match self.bump() {
            None => return Err(self.err("unterminated escape sequence")),
            Some(c) => c,
        };
        Ok(match c {
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            'b' => Some('\u{8}'),
            'f' => Some('\u{C}'),
            'v' => Some('\u{B}'),
            '0'..='7' => Some(self.read_octal_escape(c)?),
            'x' => Some(self.read_hex_escape(2)?),
            'u' => Some(self.read_unicode_escape()?),
            '\n' | '\u{2028}' | '\u{2029}' => None, // line continuation
            other => Some(other),
        })
    }

    fn read_octal_escape(&mut self, first: char) -> Result<char, LexError> {
        // `\0` not followed by a digit is NUL; otherwise up to 3 octal digits.
        let mut value = first.to_digit(8).unwrap();
        let mut count = 1;
        // First octal digit limits length to keep the value <= 0o377.
        let max = if first <= '3' { 3 } else { 2 };
        while count < max {
            match self.peek(0) {
                Some(d) if ('0'..='7').contains(&d) => {
                    value = value * 8 + d.to_digit(8).unwrap();
                    self.bump();
                    count += 1;
                }
                _ => break,
            }
        }
        char::from_u32(value).ok_or_else(|| self.err("invalid octal escape"))
    }

    fn read_hex_escape(&mut self, n: usize) -> Result<char, LexError> {
        let mut value: u32 = 0;
        for _ in 0..n {
            match self.peek(0) {
                Some(d) if d.is_ascii_hexdigit() => {
                    value = value * 16 + d.to_digit(16).unwrap();
                    self.bump();
                }
                _ => return Err(self.err("invalid hex escape")),
            }
        }
        char::from_u32(value).ok_or_else(|| self.err("invalid code point"))
    }

    fn read_unicode_escape(&mut self) -> Result<char, LexError> {
        if self.peek(0) == Some('{') {
            self.bump();
            let mut value: u32 = 0;
            let mut any = false;
            while let Some(d) = self.peek(0) {
                if d == '}' {
                    break;
                }
                if !d.is_ascii_hexdigit() {
                    return Err(self.err("invalid \\u{...} escape"));
                }
                value = value * 16 + d.to_digit(16).unwrap();
                if value > 0x10_FFFF {
                    return Err(self.err("code point out of range"));
                }
                any = true;
                self.bump();
            }
            if !any || self.peek(0) != Some('}') {
                return Err(self.err("invalid \\u{...} escape"));
            }
            self.bump(); // }
            char::from_u32(value).ok_or_else(|| self.err("invalid code point"))
        } else {
            self.read_hex_escape(4)
        }
    }

    // ---- regular expressions ---------------------------------------------

    fn scan_regex(&mut self) -> Result<Tok, LexError> {
        self.bump(); // opening /
        let mut body = String::new();
        let mut in_class = false; // inside a [...] character class
        loop {
            match self.peek(0) {
                None => return Err(self.err("unterminated regular expression")),
                Some(c) if is_line_term(c) => {
                    return Err(self.err("unterminated regular expression"));
                }
                Some('\\') => {
                    body.push('\\');
                    self.bump();
                    match self.peek(0) {
                        None | Some('\n') | Some('\r') => {
                            return Err(self.err("unterminated regular expression"));
                        }
                        Some(c) => {
                            body.push(c);
                            self.bump();
                        }
                    }
                }
                Some('[') => {
                    in_class = true;
                    body.push('[');
                    self.bump();
                }
                Some(']') => {
                    in_class = false;
                    body.push(']');
                    self.bump();
                }
                Some('/') if !in_class => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    body.push(c);
                    self.bump();
                }
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek(0) {
            if is_id_continue(c) {
                flags.push(c);
                self.bump();
            } else {
                break;
            }
        }
        Ok(Tok::Regex { body, flags })
    }

    // ---- punctuators ------------------------------------------------------

    fn scan_punct(&mut self) -> Result<Tok, LexError> {
        let c0 = self.peek(0).unwrap();
        let c1 = self.peek(1);
        let c2 = self.peek(2);
        let c3 = self.peek(3);

        // Greedy longest-match. `n` chars consumed → emit punct.
        macro_rules! p {
            ($n:expr, $punct:expr) => {{
                for _ in 0..$n {
                    self.bump();
                }
                if matches!($punct, Punct::LBrace) {
                    self.brace_stack.push(Brace::Normal);
                }
                return Ok(Tok::Punct($punct));
            }};
        }

        match c0 {
            '{' => p!(1, Punct::LBrace),
            '(' => p!(1, Punct::LParen),
            ')' => p!(1, Punct::RParen),
            '[' => p!(1, Punct::LBracket),
            ']' => p!(1, Punct::RBracket),
            ';' => p!(1, Punct::Semi),
            ',' => p!(1, Punct::Comma),
            '~' => p!(1, Punct::Tilde),
            ':' => p!(1, Punct::Colon),
            '.' => {
                if c1 == Some('.') && c2 == Some('.') {
                    p!(3, Punct::DotDotDot);
                }
                p!(1, Punct::Dot);
            }
            '?' => {
                if c1 == Some('?') && c2 == Some('=') {
                    p!(3, Punct::NullishEq);
                }
                if c1 == Some('?') {
                    p!(2, Punct::NullishCoalesce);
                }
                // `?.` is optional chaining only if not `?.<digit>` (conditional
                // on a number: `a ? .5 : .6`).
                if c1 == Some('.') && !c2.is_some_and(|d| d.is_ascii_digit()) {
                    p!(2, Punct::OptChain);
                }
                p!(1, Punct::Question);
            }
            '<' => {
                if c1 == Some('<') && c2 == Some('=') {
                    p!(3, Punct::ShlEq);
                }
                if c1 == Some('<') {
                    p!(2, Punct::Shl);
                }
                if c1 == Some('=') {
                    p!(2, Punct::LtEq);
                }
                p!(1, Punct::Lt);
            }
            '>' => {
                if c1 == Some('>') && c2 == Some('>') && c3 == Some('=') {
                    p!(4, Punct::UShrEq);
                }
                if c1 == Some('>') && c2 == Some('>') {
                    p!(3, Punct::UShr);
                }
                if c1 == Some('>') && c2 == Some('=') {
                    p!(3, Punct::ShrEq);
                }
                if c1 == Some('>') {
                    p!(2, Punct::Shr);
                }
                if c1 == Some('=') {
                    p!(2, Punct::GtEq);
                }
                p!(1, Punct::Gt);
            }
            '=' => {
                if c1 == Some('=') && c2 == Some('=') {
                    p!(3, Punct::EqEqEq);
                }
                if c1 == Some('=') {
                    p!(2, Punct::EqEq);
                }
                if c1 == Some('>') {
                    p!(2, Punct::Arrow);
                }
                p!(1, Punct::Eq);
            }
            '!' => {
                if c1 == Some('=') && c2 == Some('=') {
                    p!(3, Punct::NotEqEq);
                }
                if c1 == Some('=') {
                    p!(2, Punct::NotEq);
                }
                p!(1, Punct::Bang);
            }
            '+' => {
                if c1 == Some('+') {
                    p!(2, Punct::PlusPlus);
                }
                if c1 == Some('=') {
                    p!(2, Punct::PlusEq);
                }
                p!(1, Punct::Plus);
            }
            '-' => {
                if c1 == Some('-') {
                    p!(2, Punct::MinusMinus);
                }
                if c1 == Some('=') {
                    p!(2, Punct::MinusEq);
                }
                p!(1, Punct::Minus);
            }
            '*' => {
                if c1 == Some('*') && c2 == Some('=') {
                    p!(3, Punct::StarStarEq);
                }
                if c1 == Some('*') {
                    p!(2, Punct::StarStar);
                }
                if c1 == Some('=') {
                    p!(2, Punct::StarEq);
                }
                p!(1, Punct::Star);
            }
            '/' => {
                if c1 == Some('=') {
                    p!(2, Punct::SlashEq);
                }
                p!(1, Punct::Slash);
            }
            '%' => {
                if c1 == Some('=') {
                    p!(2, Punct::PercentEq);
                }
                p!(1, Punct::Percent);
            }
            '&' => {
                if c1 == Some('&') && c2 == Some('=') {
                    p!(3, Punct::AmpAmpEq);
                }
                if c1 == Some('&') {
                    p!(2, Punct::AmpAmp);
                }
                if c1 == Some('=') {
                    p!(2, Punct::AmpEq);
                }
                p!(1, Punct::Amp);
            }
            '|' => {
                if c1 == Some('|') && c2 == Some('=') {
                    p!(3, Punct::PipePipeEq);
                }
                if c1 == Some('|') {
                    p!(2, Punct::PipePipe);
                }
                if c1 == Some('=') {
                    p!(2, Punct::PipeEq);
                }
                p!(1, Punct::Pipe);
            }
            '^' => {
                if c1 == Some('=') {
                    p!(2, Punct::CaretEq);
                }
                p!(1, Punct::Caret);
            }
            other => Err(self.err(format!("unexpected character `{other}`"))),
        }
    }
}

// ---- token classification helpers -----------------------------------------

/// Whether a regular expression may legally start *after* the given token.
fn regex_allowed_after(tok: &Tok) -> bool {
    match tok {
        Tok::Ident(name) => expects_expression_after(name),
        Tok::Num(_)
        | Tok::BigInt(_)
        | Tok::Str(_)
        | Tok::TemplateNoSub(_)
        | Tok::TemplateTail(_)
        | Tok::Regex { .. }
        | Tok::Eof => false,
        Tok::TemplateHead(_) | Tok::TemplateMiddle(_) => true,
        Tok::Punct(p) => !matches!(
            p,
            Punct::RParen | Punct::RBracket | Punct::RBrace | Punct::PlusPlus | Punct::MinusMinus
        ),
    }
}

/// A readable raw lexeme for a token (used for diagnostics and the parser).
fn token_text(tok: &Tok) -> String {
    match tok {
        Tok::Ident(s) | Tok::Str(s) => s.clone(),
        Tok::Num(n) => n.to_string(),
        Tok::BigInt(s) => format!("{s}n"),
        Tok::TemplateNoSub(s) => format!("`{s}`"),
        Tok::TemplateHead(s) => format!("`{s}${{"),
        Tok::TemplateMiddle(s) => format!("}}{s}${{"),
        Tok::TemplateTail(s) => format!("}}{s}`"),
        Tok::Regex { body, flags } => format!("/{body}/{flags}"),
        Tok::Punct(p) => punct_text(*p).to_string(),
        Tok::Eof => String::new(),
    }
}

fn punct_text(p: Punct) -> &'static str {
    use Punct::*;
    match p {
        LBrace => "{",
        RBrace => "}",
        LParen => "(",
        RParen => ")",
        LBracket => "[",
        RBracket => "]",
        Dot => ".",
        DotDotDot => "...",
        Semi => ";",
        Comma => ",",
        Arrow => "=>",
        Colon => ":",
        Question => "?",
        OptChain => "?.",
        Lt => "<",
        Gt => ">",
        LtEq => "<=",
        GtEq => ">=",
        EqEq => "==",
        NotEq => "!=",
        EqEqEq => "===",
        NotEqEq => "!==",
        Plus => "+",
        Minus => "-",
        Star => "*",
        Slash => "/",
        Percent => "%",
        StarStar => "**",
        PlusPlus => "++",
        MinusMinus => "--",
        Shl => "<<",
        Shr => ">>",
        UShr => ">>>",
        Amp => "&",
        Pipe => "|",
        Caret => "^",
        Bang => "!",
        Tilde => "~",
        AmpAmp => "&&",
        PipePipe => "||",
        NullishCoalesce => "??",
        Eq => "=",
        PlusEq => "+=",
        MinusEq => "-=",
        StarEq => "*=",
        SlashEq => "/=",
        PercentEq => "%=",
        StarStarEq => "**=",
        ShlEq => "<<=",
        ShrEq => ">>=",
        UShrEq => ">>>=",
        AmpEq => "&=",
        PipeEq => "|=",
        CaretEq => "^=",
        AmpAmpEq => "&&=",
        PipePipeEq => "||=",
        NullishEq => "??=",
    }
}

// ---- character predicates --------------------------------------------------

fn is_line_term(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\u{0B}' | '\u{0C}' | ' ' | '\u{A0}' | '\u{FEFF}'
    ) || (c != '\n' && c != '\r' && c.is_whitespace())
}

fn is_id_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic()
}

fn is_id_continue(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric() || c == '\u{200C}' || c == '\u{200D}'
}

// ---- numeric helpers -------------------------------------------------------

/// Parse `digits` in the given radix into an `f64`, folding digit by digit so
/// very large literals saturate to `f64::INFINITY` instead of overflowing.
fn parse_radix(digits: &str, radix: u32) -> f64 {
    let base = radix as f64;
    let mut value = 0.0_f64;
    for c in digits.chars() {
        if let Some(d) = c.to_digit(radix) {
            value = value * base + d as f64;
        }
    }
    value
}

/// Convert a non-decimal integer literal to its decimal string form (for
/// `BigInt` values), using schoolbook base conversion on a little-endian digit
/// buffer (no external bignum).
fn to_decimal_string(digits: &str, radix: u32) -> String {
    // `acc` holds the decimal value as little-endian base-1e9 limbs.
    let mut acc: Vec<u64> = vec![0];
    for c in digits.chars() {
        let d = match c.to_digit(radix) {
            Some(d) => d as u64,
            None => continue,
        };
        // acc = acc * radix + d
        let mut carry = d;
        for limb in acc.iter_mut() {
            let v = *limb * radix as u64 + carry;
            *limb = v % 1_000_000_000;
            carry = v / 1_000_000_000;
        }
        while carry > 0 {
            acc.push(carry % 1_000_000_000);
            carry /= 1_000_000_000;
        }
    }
    let mut out = String::new();
    out.push_str(&acc.last().unwrap().to_string());
    for limb in acc.iter().rev().skip(1) {
        out.push_str(&format!("{limb:09}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        tokenize(src)
            .unwrap()
            .into_iter()
            .map(|t| t.tok)
            .filter(|t| !matches!(t, Tok::Eof))
            .collect()
    }

    #[test]
    fn punctuators_longest_match() {
        assert_eq!(
            kinds("a >>>= b ?? c => d"),
            vec![
                Tok::Ident("a".into()),
                Tok::Punct(Punct::UShrEq),
                Tok::Ident("b".into()),
                Tok::Punct(Punct::NullishCoalesce),
                Tok::Ident("c".into()),
                Tok::Punct(Punct::Arrow),
                Tok::Ident("d".into()),
            ]
        );
    }

    #[test]
    fn numbers_all_forms() {
        assert_eq!(kinds("0xFF"), vec![Tok::Num(255.0)]);
        assert_eq!(kinds("0o17"), vec![Tok::Num(15.0)]);
        assert_eq!(kinds("0b1010"), vec![Tok::Num(10.0)]);
        assert_eq!(kinds("1_000_000"), vec![Tok::Num(1_000_000.0)]);
        assert_eq!(kinds("3.14e2"), vec![Tok::Num(314.0)]);
        assert_eq!(kinds(".5"), vec![Tok::Num(0.5)]);
        assert_eq!(kinds("0777"), vec![Tok::Num(511.0)]); // legacy octal
        assert_eq!(kinds("123n"), vec![Tok::BigInt("123".into())]);
    }

    #[test]
    fn bigint_hex_to_decimal() {
        assert_eq!(kinds("0xffn"), vec![Tok::BigInt("255".into())]);
        assert_eq!(
            kinds("0x1FFFFFFFFFFFFFn"),
            vec![Tok::BigInt("9007199254740991".into())]
        );
    }

    #[test]
    fn strings_with_escapes() {
        assert_eq!(kinds(r#""a\nb\tA\x42""#), vec![Tok::Str("a\nb\tAB".into())]);
        assert_eq!(kinds(r#"'\u{1F600}'"#), vec![Tok::Str("😀".into())]);
        assert_eq!(kinds("'line\\\ncont'"), vec![Tok::Str("linecont".into())]);
    }

    #[test]
    fn regex_vs_division() {
        // After `return`, `/` starts a regex.
        assert_eq!(
            kinds("return /ab+/gi"),
            vec![
                Tok::Ident("return".into()),
                Tok::Regex { body: "ab+".into(), flags: "gi".into() },
            ]
        );
        // After an identifier, `/` is division.
        assert_eq!(
            kinds("a / b / c"),
            vec![
                Tok::Ident("a".into()),
                Tok::Punct(Punct::Slash),
                Tok::Ident("b".into()),
                Tok::Punct(Punct::Slash),
                Tok::Ident("c".into()),
            ]
        );
        // Character class containing `/`.
        assert_eq!(
            kinds("x = /[a/b]/"),
            vec![
                Tok::Ident("x".into()),
                Tok::Punct(Punct::Eq),
                Tok::Regex { body: "[a/b]".into(), flags: "".into() },
            ]
        );
    }

    #[test]
    fn template_with_substitutions() {
        assert_eq!(
            kinds("`a${1+2}b${x}c`"),
            vec![
                Tok::TemplateHead("a".into()),
                Tok::Num(1.0),
                Tok::Punct(Punct::Plus),
                Tok::Num(2.0),
                Tok::TemplateMiddle("b".into()),
                Tok::Ident("x".into()),
                Tok::TemplateTail("c".into()),
            ]
        );
    }

    #[test]
    fn template_with_nested_object() {
        // The inner `}` closes the object; the outer `}` resumes the template.
        assert_eq!(
            kinds("`a${ {x:1} }b`"),
            vec![
                Tok::TemplateHead("a".into()),
                Tok::Punct(Punct::LBrace),
                Tok::Ident("x".into()),
                Tok::Punct(Punct::Colon),
                Tok::Num(1.0),
                Tok::Punct(Punct::RBrace),
                Tok::TemplateTail("b".into()),
            ]
        );
    }

    #[test]
    fn template_no_substitution() {
        assert_eq!(kinds("`hello`"), vec![Tok::TemplateNoSub("hello".into())]);
    }

    #[test]
    fn comments_skipped_and_newline_flagged() {
        let toks = tokenize("a // comment\nb /* block */ c").unwrap();
        let names: Vec<_> = toks
            .iter()
            .filter(|t| !matches!(t.tok, Tok::Eof))
            .map(|t| (t.text.clone(), t.newline_before))
            .collect();
        assert_eq!(
            names,
            vec![
                ("a".to_string(), false),
                ("b".to_string(), true),
                ("c".to_string(), false),
            ]
        );
    }

    #[test]
    fn hashbang_ignored() {
        assert_eq!(kinds("#!/usr/bin/node\nx"), vec![Tok::Ident("x".into())]);
    }
}
