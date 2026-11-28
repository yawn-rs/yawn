use yawn::ui::mock::{MockApp, mount};
use yawn_rsx_tests::{counter, defaults, generics, misc};

fn text(app: &MockApp, sel: &str) -> String {
    app.text(app.find(sel))
}

#[test]
fn static_structure_and_text() {
    let app = mount(counter::Counter::default());
    let html = app.html();
    assert!(
        html.starts_with("<main class=\"card\"><h1>Counter</h1><p class=\"count\">0</p>"),
        "{html}"
    );
    assert!(html.contains("<p class=\"meta\">double = 0, even</p>"));
    assert!(html.contains("<span class=\"meta\">step 1</span>"));
    assert!(html.contains("<p class=\"notice small\">keep going</p>"));
    assert_eq!(app.title(), "Count: 0");
}

#[test]
fn signals_update_only_what_reads_them() {
    let app = mount(counter::Counter::default());
    app.reset_mutations();
    app.click(app.find("button.plus"));
    assert_eq!(text(&app, "p.count"), "1");
    assert_eq!(text(&app, "p.meta"), "double = 2, odd");
    assert_eq!(app.title(), "Count: 1");
    // count text, double+parity text, title... and the new log row; nothing else was touched.
    assert!(app.mutations() <= 10, "too many document writes: {}", app.mutations());
    // The stepper's label doesn't depend on count, so it wasn't rewritten.
    let before = app.mutations();
    app.click(app.find("button.plus"));
    assert!(app.mutations() - before <= 9);
}

#[test]
fn memo_that_does_not_change_does_not_rewrite() {
    let app = mount(counter::Counter::default());
    app.click(app.find("button.plus"));
    app.click(app.find("button.plus"));
    app.click(app.find("button.reset")); // 2 -> 0: parity stays "even"
    assert_eq!(text(&app, "p.meta"), "double = 0, even");
}

#[test]
fn class_toggle_and_conditional_block() {
    let app = mount(counter::Counter::default());
    let plus = app.find("button.plus");
    let step_double = app.find("button.double");
    assert_eq!(app.attr(app.find("p.count"), "class").as_deref(), Some("count"));
    for _ in 0..3 {
        app.click(step_double); // 2, 4, 8
    }
    app.click(plus); // 8
    app.click(plus); // 16
    assert_eq!(app.attr(app.find("p.count"), "class").as_deref(), Some("count big"));
    assert!(app.html().contains("<p class=\"notice\">that is a big number</p>"));
    assert!(!app.html().contains("keep going"));
    app.click(app.find("button.reset"));
    assert_eq!(app.attr(app.find("p.count"), "class").as_deref(), Some("count"));
    assert!(app.html().contains("keep going"));
}

#[test]
fn nested_component_shares_a_signal() {
    let app = mount(counter::Counter::default());
    app.click(app.find("button.double"));
    app.click(app.find("button.double"));
    assert_eq!(text(&app, "span.meta"), "step 4");
    app.click(app.find("button.plus"));
    assert_eq!(text(&app, "p.count"), "4");
    app.click(app.find("button.one"));
    assert_eq!(text(&app, "span.meta"), "step 1");
}

#[test]
fn keyed_list_add_remove() {
    let app = mount(counter::Counter::default());
    let plus = app.find("button.plus");
    app.click(plus);
    app.click(plus);
    app.click(plus);
    let rows: Vec<String> = app.find_all("ul.log li").iter().map(|n| app.text(*n)).collect();
    assert_eq!(rows, ["#1: now 1x", "#2: now 2x", "#3: now 3x"]);
    let second = app.find_all("ul.log li")[1];
    app.click(app.find_all("button.x")[1]);
    let rows: Vec<String> = app.find_all("ul.log li").iter().map(|n| app.text(*n)).collect();
    assert_eq!(rows, ["#1: now 1x", "#3: now 3x"]);
    assert!(!app.find_all("ul.log li").contains(&second));
}

#[test]
fn effect_runs_once_per_change() {
    counter::TITLE_RUNS.with(|r| r.set(0));
    let app = mount(counter::Counter::default());
    assert_eq!(counter::TITLE_RUNS.with(|r| r.get()), 1);
    app.click(app.find("button.plus"));
    assert_eq!(counter::TITLE_RUNS.with(|r| r.get()), 2);
    // Several writes in one handler are batched: `change` sets count and two other signals.
    app.click(app.find("button.minus"));
    assert_eq!(counter::TITLE_RUNS.with(|r| r.get()), 3);
    // A signal the effect doesn't read doesn't re-run it.
    app.click(app.find("button.double"));
    assert_eq!(counter::TITLE_RUNS.with(|r| r.get()), 3);
}

#[test]
fn children_are_placed_in_the_slot() {
    let app = mount(misc::Panel::default());
    assert_eq!(
        app.html(),
        "<section class=\"card\"><h2>Panel</h2><p class=\"inner\">inside 0</p><button>more</button></section>"
    );
    app.click(app.find("button"));
    assert_eq!(text(&app, "p.inner"), "inside 1");
}

#[test]
fn branch_disposes_effects_listeners_and_nodes() {
    misc::PROBE_RUNS.with(|r| r.set(0));
    misc::PROBE_ALIVE.with(|r| r.set(0));
    let app = mount(misc::Toggle::default());
    assert_eq!(misc::PROBE_ALIVE.with(|a| a.get()), 1);
    let (live, listeners) = (app.live_nodes(), app.listeners());

    app.click(app.find("button.tick"));
    assert_eq!(misc::PROBE_RUNS.with(|r| r.get()), 2);
    assert_eq!(text(&app, "i.probe"), "probe 1");

    // Off: the probe's effect is disposed and its on_cleanup ran.
    app.click(app.find("button.toggle"));
    assert!(app.find_all("i.probe").is_empty());
    assert_eq!(misc::PROBE_ALIVE.with(|a| a.get()), 0);
    let runs = misc::PROBE_RUNS.with(|r| r.get());
    app.click(app.find("button.tick"));
    assert_eq!(misc::PROBE_RUNS.with(|r| r.get()), runs, "a disposed effect ran again");
    assert!(app.live_nodes() < live, "nodes of the removed branch were not released");
    assert_eq!(
        app.listeners(),
        listeners - 1,
        "listener of the removed branch is still registered"
    );

    // On again, and many times: nothing accumulates.
    app.click(app.find("button.toggle"));
    assert_eq!(text(&app, "i.probe"), "probe 2");
    for _ in 0..20 {
        app.click(app.find("button.toggle"));
        app.click(app.find("button.toggle"));
    }
    assert_eq!(app.live_nodes(), live);
    assert_eq!(app.listeners(), listeners);
    assert_eq!(misc::PROBE_ALIVE.with(|a| a.get()), 1);
}

#[test]
fn inputs_attributes_and_interpolation() {
    let app = mount(misc::Form::default());
    let input = app.find("input.name");
    assert_eq!(app.attr(input, "placeholder").as_deref(), Some("your name"));
    assert_eq!(app.prop(input, "value").as_deref(), Some(""));
    assert_eq!(
        app.attr(app.find("button.go"), "disabled").as_deref(),
        Some(""),
        "boolean attribute present"
    );
    assert_eq!(text(&app, "p.hello"), "Hello, !");

    app.input(input, "Ada");
    assert_eq!(text(&app, "p.hello"), "Hello, Ada!");
    assert_eq!(app.attr(app.find("p.hello"), "title").as_deref(), Some("hello Ada"));
    assert_eq!(app.prop(input, "value").as_deref(), Some("Ada"));
    assert_eq!(app.attr(app.find("button.go"), "disabled"), None, "boolean attribute removed");

    app.click(app.find("button.go"));
    assert_eq!(text(&app, "span.clicks"), "1");
    assert_eq!(app.attr(app.find("span.clicks"), "data-clicks").as_deref(), Some("1"));
}

#[test]
fn else_if_chain_and_format_specs() {
    let app = mount(misc::Form::default());
    assert_eq!(text(&app, "b.lv"), "zero");
    assert_eq!(text(&app, "p.fmt"), "000|{literal}");
    app.click(app.find("button.up"));
    assert_eq!(text(&app, "b.lv"), "one");
    app.click(app.find("button.up"));
    assert_eq!(text(&app, "b.lv"), "many");
    assert_eq!(text(&app, "p.fmt"), "002|{literal}");
    assert_eq!(app.find_all("b.lv").len(), 1);
}

#[test]
fn if_let_rebuilds_when_the_value_changes_and_binds_it() {
    let app = mount(misc::Form::default());
    assert_eq!(text(&app, "u.got"), "nothing");
    app.click(app.find("button.up"));
    assert_eq!(text(&app, "u.got"), "got 1");
    assert_eq!(text(&app, "button.inc"), "inc 1");
    app.click(app.find("button.inc"));
    assert_eq!(text(&app, "u.got"), "got 2");
    assert_eq!(app.find_all("u.got").len(), 1);
}

fn ids(app: &MockApp) -> Vec<String> {
    app.find_all("li").iter().map(|n| app.attr(*n, "data-id").unwrap()).collect()
}

#[test]
fn keyed_rows_are_reused_and_moved_not_rebuilt() {
    let app = mount(misc::Items::default());
    assert_eq!(ids(&app), ["1", "2", "3"]);
    let (a, b, c) = (app.find_all("li")[0], app.find_all("li")[1], app.find_all("li")[2]);

    // Row state lives in the row: tick the second row's checkbox signal, then reorder.
    app.click(app.find_all("button.toggle")[1]);
    assert_eq!(app.attr(b, "class").as_deref(), Some("done"));

    app.click(app.find("button.rev"));
    assert_eq!(ids(&app), ["3", "2", "1"]);
    assert_eq!(app.find_all("li"), [c, b, a], "the same DOM nodes were moved");
    assert_eq!(app.attr(b, "class").as_deref(), Some("done"), "row state survived the move");

    app.click(app.find("button.add"));
    assert_eq!(ids(&app), ["3", "2", "1", "4"]);
    assert_eq!(&app.find_all("li")[..3], [c, b, a]);

    app.click(app.find("button.shuffle"));
    assert_eq!(ids(&app), ["4", "2", "1", "3"]);

    // Remove from the middle: only that row goes.
    app.click(app.find_all("button.rm")[1]);
    assert_eq!(ids(&app), ["4", "1", "3"]);
    assert!(!app.find_all("li").contains(&b));
    assert!(app.find_all("li").contains(&a));
}

#[test]
fn list_changes_touch_only_what_changed() {
    let app = mount(misc::Items::default());
    app.reset_mutations();
    app.click(app.find_all("button.toggle")[0]);
    assert_eq!(app.mutations(), 1, "toggling one row writes one class");
    app.reset_mutations();
    app.click(app.find("button.add"));
    // one new row: its nodes are built detached and inserted once
    assert!(app.mutations() <= 12, "{}", app.mutations());
}

#[test]
fn list_does_not_leak_nodes_or_listeners() {
    let app = mount(misc::Items::default());
    let (live, listeners) = (app.live_nodes(), app.listeners());
    for _ in 0..10 {
        app.click(app.find("button.add"));
    }
    while !app.find_all("button.rm").is_empty() {
        app.click(app.find_all("button.rm")[0]);
    }
    assert!(app.find_all("li").is_empty());
    // Back to the empty list: only the anchors and buttons remain.
    for _ in 0..3 {
        app.click(app.find("button.add"));
    }
    assert_eq!(ids(&app).len(), 3);
    assert!(app.live_nodes() <= live + 10, "live {} vs {}", app.live_nodes(), live);
    assert!(app.listeners() <= listeners + 6);
}

#[test]
fn nested_lists_with_patterns() {
    let app = mount(misc::Grid::default());
    assert_eq!(
        app.html(),
        "<div class=\"r\" data-row=\"1\"><b>1a</b><b>1b</b></div><div class=\"r\" data-row=\"2\"><b>2c</b></div>"
    );
}

#[test]
fn named_slots_fill_their_places_and_can_be_rebuilt() {
    let app = mount(misc::FoldingUse::default());
    assert_eq!(
        app.html(),
        "<div class=\"fold\"><button class=\"head\"><b>Title</b></button><footer><button class=\"bump\">bump</button></footer></div>"
    );
    app.click(app.find("button.head"));
    assert_eq!(text(&app, "div.body"), "first 0second");
    app.click(app.find("button.bump"));
    assert_eq!(text(&app, "p.one"), "first 1");
    // closed and opened again: the slot is built again, with the current state
    app.click(app.find("button.head"));
    assert!(app.find_all("div.body").is_empty());
    app.click(app.find("button.bump"));
    app.click(app.find("button.head"));
    assert_eq!(text(&app, "p.one"), "first 2");
}

#[test]
fn class_props_take_literals_built_strings_and_signals() {
    let app = mount(misc::BadgeUse::default());
    assert_eq!(
        app.html(),
        "<i class=\"plain badge\">b</i><i class=\"built 2 badge\">b</i><i class=\"red badge\">b</i><button class=\"flip\">flip</button>"
    );
    app.click(app.find("button.flip"));
    assert!(app.html().contains("<i class=\"blue badge\">b</i>"), "{}", app.html());
}

#[test]
fn props_left_out_take_their_defaults() {
    let app = mount(defaults::Chips::default());
    let html = app.html();
    assert!(html.contains("<span class=\"chip plain\">one</span>"), "{html}");
    assert!(html.contains("chip warn loud"), "{html}");
}

#[test]
fn generic_components_take_their_types_from_the_props() {
    let app = mount(generics::Uses::default());
    let html = app.html();
    assert!(html.contains("<span class=\"labeled\">value: 3</span>"), "{html}");
    assert!(html.contains("<span class=\"labeled\">name: x</span>"), "{html}");
    assert!(html.contains("<p class=\"joined\">#1, #2, #3</p>"), "{html}");
    assert!(html.contains("<b class=\"repeated\">aa</b>"), "{html}");
}

#[test]
fn ref_follows_the_element() {
    use yawn::ui::{Rect, run};
    let app = mount(misc::Handles::default());
    let (input, link, show) = misc::HANDLES.with(|h| h.borrow().expect("component ran"));
    let field = app.find("input.field");
    run(|| {
        assert_eq!(input.get(), yawn::ui::element("in"));
        assert_eq!(link.get(), yawn::ui::element("lnk"));

        let rect = Rect { left: 1.0, top: 2.0, right: 11.0, bottom: 22.0 };
        assert_eq!(input.rect(), None);
        app.set_rect(field, rect);
        assert_eq!(input.rect(), Some(rect));

        input.focus();
        assert_eq!(app.focused(), Some(field));
        yawn::ui::element("in").expect("by id").blur();
        assert_eq!(app.focused(), None);
        yawn::ui::element("in").expect("by id").focus();
        assert_eq!(yawn::ui::element("missing"), None);
        input.blur();
        assert_eq!(app.focused(), None);
        input.scroll_into_view();
        assert_eq!(app.scrolled(), vec![field]);

        show.set(false);
        assert_eq!(link.get(), None);
        link.focus();
        assert_eq!(app.focused(), None);
        assert_eq!(input.get(), yawn::ui::element("in"));
    });
}

#[test]
fn server_fns_run_the_body_on_the_server() {
    use std::task::{Context, Poll, Waker};
    use yawn_rsx_tests::server_fns;
    // the bodies never wait, so one poll finishes them
    fn now<F: Future>(f: F) -> F::Output {
        match std::pin::pin!(f).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("a server fn body without awaits is ready at once"),
        }
    }
    assert_eq!(now(server_fns::add(2, 3)), Ok(5));
    assert_eq!(now(server_fns::ping()), Ok(()));
    assert_eq!(now(server_fns::describe(vec![], ',')), Ok(Err("no words".into())));
    assert_eq!(now(server_fns::describe(vec!["a".into(), "b".into()], ';')), Ok(Ok("a;b;".into())));
    let (parts, ()) = yawn::server::axum::http::Request::builder()
        .header("a", "1")
        .header("b", "2")
        .body(())
        .unwrap()
        .into_parts();
    assert_eq!(now(yawn::fns::with_request(parts, server_fns::header_count())), Ok(2));
    assert!(matches!(now(server_fns::header_count()), Err(yawn::fns::Error::Status(500, _))));
    // the `_p` twin returns a pending call; its arguments are values (or pieces of answers, which
    // only the browser can take)
    assert_eq!(now(server_fns::add_p(2, 3)), Ok(5));
    assert_eq!(now(server_fns::ping_p()), Ok(()));
    let account = now(server_fns::open_account_p("ann".to_string()));
    assert_eq!(account, Ok(server_fns::Account { id: 3, name: "ann".into() }));
    assert_eq!(now(server_fns::label_p(4u32, "n".to_string())), Ok("n4".to_string()));
    // a piece of an earlier answer works on the server as in the batch
    let account = server_fns::open_account_p("bob".to_string());
    let label = server_fns::label_p(account.pick(|a| a.id), "id-".to_string());
    assert_eq!(now(label), Ok("id-3".to_string()));
    let named = server_fns::label_p(1u32, account.pick(|a| a.name));
    assert_eq!(now(named), Ok("bob1".to_string()));
    assert_eq!(now(account), Ok(server_fns::Account { id: 3, name: "bob".into() }));
    // axum checks the routes as they are added: distinct paths
    let _ = server_fns::__yawn_server_fns(yawn::server::axum::Router::new());
    let app = mount(server_fns::Adder::default());
    assert_eq!(text(&app, "button"), "0");
}
