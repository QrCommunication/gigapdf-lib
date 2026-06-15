//! The JavaScript abstract syntax tree.
//!
//! A pragmatic ES2015+ model: enough to represent real-world scripts (the kind
//! embedded in HTML templates) — expressions with full operator precedence,
//! all statement forms, functions/arrows/classes, destructuring patterns,
//! template literals and spread. `f64` literals make `PartialEq` awkward, so
//! the tree derives only `Debug` + `Clone`.

/// A complete program: a list of top-level statements.
#[derive(Debug, Clone)]
pub struct Program {
    /// Top-level statements (in source order).
    pub body: Vec<Stmt>,
}

// ---- statements ------------------------------------------------------------

/// A statement.
#[derive(Debug, Clone)]
pub enum Stmt {
    /// An expression evaluated for its side effects.
    Expr(Expr),
    /// A `{ … }` block with its own lexical scope.
    Block(Vec<Stmt>),
    /// A lone `;`.
    Empty,
    /// `var` / `let` / `const` declaration.
    VarDecl {
        /// Declaration kind.
        kind: VarKind,
        /// One or more declarators.
        decls: Vec<VarDeclarator>,
    },
    /// A hoisted `function` declaration.
    FuncDecl(Func),
    /// A `class` declaration.
    ClassDecl(Class),
    /// `return [expr];`
    Return(Option<Expr>),
    /// `if (test) cons [else alt]`
    If {
        /// Condition.
        test: Expr,
        /// Consequent branch.
        cons: Box<Stmt>,
        /// Optional `else` branch.
        alt: Option<Box<Stmt>>,
    },
    /// `for (init; test; update) body`
    For {
        /// Initializer (a declaration or an expression).
        init: Option<Box<ForInit>>,
        /// Loop condition.
        test: Option<Expr>,
        /// Per-iteration update.
        update: Option<Expr>,
        /// Loop body.
        body: Box<Stmt>,
    },
    /// `for (left in right) body`
    ForIn {
        /// Loop variable head.
        left: Box<ForHead>,
        /// Object being iterated.
        right: Expr,
        /// Loop body.
        body: Box<Stmt>,
    },
    /// `for (left of right) body`
    ForOf {
        /// Loop variable head.
        left: Box<ForHead>,
        /// Iterable being iterated.
        right: Expr,
        /// Loop body.
        body: Box<Stmt>,
    },
    /// `while (test) body`
    While {
        /// Condition.
        test: Expr,
        /// Body.
        body: Box<Stmt>,
    },
    /// `do body while (test)`
    DoWhile {
        /// Body.
        body: Box<Stmt>,
        /// Condition.
        test: Expr,
    },
    /// `switch (disc) { … }`
    Switch {
        /// Discriminant.
        disc: Expr,
        /// Cases (including an optional `default`).
        cases: Vec<SwitchCase>,
    },
    /// `break [label];`
    Break(Option<String>),
    /// `continue [label];`
    Continue(Option<String>),
    /// `throw expr;`
    Throw(Expr),
    /// `try { … } [catch (p) { … }] [finally { … }]`
    Try {
        /// Protected block.
        block: Vec<Stmt>,
        /// Optional catch clause.
        handler: Option<Catch>,
        /// Optional finally block.
        finalizer: Option<Vec<Stmt>>,
    },
    /// `label: body`
    Labeled {
        /// The label name.
        label: String,
        /// The labeled statement.
        body: Box<Stmt>,
    },
    /// `debugger;`
    Debugger,
}

/// A `var` / `let` / `const` keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// `var` (function-scoped).
    Var,
    /// `let` (block-scoped).
    Let,
    /// `const` (block-scoped, immutable binding).
    Const,
}

/// A single binding in a declaration: `pat [= init]`.
#[derive(Debug, Clone)]
pub struct VarDeclarator {
    /// The binding target (identifier or destructuring pattern).
    pub id: Pattern,
    /// The optional initializer.
    pub init: Option<Expr>,
}

/// The initializer clause of a C-style `for`.
#[derive(Debug, Clone)]
pub enum ForInit {
    /// A `var`/`let`/`const` declaration.
    VarDecl {
        /// Declaration kind.
        kind: VarKind,
        /// Declarators.
        decls: Vec<VarDeclarator>,
    },
    /// An expression.
    Expr(Expr),
}

/// The loop-variable head of a `for-in` / `for-of`.
#[derive(Debug, Clone)]
pub enum ForHead {
    /// A fresh binding: `for (let x of …)`.
    Decl {
        /// Declaration kind.
        kind: VarKind,
        /// The binding pattern.
        pat: Pattern,
    },
    /// An existing reference: `for (x of …)` / `for (a.b of …)`.
    Pattern(Pattern),
}

/// One `case`/`default` clause of a `switch`.
#[derive(Debug, Clone)]
pub struct SwitchCase {
    /// `Some(expr)` for `case expr:`, `None` for `default:`.
    pub test: Option<Expr>,
    /// The statements of this clause.
    pub body: Vec<Stmt>,
}

/// A `catch` clause.
#[derive(Debug, Clone)]
pub struct Catch {
    /// The bound parameter (`None` for the optional-catch-binding form).
    pub param: Option<Pattern>,
    /// The catch block.
    pub body: Vec<Stmt>,
}

// ---- functions & classes ---------------------------------------------------

/// A function / method / arrow.
#[derive(Debug, Clone)]
pub struct Func {
    /// Name (declarations and named expressions); `None` for anonymous.
    pub name: Option<String>,
    /// Formal parameters (patterns; may include defaults and a trailing rest).
    pub params: Vec<Pattern>,
    /// The body.
    pub body: FuncBody,
    /// `true` for arrow functions (lexical `this`).
    pub is_arrow: bool,
    /// `true` for `async` functions.
    pub is_async: bool,
    /// `true` for generator (`function*`) functions.
    pub is_generator: bool,
}

/// A function body.
#[derive(Debug, Clone)]
pub enum FuncBody {
    /// A `{ … }` statement block.
    Block(Vec<Stmt>),
    /// An arrow concise body: `x => expr`.
    Expr(Box<Expr>),
}

/// A `class` definition.
#[derive(Debug, Clone)]
pub struct Class {
    /// Class name (optional for class expressions).
    pub name: Option<String>,
    /// `extends` super-class expression.
    pub super_class: Option<Box<Expr>>,
    /// Members in source order.
    pub members: Vec<ClassMember>,
}

/// A member of a class body.
#[derive(Debug, Clone)]
pub struct ClassMember {
    /// The member key.
    pub key: PropKey,
    /// What kind of member this is.
    pub kind: ClassMemberKind,
    /// `static` member.
    pub is_static: bool,
    /// Method/accessor function, or field initializer expression.
    pub value: Option<ClassMemberValue>,
}

/// The function or value backing a class member.
#[derive(Debug, Clone)]
pub enum ClassMemberValue {
    /// A method / accessor / constructor function.
    Func(Func),
    /// A field initializer.
    Expr(Expr),
}

/// The role of a class member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassMemberKind {
    /// The `constructor`.
    Constructor,
    /// A normal method.
    Method,
    /// A getter.
    Get,
    /// A setter.
    Set,
    /// A class field (`x = …`).
    Field,
}

// ---- patterns --------------------------------------------------------------

/// A binding or assignment target.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// A simple identifier binding.
    Ident(String),
    /// An array destructuring pattern, with optional holes.
    Array(Vec<Option<Pattern>>),
    /// An object destructuring pattern.
    Object {
        /// The destructured properties.
        props: Vec<ObjectPatProp>,
        /// An optional `...rest` capturing the remaining own properties.
        rest: Option<Box<Pattern>>,
    },
    /// A pattern with a default: `pat = default`.
    Default {
        /// The inner pattern.
        target: Box<Pattern>,
        /// The default value.
        default: Box<Expr>,
    },
    /// A rest element: `...pat`.
    Rest(Box<Pattern>),
    /// A member-expression assignment target (`a.b`, `a[i]`) — only valid in
    /// assignment position, not as a binding.
    Member(Box<Expr>),
}

/// A property of an object destructuring pattern.
#[derive(Debug, Clone)]
pub struct ObjectPatProp {
    /// The source key.
    pub key: PropKey,
    /// The bound sub-pattern (for shorthand, an `Ident` matching the key).
    pub value: Pattern,
}

// ---- expressions -----------------------------------------------------------

/// An expression.
#[derive(Debug, Clone)]
pub enum Expr {
    /// Numeric literal.
    Num(f64),
    /// String literal.
    Str(String),
    /// Boolean literal.
    Bool(bool),
    /// `null`.
    Null,
    /// BigInt literal (decimal digit string).
    BigInt(String),
    /// A template literal: `` `a${x}b` `` → `quasis = ["a","b"]`, `exprs=[x]`.
    Template {
        /// The string chunks (always one more than `exprs`).
        quasis: Vec<String>,
        /// The interpolated expressions.
        exprs: Vec<Expr>,
    },
    /// A tagged template: `` tag`…` ``.
    TaggedTemplate {
        /// The tag expression.
        tag: Box<Expr>,
        /// The string chunks.
        quasis: Vec<String>,
        /// The interpolated expressions.
        exprs: Vec<Expr>,
    },
    /// A regular-expression literal.
    Regex {
        /// Pattern source.
        body: String,
        /// Flags.
        flags: String,
    },
    /// An identifier reference.
    Ident(String),
    /// `this`.
    This,
    /// `super` (only valid inside member/call within a method).
    Super,
    /// An array literal (elements may be `None` holes).
    Array(Vec<Option<Expr>>),
    /// An object literal.
    Object(Vec<Prop>),
    /// A function expression.
    Func(Box<Func>),
    /// An arrow function.
    Arrow(Box<Func>),
    /// A class expression.
    Class(Box<Class>),
    /// A unary operation (`-x`, `!x`, `typeof x`, `void x`, `delete x`).
    Unary {
        /// Operator.
        op: UnOp,
        /// Operand.
        arg: Box<Expr>,
    },
    /// A pre/post increment or decrement.
    Update {
        /// `++` or `--`.
        op: UpdateOp,
        /// `true` for prefix (`++x`), `false` for postfix (`x++`).
        prefix: bool,
        /// The (reference) operand.
        arg: Box<Expr>,
    },
    /// A binary operation.
    Binary {
        /// Operator.
        op: BinOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// A short-circuiting logical operation (`&&`, `||`, `??`).
    Logical {
        /// Operator.
        op: LogicalOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// An assignment.
    Assign {
        /// Operator (`=`, `+=`, …).
        op: AssignOp,
        /// The target (identifier, member, or destructuring literal).
        target: Box<Expr>,
        /// The assigned value.
        value: Box<Expr>,
    },
    /// A `test ? cons : alt` conditional.
    Conditional {
        /// Condition.
        test: Box<Expr>,
        /// Consequent.
        cons: Box<Expr>,
        /// Alternate.
        alt: Box<Expr>,
    },
    /// A function/method call.
    Call {
        /// The callee.
        callee: Box<Expr>,
        /// The arguments (a `Spread` arg expands in place).
        args: Vec<Expr>,
        /// `true` for an optional call `f?.()`.
        optional: bool,
    },
    /// A `new` expression.
    New {
        /// Constructor.
        callee: Box<Expr>,
        /// Arguments.
        args: Vec<Expr>,
    },
    /// A member access (`a.b` or `a[b]`).
    Member {
        /// The object.
        object: Box<Expr>,
        /// The property.
        property: Box<MemberProp>,
        /// `true` for computed access `a[b]`.
        computed: bool,
        /// `true` for optional access `a?.b`.
        optional: bool,
    },
    /// A `...spread` element (inside arrays, calls, `new`).
    Spread(Box<Expr>),
    /// A comma sequence (`a, b, c`) — evaluates to the last.
    Sequence(Vec<Expr>),
    /// `yield [*] [arg]` inside a generator.
    Yield {
        /// The yielded value.
        arg: Option<Box<Expr>>,
        /// `true` for `yield*`.
        delegate: bool,
    },
    /// `await expr` inside an async function.
    Await(Box<Expr>),
}

/// A property of an object literal.
#[derive(Debug, Clone)]
pub struct Prop {
    /// The key.
    pub key: PropKey,
    /// The value (for methods/accessors, an `Expr::Func`).
    pub value: PropValue,
    /// Whether this is a normal entry, accessor, method, or spread.
    pub kind: PropKind,
    /// `true` if a computed key `[expr]`.
    pub computed: bool,
}

/// The value side of an object-literal property.
#[derive(Debug, Clone)]
pub enum PropValue {
    /// A normal value expression.
    Expr(Expr),
    /// A `...spread` source (with `kind = Spread`).
    Spread(Expr),
    /// No explicit value (used internally before resolution); rare.
    None,
}

/// An object-literal / class member key.
#[derive(Debug, Clone)]
pub enum PropKey {
    /// An identifier or keyword used as a name.
    Ident(String),
    /// A string-literal key.
    Str(String),
    /// A numeric-literal key.
    Num(f64),
    /// A computed key `[expr]`.
    Computed(Box<Expr>),
}

/// The kind of an object-literal property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    /// `key: value` (or shorthand `key`).
    Init,
    /// A `get` accessor.
    Get,
    /// A `set` accessor.
    Set,
    /// A method `key() { … }`.
    Method,
    /// A `...spread`.
    Spread,
}

/// The property part of a member access.
#[derive(Debug, Clone)]
pub enum MemberProp {
    /// `.name`.
    Ident(String),
    /// `[expr]`.
    Computed(Expr),
}

// ---- operators -------------------------------------------------------------

/// A unary prefix operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// `-`
    Minus,
    /// `+`
    Plus,
    /// `!`
    Not,
    /// `~`
    BitNot,
    /// `typeof`
    Typeof,
    /// `void`
    Void,
    /// `delete`
    Delete,
}

/// An increment/decrement operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    /// `++`
    Inc,
    /// `--`
    Dec,
}

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `**`
    Exp,
    /// `==`
    Eq,
    /// `!=`
    Neq,
    /// `===`
    StrictEq,
    /// `!==`
    StrictNeq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `>>>`
    UShr,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// `in`
    In,
    /// `instanceof`
    Instanceof,
}

/// A short-circuiting logical operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    /// `&&`
    And,
    /// `||`
    Or,
    /// `??`
    Nullish,
}

/// An assignment operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`
    Assign,
    /// `+=`
    Add,
    /// `-=`
    Sub,
    /// `*=`
    Mul,
    /// `/=`
    Div,
    /// `%=`
    Mod,
    /// `**=`
    Exp,
    /// `<<=`
    Shl,
    /// `>>=`
    Shr,
    /// `>>>=`
    UShr,
    /// `&=`
    BitAnd,
    /// `|=`
    BitOr,
    /// `^=`
    BitXor,
    /// `&&=`
    And,
    /// `||=`
    Or,
    /// `??=`
    Nullish,
}
