use std::collections::{BTreeMap, BTreeSet};
use yawn_split::graph::{self, Node};

fn demangled(n: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(n))
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("names") {
        let g = graph::build(&std::fs::read(std::env::args().nth(2).unwrap()).unwrap()).unwrap();
        return names_sample(&g, &std::fs::read(std::env::args().nth(3).unwrap()).unwrap());
    }
    if std::env::args().nth(1).as_deref() == Some("split") {
        return split_cmd();
    }
    let path = std::env::args().nth(1).expect("usage: yawn-split <linked.wasm> [route-pattern]");
    let pattern = std::env::args().nth(2).unwrap_or_else(|| "".into());
    let bytes = std::fs::read(&path).expect("read wasm");
    let g = graph::build(&bytes).expect("graph");

    let mut points: Vec<(u32, String)> = Vec::new();
    for (idx, name) in &g.func_name {
        let d = demangled(name);
        if d.starts_with("yawn_router::routes::route_entry::<") && d.contains(&pattern) {
            points.push((*idx, d));
        }
    }
    points.sort();
    eprintln!("{} split points", points.len());

    let point_set: BTreeSet<Node> = points.iter().map(|(i, _)| Node::Func(*i)).collect();
    let main_roots = g.roots.clone();
    let main = graph::reach(&g, &main_roots, &point_set, &|_| false);
    let size = |n: &Node| -> u64 {
        match n {
            Node::Func(i) => *g.func_size.get(i).unwrap_or(&0) as u64,
            Node::Data(i) => *g.data_size.get(i).unwrap_or(&0) as u64,
            Node::Root => 0,
        }
    };
    let main_bytes: u64 = main.iter().map(size).sum();
    let total_funcs: u64 = g.func_size.values().map(|v| *v as u64).sum();
    let total_data: u64 = g.data_size.values().map(|v| *v as u64).sum();
    println!("total code {} KB, data symbols {} KB", total_funcs / 1024, total_data / 1024);
    println!("main reaches {} KB (before routes)", main_bytes / 1024);

    if std::env::var("DUMP").is_ok() { dump_main(&g, &main); }
    let mut owners: BTreeMap<Node, BTreeSet<u32>> = BTreeMap::new();
    for (i, _) in &points {
        let r = graph::reach(&g, &[Node::Func(*i)], &point_set, &|n| main.contains(&n));
        for n in r {
            owners.entry(n).or_default().insert(*i);
        }
    }
    let mut by_set: BTreeMap<Vec<u32>, u64> = BTreeMap::new();
    for (n, set) in &owners {
        if matches!(n, Node::Func(i) if point_set.contains(&Node::Func(*i)) && !set.contains(i) ) {
            continue;
        }
        *by_set.entry(set.iter().copied().collect()).or_default() += size(n);
    }
    let exclusive: u64 = by_set.iter().filter(|(k, _)| k.len() == 1).map(|(_, v)| *v).sum();
    let shared: u64 = by_set.iter().filter(|(k, _)| k.len() > 1).map(|(_, v)| *v).sum();
    println!("exclusive to one route: {} KB, shared by >=2 routes: {} KB, in main: {} KB", exclusive / 1024, shared / 1024, main_bytes / 1024);
    let mut per: Vec<(u64, String)> = by_set
        .iter()
        .filter(|(k, _)| k.len() == 1)
        .map(|(k, v)| (*v, points.iter().find(|(i, _)| *i == k[0]).unwrap().1.clone()))
        .collect();
    per.sort_by(|a, b| b.0.cmp(&a.0));
    println!("largest exclusive routes:");
    for (s, n) in per.iter().take(12) {
        println!("  {:>6} KB  {}", s / 1024, n.chars().take(110).collect::<String>());
    }
    let mut sets: Vec<_> = by_set.iter().filter(|(k, _)| k.len() > 1).collect();
    sets.sort_by(|a, b| b.1.cmp(a.1));
    println!("largest shared sets:");
    for (k, v) in sets.iter().take(8) {
        println!("  {:>6} KB shared by {} routes", *v / 1024, k.len());
    }
}

#[allow(dead_code)]
pub fn dump_main(g: &graph::Graph, main: &BTreeSet<Node>) {
    let mut v: Vec<(u32, String)> = main
        .iter()
        .filter_map(|n| match n {
            Node::Func(i) => Some((*g.func_size.get(i).unwrap_or(&0), g.func_name.get(i).cloned().unwrap_or_else(|| format!("fn{i}")))),
            _ => None,
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    let routes: u64 = v.iter().filter(|(_, n)| demangled(n).contains("::routes::")).map(|(s, _)| *s as u64).sum();
    println!("main bytes in ::routes:: functions: {} KB ({} fns)", routes / 1024, v.iter().filter(|(_, n)| demangled(n).contains("::routes::")).count());
    for (s, n) in v.iter().take(25) {
        println!("  {:>6} B {}", s, demangled(n).chars().take(120).collect::<String>());
    }
}

fn split_cmd() {
    let a: Vec<String> = std::env::args().collect();
    let (orig, bind, out) = (&a[2], &a[3], std::path::Path::new(&a[4]));
    let res = yawn_split::split(&std::fs::read(orig).expect("original"), &std::fs::read(bind).expect("bindgened")).expect("split");
    std::fs::create_dir_all(out).unwrap();
    std::fs::write(out.join("main.wasm"), &res.main).unwrap();
    println!("main {} KB, {} chunks, {} routes", res.main.len() / 1024, res.chunks.len(), res.routes.len());
    for (i, c) in res.chunks.iter().enumerate() {
        std::fs::write(out.join(format!("chunk_{i}.wasm")), c).unwrap();
    }
}

#[allow(dead_code)]
pub fn names_sample(orig: &graph::Graph, bind: &[u8]) {
    let m = walrus::Module::from_buffer(bind).unwrap();
    let names: Vec<String> = m.funcs.iter().filter_map(|f| f.name.clone()).take(5).collect();
    println!("bindgen sample: {:?}", names);
    let mut o: Vec<_> = orig.func_name.iter().take(5).collect();
    o.sort();
    println!("orig sample: {:?}", o);
    println!("funcs {} named {}", m.funcs.iter().count(), m.funcs.iter().filter(|f| f.name.is_some()).count());
}
