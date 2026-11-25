use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::time::Instant;
use yawn_reactivity::*;

static LIVE: AtomicIsize = AtomicIsize::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe {
            LIVE.fetch_add(l.size() as isize, Relaxed);
            System.alloc(l)
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe {
            LIVE.fetch_sub(l.size() as isize, Relaxed);
            System.dealloc(p, l)
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        unsafe {
            LIVE.fetch_add(n as isize - l.size() as isize, Relaxed);
            System.realloc(p, l, n)
        }
    }
}
#[global_allocator]
static A: Counting = Counting;

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

fn measure(mut op: impl FnMut(u64)) -> f64 {
    let (reps, budget) = (7usize, 30.0f64);
    let mut i = 0u64;
    // calibrate
    let mut k = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..k {
            op(i);
            i += 1;
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if ms > 4.0 || k >= 1 << 26 {
            k = ((k as f64) * budget / ms.max(0.01)) as u64;
            k = k.max(1);
            break;
        }
        k *= 4;
    }
    let mut v: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..k {
                op(i);
                i += 1;
            }
            t.elapsed().as_nanos() as f64 / k as f64
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[reps / 2]
}

fn touch(rt: &mut Runtime, m: Memo<u64>) -> Effect {
    rt.effect(move |rt| {
        rt.get(m);
    })
}

#[allow(clippy::erasing_op)]
fn main() {
    let only = std::env::args().nth(1).filter(|a| !a.ends_with(".csv"));
    let csv_path = std::env::args().find(|a| a.ends_with(".csv"));
    let mut rows: Vec<(String, f64, &'static str)> = vec![];
    let mut run = |name: &str, unit: &'static str, f: &mut dyn FnMut() -> f64| {
        if let Some(o) = &only
            && !name.contains(o.as_str())
        {
            return;
        }
        let v = f();
        println!("{name:<28} {v:>14.1} {unit}");
        rows.push((name.to_string(), v, unit));
    };

    for &n in &[1_000usize, 100_000] {
        run(&format!("chain_{n}"), "ns/write", &mut || {
            let mut rt = Runtime::new();
            let root = rt.signal(0u64);
            let mut prev = rt.memo(move |rt| rt.get(root) + 1);
            for _ in 0..n {
                let p = prev;
                prev = rt.memo(move |rt| rt.get(p) + 1);
            }
            touch(&mut rt, prev);
            measure(|i| rt.set(root, i + 1))
        });
        run(&format!("chain_cut_{n}"), "ns/write", &mut || {
            // first memo is constant: every write is absorbed after one recomputation
            let mut rt = Runtime::new();
            let root = rt.signal(0u64);
            let mut prev = rt.memo(move |rt| rt.get(root) & 0);
            for _ in 0..n {
                let p = prev;
                prev = rt.memo(move |rt| rt.get(p) + 1);
            }
            touch(&mut rt, prev);
            measure(|i| rt.set(root, i + 1))
        });
    }
    run("fanout_cut_10000", "ns/write", &mut || {
        let mut rt = Runtime::new();
        let s = rt.signal(0u64);
        let parity = rt.memo(move |rt| rt.get(s) & 1);
        for _ in 0..10_000 {
            touch(&mut rt, parity);
        }
        measure(|i| rt.set(s, i + 1))
    });
    run("effects_on_signal_1000", "ns/write", &mut || {
        let mut rt = Runtime::new();
        let s = rt.signal(0u64);
        let sink = Rc::new(Cell::new(0u64));
        for _ in 0..1000 {
            let k = sink.clone();
            rt.effect(move |rt| k.set(k.get().wrapping_add(rt.get(s))));
        }
        measure(|i| rt.set(s, i + 1))
    });
    run("tree_65536", "ns/write", &mut || {
        let mut rt = Runtime::new();
        let leaves: Vec<Signal<u64>> = (0..65_536u64).map(|i| rt.signal(i)).collect();
        let mut level: Vec<Memo<u64>> = vec![];
        let mut cur: Vec<Memo<u64>> = leaves
            .chunks(2)
            .map(|c| {
                let (a, b) = (c[0], c[1]);
                rt.memo(move |rt| rt.get(a) + rt.get(b))
            })
            .collect();
        while cur.len() > 1 {
            level = cur
                .chunks(2)
                .map(|c| {
                    let (a, b) = (c[0], c[1]);
                    rt.memo(move |rt| rt.get(a) + rt.get(b))
                })
                .collect();
            cur = level.clone();
        }
        touch(&mut rt, cur[0]);
        let mut r = Rng(7);
        measure(|i| rt.set(leaves[r.below(leaves.len())], i + 1_000_000))
    });
    run("dag_dense_100x100", "ns/write", &mut || {
        // 100 layers of 100 memos, each reading 3 of the previous layer
        let mut rt = Runtime::new();
        let mut r = Rng(11);
        let srcs: Vec<Signal<u64>> = (0..100u64).map(|i| rt.signal(i & 1)).collect();
        let mut prev: Vec<Memo<u64>> = srcs.iter().map(|&s| rt.memo(move |rt| rt.get(s))).collect();
        for _ in 1..100 {
            let layer: Vec<Memo<u64>> = (0..100)
                .map(|_| {
                    let (a, b, c) = (prev[r.below(100)], prev[r.below(100)], prev[r.below(100)]);
                    rt.memo(move |rt| (rt.get(a) + rt.get(b) + rt.get(c)) & 0xFFFF)
                })
                .collect();
            prev = layer;
        }
        for &m in &prev {
            touch(&mut rt, m);
        }
        let mut cur = vec![0u64; 100];
        let mut r2 = Rng(5);
        measure(|_| {
            let k = r2.below(100);
            cur[k] ^= 1;
            rt.set(srcs[k], cur[k])
        })
    });
    run("dynamic_switch_10000", "ns/write", &mut || {
        // each memo reads sel then one of two sources
        let mut rt = Runtime::new();
        let n = 10_000usize;
        let a: Vec<Signal<u64>> = (0..n as u64).map(|i| rt.signal(i)).collect();
        let b: Vec<Signal<u64>> = (0..n as u64).map(|i| rt.signal(i + 1)).collect();
        let sel: Vec<Signal<u64>> = (0..n).map(|_| rt.signal(0u64)).collect();
        for i in 0..n {
            let (s, x, y) = (sel[i], a[i], b[i]);
            let m = rt.memo(move |rt| if rt.get(s) & 1 == 0 { rt.get(x) } else { rt.get(y) });
            touch(&mut rt, m);
        }
        let mut r = Rng(3);
        let mut cur = vec![0u64; n];
        measure(|_| {
            let k = r.below(n);
            if r.below(4) == 0 {
                cur[k] ^= 1;
                rt.set(sel[k], cur[k]);
            } else {
                rt.set(a[k], r.next() & 0xFFFF);
            }
        })
    });
    run("hotsub_10000", "ns/write", &mut || {
        // many memos flip their subscription to one shared source
        let mut rt = Runtime::new();
        let n = 10_000usize;
        let shared = rt.signal(0u64);
        let flags: Vec<Signal<u64>> = (0..n).map(|_| rt.signal(0u64)).collect();
        for &f in &flags {
            let m = rt.memo(move |rt| if rt.get(f) & 1 == 1 { rt.get(shared) } else { 0 });
            touch(&mut rt, m);
        }
        let mut r = Rng(9);
        let mut cur = vec![0u64; n];
        measure(|_| {
            let k = r.below(n);
            cur[k] ^= 1;
            rt.set(flags[k], cur[k])
        })
    });
    run("bigdeps_5000", "ns/write", &mut || {
        let mut rt = Runtime::new();
        let n = 5000usize;
        let flags: Vec<Signal<u64>> = (0..n).map(|_| rt.signal(0u64)).collect();
        let vals: Vec<Signal<u64>> = (0..n as u64).map(|i| rt.signal(i)).collect();
        let (f2, v2) = (flags.clone(), vals.clone());
        let agg = rt.memo(move |rt| {
            let mut s = 0u64;
            for i in 0..f2.len() {
                if rt.get(f2[i]) & 1 == 1 {
                    s = s.wrapping_add(rt.get(v2[i]));
                }
            }
            s
        });
        touch(&mut rt, agg);
        let mut r = Rng(13);
        let mut cur = vec![0u64; n];
        measure(|_| {
            let k = r.below(n);
            cur[k] ^= 1;
            rt.set(flags[k], cur[k])
        })
    });
    run("read_clean_memo", "ns/read", &mut || {
        let mut rt = Runtime::new();
        let s = rt.signal(1u64);
        let m = rt.memo(move |rt| rt.get(s) + 1);
        let mut acc = 0u64;
        let v = measure(|_| acc = acc.wrapping_add(rt.get(m)));
        std::hint::black_box(acc);
        v
    });
    run("read_signal", "ns/read", &mut || {
        let mut rt = Runtime::new();
        let s = rt.signal(1u64);
        let mut acc = 0u64;
        let v = measure(|_| acc = acc.wrapping_add(rt.get(s)));
        std::hint::black_box(acc);
        v
    });
    run("write_unobserved", "ns/write", &mut || {
        let mut rt = Runtime::new();
        let s = rt.signal(0u64);
        measure(|i| rt.set(s, i + 1))
    });
    run("write_one_effect", "ns/write", &mut || {
        let mut rt = Runtime::new();
        let s = rt.signal(0u64);
        rt.effect(move |rt| {
            rt.get(s);
        });
        measure(|i| rt.set(s, i + 1))
    });
    run("batch_64_writes_dag", "ns/batch", &mut || {
        let mut rt = Runtime::new();
        let mut r = Rng(17);
        let srcs: Vec<Signal<u64>> = (0..1000u64).map(|i| rt.signal(i)).collect();
        let mids: Vec<Memo<u64>> = (0..1000)
            .map(|_| {
                let (a, b) = (srcs[r.below(1000)], srcs[r.below(1000)]);
                rt.memo(move |rt| rt.get(a) ^ rt.get(b))
            })
            .collect();
        for &m in &mids {
            touch(&mut rt, m);
        }
        measure(|i| {
            rt.batch(|rt| {
                for k in 0..64 {
                    rt.set(srcs[(i as usize * 31 + k * 17) % 1000], i + k as u64 + 7);
                }
            })
        })
    });
    run("create_dispose_memo_effect", "ns/cycle", &mut || {
        let mut rt = Runtime::new();
        let s = rt.signal(0u64);
        measure(|_| {
            let m = rt.memo(move |rt| rt.get(s) + 1);
            let e = rt.effect(move |rt| {
                rt.get(m);
            });
            rt.dispose(e);
            rt.dispose(m);
        })
    });
    run("churn_scopes_ring_1000", "ns/round", &mut || {
        let mut rt = Runtime::new();
        let np = 32usize;
        let persist: Vec<Signal<u64>> = (0..np as u64).map(|i| rt.signal(i)).collect();
        let mk = |rt: &mut Runtime, p: Signal<u64>, k: u64| {
            rt.scope(move |rt| {
                let l = rt.signal(k & 0xFF);
                let c1 = rt.memo(move |rt| rt.get(p) + rt.get(l));
                let c2 = rt.memo(move |rt| rt.get(c1) * 3 + 1);
                let c3 = rt.memo(move |rt| rt.get(c1) ^ rt.get(c2));
                rt.effect(move |rt| {
                    rt.get(c3);
                });
            })
        };
        let mut ring: std::collections::VecDeque<Scope> =
            (0..1000).map(|k| mk(&mut rt, persist[k % np], k as u64)).collect();
        measure(|i| {
            rt.set(persist[(i as usize) % np], i + 100);
            let old = ring.pop_front().unwrap();
            rt.dispose(old);
            ring.push_back(mk(&mut rt, persist[(i as usize) % np], i));
        })
    });

    // memory: bytes per node after construction (live bytes, whole graph)
    let mem = |name: &str,
               rows: &mut Vec<(String, f64, &'static str)>,
               build: &mut dyn FnMut(&mut Runtime, usize)| {
        if let Some(o) = &only
            && !name.contains(o.as_str())
        {
            return;
        }
        let n = 100_000usize;
        let before = LIVE.load(Relaxed);
        let mut rt = Runtime::new();
        build(&mut rt, n);
        let per = (LIVE.load(Relaxed) - before) as f64 / n as f64;
        println!("{name:<28} {per:>14.1} B/node");
        rows.push((name.to_string(), per, "B/node"));
        drop(rt);
    };
    mem("mem_signal", &mut rows, &mut |rt, n| {
        for i in 0..n as u64 {
            rt.signal(i);
        }
    });
    mem("mem_memo_1dep", &mut rows, &mut |rt, n| {
        let s = rt.signal(0u64);
        for _ in 0..n {
            rt.memo(move |rt| rt.get(s) + 1);
        }
    });
    mem("mem_memo_chain", &mut rows, &mut |rt, n| {
        let s = rt.signal(0u64);
        let mut p = rt.memo(move |rt| rt.get(s) + 1);
        for _ in 0..n {
            let q = p;
            p = rt.memo(move |rt| rt.get(q) + 1);
        }
    });
    mem("mem_effect_1dep", &mut rows, &mut |rt, n| {
        let s = rt.signal(0u64);
        for _ in 0..n {
            rt.effect(move |rt| {
                rt.get(s);
            });
        }
    });

    if let Some(p) = csv_path {
        let mut out = String::from("scenario,value,unit\n");
        for (n, v, u) in &rows {
            out += &format!("{n},{v:.2},{u}\n");
        }
        std::fs::write(p, out).unwrap();
    }
}
