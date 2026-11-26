//! Diagnostics: how long the card itself works on our submissions, by its own
//! timestamps (`VICTORY_CARD_SPANS=1`). The clock of the frame line
//! (`card_clock_line`) counts from the call that sends a submission to the
//! moment a thread of ours wakes behind its last signal: the time the system
//! takes to hand a submission to the card and to report it done is in it, 40
//! times a frame. Here two lists made once, of nothing but a timestamp, stand
//! around each submission.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::{Host, HostList, focus_dispatch};

/// Submissions stamped before the first stamps are read and used again.
const SPANS: usize = 512;
const STAMPS_PER_SPAN: usize = 3;
const MAX_SECTIONS: usize = 8;
const FOCUS_WIDTH: usize = MAX_SECTIONS - 3;

#[derive(Clone, Default)]
pub(super) struct FocusOps {
    counts: BTreeMap<String, u32>,
    omitted: u32,
}

impl FocusOps {
    pub fn note(&mut self, signature: String) {
        if let Some(n) = self.counts.get_mut(&signature) {
            *n += 1;
        } else if self.counts.len() < 64 {
            self.counts.insert(signature, 1);
        } else {
            self.omitted += 1;
        }
    }

    fn summary(&self) -> (u32, Vec<(&str, u32)>) {
        let mut entries: Vec<_> = self.counts.iter().map(|(name, &n)| (name.as_str(), n)).collect();
        let total = self.omitted + entries.iter().map(|&(_, n)| n).sum::<u32>();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        entries.truncate(12);
        (total, entries)
    }
}

type FocusSnapshots = Vec<(usize, Arc<FocusOps>)>;

pub(in crate::host) fn inventory_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    mode() == 3 && *ON.get_or_init(|| std::env::var_os("VICTORY_FOCUS_OPS").is_some_and(|v| v == "1"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Preparation,
    Title,
    Compare,
    Mixed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Slot {
    roles: [Option<Role>; MAX_SECTIONS],
    len: usize,
    compare_at: usize,
    ends: [usize; MAX_SECTIONS],
    fence: u64,
    list_count: usize,
    focus: Option<(usize, usize)>,
}

/// The stamps of one queue's submissions.
pub(super) struct Spans {
    _allocator: ID3D12CommandAllocator,
    _heap: ID3D12QueryHeap,
    _back: ID3D12Resource,
    /// The mapped results: start, before comparison, and end.
    data: usize,
    lists: Vec<ID3D12CommandList>,
    ticks_per_ms: f64,
    next: usize,
    stride: usize,
    slots: Vec<Slot>,
    focus_turn: HashMap<usize, usize>,
    inventories: Vec<FocusSnapshots>,
    inventory_logs: u32,
    dispatches: Vec<Vec<(usize, Arc<focus_dispatch::Sample>)>>,
    dispatch_logs: u32,
    sums: Sums,
}

// SAFETY: free-threaded objects and a mapping only read behind a lock.
unsafe impl Send for Spans {}

/// What the stamps read so far add up to.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Sums {
    /// Submissions, ticks the card worked on them, ticks between them.
    count: u64,
    busy: u64,
    between: u64,
    /// Ticks of the longest one, and the last one's end.
    longest: u64,
    longest_tail: u64,
    tail: u64,
    last: u64,
    longest_sections: Option<[u64; 4]>,
    longest_slot: Slot,
    detailed: u64,
}

impl Sums {
    /// A submission that ran from `start` to `end` (card ticks), after those
    /// before it. The start's stamp may be of before the last one's end: the
    /// part before that end is the last one's.
    fn add(&mut self, start: u64, before_compare: u64, end: u64) {
        if start == 0 || end < start {
            return;
        }
        let start = start.max(self.last);
        let took = end.saturating_sub(start);
        if self.last != 0 {
            self.between += start - self.last;
        }
        self.count += 1;
        self.busy += took;
        let tail = if before_compare != 0 && before_compare <= end {
            end.saturating_sub(before_compare.max(start))
        } else {
            0
        };
        self.tail += tail;
        if took > self.longest {
            self.longest = took;
            self.longest_tail = tail;
            self.longest_sections = None;
        }
        self.last = end.max(self.last);
    }
}

fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_CARD_SPANS").as_deref() {
        _ if crate::pipeline::host::hot_buffer_probe() => 3,
        Ok("1") => 1,
        Ok("2") => 2,
        Ok("3") => 3,
        _ => 0,
    })
}

fn on() -> bool { mode() != 0 }

pub(super) fn detailed() -> bool { mode() >= 2 }

fn section_ticks(ticks: &[u64], roles: &[Option<Role>], floor: u64) -> Option<[u64; 4]> {
    if ticks.len() != roles.len() + 1 || ticks[0] == 0 {
        return None;
    }
    let mut work = [0; 4];
    for (pair, role) in ticks.windows(2).zip(roles) {
        if pair[0] > pair[1] {
            return None;
        }
        let part = match (*role)? {
            Role::Preparation => 0,
            Role::Title => 1,
            Role::Compare => 2,
            Role::Mixed => 3,
        };
        work[part] += pair[1].saturating_sub(pair[0].max(floor));
    }
    Some(work)
}

/// Keep individual lists for short submissions. For long submissions, group
/// consecutive lists within the same marker budget and keep comparison separate.
fn partition(roles: &[Role], list_count: usize, fence: u64) -> Slot {
    let mut slot = Slot { fence, list_count, ..Slot::default() };
    if roles.is_empty() {
        return slot;
    }
    let title_end = roles.iter().position(|r| *r == Role::Compare).unwrap_or(roles.len());
    let mut start = 0;
    for (phase, (end, room)) in [(title_end, MAX_SECTIONS - usize::from(title_end < roles.len())), (roles.len(), 1)].into_iter().enumerate() {
        let count = (end - start).min(room);
        let size = end - start;
        for group in 1..=count {
            let stop = start + size * group / count;
            let first = if slot.len == 0 { 0 } else { slot.ends[slot.len - 1] };
            let role = roles[first];
            slot.roles[slot.len] = Some(if roles[first..stop].iter().all(|r| *r == role) { role } else { Role::Mixed });
            slot.ends[slot.len] = stop;
            slot.len += 1;
        }
        if phase == 0 {
            slot.compare_at = slot.len;
        }
        start = end;
    }
    slot
}

/// One contiguous window, with consecutive submissions of the same size
/// advancing it. With a bounded number of markers we still see each list.
fn partition_focus(roles: &[Role], fence: u64, turn: usize) -> Slot {
    let title_end = roles.iter().position(|r| *r == Role::Compare).unwrap_or(roles.len());
    if title_end <= FOCUS_WIDTH {
        return partition(roles, roles.len(), fence);
    }
    let first = (turn % title_end.div_ceil(FOCUS_WIDTH)) * FOCUS_WIDTH;
    let last = (first + FOCUS_WIDTH).min(title_end);
    let mut slot = Slot { fence, list_count: roles.len(), focus: Some((first, last)), ..Slot::default() };
    for end in (first..=last).chain([title_end, roles.len()]) {
        let start = if slot.len == 0 { 0 } else { slot.ends[slot.len - 1] };
        if start == end {
            continue;
        }
        let role = roles[start];
        slot.roles[slot.len] = Some(if roles[start..end].iter().all(|r| *r == role) { role } else { Role::Mixed });
        slot.ends[slot.len] = end;
        slot.len += 1;
        if end == title_end {
            slot.compare_at = slot.len;
        }
    }
    slot
}

pub(super) struct SpanMarkers {
    pub lists: Vec<ID3D12CommandList>,
    pub ends: Vec<usize>,
}

static SPANS_MADE: Mutex<Option<Option<Spans>>> = Mutex::new(None);

impl Host {
    /// The lists that go in front of and behind a submission about to be
    /// sent to the drawing queue, if stamps are asked for.
    pub(super) fn span_lists(&self, roles: &[Role], list_count: usize, fence: u64, title_lists: &[(usize, &HostList)]) -> Option<SpanMarkers> {
        if !on() {
            return None;
        }
        let mut made = SPANS_MADE.lock().unwrap_or_else(|e| e.into_inner());
        let spans = made
            .get_or_insert_with(|| {
                self.make_spans()
                    .inspect_err(|e| tracing::warn!(error = %e, "no stamps around submissions"))
                    .ok()
            })
            .as_mut()?;
        let i = spans.next;
        spans.next = (i + 1) % SPANS;
        // The stamps of the submission that had these lists, that many
        // submissions ago: long done.
        // SAFETY: inside the mapping; written by the card at whole words.
        let base = spans.stride * i;
        let old = spans.slots[i];
        let slot_index = i;
        let old_count = if old.len == 0 { STAMPS_PER_SPAN } else { old.len + 1 };
        let mut ticks = [0; MAX_SECTIONS + 1];
        for (index, tick) in ticks.iter_mut().enumerate().take(old_count) {
            // SAFETY: the mapped array owns this slot; its prior flight has landed.
            unsafe {
                let at = (spans.data as *mut u64).add(base + index);
                *tick = at.read_volatile();
                at.write_volatile(0);
            }
        }
        let floor = spans.sums.last;
        let longest = spans.sums.longest;
        let compare_at = if old.len == 0 { 1 } else { old.compare_at };
        spans.sums.add(ticks[0], ticks[compare_at], ticks[old_count - 1]);
        if spans.sums.longest > longest {
            spans.sums.longest_slot = old;
        }
        if old.len != 0
            && let Some(sections) = section_ticks(&ticks[..old_count], &old.roles[..old.len], floor)
        {
            spans.sums.detailed += 1;
            if spans.sums.longest > longest {
                spans.sums.longest_sections = Some(sections);
            }
        }
        let hot = crate::pipeline::host::hot_buffer_probe();
        if hot && !spans.dispatches[slot_index].is_empty() && spans.dispatch_logs < 400 {
            // Each target sample has its own timestamps. Its enclosing section
            // may group multiple lists and does not constrain the measurement.
            let completed = unsafe { self.fence.GetCompletedValue() };
            if completed != u64::MAX && completed >= old.fence {
                for (index, sample) in &spans.dispatches[slot_index] {
                    if spans.dispatch_logs >= 400 { break; }
                    if sample.report(old.fence, *index, None, spans.ticks_per_ms) {
                        spans.dispatch_logs += 1;
                    }
                }
            }
        }
        if let Some((first, last)) = old.focus
            && ticks[old_count - 1].saturating_sub(ticks[0].max(floor)) >= (100.0 * spans.ticks_per_ms) as u64
            && section_ticks(&ticks[..old_count], &old.roles[..old.len], floor).is_some()
        {
            for i in 0..old.len {
                let end = old.ends[i];
                if end <= first || end > last {
                    continue;
                }
                let ms = ticks[i + 1].saturating_sub(ticks[i].max(floor)) as f64 / spans.ticks_per_ms;
                tracing::info!(fence = old.fence, lists = old.list_count, index = end - 1, role = ?old.roles[i], ms, "GPU focus list");
                if ms >= 20.0 && spans.inventory_logs < 24
                    && let Some((_, ops)) = spans.inventories[slot_index].iter().find(|(index, _)| *index == end - 1)
                {
                    let (total, top) = ops.summary();
                    tracing::info!(fence = old.fence, lists = old.list_count, index = end - 1, ms, total, omitted = ops.omitted, top = ?top, "GPU focus command inventory");
                    spans.inventory_logs += 1;
                }
                let dispatch_limit = if crate::pipeline::host::load4_on() { 400 } else { 16 };
                if !hot && ms >= 50.0 && spans.dispatch_logs < dispatch_limit
                    && let Some((_, sample)) = spans.dispatches[slot_index].iter().find(|(index, _)| *index == end - 1)
                    && {
                        // SAFETY: an ordinary fence query. MAX represents device removal.
                        let completed = unsafe { self.fence.GetCompletedValue() };
                        completed != u64::MAX && completed >= old.fence
                    }
                    && sample.report(old.fence, end - 1, Some(ms), spans.ticks_per_ms)
                {
                    spans.dispatch_logs += 1;
                }
            }
        }
        let slot = if mode() == 3 && roles.len() > MAX_SECTIONS {
            let turn = spans.focus_turn.entry(list_count).or_default();
            let slot = partition_focus(roles, fence, *turn);
            *turn = turn.wrapping_add(1);
            slot
        } else {
            partition(roles, list_count, fence)
        };
        spans.slots[i] = slot;
        let inventory = &mut spans.inventories[i];
        inventory.clear();
        let dispatches = &mut spans.dispatches[i];
        dispatches.clear();
        if let Some((first, last)) = slot.focus {
            for &(index, list) in title_lists {
                if (first..last).contains(&index) {
                    if inventory_on() {
                        inventory.push((index, list.focus_ops.lock().unwrap_or_else(|e| e.into_inner()).clone()));
                    }
                    if !hot && focus_dispatch::on()
                        && let Some(sample) = list.focus_dispatch_sample()
                    {
                        dispatches.push((index, sample));
                    }
                }
            }
        }
        if hot {
            // A fast dispatch in a short or unfocused list is equally relevant.
            dispatches.extend(title_lists.iter().filter_map(|(index, list)| {
                list.focus_dispatch_sample().map(|sample| (*index, sample))
            }));
        }
        let count = if slot.len == 0 { STAMPS_PER_SPAN } else { slot.len + 1 };
        Some(SpanMarkers { lists: spans.lists[base..base + count].to_vec(), ends: slot.ends[..slot.len].to_vec() })
    }

    fn make_spans(&self) -> windows::core::Result<Spans> {
        let device = &self.device;
        let stride = if detailed() { MAX_SECTIONS + 1 } else { STAMPS_PER_SPAN };
        let stamps = SPANS * stride;
        let back_bytes = stamps * std::mem::size_of::<u64>();
        // SAFETY: plain object creation; lists of one allocator recorded one
        // after the other.
        unsafe {
            let mut heap: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: stamps as u32,
                    NodeMask: 0,
                },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let back = self.read_back_buffer(back_bytes as u64).ok_or_else(windows::core::Error::empty)?;
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data))?;
            std::ptr::write_bytes(data.cast::<u8>(), 0, back_bytes);
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let mut lists = Vec::with_capacity(stamps);
            for i in 0..stamps as u32 {
                let list: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i);
                list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 1, &back, u64::from(i) * 8);
                list.Close()?;
                lists.push(list.cast()?);
            }
            Ok(Spans {
                _allocator: allocator,
                _heap: heap,
                _back: back,
                data: data as usize,
                lists,
                ticks_per_ms: self.queue.GetTimestampFrequency()? as f64 / 1000.0,
                next: 0,
                stride,
                slots: vec![Slot::default(); SPANS],
                focus_turn: HashMap::new(),
                inventories: vec![Vec::new(); SPANS],
                inventory_logs: 0,
                dispatches: vec![Vec::new(); SPANS],
                dispatch_logs: 0,
                sums: Sums::default(),
            })
        }
    }
}

/// The GPU timestamps the title was given since the last frame-rate line:
/// how many, how often a later one was 1 ms or more after the one before
/// (written with another submission), and where the first few lie.
static STAMPED: Mutex<(u64, u64, u64, Vec<usize>)> = Mutex::new((0, 0, 0, Vec::new()));

/// The title's GPU timestamp at `at` was given the value `now` (ticks of 10
/// ns).
pub(crate) fn stamped(at: usize, now: u64) {
    if !on() {
        return;
    }
    let mut s = STAMPED.lock().unwrap_or_else(|e| e.into_inner());
    s.0 += 1;
    s.1 += u64::from(now.saturating_sub(s.2) > 100_000);
    s.2 = now;
    if s.3.len() < 64 && !s.3.contains(&at) {
        s.3.push(at);
        tracing::info!(address = format_args!("{at:#x}"), now, "a GPU timestamp of the title");
    }
}

/// The viewports' sizes seen (each said once): whether the title draws its
/// scene at sizes of its choice.
pub(crate) fn viewport(width: u32, height: u32) {
    if !on() || width < 1024 || height < 576 {
        return;
    }
    static SEEN: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 200 && !seen.contains(&(width, height)) {
        seen.push((width, height));
        tracing::info!(width, height, "a viewport of the title");
    }
}

/// For the frame-rate line: what the stamps read since the last one say, per
/// frame (they are of some [`SPANS`] submissions ago).
pub(super) fn line(frames: f64) {
    {
        let mut s = STAMPED.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 > 0 {
            tracing::info!(
                "GPU timestamps the title asked for per frame: {:.1}, in {:.1} groups written at one time",
                s.0 as f64 / frames,
                s.1 as f64 / frames
            );
        }
        (s.0, s.1) = (0, 0);
    }
    let mut made = SPANS_MADE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(spans)) = made.as_mut() else {
        return;
    };
    let last = spans.sums.last;
    let s = std::mem::replace(&mut spans.sums, Sums { last, ..Sums::default() });
    let ms = |ticks: u64| ticks as f64 / spans.ticks_per_ms / frames;
    tracing::info!(
        "the card's own stamps per frame: {:.1} submissions, working {:.1} ms, idle between them {:.1} ms, the longest {:.1} ms",
        s.count as f64 / frames,
        ms(s.busy),
        ms(s.between),
        s.longest as f64 / spans.ticks_per_ms
    );
    tracing::info!(
        "the card's post-title comparison region: {:.1} ms per frame, {:.1} ms in the longest submission",
        ms(s.tail),
        s.longest_tail as f64 / spans.ticks_per_ms,
    );
    if detailed() {
        let slot = s.longest_slot;
        if let Some([preparation, title, compare, mixed]) = s.longest_sections {
            let single_ms = |ticks| ticks as f64 / spans.ticks_per_ms;
            tracing::info!(
                "longest GPU submission by list: preparation {:.2} ms, title {:.2} ms, compare {:.2} ms, mixed {:.2} ms; fence {}, lists {}, groups {}, coverage {}/{}",
                single_ms(preparation), single_ms(title), single_ms(compare), single_ms(mixed),
                slot.fence, slot.list_count, slot.len, s.detailed, s.count,
            );
        } else if s.count != 0 {
            let reason = if slot.len == 0 { "no list markers" } else { "invalid or missing boundary timestamp" };
            tracing::info!("longest GPU submission unclassified by list: {reason}; fence {}, lists {}, groups {}, coverage {}/{}",
                slot.fence, slot.list_count, slot.len, s.detailed, s.count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusOps, MAX_SECTIONS, Role, Sums, partition, partition_focus, section_ticks};

    #[test]
    fn submissions_add_up_without_counting_anything_twice() {
        let mut s = Sums::default();
        s.add(100, 140, 150);
        s.add(140, 190, 200); // began before the last one ended
        s.add(260, 270, 270);
        assert_eq!((s.count, s.busy, s.between, s.longest, s.last), (3, 110, 60, 50, 270));
        // Never written, or nonsense: nothing.
        s.add(0, 0, 0);
        s.add(300, 305, 290);
        assert_eq!(s.count, 3);
        // Whole inside the last one.
        s.add(265, 267, 268);
        assert_eq!((s.count, s.busy, s.between), (4, 110, 60));
        assert_eq!((s.tail, s.longest_tail), (20, 10));
    }

    #[test]
    fn sections_follow_the_original_order_and_do_not_recount_overlaps() {
        let roles = [Some(Role::Preparation), Some(Role::Title), Some(Role::Preparation), Some(Role::Compare)];
        assert_eq!(section_ticks(&[10, 30, 100, 110, 115], &roles, 20), Some([20, 70, 5, 0]));
        assert_eq!(section_ticks(&[10, 30, 0, 110, 115], &roles, 20), None);
    }

    #[test]
    fn command_inventory_keeps_counts_after_its_signature_limit() {
        let mut ops = FocusOps::default();
        for n in 0..65 {
            ops.note(format!("dispatch {n}"));
        }
        ops.note("dispatch 0".into());
        let (total, top) = ops.summary();
        assert_eq!(total, 66);
        assert_eq!(ops.omitted, 1);
        assert_eq!(top[0], ("dispatch 0", 2));
    }

    #[test]
    fn rotating_focus_covers_each_list_with_a_bounded_marker_count() {
        let mut roles = [Role::Title; 47];
        roles[0] = Role::Preparation;
        roles[46] = Role::Compare;
        let mut covered = Vec::new();
        for turn in 0..10 {
            let slot = partition_focus(&roles, turn as u64, turn);
            assert!(slot.len <= MAX_SECTIONS);
            assert_eq!(slot.ends[slot.compare_at - 1], 46);
            assert_eq!(slot.ends[slot.len - 1], 47);
            let (first, last) = slot.focus.unwrap();
            assert!(slot.ends[..slot.len].windows(2).all(|pair| pair[0] < pair[1]));
            for index in first..last {
                assert!(slot.ends[..slot.len].contains(&(index + 1)));
            }
            covered.extend(first..last);
        }
        assert_eq!(covered, (0..46).collect::<Vec<_>>());
    }

    #[test]
    fn long_submissions_retain_order_and_keep_compare_separate() {
        let mut roles = [Role::Title; 19];
        roles[0] = Role::Preparation;
        roles[18] = Role::Compare;
        let plan = partition(&roles, roles.len(), 42);
        assert_eq!((plan.len, plan.compare_at, plan.fence), (MAX_SECTIONS, MAX_SECTIONS - 1, 42));
        assert_eq!((plan.ends[6], plan.ends[7]), (18, 19));
        assert_eq!((plan.roles[0], plan.roles[7]), (Some(Role::Mixed), Some(Role::Compare)));
        assert!(plan.ends.windows(2).all(|ends| ends[0] < ends[1]));
        let compare_only = partition(&[Role::Compare], 1, 43);
        assert_eq!((compare_only.len, compare_only.compare_at), (1, 0));
    }
}
