use crate::graph::{self, Graph, Node};
use std::collections::{BTreeMap, BTreeSet};

pub struct Route {
    pub key: String,
    pub entry: u32,
}

pub struct Chunk {
    pub id: usize,
    pub routes: BTreeSet<usize>,
    pub funcs: BTreeSet<u32>,
    pub data: Vec<(u32, u32)>,
    pub bytes: u64,
}

pub struct Plan {
    pub routes: Vec<Route>,
    pub chunks: Vec<Chunk>,
    pub main_bytes: u64,
}

pub struct Options {
    /// A chunk shared by at least this share of the routes stays in the main module
    pub main_share: f64,
    /// A group of code smaller than this stays in the main module: a request costs more than it saves
    pub min_chunk_bytes: u64,
    /// What one more request costs, in bytes of code: groups of code are merged while a merge costs
    /// less than the requests it saves (0 keeps every group apart)
    pub request_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
        Options {
            main_share: env("YAWN_SPLIT_SHARE").unwrap_or(0.5),
            min_chunk_bytes: env("YAWN_SPLIT_MIN").map_or(4 * 1024, |v| v as u64),
            request_bytes: env("YAWN_SPLIT_REQ").map_or(16 * 1024, |v| v as u64),
        }
    }
}

fn size(g: &Graph, n: &Node) -> u64 {
    match n {
        Node::Func(i) => *g.func_size.get(i).unwrap_or(&0) as u64,
        Node::Data(i) => *g.data_size.get(i).unwrap_or(&0) as u64,
        Node::Root => 0,
    }
}

pub fn route_key(demangled: &str) -> Option<String> {
    let rest = demangled.strip_prefix("yawn_router::routes::route_entry::<")?;
    Some(rest.strip_suffix('>')?.to_string())
}

pub fn make(g: &Graph, opts: &Options) -> Plan {
    let mut routes: Vec<Route> = Vec::new();
    for (idx, name) in &g.func_name {
        let d = format!("{:#}", rustc_demangle::demangle(name));
        if let Some(key) = route_key(&d) {
            routes.push(Route { key, entry: *idx });
        }
    }
    routes.sort_by(|a, b| a.key.cmp(&b.key));
    let points: BTreeSet<Node> = routes.iter().map(|r| Node::Func(r.entry)).collect();

    let main = graph::reach(g, &g.roots, &points, &|_| false);
    let main_bytes = main.iter().map(|n| size(g, n)).sum();

    let mut owners: BTreeMap<Node, BTreeSet<usize>> = BTreeMap::new();
    for (ri, r) in routes.iter().enumerate() {
        let reached = graph::reach(g, &[Node::Func(r.entry)], &points, &|n| main.contains(&n));
        for n in reached {
            owners.entry(n).or_default().insert(ri);
        }
    }

    let mut groups: BTreeMap<BTreeSet<usize>, Vec<Node>> = BTreeMap::new();
    for (n, set) in owners {
        groups.entry(set).or_default().push(n);
    }
    let limit = (routes.len() as f64 * opts.main_share).ceil() as usize;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut chunk_syms: Vec<(usize, u32)> = Vec::new();
    let mut kept: Vec<(BTreeSet<usize>, Vec<Node>)> = Vec::new();
    for (set, nodes) in groups {
        if set.len() >= limit.max(2) && routes.len() > 1 {
            continue;
        }
        kept.push((set, nodes));
    }
    if opts.request_bytes > 0 {
        kept = merge(g, kept, routes.len(), opts.request_bytes);
    }
    for (set, nodes) in kept {
        let group_bytes: u64 = nodes.iter().map(|n| size(g, n)).sum();
        if group_bytes < opts.min_chunk_bytes {
            continue;
        }
        let mut chunk = Chunk {
            id: chunks.len(),
            routes: set,
            funcs: BTreeSet::new(),
            data: Vec::new(),
            bytes: 0,
        };
        for n in nodes {
            chunk.bytes += size(g, &n);
            match n {
                Node::Func(i) => {
                    chunk.funcs.insert(i);
                }
                Node::Data(i) => {
                    if let (Some(a), Some(s)) = (g.data_addr.get(&i), g.data_size.get(&i)) {
                        chunk.data.push((*a, *s));
                        chunk_syms.push((chunk.id, i));
                    }
                }
                Node::Root => {}
            }
        }
        chunks.push(chunk);
    }
    // the linker merges identical constants, so several symbols can name the same bytes: those bytes
    // may only leave main when every symbol on them belongs to the same chunk
    let owner_of: std::collections::HashMap<u32, usize> = chunk_syms.iter().map(|(c, i)| (*i, *c)).collect();
    let mut ranges: Vec<(u32, u32, Option<usize>)> = Vec::new();
    for (i, a) in &g.data_addr {
        let size = g.data_size.get(i).copied().unwrap_or(0);
        if size == 0 {
            continue;
        }
        let owner = owner_of.get(i).copied();
        ranges.push((*a, *a + size, owner));
    }
    ranges.sort();
    for chunk in chunks.iter_mut() {
        let id = chunk.id;
        chunk.data.retain(|(a, sz)| {
            let (lo, hi) = (*a, *a + *sz);
            ranges.iter().take_while(|r| r.0 < hi).all(|(rl, rh, owner)| *rh <= lo || *rl >= hi || *owner == Some(id))
        });
    }
    Plan { routes, chunks, main_bytes }
}

/// Merges code groups (keyed by the routes that need them) while it pays: a group moves into a chunk
/// of a superset of its routes, which makes the other routes of that chunk load code they do not use,
/// or into the main module, which every route then loads. The price is bytes times the routes that
/// gain nothing, the saving is one request for each route that needed the group
fn merge(
    g: &Graph,
    groups: Vec<(BTreeSet<usize>, Vec<Node>)>,
    routes: usize,
    request: u64,
) -> Vec<(BTreeSet<usize>, Vec<Node>)> {
    struct Group {
        set: BTreeSet<usize>,
        mask: Vec<u64>,
        nodes: Vec<Node>,
        bytes: u64,
        /// The cheapest merge for this group: its score (negative pays off) and the target, `None` being main
        best: Option<(i64, Option<usize>)>,
    }
    let words = routes.div_ceil(64).max(1);
    let mut gs: Vec<Option<Group>> = groups
        .into_iter()
        .map(|(set, nodes)| {
            let mut mask = vec![0u64; words];
            for r in &set {
                mask[r / 64] |= 1 << (r % 64);
            }
            let bytes = nodes.iter().map(|x| size(g, x)).sum();
            Some(Group { set, mask, nodes, bytes, best: None })
        })
        .collect();
    let subset = |a: &[u64], b: &[u64]| a.iter().zip(b).all(|(x, y)| x & !y == 0);
    let find = |gs: &[Option<Group>], i: usize| -> Option<(i64, Option<usize>)> {
        let gi = gs[i].as_ref()?;
        let saving = gi.set.len() as i64 * request as i64;
        let mut best = None;
        let to_main = gi.bytes as i64 * (routes - gi.set.len()) as i64 - saving;
        if to_main < 0 {
            best = Some((to_main, None));
        }
        for (j, other) in gs.iter().enumerate() {
            let Some(o) = other else { continue };
            if i == j || !subset(&gi.mask, &o.mask) {
                continue;
            }
            let score = gi.bytes as i64 * (o.set.len() - gi.set.len()) as i64 - saving;
            if score < 0 && best.is_none_or(|b: (i64, Option<usize>)| score < b.0) {
                best = Some((score, Some(j)));
            }
        }
        best
    };
    for i in 0..gs.len() {
        let b = find(&gs, i);
        gs[i].as_mut().unwrap().best = b;
    }
    loop {
        let pick = gs
            .iter()
            .enumerate()
            .filter_map(|(i, x)| x.as_ref().and_then(|x| x.best).map(|b| (b.0, i, b.1)))
            .min_by_key(|p| p.0);
        let Some((_, i, target)) = pick else { break };
        let gone = gs[i].take().unwrap();
        if let Some(j) = target {
            let t = gs[j].as_mut().unwrap();
            t.nodes.extend(gone.nodes);
            t.bytes += gone.bytes;
            let b = find(&gs, j);
            gs[j].as_mut().unwrap().best = b;
        }
        for k in 0..gs.len() {
            if gs[k].as_ref().is_some_and(|x| x.best.is_some_and(|b| b.1 == Some(i))) {
                let b = find(&gs, k);
                gs[k].as_mut().unwrap().best = b;
            }
        }
    }
    gs.into_iter().flatten().map(|x| (x.set, x.nodes)).collect()
}
