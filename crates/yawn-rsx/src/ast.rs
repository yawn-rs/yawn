use std::ops::Range;

pub type Span = Range<usize>;

#[derive(Debug, Clone)]
pub struct File {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub enum Item {
    /// Plain Rust (imports, helper functions, structs...), copied unchanged
    Rust(Span),
    Component(Component),
    ServerFn(ServerFn),
}

/// The HTTP method of a server function, from its attribute: `#[get]`, `#[post]`...
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    /// The method an attribute name stands for
    pub fn from_attr(name: &str) -> Option<Method> {
        Some(match name {
            "get" => Method::Get,
            "post" => Method::Post,
            "put" => Method::Put,
            "patch" => Method::Patch,
            "delete" => Method::Delete,
            _ => return None,
        })
    }

    /// The variant of `yawn::fns::Method` the generated code names
    pub fn variant(self) -> &'static str {
        match self {
            Method::Get => "Get",
            Method::Post => "Post",
            Method::Put => "Put",
            Method::Patch => "Patch",
            Method::Delete => "Delete",
        }
    }
}

/// How a server function's arguments and answer are written on the wire, from
/// `#[post(encoding = json)]`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// `yawn::bits`: positional, small, bounded; the types derive `Bits`
    #[default]
    Bits,
    /// `serde` types as JSON
    Json,
    /// `serde` types as postcard
    Postcard,
    /// `serde` types in a self-describing binary format
    Dynamic,
}

impl Encoding {
    pub fn from_name(name: &str) -> Option<Encoding> {
        Some(match name {
            "bits" => Encoding::Bits,
            "json" => Encoding::Json,
            "postcard" => Encoding::Postcard,
            "dynamic" => Encoding::Dynamic,
            _ => return None,
        })
    }

    /// The marker type in `yawn::fns::codec` the generated code names
    pub fn marker(self) -> &'static str {
        match self {
            Encoding::Bits => "Bits",
            Encoding::Json => "Json",
            Encoding::Postcard => "Postcard",
            Encoding::Dynamic => "Dynamic",
        }
    }
}

/// `#[post] fn name(params) -> Type { body }`: runs on the server, the client calls it over HTTP
#[derive(Debug, Clone)]
pub struct ServerFn {
    pub method: Method,
    pub encoding: Encoding,
    /// `name: Type` pairs of `#[post(name: Type)]`: taken from the request on the server, not passed by
    /// the caller
    pub extractors: Vec<Param>,
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    /// The type after `->`
    pub ret: Option<Span>,
    /// The body, braces included
    pub body: Span,
}

#[derive(Debug, Clone)]
pub struct Component {
    pub name: String,
    pub name_span: Span,
    /// `<T: Bound, 'a>` as written, if the component is generic
    pub generics: Option<Span>,
    /// What follows the name in an impl: `<T, 'a>`, empty when the component isn't generic
    pub generic_args: String,
    /// `where T: Bound` between the props and the body
    pub where_clause: Option<Span>,
    pub params: Vec<Param>,
    /// Rust statements that run once when the component is created
    pub setup: Span,
    pub view: Vec<Node>,
    /// Inner range of the `style { ... }` block (plain CSS, global), if any
    pub style: Option<Span>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Span,
    /// The whole parameter as written, `mut` included
    pub span: Span,
    /// `name: Type` as written, without `mut`: copied into the generated field so that hover and
    /// go-to-definition on the name map back to the source
    pub decl: Span,
    /// `= expr` after the type: the value when the caller leaves the prop out
    pub default: Option<Span>,
}

#[derive(Debug, Clone)]
pub enum Node {
    Element(Element),
    Text(Text),
    /// `{expr}`; the range excludes the braces
    Expr(Span),
    /// `<slot />` places the component's `children`; `<slot name="x" />` places its `x` prop (also
    /// a `Children`), which the caller fills with `slot="x"` on a child or `<template slot="x">`
    Slot(Span, Option<String>),
    If(If),
    For(For),
}

#[derive(Debug, Clone)]
pub struct Element {
    pub name: String,
    pub name_span: Span,
    pub is_component: bool,
    pub attrs: Vec<Attr>,
    pub children: Vec<Node>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub name: String,
    pub name_span: Span,
    pub value: AttrValue,
}

#[derive(Debug, Clone)]
pub enum AttrValue {
    /// `disabled`
    Flag,
    /// `class="a {b}"`
    Text(Text),
    /// `value={expr}`
    Expr(Span),
}

/// A string literal, possibly with `{expr}` interpolations
#[derive(Debug, Clone)]
pub struct Text {
    /// Range of the whole literal including quotes
    pub span: Span,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone)]
pub enum Part {
    /// Literal text exactly as written between the quotes (escapes included, `{{` kept)
    Lit(Span),
    /// `{expr}` or `{expr:spec}`
    Interp { expr: Span, spec: Option<Span> },
}

impl Text {
    pub fn is_static(&self) -> bool {
        self.parts.iter().all(|p| matches!(p, Part::Lit(_)))
    }
}

#[derive(Debug, Clone)]
pub struct If {
    pub span: Span,
    pub cond: Span,
    /// For `if let PAT = EXPR`: the pattern and the scrutinee, both inside `cond`
    pub binding: Option<(Span, Span)>,
    pub then: Vec<Node>,
    /// Either the `else` block, or a single nested `If` for `else if`
    pub otherwise: Option<Vec<Node>>,
}

#[derive(Debug, Clone)]
pub struct For {
    pub span: Span,
    pub pat: Span,
    pub iter: Span,
    pub key: Span,
    pub body: Vec<Node>,
}
