#![allow(clippy::type_complexity, clippy::needless_range_loop)]
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use yawn_reactivity::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone, Copy)]
enum H {
    S(Signal<i64>),
    M(Memo<i64>),
}
fn read(rt: &mut Runtime, h: H) -> i64 {
    match h {
        H::S(s) => rt.get(s),
        H::M(m) => rt.get(m),
    }
}

#[derive(Clone, Copy)]
struct Formula {
    base: usize,
    gate: usize,
    tgt: Option<usize>,
    c: i64,
}

struct Model {
    alive: Vec<bool>,
    value: Vec<i64>, // sources only
    formula: Vec<Option<Formula>>,
}
impl Model {
    fn eval(&self, i: usize) -> i64 {
        match self.formula[i] {
            None => self.value[i],
            Some(f) => {
                let b = self.eval(f.base);
                match f.tgt {
                    Some(t) if self.eval(f.gate) & 1 == 1 => b.wrapping_add(self.eval(t)),
                    _ => b.wrapping_add(f.c),
                }
            }
        }
    }
    fn reaches(&self, from: usize, target: usize) -> bool {
        if from == target {
            return true;
        }
        match self.formula[from] {
            None => false,
            Some(f) => {
                self.reaches(f.base, target)
                    || self.reaches(f.gate, target)
                    || f.tgt.is_some_and(|t| self.reaches(t, target))
            }
        }
    }
}

static DISPOSED: AtomicUsize = AtomicUsize::new(0);
static MID_READS: AtomicUsize = AtomicUsize::new(0);
static REWIRES: AtomicUsize = AtomicUsize::new(0);
static BACKWARD: AtomicUsize = AtomicUsize::new(0);

struct Ctx {
    handles: Vec<H>,
    tcells: Vec<Rc<Cell<Option<H>>>>,
    kicks: Vec<Option<Signal<i64>>>,
    m: Model,
    effects: Vec<(Effect, usize, Rc<RefCell<Vec<i64>>>)>,
}

fn run(seed: u64, steps: usize) {
    let mut rng = Rng(seed | 1);
    let mut rt = Runtime::new();
    let mut cx = Ctx {
        handles: vec![],
        tcells: vec![],
        kicks: vec![],
        m: Model { alive: vec![], value: vec![], formula: vec![] },
        effects: vec![],
    };
    let mut kick_counter = 0i64;

    fn new_source(rt: &mut Runtime, cx: &mut Ctx, v: i64) -> usize {
        cx.handles.push(H::S(rt.signal(v)));
        cx.tcells.push(Rc::new(Cell::new(None)));
        cx.kicks.push(None);
        cx.m.alive.push(true);
        cx.m.value.push(v);
        cx.m.formula.push(None);
        cx.handles.len() - 1
    }
    for i in 0..4 {
        new_source(&mut rt, &mut cx, i);
    }

    for step in 0..steps {
        let alive: Vec<usize> = (0..cx.handles.len()).filter(|&i| cx.m.alive[i]).collect();
        let srcs: Vec<usize> =
            alive.iter().copied().filter(|&i| cx.m.formula[i].is_none()).collect();
        match rng.below(14) {
            0 | 1 => {
                let v = (rng.next() % 5) as i64;
                new_source(&mut rt, &mut cx, v);
            }
            2..=4 => {
                let base = alive[rng.below(alive.len())];
                let gate = srcs[rng.below(srcs.len())];
                let tgt = (rng.below(3) > 0).then(|| alive[rng.below(alive.len())]);
                let c = (rng.next() % 7) as i64;
                let (hb, hg) = (cx.handles[base], cx.handles[gate]);
                let tcell = Rc::new(Cell::new(tgt.map(|t| cx.handles[t])));
                let tc = tcell.clone();
                let kick = rt.signal(0i64);
                let memo = rt.memo(move |rt| {
                    rt.get(kick); // rewiring bumps this to force a recompute
                    let b = read(rt, hb);
                    match tc.get() {
                        Some(t) if read(rt, hg) & 1 == 1 => b.wrapping_add(read(rt, t)),
                        _ => b.wrapping_add(c),
                    }
                });
                cx.handles.push(H::M(memo));
                cx.tcells.push(tcell);
                cx.kicks.push(Some(kick));
                cx.m.alive.push(true);
                cx.m.value.push(0);
                cx.m.formula.push(Some(Formula { base, gate, tgt, c }));
                if rng.below(3) == 0 {
                    let id = cx.handles.len() - 1;
                    let seen = Rc::new(RefCell::new(vec![]));
                    let s2 = seen.clone();
                    let e = rt.effect(move |rt| s2.borrow_mut().push(rt.get(memo)));
                    cx.effects.push((e, id, seen));
                }
            }
            5..=6 => {
                // rewire a derived node's target to any alive node that doesn't depend on it
                let derived: Vec<usize> =
                    alive.iter().copied().filter(|&i| cx.m.formula[i].is_some()).collect();
                if !derived.is_empty() {
                    let n = derived[rng.below(derived.len())];
                    let t = alive[rng.below(alive.len())];
                    if !cx.m.reaches(t, n) {
                        if t > n {
                            BACKWARD.fetch_add(1, Relaxed); // target created after the node: rank order must be repaired
                        }
                        REWIRES.fetch_add(1, Relaxed);
                        let mut f = cx.m.formula[n].unwrap();
                        f.tgt = Some(t);
                        cx.m.formula[n] = Some(f);
                        cx.tcells[n].set(Some(cx.handles[t]));
                        kick_counter += 1;
                        rt.set(cx.kicks[n].unwrap(), kick_counter);
                    }
                }
            }
            7..=8 => {
                let i = srcs[rng.below(srcs.len())];
                let v = (rng.next() % 5) as i64;
                cx.m.value[i] = v;
                if let H::S(s) = cx.handles[i] {
                    rt.set(s, v);
                }
            }
            9 | 10 => {
                // batch of writes with reads in the middle: they must see the writes made so far
                let writes: Vec<(usize, i64)> = (0..1 + rng.below(4))
                    .map(|_| (srcs[rng.below(srcs.len())], (rng.next() % 5) as i64))
                    .collect();
                let probe = alive[rng.below(alive.len())];
                let hp = cx.handles[probe];
                rt.batch(|rt| {
                    for &(i, v) in &writes {
                        cx.m.value[i] = v;
                        if let H::S(s) = cx.handles[i] {
                            rt.set(s, v);
                        }
                        assert_eq!(
                            read(rt, hp),
                            cx.m.eval(probe),
                            "seed {seed} step {step}: mid-batch read"
                        );
                        MID_READS.fetch_add(1, Relaxed);
                    }
                });
            }
            _ => {
                // dispose a node together with everything that uses it as `base` or gate; nodes that
                // only use it as the optional target stop reading it and recompute
                if alive.len() > 6 {
                    let x = alive[rng.below(alive.len())];
                    let mut doomed = vec![x];
                    let mut k = 0;
                    while k < doomed.len() {
                        let d = doomed[k];
                        for i in 0..cx.handles.len() {
                            if cx.m.alive[i]
                                && !doomed.contains(&i)
                                && let Some(f) = cx.m.formula[i]
                                && (f.base == d || f.gate == d)
                            {
                                doomed.push(i);
                            }
                        }
                        k += 1;
                    }
                    rt.batch(|rt| {
                        for i in 0..cx.handles.len() {
                            if cx.m.alive[i]
                                && !doomed.contains(&i)
                                && let Some(mut f) = cx.m.formula[i]
                                && f.tgt.is_some_and(|t| doomed.contains(&t))
                            {
                                cx.tcells[i].set(None);
                                f.tgt = None;
                                cx.m.formula[i] = Some(f);
                            }
                        }
                        for (e, id, _) in &cx.effects {
                            if doomed.contains(id) {
                                assert!(rt.dispose(*e));
                            }
                        }
                        for &d in doomed.iter().rev() {
                            let ok = match cx.handles[d] {
                                H::S(s) => rt.dispose(s),
                                H::M(mm) => rt.dispose(mm),
                            };
                            assert!(ok);
                            if let Some(k) = cx.kicks[d] {
                                assert!(rt.dispose(k));
                            }
                            cx.m.alive[d] = false;
                            DISPOSED.fetch_add(1, Relaxed);
                        }
                    });
                    let alive_now = cx.m.alive.clone();
                    cx.effects.retain(|(_, id, _)| alive_now[*id]);
                }
            }
        }
        rt.debug_check();
        // every live node agrees with the model; every effect saw the current value last
        for i in 0..cx.handles.len() {
            if cx.m.alive[i] {
                assert_eq!(
                    read(&mut rt, cx.handles[i]),
                    cx.m.eval(i),
                    "seed {seed} step {step} node {i}"
                );
            }
        }
        for (_, id, seen) in &cx.effects {
            assert_eq!(
                *seen.borrow().last().unwrap(),
                cx.m.eval(*id),
                "seed {seed} step {step} effect on {id} is stale"
            );
        }
        rt.debug_check();
    }
}

#[test]
fn random_graphs_match_the_model() {
    for seed in 1..=300u64 {
        run(seed * 2654435761, 600);
    }
    let (d, w, r, b) = (
        DISPOSED.load(Relaxed),
        MID_READS.load(Relaxed),
        REWIRES.load(Relaxed),
        BACKWARD.load(Relaxed),
    );
    assert!(
        d > 500 && w > 500 && r > 500 && b > 200,
        "test exercised too little: {d} disposes, {w} mid-batch reads, {r} rewires, {b} backward"
    );
}
