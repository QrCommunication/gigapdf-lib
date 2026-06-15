//! JavaScript token model.
//!
//! The lexer emits identifiers (including reserved words) as [`Tok::Ident`];
//! keyword-ness in JavaScript is *contextual* (`of`, `as`, `let`, `yield`,
//! `await`, `async`, `get`, `set`, `static`, `from` are reserved only in some
//! positions), so classification is the parser's job. [`is_reserved_word`] and
//! [`expects_expression_after`] expose the small amount of lexical context the
//! scanner itself needs (e.g. for regex/division disambiguation).

/// A lexical token with its raw source slice and position.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// The classified token.
    pub tok: Tok,
    /// The raw source lexeme (as written).
    pub text: String,
    /// 1-based line of the token start.
    pub line: u32,
    /// 1-based column of the token start.
    pub col: u32,
    /// `true` if a line terminator appeared between the previous token and
    /// this one — the signal the parser needs for Automatic Semicolon
    /// Insertion (ASI).
    pub newline_before: bool,
}

/// Token kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Identifier or reserved word (the lexeme is in `Token.text`).
    Ident(String),
    /// Numeric literal, already parsed to an IEEE-754 double.
    Num(f64),
    /// BigInt literal — the raw digit string without the trailing `n`.
    BigInt(String),
    /// String literal, with escape sequences already decoded ("cooked").
    Str(String),
    /// A template with no substitutions: `` `abc` `` → cooked `abc`.
    TemplateNoSub(String),
    /// The leading chunk of a template, up to the first `${`.
    TemplateHead(String),
    /// A chunk between two substitutions: `}...${`.
    TemplateMiddle(String),
    /// The trailing chunk of a template: `}...` `` ` ``.
    TemplateTail(String),
    /// A regular-expression literal `/body/flags`.
    Regex {
        /// The pattern source, without the slashes.
        body: String,
        /// The trailing flag characters (e.g. `gi`).
        flags: String,
    },
    /// A punctuator or operator.
    Punct(Punct),
    /// End of input.
    Eof,
}

/// Punctuators and operators (the full ES2021 set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    // Brackets & separators
    LBrace,    // {
    RBrace,    // }
    LParen,    // (
    RParen,    // )
    LBracket,  // [
    RBracket,  // ]
    Dot,       // .
    DotDotDot, // ...
    Semi,      // ;
    Comma,     // ,
    Arrow,     // =>
    Colon,     // :
    Question,  // ?
    OptChain,  // ?.

    // Comparison
    Lt,      // <
    Gt,      // >
    LtEq,    // <=
    GtEq,    // >=
    EqEq,    // ==
    NotEq,   // !=
    EqEqEq,  // ===
    NotEqEq, // !==

    // Arithmetic / bitwise
    Plus,            // +
    Minus,           // -
    Star,            // *
    Slash,           // /
    Percent,         // %
    StarStar,        // **
    PlusPlus,        // ++
    MinusMinus,      // --
    Shl,             // <<
    Shr,             // >>
    UShr,            // >>>
    Amp,             // &
    Pipe,            // |
    Caret,           // ^
    Bang,            // !
    Tilde,           // ~
    AmpAmp,          // &&
    PipePipe,        // ||
    NullishCoalesce, // ??

    // Assignment
    Eq,         // =
    PlusEq,     // +=
    MinusEq,    // -=
    StarEq,     // *=
    SlashEq,    // /=
    PercentEq,  // %=
    StarStarEq, // **=
    ShlEq,      // <<=
    ShrEq,      // >>=
    UShrEq,     // >>>=
    AmpEq,      // &=
    PipeEq,     // |=
    CaretEq,    // ^=
    AmpAmpEq,   // &&=
    PipePipeEq, // ||=
    NullishEq,  // ??=
}

/// The ECMAScript reserved words (strict-mode inclusive).
const RESERVED: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    // strict-mode reserved
    "implements",
    "interface",
    "let",
    "package",
    "private",
    "protected",
    "public",
    "static",
    "yield",
];

/// Whether `word` is an ECMAScript reserved word.
pub fn is_reserved_word(word: &str) -> bool {
    RESERVED.contains(&word)
}

/// Whether a regular expression may legally begin immediately after an
/// identifier/keyword with the given lexeme — i.e. the keyword expects an
/// expression to follow. Used to disambiguate `/` (regex vs division) after a
/// word token. Plain identifiers and value keywords (`this`, `true`, …) return
/// `false` (a following `/` is division).
pub fn expects_expression_after(word: &str) -> bool {
    matches!(
        word,
        "return"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "new"
            | "delete"
            | "void"
            | "throw"
            | "do"
            | "else"
            | "case"
            | "yield"
            | "await"
    )
}
