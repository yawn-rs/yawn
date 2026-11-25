use std::any::{Any, TypeId};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicU32, Ordering};

const MAX_FLUSH_PASSES: usize = 10_000;
const NIL: u32 = u32::MAX;
const MAX_PULL_DEPTH: u32 = 4_000;
const SMALL: usize = 8;

static NEXT_RUNTIME: AtomicU32 = AtomicU32::new(1);

/// Handle to a node: slot index plus the generation the slot had when the node was created
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeId {
    pub(crate) rt: u32,
    pub(crate) idx: u32,
    pub(crate) r#gen: u32,
}

// flag bits (one byte per node)
const ALIVE: u8 = 1;
const MEMO: u8 = 2;
const EFFECT: u8 = 4;
const SCOPE: u8 = 8;
const QUEUED: u8 = 16;
const STALE: u8 = 32;
const RUNNING: u8 = 64;
const VIS: u8 = 128;

pub(crate) type Slot = Option<Box<dyn Any>>;
pub(crate) type Compute = Box<dyn FnMut(&mut Runtime, &mut Slot) -> bool>;

pub(crate) type EqBox<T> = Box<dyn Fn(&T, &T) -> bool>;

pub(crate) struct SigCell<T> {
    pub value: T,
    pub eq: EqBox<T>,
}

/// The value of a signal while a closure works on it outside the runtime
pub struct TakenSignal {
    idx: usize,
    r#gen: u32,
    boxed: Box<dyn Any>,
}

impl TakenSignal {
    pub fn update<T: 'static, O>(&mut self, f: impl FnOnce(&mut T) -> O) -> O {
        f(&mut self
            .boxed
            .downcast_mut::<SigCell<T>>()
            .expect("handle type matches stored value")
            .value)
    }

    pub fn read<T: 'static, O>(&self, f: impl FnOnce(&T) -> O) -> O {
        f(&self.boxed.downcast_ref::<SigCell<T>>().expect("handle type matches stored value").value)
    }
}

#[derive(Clone, Copy)]
struct Link {
    node: u32,
    back: u32,
}

enum Adj {
    Zero,
    One([Link; 1]),
    Two([Link; 2]),
    Big(Vec<Link>),
}

impl Adj {
    const EMPTY: Adj = Adj::Zero;

    #[inline]
    fn with_capacity(n: usize) -> Adj {
        if n > 2 { Adj::Big(Vec::with_capacity(n)) } else { Adj::Zero }
    }

    #[inline]
    fn push(&mut self, l: Link) {
        if let Adj::Big(v) = self {
            v.push(l);
            return;
        }
        *self = match std::mem::replace(self, Adj::Zero) {
            Adj::Zero => Adj::One([l]),
            Adj::One([a]) => Adj::Two([a, l]),
            Adj::Two([a, b]) => {
                let mut v = Vec::with_capacity(6);
                v.push(a);
                v.push(b);
                v.push(l);
                Adj::Big(v)
            }
            Adj::Big(mut v) => {
                v.push(l);
                Adj::Big(v)
            }
        };
    }

    #[inline]
    fn swap_remove(&mut self, pos: usize) {
        match self {
            Adj::Zero => unreachable!("removal from an empty adjacency list"),
            Adj::One(_) => *self = Adj::Zero,
            Adj::Two(a) => *self = Adj::One([a[1 - pos]]),
            Adj::Big(v) => {
                v.swap_remove(pos);
            }
        }
    }

    #[inline]
    fn pop(&mut self) {
        let n = self.len();
        self.swap_remove(n - 1);
    }
}

impl std::ops::Deref for Adj {
    type Target = [Link];
    #[inline]
    fn deref(&self) -> &[Link] {
        match self {
            Adj::Zero => &[],
            Adj::One(a) => a,
            Adj::Two(a) => a,
            Adj::Big(v) => v,
        }
    }
}

impl std::ops::DerefMut for Adj {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Link] {
        match self {
            Adj::Zero => &mut [],
            Adj::One(a) => a,
            Adj::Two(a) => a,
            Adj::Big(v) => v,
        }
    }
}

#[derive(Default)]
struct Aux {
    seq: u64,
    children: Vec<u32>,
    cleanups: Vec<Box<dyn FnOnce(&mut Runtime)>>,
    context: Vec<(TypeId, Box<dyn Any>)>,
}

struct RankSet {
    lv: Vec<Vec<u64>>,
    cw: usize,
    cnt: usize,
}

impl RankSet {
    fn new() -> Self {
        RankSet { lv: vec![vec![0], vec![0]], cw: 0, cnt: 0 }
    }

    fn ensure(&mut self, nbits: usize) {
        let mut need = nbits.div_ceil(64).max(1);
        let mut l = 0;
        loop {
            if self.lv.len() == l {
                let mut fresh = vec![0u64; need];
                for (w, &x) in self.lv[l - 1].iter().enumerate() {
                    if x != 0 {
                        fresh[w >> 6] |= 1 << (w & 63);
                    }
                }
                self.lv.push(fresh);
            }
            if self.lv[l].len() < need {
                self.lv[l].resize(need, 0);
            }
            if need == 1 {
                break;
            }
            need = need.div_ceil(64);
            l += 1;
        }
        if self.lv.len() < 2 {
            self.lv.push(vec![0]);
        }
    }

    #[inline]
    fn set(&mut self, r: usize) {
        let w = r >> 6;
        let was = self.lv[0][w];
        let bit = 1u64 << (r & 63);
        self.lv[0][w] = was | bit;
        self.cnt += (was & bit == 0) as usize;
        if was == 0 {
            let mut i = w;
            for l in 1..self.lv.len() {
                let ww = i >> 6;
                let u = self.lv[l][ww];
                self.lv[l][ww] = u | (1 << (i & 63));
                if u != 0 {
                    break;
                }
                i = ww;
            }
        }
        if w < self.cw {
            self.cw = w;
        }
    }

    fn clear_bit(&mut self, r: usize) {
        let bit = 1u64 << (r & 63);
        let w = &mut self.lv[0][r >> 6];
        self.cnt -= (*w & bit != 0) as usize;
        *w &= !bit;
    }

    fn reset(&mut self) {
        for l in self.lv.iter_mut() {
            l.iter_mut().for_each(|w| *w = 0);
        }
        self.cw = 0;
        self.cnt = 0;
    }

    #[inline]
    fn pop_min(&mut self) -> Option<usize> {
        let w0 = self.lv[0][self.cw];
        if w0 != 0 {
            self.lv[0][self.cw] = w0 & (w0 - 1);
            self.cnt -= 1;
            return Some((self.cw << 6) | w0.trailing_zeros() as usize);
        }
        self.pop_slow()
    }

    #[cold]
    fn pop_slow(&mut self) -> Option<usize> {
        let r = self.find()?;
        self.cw = r >> 6;
        let w0 = self.lv[0][self.cw];
        self.lv[0][self.cw] = w0 & (w0 - 1);
        self.cnt -= 1;
        Some(r)
    }

    fn find(&mut self) -> Option<usize> {
        'restart: loop {
            let top = self.lv.len() - 1;
            let t = self.lv[top][0];
            if t == 0 {
                return None;
            }
            let mut idx = t.trailing_zeros() as usize;
            for l in (0..top).rev() {
                let x = self.lv[l][idx];
                if x == 0 {
                    // the summary pointed at an empty word: clear that bit and start over
                    let mut i = idx;
                    for u in (l + 1)..self.lv.len() {
                        let ww = i >> 6;
                        self.lv[u][ww] &= !(1u64 << (i & 63));
                        if self.lv[u][ww] != 0 {
                            break;
                        }
                        i = ww;
                    }
                    continue 'restart;
                }
                idx = (idx << 6) | x.trailing_zeros() as usize;
            }
            return Some(idx);
        }
    }

    fn has(&self, r: usize) -> bool {
        r >> 6 < self.lv[0].len() && self.lv[0][r >> 6] & (1u64 << (r & 63)) != 0
    }
}

struct Small {
    n: usize,
    a: [u32; SMALL],
}
impl Small {
    #[inline]
    fn insert(&mut self, r: u32) {
        let mut i = self.n;
        while i > 0 && self.a[i - 1] < r {
            self.a[i] = self.a[i - 1];
            i -= 1;
        }
        self.a[i] = r;
        self.n += 1;
    }
    #[inline]
    fn pop(&mut self) -> u32 {
        self.n -= 1;
        self.a[self.n]
    }
}

pub struct Runtime {
    id: u32,
    // node columns, indexed by slot
    flags: Vec<u8>,
    r#gen: Vec<u32>,
    rank: Vec<u32>,
    owner: Vec<u32>,
    deps: Vec<Adj>,
    obs: Vec<Adj>,
    value: Vec<Slot>,
    func: Vec<Option<Compute>>,
    aux: Vec<Option<Box<Aux>>>,
    mark: Vec<u32>,
    scratch: Vec<u32>,
    free: Vec<u32>,
    // rank scheduling
    at: Vec<u32>,
    next_rank: u32,
    rs: RankSet,
    sm: Small,
    reg: u32,
    pending_fx: Vec<u32>,
    qlen: u32,
    pull_depth: u32,
    // execution state
    tracking: Option<Vec<(u32, u32)>>,
    frame_pool: Vec<Vec<(u32, u32)>>,
    owner_now: Option<u32>,
    in_handler: bool,
    in_memo: bool,
    propagating: bool,
    batch_depth: u32,
    flushing: bool,
    seq: u64,
    epoch: u32,
    poisoned: bool,
    added: Vec<u32>,
    pk_f: Vec<u32>,
    pk_b: Vec<u32>,
    pk_stack: Vec<u32>,
    pk_slots: Vec<u32>,
}

impl Runtime {
    pub fn new() -> Self {
        Runtime {
            id: NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed),
            flags: Vec::new(),
            r#gen: Vec::new(),
            rank: Vec::new(),
            owner: Vec::new(),
            deps: Vec::new(),
            obs: Vec::new(),
            value: Vec::new(),
            func: Vec::new(),
            aux: Vec::new(),
            mark: Vec::new(),
            scratch: Vec::new(),
            free: Vec::new(),
            at: Vec::new(),
            next_rank: 0,
            rs: RankSet::new(),
            sm: Small { n: 0, a: [0; SMALL] },
            reg: NIL,
            pending_fx: Vec::new(),
            qlen: 0,
            pull_depth: 0,
            tracking: None,
            frame_pool: Vec::new(),
            owner_now: None,
            in_handler: false,
            in_memo: false,
            propagating: false,
            batch_depth: 0,
            flushing: false,
            seq: 0,
            epoch: 0,
            poisoned: false,
            added: Vec::new(),
            pk_f: Vec::new(),
            pk_b: Vec::new(),
            pk_stack: Vec::new(),
            pk_slots: Vec::new(),
        }
    }

    fn check_usable(&self) {
        assert!(
            !self.poisoned,
            "runtime is poisoned: a computation panicked and left the graph in an unknown state"
        );
    }

    pub(crate) fn resolve(&self, id: NodeId) -> Option<usize> {
        assert!(id.rt == self.id, "handle belongs to a different Runtime");
        let i = id.idx as usize;
        (i < self.flags.len() && self.flags[i] & ALIVE != 0 && self.r#gen[i] == id.r#gen)
            .then_some(i)
    }

    pub(crate) fn node_alive(&self, id: NodeId) -> bool {
        self.resolve(id).is_some()
    }

    pub(crate) fn live_nodes(&self) -> usize {
        self.flags.len() - self.free.len()
    }

    fn id_of(&self, idx: usize) -> NodeId {
        NodeId { rt: self.id, idx: idx as u32, r#gen: self.r#gen[idx] }
    }

    fn alloc(&mut self, kind: u8, value: Slot, compute: Option<Compute>) -> usize {
        self.check_usable();
        self.maybe_renumber();
        self.seq += 1;
        let rank = self.next_rank;
        self.next_rank += 1;
        if rank.is_multiple_of(64) {
            self.rs.ensure(rank as usize + 64);
        }
        let owner = self.owner_now.unwrap_or(NIL);
        let idx = match self.free.pop() {
            Some(i) => {
                let i = i as usize;
                self.flags[i] = ALIVE | kind;
                self.rank[i] = rank;
                self.owner[i] = owner;
                self.value[i] = value;
                self.func[i] = compute;
                i
            }
            None => {
                self.flags.push(ALIVE | kind);
                self.r#gen.push(0);
                self.rank.push(rank);
                self.owner.push(owner);
                self.deps.push(Adj::EMPTY);
                self.obs.push(Adj::EMPTY);
                self.value.push(value);
                self.func.push(compute);
                self.aux.push(None);
                self.mark.push(0);
                self.scratch.push(0);
                self.flags.len() - 1
            }
        };
        self.at.push(idx as u32);
        debug_assert_eq!(self.at.len(), self.next_rank as usize);
        if kind & EFFECT != 0 {
            let seq = self.seq;
            self.aux_mut(idx).seq = seq;
        }
        if owner != NIL {
            self.aux_mut(owner as usize).children.push(idx as u32);
        }
        idx
    }

    fn aux_mut(&mut self, i: usize) -> &mut Aux {
        self.aux[i].get_or_insert_with(Default::default)
    }

    pub(crate) fn create_signal<T: 'static>(&mut self, value: T, eq: EqBox<T>) -> NodeId {
        let cell: Box<dyn Any> = Box::new(SigCell { value, eq });
        let idx = self.alloc(0, Some(cell), None);
        self.id_of(idx)
    }

    pub(crate) fn create_signal_unowned<T: 'static>(&mut self, value: T, eq: EqBox<T>) -> NodeId {
        let saved = self.owner_now.take();
        let id = self.create_signal(value, eq);
        self.owner_now = saved;
        id
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn create_memo(&mut self, compute: Compute) -> NodeId {
        self.settle_if_top_level();
        let idx = self.alloc(MEMO, None, Some(compute));
        self.run_node(idx);
        self.id_of(idx)
    }

    pub(crate) fn create_effect(&mut self, compute: Compute) -> NodeId {
        self.settle_if_top_level();
        let idx = self.alloc(EFFECT, None, Some(compute));
        self.run_node(idx);
        self.id_of(idx)
    }

    pub(crate) fn create_scope(&mut self, f: impl FnOnce(&mut Runtime)) -> NodeId {
        let idx = self.alloc(SCOPE, None, None);
        self.run_scope(idx, f)
    }

    pub(crate) fn create_scope_in(
        &mut self,
        owner: NodeId,
        f: impl FnOnce(&mut Runtime),
    ) -> NodeId {
        let o = self.resolve(owner).expect("scope_in: the owner scope was disposed");
        let saved = self.owner_now.replace(o as u32);
        let idx = self.alloc(SCOPE, None, None);
        self.owner_now = saved;
        self.run_scope(idx, f)
    }

    fn run_scope(&mut self, idx: usize, f: impl FnOnce(&mut Runtime)) -> NodeId {
        let prev_owner = self.owner_now.replace(idx as u32);
        let prev_track = self.tracking.take();
        let prev_memo = std::mem::replace(&mut self.in_memo, false);
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        self.owner_now = prev_owner;
        self.tracking = prev_track;
        self.in_memo = prev_memo;
        if let Err(p) = r {
            self.poisoned = true;
            resume_unwind(p);
        }
        self.id_of(idx)
    }

    pub(crate) fn current_scope_id(&self) -> Option<NodeId> {
        self.owner_now.filter(|o| *o != NIL).map(|o| self.id_of(o as usize))
    }

    pub(crate) fn run_in_owner<R>(
        &mut self,
        owner: NodeId,
        f: impl FnOnce(&mut Runtime) -> R,
    ) -> Option<R> {
        // an owner of an application that was replaced since (a timer or an answer outliving a reload) is as gone as a disposed one
        if owner.rt != self.id {
            return None;
        }
        let o = self.resolve(owner)?;
        let saved =
            (self.owner_now.replace(o as u32), std::mem::replace(&mut self.in_handler, true));
        let out = f(self);
        self.owner_now = saved.0;
        self.in_handler = saved.1;
        Some(out)
    }

    pub(crate) fn in_handler_now(&self) -> bool {
        self.in_handler
    }

    pub(crate) fn has_owner_now(&self) -> bool {
        self.owner_now.is_some_and(|o| o != NIL)
    }

    pub(crate) fn provide_context_dyn(&mut self, key: TypeId, value: Box<dyn Any>) {
        let o = self.owner_now.expect("provide_context called outside an effect, memo or scope");
        self.aux_mut(o as usize).context.push((key, value));
    }

    pub(crate) fn context_dyn(&self, key: TypeId) -> Option<&dyn Any> {
        let mut cur = self.owner_now;
        while let Some(i) = cur.filter(|i| *i != NIL) {
            if let Some(a) = &self.aux[i as usize]
                && let Some((_, v)) = a.context.iter().rev().find(|(t, _)| *t == key)
            {
                return Some(v.as_ref());
            }
            cur = Some(self.owner[i as usize]);
        }
        None
    }

    pub(crate) fn register_cleanup(&mut self, f: Box<dyn FnOnce(&mut Runtime)>) {
        let o = self.owner_now.expect("on_cleanup called outside an effect, memo or scope");
        self.aux_mut(o as usize).cleanups.push(f);
    }

    #[inline]
    fn pending_rank(&self) -> bool {
        self.qlen != 0
    }

    #[inline]
    fn settle_if_top_level(&mut self) {
        if !self.propagating && !self.in_memo && self.pending_rank() {
            self.propagate();
        }
    }

    fn read_prepare(&mut self, id: NodeId, track: bool) -> Option<usize> {
        self.check_usable();
        let idx = self.resolve(id)?;
        let fl = self.flags[idx];
        assert!(
            fl & RUNNING == 0,
            "dependency cycle: a node read its own value while computing it"
        );
        if fl & MEMO != 0 {
            self.settle_if_top_level();
            if self.flags[idx] & (QUEUED | STALE) != 0 {
                self.ensure(idx);
            }
        }
        if track && let Some(fr) = self.tracking.as_mut() {
            let entry = (idx as u32, self.r#gen[idx]);
            if fr.last() != Some(&entry) {
                fr.push(entry);
            }
        }
        Some(idx)
    }

    #[inline]
    fn ensure(&mut self, idx: usize) {
        let fl = self.flags[idx];
        if fl & (QUEUED | STALE) != 0 {
            self.flags[idx] = fl & !(QUEUED | STALE);
            self.pull_depth += 1;
            if self.pull_depth > MAX_PULL_DEPTH {
                self.poisoned = true;
                panic!(
                    "dependency chain of {MAX_PULL_DEPTH} queued nodes evaluated on demand; build graphs bottom-up or settle them with a flush first"
                );
            }
            self.run_node(idx);
            self.pull_depth -= 1;
        }
    }

    pub(crate) fn with_signal<T: 'static, O>(
        &mut self,
        id: NodeId,
        track: bool,
        f: impl FnOnce(&T) -> O,
    ) -> Option<O> {
        let idx = self.read_prepare(id, track)?;
        let v = self.value[idx].as_ref().expect("signal has a value");
        let cell = v.downcast_ref::<SigCell<T>>().expect("handle type matches stored value");
        Some(f(&cell.value))
    }

    pub(crate) fn with_memo<T: 'static, O>(
        &mut self,
        id: NodeId,
        track: bool,
        f: impl FnOnce(&T) -> O,
    ) -> Option<O> {
        let idx = self.read_prepare(id, track)?;
        let v = self.value[idx].as_ref().expect("memo has a value");
        Some(f(v.downcast_ref::<T>().expect("handle type matches stored value")))
    }

    pub(crate) fn take_signal_id(&mut self, id: NodeId, write: bool) -> Option<TakenSignal> {
        let idx = if write {
            self.check_writable();
            self.resolve(id)?
        } else {
            self.read_prepare(id, true)?
        };
        let boxed = self.value[idx]
            .take()
            .expect("a signal cannot be used inside the closure that is updating or reading it");
        Some(TakenSignal { idx, r#gen: self.r#gen[idx], boxed })
    }

    pub(crate) fn put_signal_id(&mut self, taken: TakenSignal, changed: bool) {
        let idx = taken.idx;
        if idx >= self.flags.len() || self.flags[idx] & ALIVE == 0 || self.r#gen[idx] != taken.r#gen
        {
            return;
        }
        self.value[idx] = Some(taken.boxed);
        if changed {
            self.enqueue_observers(idx);
            self.maybe_flush();
        }
    }

    fn check_writable(&self) {
        self.check_usable();
        assert!(!self.in_memo, "signals cannot be written from inside a memo; use an effect");
    }

    pub(crate) fn write_signal<T: 'static>(&mut self, id: NodeId, new: T) -> Option<()> {
        self.check_writable();
        let idx = self.resolve(id)?;
        let cell = self.value[idx]
            .as_mut()
            .expect("signal has a value")
            .downcast_mut::<SigCell<T>>()
            .expect("handle type matches stored value");
        if (cell.eq)(&cell.value, &new) {
            return Some(());
        }
        cell.value = new;
        self.enqueue_observers(idx);
        self.maybe_flush();
        Some(())
    }

    pub(crate) fn update_signal<T: 'static>(
        &mut self,
        id: NodeId,
        f: impl FnOnce(&mut T),
    ) -> Option<()> {
        self.check_writable();
        let idx = self.resolve(id)?;
        let cell = self.value[idx]
            .as_mut()
            .expect("signal has a value")
            .downcast_mut::<SigCell<T>>()
            .expect("handle type matches stored value");
        f(&mut cell.value);
        self.enqueue_observers(idx);
        self.maybe_flush();
        Some(())
    }

    fn enqueue_observers(&mut self, src: usize) {
        for k in 0..self.obs[src].len() {
            let o = self.obs[src][k].node as usize;
            self.enqueue_node(o);
        }
    }

    fn enqueue_node(&mut self, o: usize) {
        let fl = self.flags[o];
        if fl & QUEUED != 0 {
            return;
        }
        if fl & EFFECT != 0 {
            self.flags[o] = fl | QUEUED;
            self.pending_fx.push(o as u32);
            return;
        }
        if fl & RUNNING != 0 {
            return; // a running memo reads the fresh value anyway
        }
        if self.obs[o].is_empty() {
            self.flags[o] = fl | STALE; // nobody watches it: recompute on demand
            return;
        }
        self.flags[o] = fl | QUEUED;
        self.qlen += 1;
        let r = self.rank[o];
        if self.reg == NIL && self.sm.n == 0 && self.rs.cnt == 0 {
            self.reg = r;
        } else if self.rs.cnt == 0
            && (self.sm.n < SMALL - 1 || (self.sm.n < SMALL && self.reg == NIL))
        {
            if self.reg != NIL {
                let q = std::mem::replace(&mut self.reg, NIL);
                self.sm.insert(q);
            }
            self.sm.insert(r);
        } else {
            self.spill_small();
            self.rs.set(r as usize);
        }
    }

    #[inline]
    fn spill_small(&mut self) {
        if self.reg != NIL {
            let q = std::mem::replace(&mut self.reg, NIL);
            self.rs.set(q as usize);
        }
        while self.sm.n > 0 {
            let r = self.sm.pop();
            self.rs.set(r as usize);
        }
    }

    fn propagate(&mut self) {
        let was = std::mem::replace(&mut self.propagating, true);
        loop {
            let r = if self.reg != NIL {
                std::mem::replace(&mut self.reg, NIL) as usize
            } else if self.sm.n > 0 {
                self.sm.pop() as usize
            } else {
                match self.rs.pop_min() {
                    Some(r) => r,
                    None => break,
                }
            };
            self.qlen -= 1;
            let idx = self.at[r];
            if idx == NIL {
                continue; // rank of a disposed node
            }
            let i = idx as usize;
            if self.flags[i] & QUEUED == 0 {
                continue; // already pulled by a reader
            }
            self.flags[i] &= !QUEUED;
            self.run_node(i);
        }
        self.qlen = 0;
        self.propagating = was;
    }

    pub(crate) fn batch_impl<R>(&mut self, f: impl FnOnce(&mut Runtime) -> R) -> R {
        self.check_usable();
        self.batch_depth += 1;
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        self.batch_depth -= 1;
        match r {
            Ok(v) => {
                self.maybe_flush();
                v
            }
            Err(p) => resume_unwind(p),
        }
    }

    pub(crate) fn untracked_impl<R>(&mut self, f: impl FnOnce(&mut Runtime) -> R) -> R {
        let prev = self.tracking.take();
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        self.tracking = prev;
        match r {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }

    fn maybe_flush(&mut self) {
        if self.batch_depth == 0 && !self.flushing {
            self.flush();
        }
    }

    pub(crate) fn flush(&mut self) {
        self.check_usable();
        if self.flushing {
            return;
        }
        self.flushing = true;
        let mut passes = 0;
        loop {
            if self.pending_rank() {
                self.propagate();
            }
            if self.pending_fx.is_empty() {
                break;
            }
            passes += 1;
            if passes > MAX_FLUSH_PASSES {
                self.flushing = false;
                let pend = std::mem::take(&mut self.pending_fx);
                for i in pend {
                    self.flags[i as usize] &= !QUEUED;
                }
                panic!(
                    "effects kept re-triggering each other for {MAX_FLUSH_PASSES} passes (feedback loop?)"
                );
            }
            let mut batch = std::mem::take(&mut self.pending_fx);
            sort_ids(&mut batch, &|i| self.aux[i as usize].as_ref().map_or(0, |a| a.seq));
            for i in batch {
                let i = i as usize;
                let fl = self.flags[i];
                if fl & ALIVE == 0 || fl & QUEUED == 0 || fl & EFFECT == 0 {
                    continue; // disposed, or the slot now holds something else
                }
                self.flags[i] = fl & !QUEUED;
                self.run_node(i);
            }
        }
        self.flushing = false;
    }

    fn run_node(&mut self, idx: usize) {
        self.dispose_children(idx);
        self.run_cleanups(idx);
        let mut f = self.func[idx].take().expect("computed node has a function");
        let mut value = self.value[idx].take();
        let is_memo = self.flags[idx] & MEMO != 0;
        self.flags[idx] |= RUNNING;

        let frame = self.frame_pool.pop().unwrap_or_default();
        let prev_track = self.tracking.replace(frame);
        let prev_owner = self.owner_now.replace(idx as u32);
        let prev_memo = std::mem::replace(&mut self.in_memo, is_memo);
        let result = catch_unwind(AssertUnwindSafe(|| f(self, &mut value)));
        let mut frame = std::mem::replace(&mut self.tracking, prev_track).expect("frame set above");
        self.owner_now = prev_owner;
        self.in_memo = prev_memo;

        self.flags[idx] &= !RUNNING;
        self.func[idx] = Some(f);
        self.value[idx] = value;
        match result {
            Err(p) => {
                self.poisoned = true;
                resume_unwind(p);
            }
            Ok(changed) => {
                self.set_deps(idx, &frame);
                frame.clear();
                self.frame_pool.push(frame);
                if is_memo {
                    self.check_new_edges(idx);
                    if changed {
                        self.enqueue_observers(idx);
                    }
                }
            }
        }
    }

    fn next_epoch(&mut self) -> u32 {
        if self.epoch >= (1 << 29) {
            self.mark.iter_mut().for_each(|m| *m = 0);
            self.epoch = 0;
        }
        self.epoch += 1;
        self.epoch
    }

    #[inline]
    fn mark_bits(&self, d: usize, ep: u32) -> u32 {
        let m = self.mark[d];
        if m >> 2 == ep { m & 3 } else { 0 }
    }

    #[inline]
    fn mark_set(&mut self, d: usize, ep: u32, bit: u32) {
        let m = self.mark[d];
        self.mark[d] = if m >> 2 == ep { m | bit } else { (ep << 2) | bit };
    }

    fn set_deps(&mut self, idx: usize, read: &[(u32, u32)]) {
        self.added.clear();
        // fast path: the same reads in the same order as last time
        {
            let old = &self.deps[idx];
            if old.len() == read.len()
                && old
                    .iter()
                    .zip(read)
                    .all(|(l, &(d, g))| l.node == d && self.r#gen[d as usize] == g)
            {
                return;
            }
        }
        const IN_NEW: u32 = 1;
        const IN_OLD: u32 = 2;
        let ep = self.next_epoch();
        let mut new: Vec<u32> = Vec::with_capacity(read.len());
        for &(d, r#gen) in read {
            let du = d as usize;
            if self.flags[du] & ALIVE != 0
                && self.r#gen[du] == r#gen
                && self.mark_bits(du, ep) & IN_NEW == 0
            {
                self.mark_set(du, ep, IN_NEW);
                new.push(d);
            }
        }
        let old = std::mem::replace(&mut self.deps[idx], Adj::EMPTY);
        for l in old.iter() {
            let d = l.node as usize;
            self.mark_set(d, ep, IN_OLD);
            self.scratch[d] = l.back;
        }
        // drop edges that are gone
        for l in old.iter() {
            if self.mark_bits(l.node as usize, ep) & IN_NEW == 0 {
                self.remove_obs_entry(l.node as usize, l.back as usize);
            }
        }
        let mut deps = Adj::with_capacity(new.len());
        for (j, &d) in new.iter().enumerate() {
            let du = d as usize;
            let back = if self.mark_bits(du, ep) & IN_OLD != 0 {
                let pos = self.scratch[du];
                self.obs[du][pos as usize].back = j as u32;
                pos
            } else {
                self.obs[du].push(Link { node: idx as u32, back: j as u32 });
                self.added.push(d);
                (self.obs[du].len() - 1) as u32
            };
            deps.push(Link { node: d, back });
        }
        self.deps[idx] = deps;
    }

    fn remove_obs_entry(&mut self, d: usize, pos: usize) {
        let obs = &mut self.obs[d];
        obs.swap_remove(pos);
        if pos < obs.len() {
            let moved = obs[pos];
            self.deps[moved.node as usize][moved.back as usize].back = pos as u32;
        }
    }

    fn remove_deps_entry(&mut self, o: usize, pos: usize) {
        let deps = &mut self.deps[o];
        deps.swap_remove(pos);
        if pos < deps.len() {
            let moved = deps[pos];
            self.obs[moved.node as usize][moved.back as usize].back = pos as u32;
        }
    }

    fn check_new_edges(&mut self, idx: usize) {
        loop {
            let r = self.rank[idx];
            match self.added.iter().copied().find(|&d| self.rank[d as usize] > r) {
                Some(d) => self.fix_order(idx as u32, d),
                None => return,
            }
        }
    }

    #[cold]
    fn fix_order(&mut self, n: u32, d: u32) {
        self.spill_small();
        if n == d {
            self.poisoned = true;
            panic!("dependency cycle: a node depends on itself");
        }
        let lb = self.rank[n as usize];
        let ub = self.rank[d as usize];
        let (mut f, mut b, mut st) = (
            std::mem::take(&mut self.pk_f),
            std::mem::take(&mut self.pk_b),
            std::mem::take(&mut self.pk_stack),
        );
        f.clear();
        b.clear();
        st.clear();
        st.push(n);
        self.flags[n as usize] |= VIS;
        f.push(n);
        while let Some(x) = st.pop() {
            for k in 0..self.obs[x as usize].len() {
                let o = self.obs[x as usize][k].node;
                if o == d {
                    self.poisoned = true;
                    panic!("dependency cycle detected while reordering nodes");
                }
                if self.rank[o as usize] < ub && self.flags[o as usize] & VIS == 0 {
                    self.flags[o as usize] |= VIS;
                    f.push(o);
                    st.push(o);
                }
            }
        }
        st.push(d);
        self.flags[d as usize] |= VIS;
        b.push(d);
        while let Some(x) = st.pop() {
            for k in 0..self.deps[x as usize].len() {
                let p = self.deps[x as usize][k].node;
                if self.rank[p as usize] > lb && self.flags[p as usize] & VIS == 0 {
                    self.flags[p as usize] |= VIS;
                    b.push(p);
                    st.push(p);
                }
            }
        }
        let rank = &self.rank;
        sort_ids(&mut b, &|x| u64::from(rank[x as usize]));
        sort_ids(&mut f, &|x| u64::from(rank[x as usize]));
        let mut slots = std::mem::take(&mut self.pk_slots);
        slots.clear();
        slots.extend(b.iter().chain(f.iter()).map(|&x| self.rank[x as usize]));
        slots.sort_unstable();
        for &x in b.iter().chain(f.iter()) {
            self.flags[x as usize] &= !VIS;
            if self.flags[x as usize] & (QUEUED | MEMO) == (QUEUED | MEMO) {
                let r = self.rank[x as usize] as usize;
                self.rs.clear_bit(r);
            }
        }
        for (k, &x) in b.iter().chain(f.iter()).enumerate() {
            let r = slots[k];
            self.rank[x as usize] = r;
            self.at[r as usize] = x;
            if self.flags[x as usize] & (QUEUED | MEMO) == (QUEUED | MEMO) {
                self.rs.set(r as usize);
            }
        }
        self.pk_f = f;
        self.pk_b = b;
        self.pk_stack = st;
        self.pk_slots = slots;
    }

    fn maybe_renumber(&mut self) {
        if (self.next_rank as usize) <= 2 * self.live_nodes() + 1024 {
            return;
        }
        let mut at2: Vec<u32> = Vec::with_capacity(self.live_nodes() + 64);
        for r in 0..self.at.len() {
            let id = self.at[r];
            if id != NIL {
                self.rank[id as usize] = at2.len() as u32;
                at2.push(id);
            }
        }
        self.next_rank = at2.len() as u32;
        self.at = at2;
        self.reg = NIL;
        self.sm.n = 0;
        self.rs.reset();
        self.rs.ensure(self.next_rank as usize + 64);
        self.qlen = 0;
        for r in 0..self.at.len() {
            let i = self.at[r] as usize;
            if self.flags[i] & (QUEUED | MEMO) == (QUEUED | MEMO) {
                self.rs.set(r);
                self.qlen += 1;
            }
        }
    }

    #[inline]
    fn run_cleanups(&mut self, idx: usize) {
        if let Some(a) = self.aux[idx].as_mut() {
            a.context.clear();
        }
        let cs = match self.aux[idx].as_mut() {
            Some(a) if !a.cleanups.is_empty() => std::mem::take(&mut a.cleanups),
            _ => return,
        };
        for c in cs.into_iter().rev() {
            c(self);
        }
    }

    #[inline]
    fn dispose_children(&mut self, idx: usize) {
        let kids = match self.aux[idx].as_mut() {
            Some(a) if !a.children.is_empty() => std::mem::take(&mut a.children),
            _ => return,
        };
        for k in kids.into_iter().rev() {
            // the children list is already detached, so disposal must not touch it
            self.dispose_subtree(k as usize, false);
        }
    }

    pub(crate) fn dispose_node(&mut self, id: NodeId) -> bool {
        self.check_usable();
        let Some(idx) = self.resolve(id) else {
            return false;
        };
        self.dispose_subtree(idx, true);
        self.maybe_flush();
        true
    }

    fn dispose_subtree(&mut self, root: usize, detach_from_owner: bool) {
        // what a cleanup writes waits until the disposal is over, so no effect sees a half-disposed subtree
        self.batch_depth += 1;
        self.dispose_subtree_inner(root, detach_from_owner);
        self.batch_depth -= 1;
    }

    fn dispose_subtree_inner(&mut self, root: usize, detach_from_owner: bool) {
        // collect the subtree, children before parents, and refuse before changing anything if
        // part of it is currently executing
        let mut order = Vec::new();
        let mut stack = vec![root];
        while let Some(x) = stack.pop() {
            assert!(
                self.flags[x] & RUNNING == 0,
                "cannot dispose a node (or its owner) while it is running"
            );
            order.push(x);
            if let Some(a) = &self.aux[x] {
                stack.extend(a.children.iter().map(|&c| c as usize));
            }
        }
        if detach_from_owner {
            let o = self.owner[root];
            if o != NIL
                && let Some(a) = self.aux[o as usize].as_mut()
                && let Some(p) = a.children.iter().position(|&c| c as usize == root)
            {
                a.children.swap_remove(p);
            }
        }
        for &x in order.iter().rev() {
            self.run_cleanups(x);
            // detach from dependencies
            while let Some(l) = self.deps[x].last().copied() {
                self.deps[x].pop();
                self.remove_obs_entry(l.node as usize, l.back as usize);
            }
            // detach observers; they lose this dependency and must recompute
            while let Some(l) = self.obs[x].last().copied() {
                self.obs[x].pop();
                let o = l.node as usize;
                self.remove_deps_entry(o, l.back as usize);
                self.enqueue_node(o);
            }
            // every node holds a rank; freeing it makes a queue entry for it, if any, empty
            let r = self.rank[x] as usize;
            self.at[r] = NIL;
            self.flags[x] = 0;
            self.r#gen[x] = self.r#gen[x].wrapping_add(1);
            self.value[x] = None;
            self.func[x] = None;
            self.aux[x] = None;
            self.owner[x] = NIL;
            self.free.push(x as u32);
        }
    }
}

impl Runtime {
    /// Verifies internal invariants and panics on the first violation
    #[doc(hidden)]
    pub fn debug_check(&self) {
        let mut dead = 0;
        let mut live_memo_ranks = std::collections::HashSet::new();
        for i in 0..self.flags.len() {
            let fl = self.flags[i];
            if fl & ALIVE == 0 {
                dead += 1;
                assert!(
                    self.deps[i].is_empty() && self.obs[i].is_empty(),
                    "dead node {i} keeps links"
                );
                assert!(self.aux[i].is_none(), "dead node {i} keeps aux data");
                continue;
            }
            for (j, l) in self.deps[i].iter().enumerate() {
                assert!(
                    self.flags[l.node as usize] & ALIVE != 0,
                    "node {i} depends on dead node {}",
                    l.node
                );
                let m = self.obs[l.node as usize][l.back as usize];
                assert!(
                    m.node as usize == i && m.back as usize == j,
                    "deps/obs mirror broken at {i}<-{}",
                    l.node
                );
                if fl & MEMO != 0 {
                    assert!(
                        self.rank[l.node as usize] < self.rank[i],
                        "rank order violated: dep {} !< memo {i}",
                        l.node
                    );
                }
            }
            for (j, l) in self.obs[i].iter().enumerate() {
                assert!(
                    self.flags[l.node as usize] & ALIVE != 0,
                    "node {i} observed by dead node {}",
                    l.node
                );
                let m = self.deps[l.node as usize][l.back as usize];
                assert!(
                    m.node as usize == i && m.back as usize == j,
                    "obs/deps mirror broken at {i}->{}",
                    l.node
                );
            }
            if let Some(a) = &self.aux[i] {
                for &c in &a.children {
                    assert!(
                        self.flags[c as usize] & ALIVE != 0 && self.owner[c as usize] == i as u32,
                        "child/owner mismatch under {i}"
                    );
                }
            }
            if fl & MEMO != 0 {
                let r = self.rank[i] as usize;
                assert_eq!(self.at[r], i as u32, "rank/at mismatch for memo {i}");
                assert!(live_memo_ranks.insert(r), "duplicate rank {r}");
                if fl & QUEUED != 0 {
                    let q = self.reg as usize == r
                        || self.sm.a[..self.sm.n].contains(&(r as u32))
                        || self.rs.has(r);
                    assert!(q, "queued memo {i} has no queue entry");
                }
            }
            if self.batch_depth == 0 && !self.flushing && !self.propagating {
                if fl & EFFECT != 0 {
                    assert!(fl & QUEUED == 0, "effect {i} left queued after a flush");
                }
                if fl & MEMO != 0 {
                    assert!(fl & QUEUED == 0, "memo {i} left in the rank queue after a flush");
                }
            }
        }
        assert_eq!(dead, self.free.len(), "free list out of sync");
        assert!(
            self.qlen as usize >= (self.reg != NIL) as usize + self.sm.n + self.rs.cnt,
            "queue length undercounts"
        );
        if self.batch_depth == 0 && !self.flushing && !self.propagating {
            assert!(!self.pending_rank(), "rank queue not empty after a flush");
        }
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

fn sort_ids(ids: &mut [u32], key: &dyn Fn(u32) -> u64) {
    if ids.len() <= 20 {
        for i in 1..ids.len() {
            let (x, kx) = (ids[i], key(ids[i]));
            let mut j = i;
            while j > 0 && key(ids[j - 1]) > kx {
                ids[j] = ids[j - 1];
                j -= 1;
            }
            ids[j] = x;
        }
    } else {
        ids.sort_unstable_by_key(|&x| key(x));
    }
}
