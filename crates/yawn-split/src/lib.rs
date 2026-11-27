//! Cuts a linked wasm module into a main module and lazily loaded route modules.
//!
//! The input is the wasm a build links with `--emit-relocs` (to build the call graph) and the
//! same program after `wasm-bindgen --no-demangle` (the one that gets shipped).

pub mod emit;
pub mod graph;
pub mod plan;

pub struct Split {
    pub main: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
    /// Route component type name and the chunks it needs
    pub routes: Vec<(String, Vec<usize>)>,
    pub unmatched: usize,
    /// Seconds spent reading the call graph, planning and emitting
    pub seconds: [f32; 3],
}

pub fn split(original: &[u8], bindgened: &[u8]) -> Result<Split, String> {
    let t = std::time::Instant::now();
    let g = graph::build(original)?;
    let graphed = t.elapsed().as_secs_f32();
    let plan = plan::make(&g, &plan::Options::default());
    if plan.routes.is_empty() || plan.chunks.is_empty() {
        return Err("no route to split out".into());
    }
    let planned = t.elapsed().as_secs_f32();
    let out = emit::split(bindgened, &g, &plan)?;
    let done = t.elapsed().as_secs_f32();
    let routes = plan
        .routes
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let chunks =
                plan.chunks.iter().filter(|c| c.routes.contains(&i)).map(|c| c.id).collect();
            (r.key.clone(), chunks)
        })
        .collect();
    Ok(Split {
        main: out.main,
        chunks: out.chunks,
        routes,
        unmatched: out.unmatched,
        seconds: [graphed, planned - graphed, done - planned],
    })
}
