use crate::graph::Graph;
use crate::plan::Plan;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use walrus::ir::{self, Visitor};
use walrus::{
    ConstExpr, DataKind, ElementItems, ElementKind, ExportItem, FunctionBuilder, FunctionId,
    FunctionKind, GlobalKind, ImportKind, ImportedFunction, Module, ModuleConfig,
};

pub struct Output {
    pub main: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
    pub unmatched: usize,
}

const MAIN: &str = "main";

fn parse(bytes: &[u8]) -> Result<Module, String> {
    let mut cfg = ModuleConfig::new();
    cfg.generate_producers_section(false);
    cfg.parse(bytes).map_err(|e| e.to_string())
}

fn strip_customs(m: &mut Module) {
    let names: Vec<String> = m.customs.iter().map(|(_, c)| c.name().to_string()).collect();
    for n in names {
        if n.starts_with(".debug") || n.starts_with("reloc.") || n == "linking" {
            m.customs.remove_raw(&n);
        }
    }
}

fn leb(out: &mut Vec<u8>, mut v: usize) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// The module with every function body except `keep` replaced by `unreachable`. Function indices stay
/// the same, and the module is cheap to parse because a chunk needs a few bodies out of tens of
/// thousands
fn hollow(bytes: &[u8], keep: &BTreeSet<u32>) -> Result<Vec<u8>, String> {
    use wasmparser::{Parser, Payload, TypeRef};
    let mut imported = 0u32;
    let mut section: Option<(usize, usize)> = None;
    let mut bodies: Vec<(usize, usize)> = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| e.to_string())? {
            Payload::ImportSection(r) => {
                for i in r.into_imports() {
                    if matches!(i.map_err(|e| e.to_string())?.ty, TypeRef::Func(_)) {
                        imported += 1;
                    }
                }
            }
            Payload::CodeSectionStart { range, .. } => section = Some((range.start, range.end)),
            Payload::CodeSectionEntry(b) => bodies.push((b.range().start, b.range().end)),
            _ => {}
        }
    }
    let Some((start, end)) = section else { return Ok(bytes.to_vec()) };
    let mut content = Vec::new();
    leb(&mut content, bodies.len());
    for (i, (s, e)) in bodies.iter().enumerate() {
        if keep.contains(&(imported + i as u32)) {
            leb(&mut content, e - s);
            content.extend_from_slice(&bytes[*s..*e]);
        } else {
            content.extend_from_slice(&[3, 0, 0, 0x0b]);
        }
    }
    let mut size_len = Vec::new();
    leb(&mut size_len, end - start);
    let head = start - 1 - size_len.len();
    let mut out = bytes[..head].to_vec();
    out.push(10);
    leb(&mut out, content.len());
    out.extend_from_slice(&content);
    out.extend_from_slice(&bytes[end..]);
    Ok(out)
}

struct Used(std::collections::HashSet<FunctionId>);

impl<'i> Visitor<'i> for Used {
    fn visit_function_id(&mut self, f: &FunctionId) {
        self.0.insert(*f);
    }
}

struct UsedTypes(std::collections::HashSet<walrus::TypeId>);

impl<'i> Visitor<'i> for UsedTypes {
    fn visit_type_id(&mut self, t: &walrus::TypeId) {
        self.0.insert(*t);
    }
}

struct Callees<'a> {
    ids: &'a HashMap<FunctionId, u32>,
    out: BTreeSet<u32>,
}

impl<'a, 'i> Visitor<'i> for Callees<'a> {
    fn visit_function_id(&mut self, f: &FunctionId) {
        if let Some(o) = self.ids.get(f) {
            self.out.insert(*o);
        }
    }
}

fn callees(m: &Module, ids: &HashMap<FunctionId, u32>, f: FunctionId) -> BTreeSet<u32> {
    let mut v = Callees { ids, out: BTreeSet::new() };
    if let FunctionKind::Local(l) = &m.funcs.get(f).kind {
        ir::dfs_in_order(&mut v, l, l.entry_block());
    }
    v.out
}

fn i32_expr(v: u32) -> ConstExpr {
    ConstExpr::Value(ir::Value::I32(v as i32))
}

pub fn split(bindgened: &[u8], g: &Graph, plan: &Plan) -> Result<Output, String> {
    let mut base = parse(bindgened)?;
    strip_customs(&mut base);
    let order: Vec<FunctionId> = base.funcs.iter().map(|f| f.id()).collect();
    let ord_of: HashMap<FunctionId, u32> =
        order.iter().enumerate().map(|(i, f)| (*f, i as u32)).collect();

    let mut by_name: HashMap<&str, Vec<u32>> = HashMap::new();
    for (i, f) in base.funcs.iter().enumerate() {
        if let Some(n) = &f.name {
            by_name.entry(n.as_str()).or_default().push(i as u32);
        }
    }

    let mut owner: HashMap<u32, usize> = HashMap::new();
    let mut unmatched = 0usize;
    let mut chunk_funcs: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); plan.chunks.len()];
    for c in &plan.chunks {
        for idx in &c.funcs {
            let hit = g.func_name.get(idx).and_then(|n| by_name.get(n.as_str()));
            match hit {
                Some(v) if v.len() == 1 => {
                    let o = v[0];
                    if matches!(base.funcs.get(order[o as usize]).kind, FunctionKind::Local(_)) {
                        owner.insert(o, c.id);
                        chunk_funcs[c.id].insert(o);
                    }
                }
                _ => unmatched += 1,
            }
        }
    }
    // a route's entry must stay addressable from the router, so it is always a stub in main
    let mut entry_ords: BTreeSet<u32> = BTreeSet::new();
    for r in &plan.routes {
        if let Some(v) = g.func_name.get(&r.entry).and_then(|n| by_name.get(n.as_str())) {
            if v.len() == 1 {
                entry_ords.insert(v[0]);
            }
        }
    }

    // who calls what, in terms of the bindgened module
    let mut callee_sets: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    for (o, f) in order.iter().enumerate() {
        callee_sets.insert(o as u32, callees(&base, &ord_of, *f));
    }

    // functions that something outside their own chunk calls or takes the address of
    let mut stubbed: BTreeSet<u32> = entry_ords.iter().copied().filter(|o| owner.contains_key(o)).collect();
    for (caller, set) in &callee_sets {
        let cc = owner.get(caller).copied();
        for callee in set {
            if let Some(oc) = owner.get(callee) {
                if Some(*oc) != cc {
                    stubbed.insert(*callee);
                }
            }
        }
    }
    for e in base.exports.iter() {
        if let ExportItem::Function(f) = e.item {
            let o = ord_of[&f];
            if owner.contains_key(&o) {
                stubbed.insert(o);
            }
        }
    }

    // the function table and where each stub gets its slot
    let table_id = base
        .tables
        .iter()
        .find(|t| t.element_ty == walrus::RefType::Funcref && !t.elem_segments.is_empty())
        .or_else(|| base.tables.iter().find(|t| t.element_ty == walrus::RefType::Funcref))
        .map(|t| t.id())
        .ok_or("the module has no function table")?;
    let table_base = base.tables.get(table_id).initial as u32;
    let slot_of: BTreeMap<u32, u32> =
        stubbed.iter().enumerate().map(|(i, o)| (*o, table_base + i as u32)).collect();

    // the slots the module's own element segments give to moved functions
    let mut elem_slots: Vec<(u32, u32)> = Vec::new();
    for e in base.elements.iter() {
        if let ElementKind::Active { table, offset: ConstExpr::Value(ir::Value::I32(off)) } = &e.kind {
            if *table != table_id {
                continue;
            }
            if let ElementItems::Functions(fs) = &e.items {
                for (i, f) in fs.iter().enumerate() {
                    let o = ord_of[f];
                    if owner.contains_key(&o) {
                        elem_slots.push((*off as u32 + i as u32, o));
                    }
                }
            }
        }
    }

    // what every chunk needs from main: callees it does not hold itself
    let mut imports_of: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); plan.chunks.len()];
    for c in &plan.chunks {
        for f in &chunk_funcs[c.id] {
            for callee in &callee_sets[f] {
                if !chunk_funcs[c.id].contains(callee) {
                    imports_of[c.id].insert(*callee);
                }
            }
        }
    }
    let mut main_exports: BTreeSet<u32> = BTreeSet::new();
    for s in &imports_of {
        main_exports.extend(s.iter().copied());
    }

    let width = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4);
    let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(plan.chunks.len());
    for batch in plan.chunks.chunks(width) {
        let done: Vec<Result<Vec<u8>, String>> = std::thread::scope(|scope| {
            let jobs: Vec<_> = batch
                .iter()
                .map(|c| {
                    let (funcs, needs, data) = (&chunk_funcs[c.id], &imports_of[c.id], &plan.chunks[c.id].data);
                    let (elem_slots, slot_of, owner) = (&elem_slots, &slot_of, &owner);
                    scope.spawn(move || {
                        emit_chunk(bindgened, c.id, funcs, needs, data, elem_slots, slot_of, owner)
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|j| j.join().unwrap_or_else(|_| Err("a chunk thread panicked".into())))
                .collect()
        });
        for r in done {
            chunks.push(r?);
        }
    }

    let mut main = base;
    let main_order = order;
    for (o, f) in main_order.iter().enumerate() {
        let o = o as u32;
        if !owner.contains_key(&o) {
            continue;
        }
        let ty = main.funcs.get(*f).ty();
        let (params, results) = {
            let t = main.types.get(ty);
            (t.params().to_vec(), t.results().to_vec())
        };
        let mut b = FunctionBuilder::new(&mut main.types, &params, &results);
        let args: Vec<_> = params.iter().map(|p| main.locals.add(*p)).collect();
        if let Some(slot) = slot_of.get(&o) {
            let mut body = b.func_body();
            for a in &args {
                body.local_get(*a);
            }
            body.i32_const(*slot as i32).call_indirect(ty, table_id);
        } else {
            b.func_body().unreachable();
        }
        main.funcs.get_mut(*f).kind = FunctionKind::Local(b.local_func(args));
    }

    // moved data is zeroed here; the chunk writes it back when it loads
    let mut ranges: Vec<(u32, u32)> =
        plan.chunks.iter().flat_map(|c| c.data.iter().copied()).collect();
    ranges.sort();
    let data_ids: Vec<_> = main.data.iter().map(|d| d.id()).collect();
    for did in data_ids {
        let d = main.data.get_mut(did);
        let DataKind::Active { offset: ConstExpr::Value(ir::Value::I32(off)), .. } = &d.kind else {
            continue;
        };
        let (start, end) = (*off as u32, *off as u32 + d.value.len() as u32);
        let first = ranges.partition_point(|(a, s)| a + s <= start);
        for (a, s) in &ranges[first..] {
            if *a >= end {
                break;
            }
            let from = a.max(&start) - start;
            let to = (a + s).min(end) - start;
            d.value[from as usize..to as usize].fill(0);
        }
    }

    {
        let t = main.tables.get_mut(table_id);
        t.initial += slot_of.len() as u64;
        if let Some(m) = &mut t.maximum {
            *m += slot_of.len() as u64;
        }
    }

    for o in &main_exports {
        main.exports.add(&format!("__s{o}"), main_order[*o as usize]);
    }
    let mem = main.memories.iter().next().map(|m| m.id());
    if let Some(m) = mem {
        main.exports.add("__yawn_memory", m);
    }
    let tables: Vec<_> = main.tables.iter().map(|t| t.id()).collect();
    for (i, t) in tables.iter().enumerate() {
        main.exports.add(&format!("__yawn_table{i}"), *t);
    }
    let globals: Vec<_> = main.globals.iter().map(|g| g.id()).collect();
    for (i, gl) in globals.iter().enumerate() {
        main.exports.add(&format!("__yawn_global{i}"), *gl);
    }

    Ok(Output { main: main.emit_wasm(), chunks, unmatched })
}

#[allow(clippy::too_many_arguments)]
fn emit_chunk(
    bindgened: &[u8],
    id: usize,
    funcs: &BTreeSet<u32>,
    needs: &BTreeSet<u32>,
    data: &[(u32, u32)],
    elem_slots: &[(u32, u32)],
    slot_of: &BTreeMap<u32, u32>,
    owner: &HashMap<u32, usize>,
) -> Result<Vec<u8>, String> {
    let hollow = hollow(bindgened, funcs)?;
    let mut m = parse(&hollow)?;
    strip_customs(&mut m);
    let order: Vec<FunctionId> = m.funcs.iter().map(|f| f.id()).collect();

    // the bytes of the data this chunk owns, read before the segments go
    let mut pieces: Vec<(u32, Vec<u8>)> = Vec::new();
    for (addr, size) in data {
        let mut bytes = vec![0u8; *size as usize];
        for d in m.data.iter() {
            if let DataKind::Active { offset: ConstExpr::Value(ir::Value::I32(off)), .. } = &d.kind {
                let (s, e) = (*off as u32, *off as u32 + d.value.len() as u32);
                let (a, b) = ((*addr).max(s), (addr + size).min(e));
                if a < b {
                    bytes[(a - addr) as usize..(b - addr) as usize]
                        .copy_from_slice(&d.value[(a - s) as usize..(b - s) as usize]);
                }
            }
        }
        pieces.push((*addr, bytes));
    }

    let datas: Vec<_> = m.data.iter().map(|d| d.id()).collect();
    for d in datas {
        m.data.delete(d);
    }
    let elems: Vec<_> = m.elements.iter().map(|e| e.id()).collect();
    for e in elems {
        m.elements.delete(e);
    }
    let exports: Vec<_> = m.exports.iter().map(|e| e.id()).collect();
    for e in exports {
        m.exports.delete(e);
    }
    m.start = None;

    let table_id = m
        .tables
        .iter()
        .find(|t| t.element_ty == walrus::RefType::Funcref)
        .map(|t| t.id())
        .ok_or("no function table")?;
    let mem = m.memories.iter().next().map(|x| x.id());

    for (o, f) in order.iter().enumerate() {
        let o = o as u32;
        if funcs.contains(&o) {
            continue;
        }
        let is_local = matches!(m.funcs.get(*f).kind, FunctionKind::Local(_));
        if !is_local {
            continue;
        }
        if needs.contains(&o) {
            let ty = m.funcs.get(*f).ty();
            let imp = m.imports.add(MAIN, &format!("__s{o}"), ImportKind::Function(*f));
            m.funcs.get_mut(*f).kind = FunctionKind::Import(ImportedFunction { import: imp, ty });
        } else {
            m.funcs.delete(*f);
        }
    }

    if let Some(mem) = mem {
        let imp = m.imports.add(MAIN, "__yawn_memory", ImportKind::Memory(mem));
        m.memories.get_mut(mem).import = Some(imp);
        pieces.sort_by_key(|p| p.0);
        let mut merged: Vec<(u32, Vec<u8>)> = Vec::new();
        for (addr, bytes) in pieces {
            match merged.last_mut() {
                Some((start, run)) if *start + run.len() as u32 == addr => run.extend(bytes),
                _ => merged.push((addr, bytes)),
            }
        }
        for (addr, bytes) in merged {
            m.data.add(DataKind::Active { memory: mem, offset: i32_expr(addr) }, bytes);
        }
    }
    let tables: Vec<_> = m.tables.iter().map(|t| t.id()).collect();
    for (i, t) in tables.iter().enumerate() {
        let imp = m.imports.add(MAIN, &format!("__yawn_table{i}"), ImportKind::Table(*t));
        let tb = m.tables.get_mut(*t);
        tb.import = Some(imp);
        if *t == table_id {
            tb.initial += slot_of.len() as u64;
            if let Some(max) = &mut tb.maximum {
                *max += slot_of.len() as u64;
            }
        }
    }
    let globals: Vec<_> = m.globals.iter().map(|g| g.id()).collect();
    for (i, gl) in globals.iter().enumerate() {
        let imp = m.imports.add(MAIN, &format!("__yawn_global{i}"), ImportKind::Global(*gl));
        m.globals.get_mut(*gl).kind = GlobalKind::Import(imp);
    }

    // an import nothing left in the chunk calls is dead weight in every chunk
    let mut used = Used(std::collections::HashSet::new());
    for f in m.funcs.iter() {
        if let FunctionKind::Local(l) = &f.kind {
            ir::dfs_in_order(&mut used, l, l.entry_block());
        }
    }
    for (_, f) in elem_slots {
        if owner.get(f) == Some(&id) {
            used.0.insert(order[*f as usize]);
        }
    }
    for o in funcs {
        used.0.insert(order[*o as usize]);
    }
    let dead: Vec<(FunctionId, walrus::ImportId)> = m
        .funcs
        .iter()
        .filter_map(|f| match &f.kind {
            FunctionKind::Import(i) if !used.0.contains(&f.id()) => Some((f.id(), i.import)),
            _ => None,
        })
        .collect();
    for (f, import) in dead {
        m.funcs.delete(f);
        m.imports.delete(import);
    }

    // the same for types: a chunk keeps the signatures of its own functions and imports only
    let mut types = UsedTypes(std::collections::HashSet::new());
    for f in m.funcs.iter() {
        types.0.insert(f.ty());
        if let FunctionKind::Local(l) = &f.kind {
            ir::dfs_in_order(&mut types, l, l.entry_block());
        }
    }
    let unused: Vec<walrus::TypeId> = m.types.iter().map(|t| t.id()).filter(|t| !types.0.contains(t)).collect();
    for t in unused {
        m.types.delete(t);
    }

    let mut entries: Vec<(u32, FunctionId)> = Vec::new();
    for (slot, o) in elem_slots {
        if owner.get(o) == Some(&id) {
            entries.push((*slot, order[*o as usize]));
        }
    }
    for o in funcs {
        if let Some(slot) = slot_of.get(o) {
            entries.push((*slot, order[*o as usize]));
        }
    }
    entries.sort_by_key(|e| e.0);
    let mut i = 0;
    while i < entries.len() {
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == entries[j - 1].0 + 1 {
            j += 1;
        }
        let items: Vec<FunctionId> = entries[i..j].iter().map(|e| e.1).collect();
        m.elements.add(
            ElementKind::Active { table: table_id, offset: i32_expr(entries[i].0) },
            ElementItems::Functions(items),
        );
        i = j;
    }
    Ok(m.emit_wasm())
}
