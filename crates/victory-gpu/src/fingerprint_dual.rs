//! Passive validation of texture fingerprint reads in the graphics alias reservation.
use std::ffi::c_void;
use std::io::{BufWriter, Write};
use std::sync::{LazyLock, Mutex, atomic::{AtomicU64, AtomicUsize, Ordering}};
use windows_sys::Win32::System::{
    Diagnostics::Debug::ReadProcessMemory,
    Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_RESERVE},
    Threading::GetCurrentProcess,
};

const PAGE:usize=64<<10;
const WINDOW:usize=16usize<<30;
static BASE:AtomicUsize=AtomicUsize::new(0);
static ID:AtomicU64=AtomicU64::new(0);
static DONE:AtomicU64=AtomicU64::new(0);
static PARTIAL:AtomicU64=AtomicU64::new(0);
static FILE:LazyLock<Option<Mutex<BufWriter<std::fs::File>>>>=LazyLock::new(||{
    let path=std::env::var_os("VICTORY_FINGERPRINT_DUAL")?;
    let mut f=BufWriter::new(std::fs::File::create(path).expect("fingerprint output"));
    writeln!(f,"fid\tpart\tparts\tqpc_begin\tqpc_end\tat\tlen\tsource\tbytes\tepoch_begin\tepoch_end\tstatus\tcross\thole_bytes\talias_bytes\tunresolved_bytes\tmismatch_bytes\trecheck_changed_bytes\treinstalled_pages\trebacked_pages\tpreviously_mapped_hole_pages\tref_nonzero_bytes\talias_nonzero_bytes\talias_read_fail_bytes\treference_raw_fail_bytes\tfirst_difference").unwrap();
    f.flush().unwrap();
    Some(Mutex::new(f))
});

pub fn register_window(at:usize) {
    if FILE.is_some() {
        assert!(BASE.compare_exchange(0,at,Ordering::AcqRel,Ordering::Acquire).is_ok() || BASE.load(Ordering::Acquire)==at);
        tracing::info!(at,size=WINDOW,"fingerprint dual reservation");
    }
}

pub(crate) fn in_window(at:usize)->bool {
    let b=BASE.load(Ordering::Acquire);
    b!=0 && at.wrapping_sub(b)<WINDOW
}
pub(crate) fn checkpoint()->(u64,u64,u64) {
    if let Some(file)=FILE.as_ref(){
        let mut f=file.lock().unwrap_or_else(|e|e.into_inner());
        f.flush().unwrap();
        (ID.load(Ordering::Acquire),DONE.load(Ordering::Acquire),PARTIAL.load(Ordering::Acquire))
    }else{(0,0,0)}
}
fn clock()->i64{
    let mut t=0;
    unsafe{windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut t)};
    t
}
fn read_exact(at:usize,data:&mut [u8])->bool{
    let mut done=0;
    unsafe{ReadProcessMemory(GetCurrentProcess(),at as *const c_void,data.as_mut_ptr().cast(),data.len(),&mut done)!=0 && done==data.len()}
}
fn reserved(at:usize,len:usize)->bool{
    let mut q:MEMORY_BASIC_INFORMATION=unsafe{std::mem::zeroed()};
    let n=unsafe{VirtualQuery(at as *const _,&mut q,std::mem::size_of_val(&q))};
    n==std::mem::size_of_val(&q) && q.State==MEM_RESERVE && (q.BaseAddress as usize).saturating_add(q.RegionSize)>=at+len
}

#[derive(Default)]
struct Span {
    source:usize, bytes:usize, cross:bool, holes:usize, aliases:usize, unresolved:usize,
    diffs:usize, changed:usize, installs:usize, rebacks:usize, hole_history:usize,
    positive:usize, alt_positive:usize, alt_fail:usize, ref_fail:usize, first:Option<usize>,
}
pub(crate) struct Probe {
    id:u64,at:usize,len:usize,begin:i64,epoch:u64,guard:Option<crate::watch::Reading>,spans:Vec<Span>,
}
impl Probe {
    pub(crate) fn begin(at:usize,len:usize)->Option<Self> {
        if len==0 {return None;}
        let base=BASE.load(Ordering::Acquire);
        let last=at.checked_add(len-1)?;
        if base==0 || at>=base+WINDOW || last<base {return None;}
        if at<base || last>=base+WINDOW { PARTIAL.fetch_add(1,Ordering::Relaxed);return None; }
        let id=ID.fetch_add(1,Ordering::Relaxed)+1;
        let begin=clock();
        let guard=crate::watch::title_memory_reading_now();
        let epoch=crate::watch::changes();
        Some(Self{id,at,len,begin,epoch,guard,spans:Vec::with_capacity(3)})
    }
    pub(crate) fn observe(&mut self,from:usize,reference:&[u8]){
        let source=self.at+from;
        let mut s=Span{source,bytes:reference.len(),cross:source/PAGE!=(source+reference.len()-1)/PAGE,..Span::default()};
        if self.guard.is_none(){s.unresolved=s.bytes;self.spans.push(s);return;}
        s.positive=reference.iter().filter(|&&b|b!=0).count();
        let mut alternate=vec![0;reference.len()];
        let mut offset=0;
        while offset<reference.len(){
            let at=source+offset;
            let page=at&!(PAGE-1);
            let within=at-page;
            let size=(PAGE-within).min(reference.len()-offset);
            let range=offset..offset+size;
            let (mapped,other,installs,rebounds)=crate::inplace::fingerprint_page(page);
            s.installs+=usize::from(installs>1);
            s.rebacks+=usize::from(rebounds>0);
            let target=&mut alternate[range.clone()];
            let primary=&reference[range];
            let verified=if mapped {
                other.is_some_and(|other|{
                    // RPM success rules out two independently failed zero-filled reads.
                    if !read_exact(other+within,target){s.alt_fail+=size;return false;}
                    let mut check=vec![0;size];
                    if !read_exact(at,&mut check){s.ref_fail+=size;return false;}
                    s.changed+=primary.iter().zip(check.iter()).filter(|(a,b)|a!=b).count();
                    s.aliases+=size;
                    s.alt_positive+=target.iter().filter(|&&b|b!=0).count();
                    true
                })
            } else if reserved(at,size) {
                s.holes+=size;s.hole_history+=usize::from(installs>0);true
            } else {false};
            if verified {
                for (i,(a,b)) in primary.iter().zip(target.iter()).enumerate(){
                    if a!=b {s.diffs+=1;s.first.get_or_insert(at+i);}
                }
            }else{s.unresolved+=size;}
            offset+=size;
        }
        self.spans.push(s);
    }
    pub(crate) fn finish(mut self){
        let end_epoch=crate::watch::changes();
        let end=clock();
        let status=if self.guard.is_none(){"busy"}else if end_epoch!=self.epoch || self.epoch%2!=0 {"generation_changed"}else{"checked"};
        drop(self.guard.take());
        if let Some(file)=FILE.as_ref(){
            let mut f=file.lock().unwrap_or_else(|e|e.into_inner());
            for (i,s) in self.spans.iter().enumerate(){
                writeln!(f,"{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    self.id,i,self.spans.len(),self.begin,end,self.at,self.len,s.source,s.bytes,self.epoch,end_epoch,status,u8::from(s.cross),s.holes,s.aliases,s.unresolved,s.diffs,s.changed,s.installs,s.rebacks,s.hole_history,s.positive,s.alt_positive,s.alt_fail,s.ref_fail,s.first.unwrap_or(0)).unwrap();
            }
            if self.id.is_multiple_of(32){f.flush().unwrap();}
            DONE.fetch_add(1,Ordering::Release);
        }
    }
}
