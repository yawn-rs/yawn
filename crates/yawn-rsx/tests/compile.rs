use yawn_rsx::{Diagnostic, compile};

fn ok(src: &str) -> String {
    match compile(src, "t.rsx") {
        Ok(c) => c.rust,
        Err(d) => panic!("{}", d.render("t.rsx", src)),
    }
}

fn err(src: &str) -> (Diagnostic, String) {
    match compile(src, "t.rsx") {
        Ok(c) => panic!("expected an error, got:\n{}", c.rust),
        Err(d) => {
            let text = d.render("t.rsx", src);
            (d, text)
        }
    }
}

fn pointed<'a>(src: &'a str, d: &Diagnostic) -> &'a str {
    &src[d.span.clone()]
}

#[test]
fn component_becomes_struct_and_impl() {
    let out = ok("component Hello(name: String, n: Sig<i32>) {\n    view { <p>\"hi\"</p> }\n}\n");
    assert!(out.contains("pub struct Hello {"));
    assert!(out.contains("pub name: String,"));
    assert!(out.contains("pub n: Sig<i32>,"));
    assert!(out.contains("impl ::yawn::ui::Component for Hello"));
    assert!(out.contains("let Self { name, n } = self;"));
    assert!(out.contains("__rt::template(\"<p>hi</p>\")"), "{out}");
}

#[test]
fn component_without_props_derives_default() {
    let out = ok("component A() { view { <b /> } }");
    assert!(out.contains("#[derive(Default)]\npub struct A {"));
}

#[test]
fn plain_rust_is_copied_unchanged() {
    let src = "use std::fmt;\n\nfn helper() -> u32 { 1 }\n\ncomponent A() { view { <b /> } }\n\nfn after() {}\n";
    let out = ok(src);
    assert!(out.contains("use std::fmt;\n\nfn helper() -> u32 { 1 }\n"));
    assert!(out.contains("fn after() {}"));
}

#[test]
fn pub_component_is_accepted() {
    let out = ok("pub component A() { view { <b /> } }");
    assert_eq!(out.matches("pub pub").count(), 0);
    assert!(out.contains("pub struct A"));
}

#[test]
fn static_and_dynamic_text() {
    let out = ok(
        "component A() { let n = signal(1); view { <p>\"plain {{braces}}\"</p> <p>\"n = {n}, {n:>4}\"</p> } }",
    );
    assert!(out.contains("__rt::text_lit(\"plain {braces}\")"));
    assert!(out.contains("__rt::text_fmt(move || ::std::format!(\"n = {}, {:>4}\", (n), (n)))"));
}

#[test]
fn attributes_events_and_directives() {
    let out = ok(r#"component A() {
    let n = signal(0);
    view {
        <button class="a b" disabled id="x-{n}" title={n.get().to_string()} class:on={n.get() > 0}
                prop:value={n.get()} on:click={move || n.set(1)} data-k="v">"x"</button>
    }
}"#);
    // the literal attributes and the text are the markup of one template; the root is the only place
    assert!(out.contains("__rt::template_holes(\"<button class=\\\"a b\\\" disabled=\\\"\\\" data-k=\\\"v\\\">x</button>\", &[&[]])"), "{out}");
    assert!(
        out.contains("__rt::bind_attr(__e2, \"id\", move || ::std::format!(\"x-{}\", (n)));"),
        "{out}"
    );
    assert!(out.contains("__rt::bind_attr(__e2, \"title\", move || (n.get().to_string()));"));
    assert!(out.contains("__rt::bind_class(__e2, \"on\", move || (n.get() > 0));"));
    assert!(out.contains("__rt::bind_prop(__e2, \"value\", move || (n.get()));"));
    assert!(out.contains("__rt::listen(__e2, \"click\", (move || n.set(1)));"));
}

#[test]
fn void_elements_need_no_closing_tag() {
    let out = ok("component A() { view { <input type=\"text\"> <br> <hr /> } }");
    assert_eq!(out.matches("__rt::template(").count(), 3);
    assert!(out.contains("__rt::template(\"<input type=\\\"text\\\">\")"), "{out}");
    assert!(
        out.contains("__rt::template(\"<br>\")") && out.contains("__rt::template(\"<hr>\")"),
        "{out}"
    );
}

#[test]
fn a_subtree_of_literals_is_one_template() {
    let out = ok(
        "component A() { view { <div class=\"a b\" data-x=\"1 & 2\"><p>\"x < y\"</p><br><input disabled></div> } }",
    );
    assert!(out.contains("__rt::template(\"<div class=\\\"a b\\\" data-x=\\\"1 &amp; 2\\\"><p>x &lt; y</p><br><input disabled=\\\"\\\"></div>\")"), "{out}");
    assert_eq!(out.matches("__rt::element(").count(), 0);
}

#[test]
fn the_static_parts_around_dynamic_ones_are_templates_of_their_own() {
    let out = ok(
        "component A(n: Sig<i32>) { view { <div class=\"row\"><span class=\"c\"><b>\"fixed\"</b></span><i>{n.get()}</i></div> } }",
    );
    // one template for the whole element: the `<i>` that takes the dynamic text is a place in it
    assert!(out.contains("__rt::template_holes(\"<div class=\\\"row\\\"><span class=\\\"c\\\"><b>fixed</b></span><i></i></div>\", &[&[], &[1u8]])"), "{out}");
    assert_eq!(out.matches("__rt::element(").count(), 0, "{out}");
}

#[test]
fn what_the_html_parser_would_rewrite_stays_node_by_node() {
    // (source, markup that must not be written into any template). The outer element is built
    // node by node; its children may still be templates, which is fine: they're separate trees.
    for (src, forbidden) in [
        ("<table><tr><td>\"x\"</td></tr></table>", "<table"),
        ("<p><div>\"x\"</div></p>", "<p><div>"),
        ("<a href=\"#\"><a href=\"#\">\"x\"</a></a>", "<a href=\\\"#\\\"><a"),
        ("<button><button>\"x\"</button></button>", "<button><button>"),
        ("<ul><li><li>\"x\"</li></li></ul>", "<li><li>"),
        ("<textarea>\"x\"</textarea>", "<textarea"),
        ("<img src=\"/assets/logo.png\">", "<img"),
        ("<span>\"\"</span>", "<span></span>"),
        ("<span class=\"a\\nb\"></span>", "<span class"),
        ("<svg><div>\"x\"</div></svg>", "<svg><div>"),
    ] {
        let out = ok(&format!("component A() {{ view {{ {src} }} }}"));
        assert!(!out.contains(forbidden), "{src}: `{forbidden}` must not be markup:\n{out}");
    }
}

#[test]
fn components_get_props_and_children() {
    let out = ok(
        "component A() { view { <Card title=\"t\" n={1 + 2} flag><p>\"x\"</p></Card> <Other /> } }",
    );
    assert!(out.contains("::yawn::ui::Component::render(Card {"));
    assert!(out.contains("title: ::core::convert::Into::into(\"t\"),"));
    assert!(out.contains("n: (1 + 2),"));
    assert!(out.contains("flag: true,"));
    assert!(out.contains("children: ::yawn::ui::Children::new(move || {"));
    assert!(out.contains("Component::render(Other {"));
}

#[test]
fn loop_variables_are_cloned_per_closure() {
    let out = ok(
        "component A() { let l = signal(vec![1]); view { for x in l.get() key *x { <button on:click={move || drop(x)}>\"{x}\"</button> } } }",
    );
    // Every closure that uses `x` gets its own copy, so none of them moves it away from the others.
    assert_eq!(out.matches("let x = ::core::clone::Clone::clone(&x);").count(), 2, "{out}");
    assert!(out.contains("__rt::listen(__e3, \"click\", (move || drop(x)))"), "{out}");
}

#[test]
fn if_else_if_else() {
    let out = ok(
        "component A() { let n = signal(0); view { if n.get() == 0 { <b>\"0\"</b> } else if n.get() == 1 { <b>\"1\"</b> } else { <b>\"2\"</b> } } }",
    );
    assert_eq!(out.matches("__rt::show(").count(), 2);
}

#[test]
fn comments_are_allowed_in_views() {
    ok("component A() { view { // line\n <b /> /* block */ } }");
}

#[test]
fn rust_in_strings_and_chars_does_not_confuse_the_scanner() {
    ok(
        "fn f() -> &'static str { \"component X() {\" }\nfn g() { let _c = '{'; let _l: &'static str = \"}\"; }\ncomponent A() { let s = \"view {\"; view { <b>\"{s}\"</b> } }",
    );
}

#[test]
fn block_expressions_with_statements() {
    let out = ok("component A() { let n = signal(1); view { <p>{ let v = n.get(); v + 1 }</p> } }");
    assert!(out.contains("move || {"), "{out}");
}

#[test]
fn error_mismatched_closing_tag() {
    let src =
        "component A() {\n    view {\n        <div>\n            <span>\"x\"</div>\n    }\n}\n";
    let (d, text) = err(src);
    assert_eq!(d.message, "mismatched closing tag");
    assert_eq!(pointed(src, &d), "</div>");
    assert!(text.contains("t.rsx:4:22"), "{text}");
    assert!(text.contains("expected `</span>`, found `</div>`"));
    assert!(text.contains("`<span>` opened here"));
}

#[test]
fn error_unclosed_tag() {
    let src = "component A() {\n    view {\n        <div>\n            \"x\"\n    }\n}\n";
    let (d, _) = err(src);
    assert!(d.message.contains("unclosed tag `<div>`"), "{}", d.message);
    assert_eq!(pointed(src, &d), "<div");
    assert_eq!(d.position(src), (3, 9));
}

#[test]
fn error_unquoted_text() {
    let src = "component A() { view { <p>hello world</p> } }";
    let (d, text) = err(src);
    assert!(d.message.contains("quoted string"));
    assert!(pointed(src, &d).starts_with("hello"));
    assert!(text.contains("help: write it as \"text\""));
}

#[test]
fn error_unterminated_string() {
    let src = "component A() { view { <p>\"oops</p> } }";
    let (d, _) = err(src);
    assert!(
        d.message.contains("unterminated string") || d.message.contains("unclosed"),
        "{}",
        d.message
    );
}

#[test]
fn error_unclosed_brace_in_expression() {
    let src = "component A() { view { <p>{foo(</p> } }";
    let (d, _) = err(src);
    assert!(
        d.message.contains("unclosed `(`") || d.message.contains("mismatched"),
        "{}",
        d.message
    );
}

#[test]
fn error_invalid_rust_expression_points_inside_the_braces() {
    let src = "component A() {\n    let n = signal(0);\n    view { <p>{n.get() +}</p> }\n}\n";
    let (d, text) = err(src);
    assert!(d.message.contains("expected"), "{}", d.message);
    assert_eq!(d.position(src).0, 3, "{text}");
}

#[test]
fn error_invalid_rust_in_setup() {
    let src = "component A() {\n    let = 5;\n    view { <p /> }\n}\n";
    let (d, _) = err(src);
    assert_eq!(d.position(src).0, 2);
}

#[test]
fn error_missing_view() {
    let (d, _) = err("component A() { let x = 1; }");
    assert!(d.message.contains("no `view"), "{}", d.message);
}

#[test]
fn error_for_without_key() {
    let src = "component A() { view { for x in 0..3 { <b /> } } }";
    let (d, text) = err(src);
    assert!(d.message.contains("needs a `key`"), "{}", d.message);
    assert!(text.contains("key todo.id"));
}

#[test]
fn if_let_becomes_a_switch_on_the_value() {
    let out = ok(
        "component A() { view { if let Some(x) = Some(1u8) { <b>\"{x}\"</b> } else { <i /> } } }",
    );
    assert!(out.contains("switch("), "{out}");
    assert!(out.contains("if let Some(x) ="), "{out}");
}

#[test]
fn error_if_let_without_a_value() {
    let (d, _) = err("component A() { view { if let Some(x) { <b /> } } }");
    assert!(d.message.contains("`if let`"), "{}", d.message);
}

#[test]
fn error_unsupported_match() {
    let (d, _) = err("component A() { view { match 1 { _ => {} } } }");
    assert!(d.message.contains("`match` is not supported"));
}

#[test]
fn error_bad_attribute_value() {
    let src = "component A() { view { <p id=5 /> } }";
    let (d, _) = err(src);
    assert!(d.message.contains("must be a \"string\" or a {expression}"), "{}", d.message);
    assert_eq!(pointed(src, &d), "5");
}

#[test]
fn error_unknown_directive() {
    let (d, _) = err("component A() { view { <p foo:bar={1} /> } }");
    assert!(d.message.contains("unknown directive"));
}

#[test]
fn error_directive_on_component() {
    let (d, _) = err("component A() { view { <Card on:click={|| ()} /> } }");
    assert!(d.message.contains("not a prop name"));
}

#[test]
fn error_stray_closing_tag() {
    let (d, _) = err("component A() { view { </b> } }");
    assert!(d.message.contains("unexpected closing tag"));
}

#[test]
fn error_unmatched_brace_in_string() {
    let (d, text) = err("component A() { view { <p>\"a } b\"</p> } }");
    assert!(d.message.contains("unmatched `}`"));
    assert!(text.contains("`}}`"));
}

#[test]
fn error_bad_component_params() {
    let (d, _) = err("component A(x) { view { <b /> } }");
    assert!(!d.message.is_empty());
}

#[test]
fn error_empty_expression() {
    let (d, _) = err("component A() { view { <p>{}</p> } }");
    assert!(d.message.contains("empty expression"));
}

#[test]
fn source_map_points_generated_expressions_back_to_the_rsx_file() {
    let src = "component A() {\n    let n = signal(0);\n    view {\n        <p>\"v = {n.get() + 1}\"</p>\n    }\n}\n";
    let c = compile(src, "t.rsx").unwrap();
    // Find the generated line holding the interpolated expression and a column inside it.
    for (i, line) in c.rust.lines().enumerate() {
        if let Some(col) = line.find("n.get() + 1") {
            let (l, cc) = c.map.map(i as u32 + 1, col as u32 + 1).unwrap();
            assert_eq!(l, 4);
            let want = src.lines().nth(3).unwrap().find("n.get() + 1").unwrap() as u32 + 1;
            assert_eq!(cc, want);
            return;
        }
    }
    panic!("expression not found in\n{}", c.rust);
}

#[test]
fn source_map_setup_code_keeps_lines_and_columns() {
    let src = "component A() {\n    let total = 1 + 2;\n    view { <b /> }\n}\n";
    let c = compile(src, "t.rsx").unwrap();
    for (i, line) in c.rust.lines().enumerate() {
        if let Some(col) = line.find("let total") {
            let (l, cc) = c.map.map(i as u32 + 1, col as u32 + 1).unwrap();
            assert_eq!((l, cc), (2, 5));
            return;
        }
    }
    panic!("setup code missing");
}

#[test]
fn source_map_covers_parameter_names_and_types() {
    // hover on `class` (the name, not only its type) must reach the generated field
    let src = "component Wave(mut class: &'static str, n: u32) {\n    view { <b /> }\n}\n";
    let c = compile(src, "t.rsx").unwrap();
    assert!(c.rust.contains("pub class: &'static str,"), "{}", c.rust);
    for (name, want_col) in [
        ("class", src.find("class").unwrap() as u32 + 1),
        ("n: u32", src.find("n: u32").unwrap() as u32 + 1),
    ] {
        let (i, col) = c
            .rust
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find(&format!("pub {name}")).map(|c| (i, c + 4)))
            .unwrap();
        let (l, cc) = c.map.map(i as u32 + 1, col as u32 + 1).unwrap();
        assert_eq!((l, cc), (1, want_col), "{name}");
    }
}

#[test]
fn source_map_roundtrips_as_text() {
    let c = compile("component A() { view { <b /> } }", "src/a.rsx").unwrap();
    let text = c.map.to_text();
    assert_eq!(yawn_rsx::SourceMap::parse(&text).unwrap(), c.map);
}

#[test]
fn style_block_is_collected_and_not_part_of_setup() {
    let src = "component A() {\n    let n = 1;\n    view { <b class=\"x\" /> }\n    style {\n        .x { color: red; }\n    }\n}\ncomponent B() {\n    style { p { margin: 0 } }\n    view { <p /> }\n}\n";
    let c = compile(src, "t.rsx").unwrap();
    assert!(c.css.contains("/* t.rsx: A */\n.x { color: red; }"), "{}", c.css);
    assert!(c.css.contains("/* t.rsx: B */\np { margin: 0 }"));
    assert!(!c.rust.contains("color: red"));
    assert!(c.rust.contains("let n = 1;"));
}

#[test]
fn generic_component() {
    let src = "component List<T: Clone, F: Fn(T) -> View>(items: Vec<T>, row: F, sep: String = \", \".into()) { view { <b /> } }";
    let out = ok(src);
    assert!(out.contains("pub struct List<T: Clone, F: Fn(T) -> View> {"), "{out}");
    assert!(out.contains("impl<T: Clone, F: Fn(T) -> View> List<T, F> {"), "{out}");
    assert!(
        out.contains("impl<T: Clone, F: Fn(T) -> View> ::yawn::ui::Component for List<T, F> {"),
        "{out}"
    );
    let h = &yawn_rsx::parser::headers(src)[0];
    assert_eq!((h.name.as_str(), h.defaults.as_slice()), ("List", ["sep".to_string()].as_slice()));
    assert_eq!(h.params.len(), 3, "the parentheses of `Fn(T)` are not the props");
}

#[test]
fn generic_component_errors() {
    let src = "component A<T = u8>(x: T) { view { <b /> } }";
    let (d, _) = err(src);
    assert!(d.message.contains("has no default"), "{}", d.message);
    assert_eq!(pointed(src, &d), "=");
    let src = "component A<T U>(x: T) { view { <b /> } }";
    let (d, _) = err(src);
    assert_eq!(pointed(src, &d), "U", "{}", d.message);
    let (d, _) = err("component A<T(x: T) { view { <b /> } }");
    assert_eq!(d.message, "unclosed `<`");
    for (src, name) in [
        ("component A<T>() { view { <b /> } }", "T"),
        ("component A<'a, T>(x: T) { view { <b /> } }", "'a"),
        // a bound of another parameter decides nothing: the struct's fields have to hold `T`
        ("component A<T, F: Fn(T)>(f: F) { view { <b /> } }", "T"),
    ] {
        let (d, _) = err(src);
        assert_eq!(d.message, format!("`{name}` isn't in the type of any prop"));
        assert_eq!(pointed(src, &d), name);
    }
    let src = "component A<T>(x: T) where T: { view { <b /> } }";
    assert!(ok(src).contains("A<T> where T:\n {"), "an empty bound list is valid Rust");
    let src = "component A<T>(x: T) where T Clone { view { <b /> } }";
    let (d, _) = err(src);
    assert_eq!(pointed(src, &d), "Clone", "{}", d.message);
}

#[test]
fn generic_component_with_where() {
    let src = "component A<'a, T, F>(x: &'a T, f: F)\nwhere\n    F: Fn(&T) -> String + 'static, // f\n{\n    view { <b /> }\n}";
    let out = ok(src);
    assert!(
        out.contains("pub struct A<'a, T, F> where\n    F: Fn(&T) -> String + 'static, // f\n {"),
        "{out}"
    );
    assert!(out.contains("impl<'a, T, F> ::yawn::ui::Component for A<'a, T, F> where\n"), "{out}");
}

#[test]
fn props_may_have_defaults() {
    let out = ok(
        "component Tag(label: String, tone: u8 = 3, items: Vec<u8> = vec![1, 2], note: Option<String> = None) { view { <b /> } }",
    );
    assert!(out.contains("pub tone: u8,"), "{out}");
    assert!(
        out.contains("pub fn __default_tone(_: ::core::marker::PhantomData<Self>) -> u8 { 3 }"),
        "{out}"
    );
    assert!(
        out.contains(
            "pub fn __default_items(_: ::core::marker::PhantomData<Self>) -> Vec<u8> { vec![1, 2] }"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "pub fn __default_note(_: ::core::marker::PhantomData<Self>) -> Option<String> { None }"
        ),
        "{out}"
    );
    assert!(!out.contains("__default_label"), "{out}");
}

#[test]
fn a_prop_left_out_takes_its_default_when_the_component_is_known() {
    let mut known = yawn_rsx::codegen::Registry::new();
    known.insert("Tag".into(), vec!["tone".into(), "note".into()]);
    let src = "component Page() { view { <Tag label=\"a\" tone={5} /> } }";
    let out = yawn_rsx::compile_with(src, "t.rsx", &known).unwrap().rust;
    assert!(out.contains("tone: (5),"), "{out}");
    assert!(out.contains("note: Tag::__default_note(__d"), "{out}");
    assert!(!out.contains("Tag::__default_tone"), "a prop that was given is not filled in: {out}");
}

#[test]
fn a_prop_filled_by_a_slot_is_not_defaulted_as_well() {
    let mut known = yawn_rsx::codegen::Registry::new();
    known.insert("Box".into(), vec!["action".into()]);
    let src = "component Page() { view { <Box><template slot=\"action\"><b /></template></Box> <Box /> } }";
    let out = yawn_rsx::compile_with(src, "t.rsx", &known).unwrap().rust;
    assert_eq!(
        out.matches("action: ").count() - out.matches("action: Box::__default_action(").count(),
        1,
        "a slot fills it once: {out}"
    );
    assert_eq!(
        out.matches("Box::__default_action(").count(),
        1,
        "only the call that left it out takes the default: {out}"
    );
}

#[test]
fn a_default_must_have_a_value() {
    let (d, _) = err("component A(x: u8 = ) { view { <b /> } }");
    assert!(d.message.contains("default needs a value"), "{}", d.message);
}

#[test]
fn server_fn_has_a_server_and_a_client_version() {
    let src = "use x::Y;\n/// docs\n#[post]\npub async fn add(mut a: i32, b: Vec<(u8, u8)>) -> Option<Y> {\n    a += 1;\n    None\n}\n#[get]\nfn ping() {}\n#[derive(Debug)]\nstruct S;\n";
    let rust = ok(src);
    assert!(rust.contains("/// docs\n#[cfg(not(target_arch = \"wasm32\"))]\n#[allow(clippy::unit_arg)]\npub async fn add(mut a: i32, b: Vec<(u8, u8)>) -> ::core::result::Result<Option<Y>, ::yawn::fns::Error> { ::core::result::Result::Ok(async move {\n    a += 1;"), "{rust}");
    assert!(rust.contains("pub async fn add(a: i32, b: Vec<(u8, u8)>) -> ::core::result::Result<Option<Y>, ::yawn::fns::Error> { ::yawn::fns::call::<::yawn::fns::codec::Bits, _, _>(::yawn::fns::Method::Post, ::core::module_path!(), \"add\", &(a, b, )).await }"), "{rust}");
    assert!(
        rust.contains("pub async fn ping() -> ::core::result::Result<(), ::yawn::fns::Error>"),
        "{rust}"
    );
    assert!(rust.contains("let r = ::yawn::fns::route::<::yawn::fns::codec::Bits, _, _>(r, &::yawn::fns::url(::core::module_path!(), \"ping\"), ::yawn::fns::Method::Get, ping);"), "{rust}");
    assert!(
        rust.contains("#[derive(Debug)]\nstruct S;"),
        "any other attribute is plain Rust\n{rust}"
    );
    assert!(!rust.contains("#[post]") && !rust.contains("#[get]"), "{rust}");
    assert!(!ok("#[put] fn x() {}").is_empty() && !ok("fn a() {}").contains("__yawn_server_fns"));
    for (attr, method) in
        [("get", "Get"), ("post", "Post"), ("put", "Put"), ("patch", "Patch"), ("delete", "Delete")]
    {
        let rust = ok(&format!("#[{attr}]\nasync fn f() {{}}"));
        assert!(
            rust.contains(&format!("::yawn::fns::Method::{method}, ::core::module_path!()")),
            "{rust}"
        );
    }
    let c = compile(src, "t.rsx").unwrap();
    let body = c.rust.find("    a += 1;").unwrap();
    let line = c.rust[..body].matches('\n').count() + 1;
    assert_eq!(c.map.map(line as u32, 5), Some((5, 5)), "the body maps to its .rsx lines");
}

#[test]
fn server_fn_errors() {
    let src = "#[post] fn add(a: i32 = 1) {}";
    let (d, _) = err(src);
    assert!(d.message.contains("no default"), "{}", d.message);
    assert_eq!(pointed(src, &d), "a: i32 = 1");
    let src = "#[post] fn add(a: i32) => i32 {}";
    let (d, _) = err(src);
    assert!(d.message.contains("expected `->`"), "{}", d.message);
    let src = "#[post] fn add(a: i32) -> {}";
    let (d, _) = err(src);
    assert_eq!((d.message.as_str(), pointed(src, &d)), ("expected a type after `->`", "->"));
    let (d, _) = err("#[post] fn (a: i32) {}");
    assert!(d.message.contains("name of the server function"), "{}", d.message);
    let (d, _) = err("#[post] fn add(a: i32) -> u8");
    assert!(d.message.contains("has no body"), "{}", d.message);
    let src = "#[post] fn add() { let = 1; }";
    let (d, _) = err(src);
    assert_eq!(pointed(src, &d), "=");

    let src = "#[head] fn ping() {}";
    let (d, _) = err(src);
    assert!(d.message.contains("cannot make a server function"), "{}", d.message);
    assert_eq!(pointed(src, &d), "#[head]");
    let (d, _) = err("#[options] fn ping() {}");
    assert!(d.message.contains("`#[options]`"), "{}", d.message);
    let src = "#[get]\nstruct S;";
    let (d, _) = err(src);
    assert!(d.message.contains("goes before a function"), "{}", d.message);
    assert_eq!(pointed(src, &d), "#[get]");
}

#[test]
fn server_fn_extractors_exist_on_the_server_only() {
    let rust = ok(
        "#[post(headers: axum::http::HeaderMap, who: Who)]\nasync fn add(n: u8) -> u8 {\n    n\n}\n",
    );
    assert!(rust.contains("pub async fn add(n: u8) -> ::core::result::Result<u8, ::yawn::fns::Error> { let headers = match ::yawn::fns::extract::<axum::http::HeaderMap>().await { ::core::result::Result::Ok(v) => v, ::core::result::Result::Err(e) => return ::core::result::Result::Err(e) }; let who = match ::yawn::fns::extract::<Who>().await {"), "{rust}");
    assert!(rust.contains("pub async fn add(n: u8) -> ::core::result::Result<u8, ::yawn::fns::Error> { ::yawn::fns::call::<"), "{rust}");
    let src = "#[post(a: u8 = 1)] fn f() {}";
    let (d, _) = err(src);
    assert!(d.message.contains("no default"), "{}", d.message);
    let (d, _) = err("#[get(a: u8) x] fn f() {}");
    assert!(d.message.contains("ends after the options and extractors"), "{}", d.message);
}

#[test]
fn server_fn_encoding_is_an_option_of_the_attribute() {
    for (name, marker) in
        [("bits", "Bits"), ("json", "Json"), ("postcard", "Postcard"), ("dynamic", "Dynamic")]
    {
        let rust =
            ok(&format!("#[post(encoding = {name})]\nasync fn f(n: u8) -> u8 {{\n    n\n}}\n"));
        assert!(
            rust.contains(&format!("::yawn::fns::call::<::yawn::fns::codec::{marker}, _, _>")),
            "{rust}"
        );
        assert!(
            rust.contains(&format!("::yawn::fns::route::<::yawn::fns::codec::{marker}, _, _>")),
            "{rust}"
        );
    }
    let rust =
        ok("#[get(encoding = json, headers: axum::http::HeaderMap)]\nasync fn f() -> u8 { 1 }\n");
    assert!(
        rust.contains("codec::Json") && rust.contains("extract::<axum::http::HeaderMap>"),
        "{rust}"
    );
    let rust = ok(
        "#[get(headers: axum::http::HeaderMap, encoding = dynamic)]\nasync fn f() -> u8 { 1 }\n",
    );
    assert!(
        rust.contains("codec::Dynamic") && rust.contains("extract::<axum::http::HeaderMap>"),
        "{rust}"
    );
    let src = "#[post(encoding = xml)] fn f() {}";
    let (d, _) = err(src);
    assert!(d.message.contains("not an encoding"), "{}", d.message);
    let (d, _) = err("#[post(speed = fast)] fn f() {}");
    assert!(d.message.contains("not an option"), "{}", d.message);
    let (d, _) = err("#[post(encoding = json, encoding = bits)] fn f() {}");
    assert!(d.message.contains("given twice"), "{}", d.message);
}

#[test]
fn server_fn_has_a_pipelined_twin_with_the_default_encoding_only() {
    let rust = ok(
        "#[post]\nasync fn add(mut a: i32, b: Vec<u8>) -> Option<u8> {\n    None\n}\n#[get(encoding = json)]\nasync fn plain(a: u8) -> u8 { a }\n",
    );
    assert!(rust.contains("pub fn add_p(a: impl ::yawn::fns::IntoArg<i32>, b: impl ::yawn::fns::IntoArg<Vec<u8>>) -> ::yawn::fns::Pending<Option<u8>> {"), "{rust}");
    assert!(rust.contains("let a = ::yawn::fns::IntoArg::<i32>::value(a); let b = ::yawn::fns::IntoArg::<Vec<u8>>::value(b); ::yawn::fns::pending(async move { let a = a.await?; let b = b.await?; add(a, b).await })"), "{rust}");
    assert!(rust.contains("let mut args = ::yawn::fns::ArgBuilder::new::<(i32, Vec<u8>, )>(); ::yawn::fns::IntoArg::<i32>::part(a, &mut args); ::yawn::fns::IntoArg::<Vec<u8>>::part(b, &mut args); ::yawn::fns::pipe::<Option<u8>>(::yawn::fns::Method::Post, ::core::module_path!(), \"add\", args) }"), "{rust}");
    assert!(!rust.contains("plain_p"), "only the default encoding can be piped\n{rust}");
    assert!(ok("#[get]\nfn ping() {}").contains("pub fn ping_p() -> ::yawn::fns::Pending<()> {"));
}
