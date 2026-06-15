//! A zero-dependency JavaScript parser: token stream → [`Program`] AST.
//!
//! Recursive descent for statements; operator-precedence ("Pratt") climbing for
//! expressions. Arrow functions are recognised by a balanced-parenthesis
//! look-ahead (so `(a, b) => …` is parsed with a real parameter grammar rather
//! than a cover-grammar reinterpretation), while destructuring **assignment**
//! targets are converted from expression literals on demand. Automatic
//! Semicolon Insertion is applied at statement boundaries.

use super::ast::*;
use super::lexer::tokenize;
use super::token::{Punct, Tok, Token};

/// A parse error with source position.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    /// Message.
    pub msg: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub col: u32,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} (line {}, col {})", self.msg, self.line, self.col)
    }
}

type PResult<T> = Result<T, ParseError>;

/// Lex and parse `src` into a [`Program`].
pub fn parse(src: &str) -> PResult<Program> {
    let toks = tokenize(src).map_err(|e| ParseError {
        msg: e.msg,
        line: e.line,
        col: e.col,
    })?;
    Parser { toks, pos: 0 }.parse_program()
}

/// The recursive-descent parser.
#[derive(Debug)]
pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

impl Parser {
    // ---- cursor -----------------------------------------------------------

    fn cur(&self) -> &Token {
        &self.toks[self.pos]
    }

    fn nth(&self, n: usize) -> &Token {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i]
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        matches!(self.cur().tok, Tok::Eof)
    }

    fn is_punct(&self, p: Punct) -> bool {
        matches!(&self.cur().tok, Tok::Punct(x) if *x == p)
    }

    fn nth_is_punct(&self, n: usize, p: Punct) -> bool {
        matches!(&self.nth(n).tok, Tok::Punct(x) if *x == p)
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.is_punct(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: Punct) -> PResult<()> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", punct_str(p))))
        }
    }

    fn is_kw(&self, s: &str) -> bool {
        matches!(&self.cur().tok, Tok::Ident(x) if x == s)
    }

    fn nth_is_kw(&self, n: usize, s: &str) -> bool {
        matches!(&self.nth(n).tok, Tok::Ident(x) if x == s)
    }

    fn eat_kw(&mut self, s: &str) -> bool {
        if self.is_kw(s) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn ident_name(&mut self) -> PResult<String> {
        if let Tok::Ident(s) = &self.cur().tok {
            let s = s.clone();
            self.bump();
            Ok(s)
        } else {
            Err(self.err("expected identifier"))
        }
    }

    fn err(&self, m: &str) -> ParseError {
        let t = self.cur();
        let found = if t.text.is_empty() {
            "<eof>".to_string()
        } else {
            t.text.clone()
        };
        ParseError {
            msg: format!("{m}, found `{found}`"),
            line: t.line,
            col: t.col,
        }
    }

    /// Apply Automatic Semicolon Insertion at a statement boundary.
    fn semicolon(&mut self) -> PResult<()> {
        if self.eat_punct(Punct::Semi) {
            return Ok(());
        }
        if self.at_eof() || self.is_punct(Punct::RBrace) || self.cur().newline_before {
            return Ok(());
        }
        Err(self.err("expected `;`"))
    }

    // ---- program & statements --------------------------------------------

    fn parse_program(&mut self) -> PResult<Program> {
        let mut body = Vec::new();
        while !self.at_eof() {
            body.push(self.parse_stmt()?);
        }
        Ok(Program { body })
    }

    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_punct(Punct::LBrace)?;
        let mut body = Vec::new();
        while !self.is_punct(Punct::RBrace) && !self.at_eof() {
            body.push(self.parse_stmt()?);
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(body)
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        if self.is_punct(Punct::LBrace) {
            return Ok(Stmt::Block(self.parse_block()?));
        }
        if self.eat_punct(Punct::Semi) {
            return Ok(Stmt::Empty);
        }
        if let Some(kind) = self.var_decl_kind() {
            self.bump(); // var/let/const
            let decls = self.parse_declarators()?;
            self.semicolon()?;
            return Ok(Stmt::VarDecl { kind, decls });
        }
        if self.is_kw("function") || (self.is_kw("async") && self.nth_is_kw(1, "function")) {
            let f = self.parse_function(true)?;
            return Ok(Stmt::FuncDecl(f));
        }
        if self.is_kw("class") {
            let c = self.parse_class(true)?;
            return Ok(Stmt::ClassDecl(c));
        }
        if self.is_kw("export") {
            return self.parse_export();
        }
        if self.is_kw("import")
            && !self.nth_is_punct(1, Punct::LParen)
            && !self.nth_is_punct(1, Punct::Dot)
        {
            return self.parse_import();
        }
        if self.is_kw("if") {
            return self.parse_if();
        }
        if self.is_kw("for") {
            return self.parse_for();
        }
        if self.is_kw("while") {
            return self.parse_while();
        }
        if self.is_kw("do") {
            return self.parse_do_while();
        }
        if self.is_kw("switch") {
            return self.parse_switch();
        }
        if self.is_kw("return") {
            self.bump();
            let arg = if self.is_punct(Punct::Semi)
                || self.is_punct(Punct::RBrace)
                || self.at_eof()
                || self.cur().newline_before
            {
                None
            } else {
                Some(self.parse_expression()?)
            };
            self.semicolon()?;
            return Ok(Stmt::Return(arg));
        }
        if self.is_kw("break") || self.is_kw("continue") {
            let is_break = self.is_kw("break");
            self.bump();
            let label = if !self.cur().newline_before && self.cur_is_plain_ident() {
                Some(self.ident_name()?)
            } else {
                None
            };
            self.semicolon()?;
            return Ok(if is_break {
                Stmt::Break(label)
            } else {
                Stmt::Continue(label)
            });
        }
        if self.is_kw("throw") {
            self.bump();
            let e = self.parse_expression()?;
            self.semicolon()?;
            return Ok(Stmt::Throw(e));
        }
        if self.is_kw("try") {
            return self.parse_try();
        }
        if self.eat_kw("debugger") {
            self.semicolon()?;
            return Ok(Stmt::Debugger);
        }
        // Labeled statement: `ident :`
        if self.cur_is_plain_ident() && self.nth_is_punct(1, Punct::Colon) {
            let label = self.ident_name()?;
            self.expect_punct(Punct::Colon)?;
            let body = Box::new(self.parse_stmt()?);
            return Ok(Stmt::Labeled { label, body });
        }
        // Expression statement.
        let e = self.parse_expression()?;
        self.semicolon()?;
        Ok(Stmt::Expr(e))
    }

    /// `var` / `const`, or `let` when it begins a binding.
    fn var_decl_kind(&self) -> Option<VarKind> {
        if self.is_kw("var") {
            return Some(VarKind::Var);
        }
        if self.is_kw("const") {
            return Some(VarKind::Const);
        }
        if self.is_kw("let") {
            let starts_binding = matches!(&self.nth(1).tok, Tok::Ident(_))
                || self.nth_is_punct(1, Punct::LBracket)
                || self.nth_is_punct(1, Punct::LBrace);
            if starts_binding {
                return Some(VarKind::Let);
            }
        }
        None
    }

    /// `true` if the current token is an identifier that is not a reserved word
    /// (so it may be a label / break-target).
    fn cur_is_plain_ident(&self) -> bool {
        matches!(&self.cur().tok, Tok::Ident(s) if !super::token::is_reserved_word(s))
    }

    fn parse_declarators(&mut self) -> PResult<Vec<VarDeclarator>> {
        let mut out = Vec::new();
        loop {
            let id = self.parse_binding_target()?;
            let init = if self.eat_punct(Punct::Eq) {
                Some(self.parse_assign(false)?)
            } else {
                None
            };
            out.push(VarDeclarator { id, init });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(out)
    }

    fn parse_if(&mut self) -> PResult<Stmt> {
        self.bump(); // if
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expression()?;
        self.expect_punct(Punct::RParen)?;
        let cons = Box::new(self.parse_stmt()?);
        let alt = if self.eat_kw("else") {
            Some(Box::new(self.parse_stmt()?))
        } else {
            None
        };
        Ok(Stmt::If { test, cons, alt })
    }

    fn parse_while(&mut self) -> PResult<Stmt> {
        self.bump();
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expression()?;
        self.expect_punct(Punct::RParen)?;
        let body = Box::new(self.parse_stmt()?);
        Ok(Stmt::While { test, body })
    }

    fn parse_do_while(&mut self) -> PResult<Stmt> {
        self.bump();
        let body = Box::new(self.parse_stmt()?);
        if !self.eat_kw("while") {
            return Err(self.err("expected `while`"));
        }
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expression()?;
        self.expect_punct(Punct::RParen)?;
        self.semicolon()?;
        Ok(Stmt::DoWhile { body, test })
    }

    fn parse_for(&mut self) -> PResult<Stmt> {
        self.bump(); // for
                     // `for await` is accepted and ignored (no async iteration yet).
        self.eat_kw("await");
        self.expect_punct(Punct::LParen)?;

        // Declaration head?
        if let Some(kind) = self.var_decl_kind() {
            self.bump();
            let pat = self.parse_binding_target()?;
            if self.is_kw("of") || self.is_kw("in") {
                let is_of = self.is_kw("of");
                self.bump();
                let right = self.parse_assign(false)?;
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.parse_stmt()?);
                let left = Box::new(ForHead::Decl { kind, pat });
                return Ok(if is_of {
                    Stmt::ForOf { left, right, body }
                } else {
                    Stmt::ForIn { left, right, body }
                });
            }
            // C-style with one or more declarators.
            let init0_init = if self.eat_punct(Punct::Eq) {
                Some(self.parse_assign(true)?)
            } else {
                None
            };
            let mut decls = vec![VarDeclarator {
                id: pat,
                init: init0_init,
            }];
            while self.eat_punct(Punct::Comma) {
                let id = self.parse_binding_target()?;
                let init = if self.eat_punct(Punct::Eq) {
                    Some(self.parse_assign(true)?)
                } else {
                    None
                };
                decls.push(VarDeclarator { id, init });
            }
            let init = Some(Box::new(ForInit::VarDecl { kind, decls }));
            return self.finish_c_for(init);
        }

        // No declaration: empty init, or expression init / for-in/of target.
        if self.is_punct(Punct::Semi) {
            return self.finish_c_for(None);
        }
        let left_expr = self.parse_expression_no_in()?;
        if self.is_kw("of") || self.is_kw("in") {
            let is_of = self.is_kw("of");
            self.bump();
            let right = self.parse_assign(false)?;
            self.expect_punct(Punct::RParen)?;
            let body = Box::new(self.parse_stmt()?);
            let pat = expr_to_pattern(left_expr)?;
            let left = Box::new(ForHead::Pattern(pat));
            return Ok(if is_of {
                Stmt::ForOf { left, right, body }
            } else {
                Stmt::ForIn { left, right, body }
            });
        }
        self.finish_c_for(Some(Box::new(ForInit::Expr(left_expr))))
    }

    fn finish_c_for(&mut self, init: Option<Box<ForInit>>) -> PResult<Stmt> {
        self.expect_punct(Punct::Semi)?;
        let test = if self.is_punct(Punct::Semi) {
            None
        } else {
            Some(self.parse_expression()?)
        };
        self.expect_punct(Punct::Semi)?;
        let update = if self.is_punct(Punct::RParen) {
            None
        } else {
            Some(self.parse_expression()?)
        };
        self.expect_punct(Punct::RParen)?;
        let body = Box::new(self.parse_stmt()?);
        Ok(Stmt::For {
            init,
            test,
            update,
            body,
        })
    }

    fn parse_switch(&mut self) -> PResult<Stmt> {
        self.bump();
        self.expect_punct(Punct::LParen)?;
        let disc = self.parse_expression()?;
        self.expect_punct(Punct::RParen)?;
        self.expect_punct(Punct::LBrace)?;
        let mut cases = Vec::new();
        while !self.is_punct(Punct::RBrace) && !self.at_eof() {
            let test = if self.eat_kw("case") {
                let e = self.parse_expression()?;
                Some(e)
            } else if self.eat_kw("default") {
                None
            } else {
                return Err(self.err("expected `case` or `default`"));
            };
            self.expect_punct(Punct::Colon)?;
            let mut body = Vec::new();
            while !self.is_kw("case")
                && !self.is_kw("default")
                && !self.is_punct(Punct::RBrace)
                && !self.at_eof()
            {
                body.push(self.parse_stmt()?);
            }
            cases.push(SwitchCase { test, body });
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Stmt::Switch { disc, cases })
    }

    fn parse_try(&mut self) -> PResult<Stmt> {
        self.bump();
        let block = self.parse_block()?;
        let handler = if self.eat_kw("catch") {
            let param = if self.eat_punct(Punct::LParen) {
                let p = self.parse_binding_target()?;
                self.expect_punct(Punct::RParen)?;
                Some(p)
            } else {
                None
            };
            let body = self.parse_block()?;
            Some(Catch { param, body })
        } else {
            None
        };
        let finalizer = if self.eat_kw("finally") {
            Some(self.parse_block()?)
        } else {
            None
        };
        if handler.is_none() && finalizer.is_none() {
            return Err(self.err("`try` requires `catch` or `finally`"));
        }
        Ok(Stmt::Try {
            block,
            handler,
            finalizer,
        })
    }

    // ---- ES modules (single-module; exports are transparent decls) --------

    /// Parse an `export …` declaration. Exported declarations are evaluated as
    /// ordinary declarations (so their bindings exist); export specifier lists
    /// and re-exports are accepted and elided.
    fn parse_export(&mut self) -> PResult<Stmt> {
        self.bump(); // export
        if self.eat_kw("default") {
            if self.is_kw("function")
                || self.is_kw("class")
                || (self.is_kw("async") && self.nth_is_kw(1, "function"))
            {
                return self.parse_stmt();
            }
            let e = self.parse_assign(false)?;
            self.semicolon()?;
            return Ok(Stmt::Expr(e));
        }
        if self.is_punct(Punct::LBrace) {
            self.skip_braces();
            self.eat_kw("from");
            if matches!(self.cur().tok, Tok::Str(_)) {
                self.bump();
            }
            self.semicolon()?;
            return Ok(Stmt::Empty);
        }
        if self.is_punct(Punct::Star) {
            while !matches!(self.cur().tok, Tok::Str(_)) && !self.at_eof() {
                self.bump();
            }
            if matches!(self.cur().tok, Tok::Str(_)) {
                self.bump();
            }
            self.semicolon()?;
            return Ok(Stmt::Empty);
        }
        self.parse_stmt()
    }

    /// Parse and elide an `import …` declaration (no cross-module resolution in
    /// a single inline script).
    fn parse_import(&mut self) -> PResult<Stmt> {
        self.bump(); // import
        while !matches!(self.cur().tok, Tok::Str(_))
            && !self.at_eof()
            && !self.is_punct(Punct::Semi)
        {
            self.bump();
        }
        if matches!(self.cur().tok, Tok::Str(_)) {
            self.bump();
        }
        self.semicolon()?;
        Ok(Stmt::Empty)
    }

    fn skip_braces(&mut self) {
        let mut depth = 0i32;
        loop {
            if self.is_punct(Punct::LBrace) {
                depth += 1;
                self.bump();
            } else if self.is_punct(Punct::RBrace) {
                depth -= 1;
                self.bump();
                if depth <= 0 {
                    break;
                }
            } else if self.at_eof() {
                break;
            } else {
                self.bump();
            }
        }
    }

    // ---- binding patterns -------------------------------------------------

    fn parse_binding_target(&mut self) -> PResult<Pattern> {
        if self.is_punct(Punct::LBracket) {
            return self.parse_array_pattern();
        }
        if self.is_punct(Punct::LBrace) {
            return self.parse_object_pattern();
        }
        Ok(Pattern::Ident(self.ident_name()?))
    }

    /// A binding element: `...target` / `target` / `target = default`.
    fn parse_binding_element(&mut self) -> PResult<Pattern> {
        if self.eat_punct(Punct::DotDotDot) {
            return Ok(Pattern::Rest(Box::new(self.parse_binding_target()?)));
        }
        let target = self.parse_binding_target()?;
        if self.eat_punct(Punct::Eq) {
            let default = Box::new(self.parse_assign(false)?);
            return Ok(Pattern::Default {
                target: Box::new(target),
                default,
            });
        }
        Ok(target)
    }

    fn parse_array_pattern(&mut self) -> PResult<Pattern> {
        self.expect_punct(Punct::LBracket)?;
        let mut elems = Vec::new();
        while !self.is_punct(Punct::RBracket) {
            if self.is_punct(Punct::Comma) {
                elems.push(None); // hole
                self.bump();
                continue;
            }
            elems.push(Some(self.parse_binding_element()?));
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBracket)?;
        Ok(Pattern::Array(elems))
    }

    fn parse_object_pattern(&mut self) -> PResult<Pattern> {
        self.expect_punct(Punct::LBrace)?;
        let mut props = Vec::new();
        let mut rest = None;
        while !self.is_punct(Punct::RBrace) {
            if self.eat_punct(Punct::DotDotDot) {
                rest = Some(Box::new(self.parse_binding_target()?));
                break;
            }
            let (key, shorthand_name) = self.parse_prop_key()?;
            let value = if self.eat_punct(Punct::Colon) {
                self.parse_binding_element()?
            } else {
                // shorthand, with optional default
                let name = shorthand_name.ok_or_else(|| self.err("invalid pattern key"))?;
                let base = Pattern::Ident(name);
                if self.eat_punct(Punct::Eq) {
                    Pattern::Default {
                        target: Box::new(base),
                        default: Box::new(self.parse_assign(false)?),
                    }
                } else {
                    base
                }
            };
            props.push(ObjectPatProp { key, value });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Pattern::Object { props, rest })
    }

    /// Parse a property key, returning the key and (for plain identifiers) the
    /// shorthand name.
    fn parse_prop_key(&mut self) -> PResult<(PropKey, Option<String>)> {
        if self.is_punct(Punct::LBracket) {
            self.bump();
            let e = self.parse_assign(false)?;
            self.expect_punct(Punct::RBracket)?;
            return Ok((PropKey::Computed(Box::new(e)), None));
        }
        match &self.cur().tok {
            Tok::Str(s) => {
                let s = s.clone();
                self.bump();
                Ok((PropKey::Str(s), None))
            }
            Tok::Num(n) => {
                let n = *n;
                self.bump();
                Ok((PropKey::Num(n), None))
            }
            Tok::Ident(s) => {
                let s = s.clone();
                self.bump();
                Ok((PropKey::Ident(s.clone()), Some(s)))
            }
            _ => Err(self.err("expected property name")),
        }
    }

    // ---- functions & classes ---------------------------------------------

    /// Parse a `function` (declaration or expression). `require_name` is true in
    /// statement position.
    fn parse_function(&mut self, require_name: bool) -> PResult<Func> {
        let is_async = self.eat_kw("async");
        self.bump(); // function
        let is_generator = self.eat_punct(Punct::Star);
        let name = if self.cur_is_ident_like() {
            Some(self.ident_name()?)
        } else if require_name {
            return Err(self.err("function declaration requires a name"));
        } else {
            None
        };
        let params = self.parse_param_list()?;
        let body = FuncBody::Block(self.parse_block()?);
        Ok(Func {
            name,
            params,
            body,
            is_arrow: false,
            is_async,
            is_generator,
        })
    }

    fn cur_is_ident_like(&self) -> bool {
        matches!(&self.cur().tok, Tok::Ident(_))
    }

    fn parse_param_list(&mut self) -> PResult<Vec<Pattern>> {
        self.expect_punct(Punct::LParen)?;
        let mut params = Vec::new();
        while !self.is_punct(Punct::RParen) {
            params.push(self.parse_binding_element()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(params)
    }

    fn parse_class(&mut self, _require_name: bool) -> PResult<Class> {
        self.bump(); // class
                     // A name is optional in both positions (anonymous class declarations
                     // occur after `export default`).
        let name = if self.cur_is_ident_like() && !self.is_kw("extends") {
            Some(self.ident_name()?)
        } else {
            None
        };
        let super_class = if self.eat_kw("extends") {
            Some(Box::new(self.parse_lhs_expr()?))
        } else {
            None
        };
        self.expect_punct(Punct::LBrace)?;
        let mut members = Vec::new();
        while !self.is_punct(Punct::RBrace) && !self.at_eof() {
            if self.eat_punct(Punct::Semi) {
                continue;
            }
            members.push(self.parse_class_member()?);
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Class {
            name,
            super_class,
            members,
        })
    }

    fn parse_class_member(&mut self) -> PResult<ClassMember> {
        let is_static = self.is_kw("static") && !self.nth_is_punct(1, Punct::LParen) && {
            self.bump();
            true
        };

        // Detect get/set/async/generator prefixes (each only if a key follows).
        let mut kind = ClassMemberKind::Method;
        let mut is_async = false;
        let mut is_generator = false;

        if self.is_kw("async")
            && !self.nth_is_punct(1, Punct::LParen)
            && !self.nth_is_punct(1, Punct::Eq)
        {
            is_async = true;
            self.bump();
        }
        if self.eat_punct(Punct::Star) {
            is_generator = true;
        }
        if self.is_kw("get")
            && !self.nth_is_punct(1, Punct::LParen)
            && !self.nth_is_punct(1, Punct::Eq)
        {
            kind = ClassMemberKind::Get;
            self.bump();
        } else if self.is_kw("set")
            && !self.nth_is_punct(1, Punct::LParen)
            && !self.nth_is_punct(1, Punct::Eq)
        {
            kind = ClassMemberKind::Set;
            self.bump();
        }

        let (key, key_name) = self.parse_prop_key()?;

        // Method / accessor: a `(` follows.
        if self.is_punct(Punct::LParen) {
            if key_name.as_deref() == Some("constructor") && kind == ClassMemberKind::Method {
                kind = ClassMemberKind::Constructor;
            }
            let params = self.parse_param_list()?;
            let body = FuncBody::Block(self.parse_block()?);
            let func = Func {
                name: key_name,
                params,
                body,
                is_arrow: false,
                is_async,
                is_generator,
            };
            return Ok(ClassMember {
                key,
                kind,
                is_static,
                value: Some(ClassMemberValue::Func(func)),
            });
        }

        // Field: `key [= init] ;`
        let value = if self.eat_punct(Punct::Eq) {
            Some(ClassMemberValue::Expr(self.parse_assign(false)?))
        } else {
            None
        };
        self.semicolon()?;
        Ok(ClassMember {
            key,
            kind: ClassMemberKind::Field,
            is_static,
            value,
        })
    }

    // ---- expressions ------------------------------------------------------

    /// Full expression — allows the comma operator (a `Sequence`).
    fn parse_expression(&mut self) -> PResult<Expr> {
        let first = self.parse_assign(false)?;
        if !self.is_punct(Punct::Comma) {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_punct(Punct::Comma) {
            items.push(self.parse_assign(false)?);
        }
        Ok(Expr::Sequence(items))
    }

    /// Expression with the `in` operator disabled (for `for` headers).
    fn parse_expression_no_in(&mut self) -> PResult<Expr> {
        let first = self.parse_assign(true)?;
        if !self.is_punct(Punct::Comma) {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_punct(Punct::Comma) {
            items.push(self.parse_assign(true)?);
        }
        Ok(Expr::Sequence(items))
    }

    /// Assignment-level expression (no comma).
    fn parse_assign(&mut self, no_in: bool) -> PResult<Expr> {
        // Arrow function?
        if let Some(is_async) = self.arrow_ahead() {
            return self.parse_arrow(is_async);
        }
        // `yield` / `await` expressions.
        if self.is_kw("yield") {
            return self.parse_yield(no_in);
        }

        let left = self.parse_conditional(no_in)?;
        if let Some(op) = self.assign_op() {
            self.bump();
            let value = self.parse_assign(no_in)?;
            // Destructuring-assignment targets (array/object literals) are kept
            // as expression literals here and converted in the interpreter.
            return Ok(Expr::Assign {
                op,
                target: Box::new(left),
                value: Box::new(value),
            });
        }
        Ok(left)
    }

    fn assign_op(&self) -> Option<AssignOp> {
        let p = match &self.cur().tok {
            Tok::Punct(p) => *p,
            _ => return None,
        };
        Some(match p {
            Punct::Eq => AssignOp::Assign,
            Punct::PlusEq => AssignOp::Add,
            Punct::MinusEq => AssignOp::Sub,
            Punct::StarEq => AssignOp::Mul,
            Punct::SlashEq => AssignOp::Div,
            Punct::PercentEq => AssignOp::Mod,
            Punct::StarStarEq => AssignOp::Exp,
            Punct::ShlEq => AssignOp::Shl,
            Punct::ShrEq => AssignOp::Shr,
            Punct::UShrEq => AssignOp::UShr,
            Punct::AmpEq => AssignOp::BitAnd,
            Punct::PipeEq => AssignOp::BitOr,
            Punct::CaretEq => AssignOp::BitXor,
            Punct::AmpAmpEq => AssignOp::And,
            Punct::PipePipeEq => AssignOp::Or,
            Punct::NullishEq => AssignOp::Nullish,
            _ => return None,
        })
    }

    fn parse_yield(&mut self, no_in: bool) -> PResult<Expr> {
        self.bump(); // yield
        let delegate = self.eat_punct(Punct::Star);
        let arg = if self.cur().newline_before
            || self.is_punct(Punct::RParen)
            || self.is_punct(Punct::RBracket)
            || self.is_punct(Punct::RBrace)
            || self.is_punct(Punct::Comma)
            || self.is_punct(Punct::Semi)
            || self.is_punct(Punct::Colon)
            || self.at_eof()
        {
            None
        } else {
            Some(Box::new(self.parse_assign(no_in)?))
        };
        Ok(Expr::Yield { arg, delegate })
    }

    fn parse_conditional(&mut self, no_in: bool) -> PResult<Expr> {
        let test = self.parse_binary(0, no_in)?;
        if self.is_punct(Punct::Question) {
            self.bump();
            let cons = self.parse_assign(false)?; // `in` allowed inside `?:`
            self.expect_punct(Punct::Colon)?;
            let alt = self.parse_assign(no_in)?;
            return Ok(Expr::Conditional {
                test: Box::new(test),
                cons: Box::new(cons),
                alt: Box::new(alt),
            });
        }
        Ok(test)
    }

    /// Operator-precedence climbing for binary & logical operators.
    fn parse_binary(&mut self, min_bp: u8, no_in: bool) -> PResult<Expr> {
        let mut left = self.parse_unary()?;
        while let Some((kind, bp, right_assoc)) = self.peek_binop(no_in) {
            if bp < min_bp {
                break;
            }
            self.bump();
            let next_min = if right_assoc { bp } else { bp + 1 };
            let right = self.parse_binary(next_min, no_in)?;
            left = match kind {
                BinKind::B(op) => Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                BinKind::L(op) => Expr::Logical {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            };
        }
        Ok(left)
    }

    fn peek_binop(&self, no_in: bool) -> Option<(BinKind, u8, bool)> {
        match &self.cur().tok {
            Tok::Ident(s) if s == "instanceof" => Some((BinKind::B(BinOp::Instanceof), 10, false)),
            Tok::Ident(s) if s == "in" && !no_in => Some((BinKind::B(BinOp::In), 10, false)),
            Tok::Punct(p) => bin_op_for(*p),
            _ => None,
        }
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        // Prefix unary operators.
        let unop = match &self.cur().tok {
            Tok::Punct(Punct::Minus) => Some(UnOp::Minus),
            Tok::Punct(Punct::Plus) => Some(UnOp::Plus),
            Tok::Punct(Punct::Bang) => Some(UnOp::Not),
            Tok::Punct(Punct::Tilde) => Some(UnOp::BitNot),
            Tok::Ident(s) if s == "typeof" => Some(UnOp::Typeof),
            Tok::Ident(s) if s == "void" => Some(UnOp::Void),
            Tok::Ident(s) if s == "delete" => Some(UnOp::Delete),
            _ => None,
        };
        if let Some(op) = unop {
            self.bump();
            let arg = self.parse_unary()?;
            return Ok(Expr::Unary {
                op,
                arg: Box::new(arg),
            });
        }
        // Prefix increment / decrement.
        if self.is_punct(Punct::PlusPlus) || self.is_punct(Punct::MinusMinus) {
            let op = if self.is_punct(Punct::PlusPlus) {
                UpdateOp::Inc
            } else {
                UpdateOp::Dec
            };
            self.bump();
            let arg = self.parse_unary()?;
            return Ok(Expr::Update {
                op,
                prefix: true,
                arg: Box::new(arg),
            });
        }
        // `await`.
        if self.is_kw("await") && !self.nth_starts_no_expr() {
            self.bump();
            let arg = self.parse_unary()?;
            return Ok(Expr::Await(Box::new(arg)));
        }
        self.parse_postfix()
    }

    /// Heuristic: after `await`, does the next token clearly *not* begin an
    /// expression (so `await` is being used as a plain identifier)?
    fn nth_starts_no_expr(&self) -> bool {
        matches!(
            &self.nth(1).tok,
            Tok::Punct(
                Punct::RParen
                    | Punct::RBracket
                    | Punct::RBrace
                    | Punct::Semi
                    | Punct::Comma
                    | Punct::Colon
                    | Punct::Eq
                    | Punct::Dot
            ) | Tok::Eof
        )
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut e = self.parse_lhs_expr()?;
        if !self.cur().newline_before {
            if self.is_punct(Punct::PlusPlus) {
                self.bump();
                e = Expr::Update {
                    op: UpdateOp::Inc,
                    prefix: false,
                    arg: Box::new(e),
                };
            } else if self.is_punct(Punct::MinusMinus) {
                self.bump();
                e = Expr::Update {
                    op: UpdateOp::Dec,
                    prefix: false,
                    arg: Box::new(e),
                };
            }
        }
        Ok(e)
    }

    /// A left-hand-side expression: primary + member/call/optional-chain tail.
    fn parse_lhs_expr(&mut self) -> PResult<Expr> {
        let primary = self.parse_primary()?;
        self.parse_call_member_tail(primary)
    }

    fn parse_call_member_tail(&mut self, mut e: Expr) -> PResult<Expr> {
        loop {
            if self.eat_punct(Punct::Dot) {
                let name = self.member_name()?;
                e = Expr::Member {
                    object: Box::new(e),
                    property: Box::new(MemberProp::Ident(name)),
                    computed: false,
                    optional: false,
                };
            } else if self.eat_punct(Punct::OptChain) {
                if self.is_punct(Punct::LParen) {
                    let args = self.parse_arguments()?;
                    e = Expr::Call {
                        callee: Box::new(e),
                        args,
                        optional: true,
                    };
                } else if self.eat_punct(Punct::LBracket) {
                    let idx = self.parse_expression()?;
                    self.expect_punct(Punct::RBracket)?;
                    e = Expr::Member {
                        object: Box::new(e),
                        property: Box::new(MemberProp::Computed(idx)),
                        computed: true,
                        optional: true,
                    };
                } else {
                    let name = self.member_name()?;
                    e = Expr::Member {
                        object: Box::new(e),
                        property: Box::new(MemberProp::Ident(name)),
                        computed: false,
                        optional: true,
                    };
                }
            } else if self.eat_punct(Punct::LBracket) {
                let idx = self.parse_expression()?;
                self.expect_punct(Punct::RBracket)?;
                e = Expr::Member {
                    object: Box::new(e),
                    property: Box::new(MemberProp::Computed(idx)),
                    computed: true,
                    optional: false,
                };
            } else if self.is_punct(Punct::LParen) {
                let args = self.parse_arguments()?;
                e = Expr::Call {
                    callee: Box::new(e),
                    args,
                    optional: false,
                };
            } else if let Some((quasis, exprs)) = self.try_template_parts()? {
                e = Expr::TaggedTemplate {
                    tag: Box::new(e),
                    quasis,
                    exprs,
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    /// A member name after `.` — any identifier or keyword.
    fn member_name(&mut self) -> PResult<String> {
        if let Tok::Ident(s) = &self.cur().tok {
            let s = s.clone();
            self.bump();
            Ok(s)
        } else {
            Err(self.err("expected property name"))
        }
    }

    fn parse_arguments(&mut self) -> PResult<Vec<Expr>> {
        self.expect_punct(Punct::LParen)?;
        let mut args = Vec::new();
        while !self.is_punct(Punct::RParen) {
            if self.eat_punct(Punct::DotDotDot) {
                args.push(Expr::Spread(Box::new(self.parse_assign(false)?)));
            } else {
                args.push(self.parse_assign(false)?);
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(args)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        match &self.cur().tok {
            Tok::Num(n) => {
                let n = *n;
                self.bump();
                Ok(Expr::Num(n))
            }
            Tok::Str(s) => {
                let s = s.clone();
                self.bump();
                Ok(Expr::Str(s))
            }
            Tok::BigInt(s) => {
                let s = s.clone();
                self.bump();
                Ok(Expr::BigInt(s))
            }
            Tok::Regex { body, flags } => {
                let (body, flags) = (body.clone(), flags.clone());
                self.bump();
                Ok(Expr::Regex { body, flags })
            }
            Tok::TemplateNoSub(_) | Tok::TemplateHead(_) => {
                let (quasis, exprs) = self.parse_template_parts()?;
                Ok(Expr::Template { quasis, exprs })
            }
            Tok::Punct(Punct::LParen) => {
                self.bump();
                let e = self.parse_expression()?;
                self.expect_punct(Punct::RParen)?;
                Ok(e)
            }
            Tok::Punct(Punct::LBracket) => self.parse_array_literal(),
            Tok::Punct(Punct::LBrace) => self.parse_object_literal(),
            Tok::Ident(s) => {
                let s = s.clone();
                match s.as_str() {
                    "true" => {
                        self.bump();
                        Ok(Expr::Bool(true))
                    }
                    "false" => {
                        self.bump();
                        Ok(Expr::Bool(false))
                    }
                    "null" => {
                        self.bump();
                        Ok(Expr::Null)
                    }
                    "this" => {
                        self.bump();
                        Ok(Expr::This)
                    }
                    "super" => {
                        self.bump();
                        Ok(Expr::Super)
                    }
                    "function" => Ok(Expr::Func(Box::new(self.parse_function(false)?))),
                    "class" => Ok(Expr::Class(Box::new(self.parse_class(false)?))),
                    "new" => self.parse_new(),
                    "async" if self.nth_is_kw(1, "function") => {
                        Ok(Expr::Func(Box::new(self.parse_function(false)?)))
                    }
                    _ => {
                        self.bump();
                        Ok(Expr::Ident(s))
                    }
                }
            }
            _ => Err(self.err("expected expression")),
        }
    }

    fn parse_new(&mut self) -> PResult<Expr> {
        self.bump(); // new
        if self.is_punct(Punct::Dot) {
            self.bump();
            let _ = self.member_name()?; // target
            return Ok(Expr::Ident("new.target".into()));
        }
        // Member chain for the constructor, without consuming a call.
        let mut callee = self.parse_primary()?;
        loop {
            if self.eat_punct(Punct::Dot) {
                let name = self.member_name()?;
                callee = Expr::Member {
                    object: Box::new(callee),
                    property: Box::new(MemberProp::Ident(name)),
                    computed: false,
                    optional: false,
                };
            } else if self.eat_punct(Punct::LBracket) {
                let idx = self.parse_expression()?;
                self.expect_punct(Punct::RBracket)?;
                callee = Expr::Member {
                    object: Box::new(callee),
                    property: Box::new(MemberProp::Computed(idx)),
                    computed: true,
                    optional: false,
                };
            } else {
                break;
            }
        }
        let args = if self.is_punct(Punct::LParen) {
            self.parse_arguments()?
        } else {
            Vec::new()
        };
        Ok(Expr::New {
            callee: Box::new(callee),
            args,
        })
    }

    fn parse_array_literal(&mut self) -> PResult<Expr> {
        self.expect_punct(Punct::LBracket)?;
        let mut elems = Vec::new();
        while !self.is_punct(Punct::RBracket) {
            if self.is_punct(Punct::Comma) {
                elems.push(None);
                self.bump();
                continue;
            }
            if self.eat_punct(Punct::DotDotDot) {
                elems.push(Some(Expr::Spread(Box::new(self.parse_assign(false)?))));
            } else {
                elems.push(Some(self.parse_assign(false)?));
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBracket)?;
        Ok(Expr::Array(elems))
    }

    fn parse_object_literal(&mut self) -> PResult<Expr> {
        self.expect_punct(Punct::LBrace)?;
        let mut props = Vec::new();
        while !self.is_punct(Punct::RBrace) {
            props.push(self.parse_object_prop()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Expr::Object(props))
    }

    fn parse_object_prop(&mut self) -> PResult<Prop> {
        // Spread.
        if self.eat_punct(Punct::DotDotDot) {
            let e = self.parse_assign(false)?;
            return Ok(Prop {
                key: PropKey::Ident(String::new()),
                value: PropValue::Spread(e),
                kind: PropKind::Spread,
                computed: false,
            });
        }

        let mut is_async = false;
        let mut is_generator = false;
        let mut accessor: Option<PropKind> = None;

        if self.is_kw("async")
            && !self.nth_is_punct(1, Punct::Colon)
            && !self.nth_is_punct(1, Punct::LParen)
            && !self.nth_is_punct(1, Punct::Comma)
            && !self.nth_is_punct(1, Punct::RBrace)
        {
            is_async = true;
            self.bump();
        }
        if self.eat_punct(Punct::Star) {
            is_generator = true;
        }
        if (self.is_kw("get") || self.is_kw("set"))
            && !self.nth_is_punct(1, Punct::Colon)
            && !self.nth_is_punct(1, Punct::LParen)
            && !self.nth_is_punct(1, Punct::Comma)
            && !self.nth_is_punct(1, Punct::RBrace)
        {
            accessor = Some(if self.is_kw("get") {
                PropKind::Get
            } else {
                PropKind::Set
            });
            self.bump();
        }

        let computed = self.is_punct(Punct::LBracket);
        let (key, shorthand_name) = self.parse_prop_key()?;

        // Method or accessor.
        if self.is_punct(Punct::LParen) {
            let params = self.parse_param_list()?;
            let body = FuncBody::Block(self.parse_block()?);
            let func = Func {
                name: shorthand_name.clone(),
                params,
                body,
                is_arrow: false,
                is_async,
                is_generator,
            };
            let kind = accessor.unwrap_or(PropKind::Method);
            return Ok(Prop {
                key,
                value: PropValue::Expr(Expr::Func(Box::new(func))),
                kind,
                computed,
            });
        }

        // `key: value`.
        if self.eat_punct(Punct::Colon) {
            let value = self.parse_assign(false)?;
            return Ok(Prop {
                key,
                value: PropValue::Expr(value),
                kind: PropKind::Init,
                computed,
            });
        }

        // Shorthand, possibly with a destructuring default (`{a = 1}`).
        let name = shorthand_name.ok_or_else(|| self.err("expected `:` after property key"))?;
        let value = if self.eat_punct(Punct::Eq) {
            Expr::Assign {
                op: AssignOp::Assign,
                target: Box::new(Expr::Ident(name.clone())),
                value: Box::new(self.parse_assign(false)?),
            }
        } else {
            Expr::Ident(name)
        };
        Ok(Prop {
            key,
            value: PropValue::Expr(value),
            kind: PropKind::Init,
            computed,
        })
    }

    // ---- templates --------------------------------------------------------

    fn try_template_parts(&mut self) -> PResult<Option<(Vec<String>, Vec<Expr>)>> {
        if matches!(
            &self.cur().tok,
            Tok::TemplateNoSub(_) | Tok::TemplateHead(_)
        ) {
            Ok(Some(self.parse_template_parts()?))
        } else {
            Ok(None)
        }
    }

    fn parse_template_parts(&mut self) -> PResult<(Vec<String>, Vec<Expr>)> {
        let mut quasis = Vec::new();
        let mut exprs = Vec::new();
        match self.cur().tok.clone() {
            Tok::TemplateNoSub(s) => {
                self.bump();
                quasis.push(s);
                return Ok((quasis, exprs));
            }
            Tok::TemplateHead(s) => {
                self.bump();
                quasis.push(s);
            }
            _ => return Err(self.err("expected template literal")),
        }
        loop {
            exprs.push(self.parse_expression()?);
            match self.cur().tok.clone() {
                Tok::TemplateMiddle(s) => {
                    self.bump();
                    quasis.push(s);
                }
                Tok::TemplateTail(s) => {
                    self.bump();
                    quasis.push(s);
                    break;
                }
                _ => return Err(self.err("unterminated template literal")),
            }
        }
        Ok((quasis, exprs))
    }

    // ---- arrow functions --------------------------------------------------

    /// Look ahead (without consuming) to decide whether an arrow function
    /// begins here. Returns `Some(is_async)` if so.
    fn arrow_ahead(&self) -> Option<bool> {
        // `ident =>` or `async ident =>` / `async ( … ) =>` / `( … ) =>`.
        // Plain single-identifier arrow (covers `x =>` and `async =>`).
        if matches!(&self.cur().tok, Tok::Ident(s) if !super::token::is_reserved_word(s) || s == "yield" || s == "await")
            && self.nth_is_punct(1, Punct::Arrow)
        {
            return Some(false);
        }
        // Parenthesised params: `( … ) =>`.
        if self.is_punct(Punct::LParen) {
            if let Some(after) = self.matching_paren(self.pos) {
                if matches!(&self.toks[after].tok, Tok::Punct(Punct::Arrow)) {
                    return Some(false);
                }
            }
        }
        // `async`-prefixed forms (no newline between `async` and params).
        if self.is_kw("async") && !self.nth(1).newline_before {
            // `async ident =>`
            if matches!(&self.nth(1).tok, Tok::Ident(_)) && self.nth_is_punct(2, Punct::Arrow) {
                return Some(true);
            }
            // `async ( … ) =>`
            if self.nth_is_punct(1, Punct::LParen) {
                if let Some(after) = self.matching_paren(self.pos + 1) {
                    if matches!(&self.toks[after].tok, Tok::Punct(Punct::Arrow)) {
                        return Some(true);
                    }
                }
            }
        }
        None
    }

    /// Given the index of a `(`, return the index just past its matching `)`.
    fn matching_paren(&self, lparen: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut i = lparen;
        while i < self.toks.len() {
            match &self.toks[i].tok {
                Tok::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
                Tok::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                Tok::Eof => return None,
                _ => {}
            }
            i += 1;
        }
        None
    }

    fn parse_arrow(&mut self, is_async: bool) -> PResult<Expr> {
        if is_async {
            self.bump(); // async
        }
        let params = if self.is_punct(Punct::LParen) {
            self.parse_param_list()?
        } else {
            vec![Pattern::Ident(self.ident_name()?)]
        };
        self.expect_punct(Punct::Arrow)?;
        let body = if self.is_punct(Punct::LBrace) {
            FuncBody::Block(self.parse_block()?)
        } else {
            FuncBody::Expr(Box::new(self.parse_assign(false)?))
        };
        Ok(Expr::Arrow(Box::new(Func {
            name: None,
            params,
            body,
            is_arrow: true,
            is_async,
            is_generator: false,
        })))
    }
}

// ---- operator tables -------------------------------------------------------

/// Either a value-producing binary op or a short-circuiting logical op.
#[derive(Debug, Clone, Copy)]
enum BinKind {
    B(BinOp),
    L(LogicalOp),
}

/// Binding power (and right-associativity) for a punctuator binary operator.
fn bin_op_for(p: Punct) -> Option<(BinKind, u8, bool)> {
    use BinKind::*;
    Some(match p {
        Punct::NullishCoalesce => (L(LogicalOp::Nullish), 4, false),
        Punct::PipePipe => (L(LogicalOp::Or), 4, false),
        Punct::AmpAmp => (L(LogicalOp::And), 5, false),
        Punct::Pipe => (B(BinOp::BitOr), 6, false),
        Punct::Caret => (B(BinOp::BitXor), 7, false),
        Punct::Amp => (B(BinOp::BitAnd), 8, false),
        Punct::EqEq => (B(BinOp::Eq), 9, false),
        Punct::NotEq => (B(BinOp::Neq), 9, false),
        Punct::EqEqEq => (B(BinOp::StrictEq), 9, false),
        Punct::NotEqEq => (B(BinOp::StrictNeq), 9, false),
        Punct::Lt => (B(BinOp::Lt), 10, false),
        Punct::Gt => (B(BinOp::Gt), 10, false),
        Punct::LtEq => (B(BinOp::Le), 10, false),
        Punct::GtEq => (B(BinOp::Ge), 10, false),
        Punct::Shl => (B(BinOp::Shl), 11, false),
        Punct::Shr => (B(BinOp::Shr), 11, false),
        Punct::UShr => (B(BinOp::UShr), 11, false),
        Punct::Plus => (B(BinOp::Add), 12, false),
        Punct::Minus => (B(BinOp::Sub), 12, false),
        Punct::Star => (B(BinOp::Mul), 13, false),
        Punct::Slash => (B(BinOp::Div), 13, false),
        Punct::Percent => (B(BinOp::Mod), 13, false),
        Punct::StarStar => (B(BinOp::Exp), 14, true),
        _ => return None,
    })
}

fn punct_str(p: Punct) -> &'static str {
    use Punct::*;
    match p {
        LBrace => "{",
        RBrace => "}",
        LParen => "(",
        RParen => ")",
        LBracket => "[",
        RBracket => "]",
        Semi => ";",
        Comma => ",",
        Colon => ":",
        Arrow => "=>",
        Eq => "=",
        _ => "<punct>",
    }
}

// ---- expression → pattern conversion ---------------------------------------

/// Convert an expression literal used in a binding position (a `for-in/of`
/// target, or a destructuring assignment) into a [`Pattern`].
fn expr_to_pattern(e: Expr) -> PResult<Pattern> {
    Ok(match e {
        Expr::Ident(name) => Pattern::Ident(name),
        Expr::Member { .. } => Pattern::Member(Box::new(e)),
        Expr::Array(elems) => {
            let mut out = Vec::new();
            for el in elems {
                out.push(match el {
                    None => None,
                    Some(Expr::Spread(inner)) => {
                        Some(Pattern::Rest(Box::new(expr_to_pattern(*inner)?)))
                    }
                    Some(other) => Some(expr_to_pattern(other)?),
                });
            }
            Pattern::Array(out)
        }
        Expr::Object(props) => {
            let mut out = Vec::new();
            let mut rest = None;
            for p in props {
                match (p.kind, p.value) {
                    (PropKind::Spread, PropValue::Spread(inner)) => {
                        rest = Some(Box::new(expr_to_pattern(inner)?));
                    }
                    (PropKind::Init, PropValue::Expr(v)) => {
                        out.push(ObjectPatProp {
                            key: p.key,
                            value: expr_to_pattern(v)?,
                        });
                    }
                    _ => {
                        return Err(ParseError {
                            msg: "invalid destructuring target".into(),
                            line: 0,
                            col: 0,
                        })
                    }
                }
            }
            Pattern::Object { props: out, rest }
        }
        Expr::Assign {
            op: AssignOp::Assign,
            target,
            value,
        } => Pattern::Default {
            target: Box::new(expr_to_pattern(*target)?),
            default: value,
        },
        Expr::Spread(inner) => Pattern::Rest(Box::new(expr_to_pattern(*inner)?)),
        _ => {
            return Err(ParseError {
                msg: "invalid binding target".into(),
                line: 0,
                col: 0,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(src: &str) -> Program {
        parse(src).unwrap_or_else(|e| panic!("parse error: {e}"))
    }

    fn one_expr(src: &str) -> Expr {
        let p = prog(src);
        match p.body.into_iter().next().unwrap() {
            Stmt::Expr(e) => e,
            other => panic!("expected expression statement, got {other:?}"),
        }
    }

    #[test]
    fn precedence_mul_over_add() {
        let e = one_expr("1 + 2 * 3");
        match e {
            Expr::Binary {
                op: BinOp::Add,
                right,
                ..
            } => assert!(matches!(*right, Expr::Binary { op: BinOp::Mul, .. })),
            _ => panic!("bad tree: {e:?}"),
        }
    }

    #[test]
    fn exponent_is_right_associative() {
        let e = one_expr("2 ** 3 ** 2");
        match e {
            Expr::Binary {
                op: BinOp::Exp,
                right,
                ..
            } => assert!(matches!(*right, Expr::Binary { op: BinOp::Exp, .. })),
            _ => panic!("bad tree"),
        }
    }

    #[test]
    fn assignment_right_associative() {
        let e = one_expr("a = b = c");
        match e {
            Expr::Assign { value, .. } => assert!(matches!(*value, Expr::Assign { .. })),
            _ => panic!("bad tree"),
        }
    }

    #[test]
    fn member_and_call_chain() {
        let e = one_expr("a.b().c[d]");
        // Outer is computed member [d].
        assert!(matches!(e, Expr::Member { computed: true, .. }));
    }

    #[test]
    fn optional_chaining() {
        let e = one_expr("a?.b?.()");
        assert!(matches!(e, Expr::Call { optional: true, .. }));
    }

    #[test]
    fn arrow_single_param() {
        let e = one_expr("x => x + 1");
        match e {
            Expr::Arrow(f) => {
                assert_eq!(f.params.len(), 1);
                assert!(f.is_arrow);
                assert!(matches!(f.body, FuncBody::Expr(_)));
            }
            _ => panic!("not an arrow: {e:?}"),
        }
    }

    #[test]
    fn arrow_parenthesised_params() {
        let e = one_expr("(a, b = 2, ...rest) => { return a; }");
        match e {
            Expr::Arrow(f) => {
                assert_eq!(f.params.len(), 3);
                assert!(matches!(f.params[1], Pattern::Default { .. }));
                assert!(matches!(f.params[2], Pattern::Rest(_)));
            }
            _ => panic!("not an arrow"),
        }
    }

    #[test]
    fn async_arrow() {
        let e = one_expr("async (x) => x");
        match e {
            Expr::Arrow(f) => assert!(f.is_async && f.is_arrow),
            _ => panic!("not an async arrow"),
        }
    }

    #[test]
    fn parenthesised_is_not_arrow() {
        // `(a)` followed by `+` must be a parenthesised expression, not arrow.
        let e = one_expr("(a) + 1");
        assert!(matches!(e, Expr::Binary { op: BinOp::Add, .. }));
    }

    #[test]
    fn template_literal_structure() {
        let e = one_expr("`a${1}b${2}c`");
        match e {
            Expr::Template { quasis, exprs } => {
                assert_eq!(quasis, vec!["a", "b", "c"]);
                assert_eq!(exprs.len(), 2);
            }
            _ => panic!("not a template"),
        }
    }

    #[test]
    fn var_let_const_and_destructuring() {
        let p = prog("const {a, b: c = 1, ...r} = obj; let [x, , y] = arr;");
        assert_eq!(p.body.len(), 2);
        match &p.body[0] {
            Stmt::VarDecl {
                kind: VarKind::Const,
                decls,
            } => assert!(matches!(decls[0].id, Pattern::Object { .. })),
            _ => panic!("bad const decl"),
        }
        match &p.body[1] {
            Stmt::VarDecl {
                kind: VarKind::Let,
                decls,
            } => assert!(matches!(decls[0].id, Pattern::Array(_))),
            _ => panic!("bad let decl"),
        }
    }

    #[test]
    fn function_declaration() {
        let p = prog("function add(a, b) { return a + b; }");
        match &p.body[0] {
            Stmt::FuncDecl(f) => {
                assert_eq!(f.name.as_deref(), Some("add"));
                assert_eq!(f.params.len(), 2);
            }
            _ => panic!("not a function decl"),
        }
    }

    #[test]
    fn class_with_members() {
        let p = prog(
            "class P extends Q { constructor(x){ this.x = x; } get val(){ return this.x; } static make(){ return new P(0); } field = 1; }",
        );
        match &p.body[0] {
            Stmt::ClassDecl(c) => {
                assert_eq!(c.name.as_deref(), Some("P"));
                assert!(c.super_class.is_some());
                assert!(c
                    .members
                    .iter()
                    .any(|m| m.kind == ClassMemberKind::Constructor));
                assert!(c.members.iter().any(|m| m.kind == ClassMemberKind::Get));
                assert!(c.members.iter().any(|m| m.is_static));
                assert!(c.members.iter().any(|m| m.kind == ClassMemberKind::Field));
            }
            _ => panic!("not a class"),
        }
    }

    #[test]
    fn control_flow_statements() {
        let p = prog(
            "for (let i = 0; i < 10; i++) { if (i % 2) continue; } for (const x of xs) f(x); while (a) b(); switch (n) { case 1: break; default: g(); }",
        );
        assert!(matches!(p.body[0], Stmt::For { .. }));
        assert!(matches!(p.body[1], Stmt::ForOf { .. }));
        assert!(matches!(p.body[2], Stmt::While { .. }));
        assert!(matches!(p.body[3], Stmt::Switch { .. }));
    }

    #[test]
    fn asi_without_semicolons() {
        let p = prog("let a = 1\nlet b = 2\nreturn a + b");
        assert_eq!(p.body.len(), 3);
    }

    #[test]
    fn object_literal_methods_and_spread() {
        let e = one_expr("({ a: 1, b() {}, get c() { return 1; }, ...rest })");
        match e {
            Expr::Object(props) => {
                assert!(props.iter().any(|p| p.kind == PropKind::Method));
                assert!(props.iter().any(|p| p.kind == PropKind::Get));
                assert!(props.iter().any(|p| p.kind == PropKind::Spread));
            }
            _ => panic!("not an object literal: {e:?}"),
        }
    }

    #[test]
    fn regex_and_division_parse() {
        // Regex literal as a primary.
        let e = one_expr("/ab+/g.test(s)");
        assert!(matches!(e, Expr::Call { .. }));
        // Division.
        let e = one_expr("a / b");
        assert!(matches!(e, Expr::Binary { op: BinOp::Div, .. }));
    }

    #[test]
    fn try_catch_finally() {
        let p = prog("try { f(); } catch (e) { g(e); } finally { h(); }");
        match &p.body[0] {
            Stmt::Try {
                handler, finalizer, ..
            } => {
                assert!(handler.is_some());
                assert!(finalizer.is_some());
            }
            _ => panic!("not try"),
        }
    }
}
