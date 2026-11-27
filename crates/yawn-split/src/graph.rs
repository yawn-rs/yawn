use std::collections::{BTreeMap, BTreeSet, HashMap};
use wasmparser::{
    Export, ExternalKind, KnownCustom, Linking, Parser, Payload, RelocationType, SymbolInfo,
    TypeRef,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Node {
    Func(u32),
    Data(u32),
    Root,
}

pub struct Graph {
    pub data_addr: HashMap<u32, u32>,
    pub edges: HashMap<Node, Vec<Node>>,
    pub func_size: HashMap<u32, u32>,
    pub data_size: HashMap<u32, u32>,
    pub func_name: HashMap<u32, String>,
    pub data_name: HashMap<u32, String>,
    pub roots: Vec<Node>,
    pub imported_funcs: u32,
}

#[derive(Debug)]
struct DataSym {
    symbol: u32,
    offset: u32,
    size: u32,
}

pub fn build(bytes: &[u8]) -> Result<Graph, String> {
    let mut imported_funcs = 0u32;
    let mut code_start = 0usize;
    let mut bodies: Vec<(usize, usize)> = Vec::new();
    let mut data_start = 0usize;
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut segment_addr: Vec<u32> = Vec::new();
    let mut symbols: Vec<SymbolInfo> = Vec::new();
    let mut code_relocs = Vec::new();
    let mut data_relocs = Vec::new();
    let mut exports: Vec<u32> = Vec::new();
    let mut start: Option<u32> = None;
    let base = bytes.as_ptr() as usize;

    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| e.to_string())? {
            Payload::ImportSection(r) => {
                for imp in r.into_imports() {
                    if matches!(imp.map_err(|e| e.to_string())?.ty, TypeRef::Func(_)) {
                        imported_funcs += 1;
                    }
                }
            }
            Payload::ExportSection(r) => {
                for e in r {
                    let Export { kind, index, .. } = e.map_err(|e| e.to_string())?;
                    if kind == ExternalKind::Func {
                        exports.push(index);
                    }
                }
            }
            Payload::StartSection { func, .. } => start = Some(func),
            Payload::CodeSectionStart { range, .. } => code_start = range.start,
            Payload::CodeSectionEntry(body) => {
                let r = body.range();
                bodies.push((r.start, r.end));
            }
            Payload::DataSection(r) => {
                data_start = r.range().start;
                for d in r {
                    let d = d.map_err(|e| e.to_string())?;
                    let off = d.data.as_ptr() as usize - base;
                    segments.push((off, d.data.len()));
                    let addr = match &d.kind {
                        wasmparser::DataKind::Active { offset_expr, .. } => {
                            let mut ops = offset_expr.get_operators_reader();
                            match ops.read() {
                                Ok(wasmparser::Operator::I32Const { value }) => value as u32,
                                _ => 0,
                            }
                        }
                        _ => 0,
                    };
                    segment_addr.push(addr);
                }
            }
            Payload::CustomSection(c) => match c.as_known() {
                KnownCustom::Linking(l) => {
                    for sub in l.subsections() {
                        if let Linking::SymbolTable(map) = sub.map_err(|e| e.to_string())? {
                            for s in map {
                                symbols.push(s.map_err(|e| e.to_string())?);
                            }
                        }
                    }
                }
                KnownCustom::Reloc(r) => {
                    let name = c.name().to_string();
                    let mut entries = Vec::new();
                    for e in r.entries() {
                        entries.push(e.map_err(|e| e.to_string())?);
                    }
                    match name.as_str() {
                        "reloc.CODE" => code_relocs = entries,
                        "reloc.DATA" => data_relocs = entries,
                        _ => {}
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if symbols.is_empty() || code_relocs.is_empty() {
        return Err("the wasm has no linking or relocation sections: build it with -Clink-args=--emit-relocs".into());
    }

    let mut func_size = HashMap::new();
    for (i, (s, e)) in bodies.iter().enumerate() {
        func_size.insert(imported_funcs + i as u32, (e - s) as u32);
    }

    let mut func_name = HashMap::new();
    let mut data_name = HashMap::new();
    let mut data_size = HashMap::new();
    let mut data_addr = HashMap::new();
    let mut data_syms: BTreeMap<u32, Vec<DataSym>> = BTreeMap::new();
    for (i, s) in symbols.iter().enumerate() {
        let i = i as u32;
        match s {
            SymbolInfo::Func { index, name: Some(n), .. } => {
                func_name.insert(*index, n.to_string());
            }
            SymbolInfo::Data { name, symbol: Some(d), .. } => {
                data_name.insert(i, name.to_string());
                data_size.insert(i, d.size);
                data_addr
                    .insert(i, segment_addr.get(d.index as usize).copied().unwrap_or(0) + d.offset);
                data_syms.entry(d.index).or_default().push(DataSym {
                    symbol: i,
                    offset: d.offset,
                    size: d.size,
                });
            }
            _ => {}
        }
    }
    for v in data_syms.values_mut() {
        v.sort_by_key(|d| d.offset);
    }

    let target = |ty: RelocationType, index: u32| -> Option<Node> {
        use RelocationType::*;
        match ty {
            FunctionIndexLeb | FunctionIndexI32 | TableIndexSleb | TableIndexSleb64
            | TableIndexI32 | TableIndexI64 | TableIndexRelSleb | TableIndexRelSleb64 => {
                match symbols.get(index as usize)? {
                    SymbolInfo::Func { index, .. } => Some(Node::Func(*index)),
                    _ => None,
                }
            }
            MemoryAddrLeb | MemoryAddrLeb64 | MemoryAddrSleb | MemoryAddrSleb64 | MemoryAddrI32
            | MemoryAddrI64 | MemoryAddrRelSleb | MemoryAddrRelSleb64 | MemoryAddrTlsSleb
            | MemoryAddrTlsSleb64 | MemoryAddrLocrelI32 => match symbols.get(index as usize)? {
                SymbolInfo::Data { symbol: Some(_), .. } => Some(Node::Data(index)),
                _ => None,
            },
            _ => None,
        }
    };

    let mut edges: HashMap<Node, Vec<Node>> = HashMap::new();
    for r in &code_relocs {
        let abs = code_start + r.offset as usize;
        let at = bodies.partition_point(|(_, e)| *e <= abs);
        let Some(&(s, e)) = bodies.get(at) else {
            continue;
        };
        if abs < s || abs >= e {
            continue;
        }
        let src = Node::Func(imported_funcs + at as u32);
        if let Some(t) = target(r.ty, r.index) {
            edges.entry(src).or_default().push(t);
        }
    }
    for r in &data_relocs {
        let abs = data_start + r.offset as usize;
        let at = segments.partition_point(|(s, len)| s + len <= abs);
        let src = segments.get(at).filter(|(s, _)| abs >= *s).and_then(|(s, _)| {
            let within = (abs - s) as u32;
            let list = data_syms.get(&(at as u32))?;
            let k = list.partition_point(|d| d.offset + d.size.max(1) <= within);
            list.get(k).filter(|d| within >= d.offset).map(|d| Node::Data(d.symbol))
        });
        let src = match src {
            Some(s) => s,
            None => Node::Root,
        };
        if let Some(t) = target(r.ty, r.index) {
            edges.entry(src).or_default().push(t);
        }
    }

    let mut roots: Vec<Node> = exports.into_iter().map(Node::Func).collect();
    if let Some(s) = start {
        roots.push(Node::Func(s));
    }
    roots.push(Node::Root);
    Ok(Graph {
        data_addr,
        edges,
        func_size,
        data_size,
        func_name,
        data_name,
        roots,
        imported_funcs,
    })
}

pub fn reach(
    g: &Graph,
    from: &[Node],
    skip: &BTreeSet<Node>,
    blocked: &dyn Fn(Node) -> bool,
) -> BTreeSet<Node> {
    let mut seen: BTreeSet<Node> = BTreeSet::new();
    let mut stack: Vec<Node> = from.to_vec();
    while let Some(n) = stack.pop() {
        if blocked(n) || (skip.contains(&n) && !from.contains(&n)) || !seen.insert(n) {
            continue;
        }
        if let Some(out) = g.edges.get(&n) {
            stack.extend(out.iter().copied());
        }
    }
    seen
}
