//! Opt-in dispatch timings, recorded inside a few original command lists.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

use super::{HostList, host};

const COMMANDS: u32 = 1024;

/// First timeline is a control; later independently timed windows collect operations.
pub(crate) fn broad() -> bool {
    if !crate::timeline::on() { return false; }
    static FIRST: OnceLock<std::time::Instant> = OnceLock::new();
    FIRST.get_or_init(std::time::Instant::now).elapsed().as_secs() >= 10
}

type Pending = Vec<(u64, usize, Arc<Sample>)>;
static PENDING: Mutex<Pending> = Mutex::new(Vec::new());
pub(super) fn collect(completed: u64, frequency: u64) {
    if crate::timeline::on() || completed == u64::MAX || frequency == 0 { return; }
    PENDING.lock().unwrap().retain(|(fence,index,sample)| {
        if *fence > completed { return true; }
        if sample.submissions.load(Relaxed) != 1 {
            tracing::info!(fence,index,"broad reused recording discarded");
        } else {
            sample.emit(*fence,*index,completed,frequency);
        }
        false
    });
}
pub(super) fn queue(runs: &[&HostList], fence: u64) {
    let mut pending = PENDING.lock().unwrap();
    for (ordinal,list) in runs.iter().enumerate() {
        if let Some(sample) = list.focus_dispatch_sample() { pending.push((fence,ordinal,sample)); }
    }
}


#[derive(Default)]
pub(super) enum Recording {
    #[default]
    Undecided,
    Skip,
    Sample(Arc<Sample>),
}

#[derive(Clone)]
struct Command {
    pipeline: String,
    groups: [u32; 3],
    load4_candidate: bool,
    before: Barriers,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Barriers {
    group_calls: u32,
    group_payloads: u32,
    sync_global: u32,
    sync_ranged: u32,
    legacy_global: u32,
    legacy_buffer: u32,
    legacy_named: u32,
    legacy_other: u32,
    adjacent_global: u32,
}

pub(crate) enum BarrierOp {
    Group { has_payload: bool },
    Sync { has_range: bool },
    Legacy { global: u32, buffer: u32, named: u32, other: u32, adjacent_global: u32 },
}

pub(super) struct Sample {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    commands: Mutex<Vec<Command>>,
    submissions: AtomicU32,
    tail: Mutex<Barriers>,
    recorded_frame: u64,
}

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::pipeline::host::hot_buffer_probe() || std::env::var_os("VICTORY_FOCUS_DISPATCH").is_some_and(|v| v == "1"))
}

fn selected(pipeline: &str) -> bool {
    static MATCHED: AtomicU32 = AtomicU32::new(0);
    let focus = [
        "47d8c225abfcb246", "6a6c7b7fde59134f", "32aa8384586e0f12",
        "528278df1274cb33", "8cbd7f5b9ce7530c", "8e614708143ef917",
        "f46117391dd81461", "f7fc6c984427fef3", "ba578b59c8c28a57",
    ];
    focus.iter().any(|id| pipeline.contains(id)) && MATCHED.fetch_add(1, Relaxed).is_multiple_of(11)
}

fn family_sampler() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PROFILE_19").is_some_and(|v| v == "1"))
}

fn barriers_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on() && std::env::var_os("VICTORY_FOCUS_BARRIERS").is_some_and(|v| v == "1"))
}

impl Sample {
    fn new() -> Option<Self> {
        let host = host()?;
        let mut heap = None;
        // SAFETY: plain object creation for this recording.
        unsafe {
            host.device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: COMMANDS * 2, NodeMask: 0 },
                &mut heap,
            ).ok()?;
        }
        Some(Self {
            heap: heap?,
            back: host.read_back_buffer(u64::from(COMMANDS) * 16)?,
            commands: Mutex::default(),
            submissions: AtomicU32::new(0),
            tail: Mutex::default(),
            recorded_frame: crate::frame::current(),
        })
    }

    fn emit(&self,fence:u64,index:usize,completed:u64,frequency:u64) {
        let cmds=self.commands.lock().unwrap();
        if cmds.is_empty() {return;}
        let count=cmds.len()*2;
        let range=D3D12_RANGE{Begin:0,End:count*8};
        let mut ptr=std::ptr::null_mut();
        let ticks=unsafe {
            if self.back.Map(0,Some(&range),Some(&mut ptr)).is_err(){return;}
            let v=std::slice::from_raw_parts(ptr.cast::<u64>(),count).to_vec();
            self.back.Unmap(0,Some(&D3D12_RANGE{Begin:0,End:0})); v
        };
        if ticks[0]==0 || ticks.windows(2).any(|s|s[1]<s[0]) {
            tracing::warn!(fence,index,"broad invalid times");return;
        }
        use std::collections::BTreeMap;
        let mut sums=BTreeMap::<&str,(u32,u64,u64)>::new();
        for (cmd,pair) in cmds.iter().zip(ticks.chunks_exact(2)) {
            let duration=pair[1]-pair[0];
            let v=sums.entry(&cmd.pipeline).or_default();
            v.0+=1; v.1+=duration; v.2=v.2.max(duration);
        }
        tracing::info!(fence,index,completed,frequency,first=ticks[0],last=ticks[count-1],
            record_frame=self.recorded_frame,commands=cmds.len(),capped=cmds.len()==COMMANDS as usize,"broad list");
        for (label,(calls,ticks,peak)) in sums {
            tracing::info!(fence,index,calls,ticks,peak,label=%label,"broad operation");
        }
    }

    pub(super) fn submitted(&self) {
        self.submissions.fetch_add(1, Relaxed);
    }

    /// After the enclosing queue fence. A reused recording can overwrite its
    /// queries, so a sample submitted more than once is discarded.
    pub(super) fn report(&self, fence: u64, index: usize, list_ms: Option<f64>, ticks_per_ms: f64) -> bool {
        if self.submissions.load(Relaxed) != 1 {
            return false;
        }
        let commands = self.commands.lock().unwrap_or_else(|e| e.into_inner());
        let count = commands.len() * 2;
        if count == 0 {
            return false;
        }
        let mut ptr = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: count * 8 };
        // SAFETY: the caller checked the fence of this one submission.
        let ticks = unsafe {
            if self.back.Map(0, Some(&range), Some(&mut ptr)).is_err() {
                return false;
            }
            let ticks = std::slice::from_raw_parts(ptr.cast::<u64>(), count).to_vec();
            self.back.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            ticks
        };
        if ticks[0] == 0 || ticks.windows(2).any(|pair| pair[1] < pair[0]) {
            return false;
        }
        let mut prior = ticks[0];
        let mut dispatch_ticks = 0;
        for (op, (command, pair)) in commands.iter().zip(ticks.chunks_exact(2)).enumerate() {
            let took = pair[1] - pair[0];
            let gap = pair[0] - prior;
            prior = pair[1];
            dispatch_ticks += took;
            tracing::info!(fence, index, op, pipeline = %command.pipeline, groups = ?command.groups,
                dispatch_ms = took as f64 / ticks_per_ms, gap_ms = gap as f64 / ticks_per_ms, load4_candidate = command.load4_candidate, "GPU focus dispatch");
            if barriers_on() {
                tracing::info!(fence, index, op, prefix = op == 0, pipeline = %command.pipeline,
                    barriers = ?command.before, "GPU focus barriers before dispatch");
            }
        }
        if barriers_on() {
            let tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
            tracing::info!(fence, index, capped = commands.len() == COMMANDS as usize,
                barriers = ?*tail, "GPU focus barriers after final sampled dispatch");
        }
        let covered_ms = (prior - ticks[0]) as f64 / ticks_per_ms;
        if family_sampler() {
            tracing::info!(fence, index, record_frame = self.recorded_frame, covered_ms,
                dispatch_ms = dispatch_ticks as f64 / ticks_per_ms, commands = commands.len(),
                "GPU residency family recording");
        }
        if let Some(list_ms) = list_ms {
            tracing::info!(fence, index, list_ms, covered_ms, dispatch_ms = dispatch_ticks as f64 / ticks_per_ms,
                commands = commands.len(), "GPU focus dispatch coverage");
        }
        true
    }
}

impl HostList {
    pub(crate) fn focus_barrier(&self, operation: impl FnOnce() -> BarrierOp) {
        if !barriers_on() {
            return;
        }
        let mut counts = self.focus_barriers.lock().unwrap_or_else(|e| e.into_inner());
        match operation() {
            BarrierOp::Group { has_payload } => {
                counts.group_calls += 1;
                counts.group_payloads += u32::from(has_payload);
            }
            BarrierOp::Sync { has_range } => {
                if has_range { counts.sync_ranged += 1; } else { counts.sync_global += 1; }
            }
            BarrierOp::Legacy { global, buffer, named, other, adjacent_global } => {
                counts.legacy_global += global;
                counts.legacy_buffer += buffer;
                counts.legacy_named += named;
                counts.legacy_other += other;
                counts.adjacent_global += adjacent_global;
            }
        }
    }

    fn take_focus_barriers(&self) -> Barriers {
        if !barriers_on() {
            return Barriers::default();
        }
        std::mem::take(&mut *self.focus_barriers.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn focus_dispatch_begin(&self, pipeline: &str, groups: [u32; 3], load4_candidate: bool) -> Option<u32> {
        if !broad() {return None;}
        let mut recording=self.focus_dispatch.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*recording,Recording::Undecided){
            *recording=Sample::new().map_or(Recording::Skip,|sample| Recording::Sample(Arc::new(sample)));
        }
        let Recording::Sample(sample) = &*recording else { return None };
        let mut commands = sample.commands.lock().unwrap_or_else(|e| e.into_inner());
        if commands.len() >= COMMANDS as usize {
            return None;
        }
        let at = commands.len() as u32 * 2;
        commands.push(Command { pipeline: pipeline.to_owned(), groups, load4_candidate, before: self.take_focus_barriers() });
        // SAFETY: an open list, a heap kept alive by this recording.
        unsafe { self.list.EndQuery(&sample.heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
        Some(at + 1)
    }

    pub(crate) fn focus_dispatch_end(&self, at: u32) {
        if let Recording::Sample(sample) = &*self.focus_dispatch.lock().unwrap_or_else(|e| e.into_inner()) {
            // SAFETY: paired with focus_dispatch_begin on this open list.
            unsafe { self.list.EndQuery(&sample.heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
        }
    }

    pub(super) fn focus_dispatch_close(&self) {
        if !on() {
            return;
        }
        if let Recording::Sample(sample) = &*self.focus_dispatch.lock().unwrap_or_else(|e| e.into_inner()) {
            let count = sample.commands.lock().unwrap_or_else(|e| e.into_inner()).len() as u32 * 2;
            if count != 0 {
                *sample.tail.lock().unwrap_or_else(|e| e.into_inner()) = self.take_focus_barriers();
                // SAFETY: all queries in this range were written in this recording.
                unsafe { self.list.ResolveQueryData(&sample.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count, &sample.back, 0) };
            }
        }
    }

    pub(super) fn focus_dispatch_sample(&self) -> Option<Arc<Sample>> {
        match &*self.focus_dispatch.lock().unwrap_or_else(|e| e.into_inner()) {
            Recording::Sample(sample) => Some(sample.clone()),
            _ => None,
        }
    }
}
