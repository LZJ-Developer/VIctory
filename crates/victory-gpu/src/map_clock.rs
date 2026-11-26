//! Exclusive thread-local intervals during the serial xmem edit lock. The
//! second half of each vector is the same set of bins during survivor repair.
use std::cell::{Cell, RefCell};
use std::sync::LazyLock;

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Phase {
    Other, Unmap, Split, Map, Commit, Query, Trace, ShownNote, ShownForget,
    ViewTable, CommitBits, SplitNotify, Unmapping, Remapped, ViewAdded,
    Texback, Descriptors, ViewsGone, InplaceFree, GuardWait, GuardState,
    ViewsMapped, OpenPages, Unlock, Count,
}
pub const N: usize = Phase::Count as usize;
const BINS: usize = 2 * N;
const OFF: usize = BINS;
pub const NAMES: &str = "other,unmap,split,map,commit,query,trace,shown_note,shown_forget,view_table,commit_bits,split_notify,unmapping,remapped,view_added,texback,descriptors,views_gone,inplace_free,guard_wait,guard_state,views_mapped,open_pages,unlock";

pub struct Snapshot {
    pub ticks: [u64; BINS],
    pub entries: [u64; BINS],
    pub max_slice: [u64; BINS],
    pub edits: u64,
}
impl Snapshot {
    const fn empty() -> Self {
        Self { ticks: [0; BINS], entries: [0; BINS], max_slice: [0; BINS], edits: 0 }
    }
}
thread_local! {
    static CURRENT: Cell<usize> = const { Cell::new(OFF) };
    static LAST: Cell<u64> = const { Cell::new(0) };
    static TOTAL: RefCell<Snapshot> = const { RefCell::new(Snapshot::empty()) };
    static BATCH: Cell<bool> = const { Cell::new(false) };
}
pub static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_LOCK_PHASES").is_some_and(|v| v == "1"));
#[inline]
fn now() -> u64 {
    let mut t = 0i64;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut t) };
    t as u64
}
#[inline]
fn switch(next: usize, entry: bool) -> usize {
    let time = now();
    let old = CURRENT.replace(next);
    let duration = time - LAST.replace(time);
    TOTAL.with_borrow_mut(|sum| {
        if old != OFF {
            sum.ticks[old] += duration;
            sum.max_slice[old] = sum.max_slice[old].max(duration);
        }
        if entry && next != OFF {
            sum.entries[next] += 1;
        }
    });
    old
}
pub struct Interval(usize, bool);
impl Drop for Interval {
    #[inline]
    fn drop(&mut self) {
        if self.1 { switch(self.0, false); }
    }
}
#[inline]
pub fn phase(which: Phase) -> Interval {
    let current = CURRENT.get();
    if current == OFF { return Interval(OFF, false); }
    Interval(switch((current / N) * N + which as usize, true), true)
}
#[inline]
pub fn restoring() -> Interval {
    if CURRENT.get() == OFF { return Interval(OFF, false); }
    Interval(switch(N + Phase::Other as usize, true), true)
}
pub fn edit() -> Interval {
    if !BATCH.get() { return Interval(OFF, false); }
    TOTAL.with_borrow_mut(|sum| sum.edits += 1);
    Interval(switch(Phase::Other as usize, true), true)
}
/// Before acquiring any xmem locks, measure 2048 empty scopes per repeat.
/// Median over nine repeats estimates two switches (clock + TLS stores).
pub fn calibrate() -> (i64, usize, u64) {
    let mut hz = 0;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut hz) };
    const TRIALS: usize = 2048;
    BATCH.set(true);
    let mut repeats = [0; 9];
    for spent in &mut repeats {
        let outer = edit();
        let start = now();
        for _ in 0..TRIALS { std::hint::black_box(phase(Phase::Map)); }
        *spent = now() - start;
        drop(outer);
    }
    repeats.sort_unstable();
    BATCH.set(false);
    TOTAL.replace(Snapshot::empty());
    (hz, TRIALS, repeats[4])
}
pub fn begin() {
    BATCH.set(*ENABLED);
    if *ENABLED { TOTAL.replace(Snapshot::empty()); }
}
pub fn finish() -> Option<Snapshot> {
    BATCH.replace(false).then(|| TOTAL.replace(Snapshot::empty()))
}
