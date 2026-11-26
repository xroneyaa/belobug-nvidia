//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! All library calls are serialised: the interface does not promise that
//! stores, loads and serialising may overlap.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Blob,
    /// Pipelines stored since the last save.
    added: u64,
}

// SAFETY: D3D12 objects are free-threaded; every call is made under the
// mutex that holds the library.
unsafe impl Send for Library {}

const LIBRARY_FILE: &str = "host-pipelines.bin";
const NEXT_FILE: &str = "host-pipelines.next";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

/// Whether the library file is memory-mapped rather than copied into heap
/// buffers (`VICTORY_LIBRARY_MAP=0` disables mapping).
fn mapped() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_MAP").is_none_or(|v| v != "0"))
}

/// Whole file mapped into virtual memory for reading (`Mapped::open`) or
/// writing (`Mapped::create`). Unmapped when dropped.
struct Mapped {
    at: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    /// Keeps the backing file handle open while mapped on Windows so replacing
    /// `host-pipelines.bin` with a newer save does not zero unpaged file blocks.
    _file: Option<std::fs::File>,
}

// SAFETY: the view is read-only when opened or uniquely owned when created, and
// unmapped once in `Drop`.
unsafe impl Send for Mapped {}

impl Mapped {
    fn map(file: &std::fs::File, len: usize, write: bool) -> Option<Mapped> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READONLY,
            PAGE_READWRITE,
        };
        if len == 0 {
            return None;
        }
        // SAFETY: `file` is valid and stays open across mapping creation; the
        // view holds a kernel reference until `UnmapViewOfFile`.
        let at = unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                if write { PAGE_READWRITE } else { PAGE_READONLY },
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return None;
            }
            let at = MapViewOfFile(
                mapping,
                if write { FILE_MAP_WRITE } else { FILE_MAP_READ },
                0,
                0,
                0,
            );
            CloseHandle(mapping);
            at
        };
        (!at.Value.is_null()).then_some(Mapped {
            at,
            len,
            _file: None,
        })
    }

    fn open(path: &std::path::Path) -> Option<Mapped> {
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        let mut mapped = Mapped::map(&file, len, false)?;
        mapped._file = Some(file);
        Some(mapped)
    }

    fn create(path: &std::path::Path, len: usize) -> Option<Mapped> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .ok()?;
        file.set_len(len as u64).ok()?;
        Mapped::map(&file, len, true)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the view holds `len` valid bytes until dropped.
        unsafe { std::slice::from_raw_parts(self.at.Value.cast(), self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: writable view of `len` bytes created exclusively by `Mapped::create`.
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: valid view created by `MapViewOfFile`.
        unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.at) };
    }
}

enum Blob {
    None,
    Held(Vec<u8>),
    Mapped(Mapped),
}

impl Blob {
    fn of(dir: &std::path::Path) -> Blob {
        let path = dir.join(LIBRARY_FILE);
        let next = dir.join(NEXT_FILE);
        if next.exists() && std::fs::rename(&next, &path).is_err() {
            tracing::warn!("the host pipeline library saved last could not take the old one's place");
        }
        if mapped()
            && let Some(file) = Mapped::open(&path)
        {
            return Blob::Mapped(file);
        }
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => Blob::Held(bytes),
            _ => Blob::None,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Blob::None => &[],
            Blob::Held(bytes) => bytes,
            Blob::Mapped(file) => file.bytes(),
        }
    }

    fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }
}

fn library() -> Option<&'static Mutex<Library>> {
    static LIBRARY: OnceLock<Option<Mutex<Library>>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let blob = DIR.get().map_or(Blob::None, |d| Blob::of(d));
            let library = open(&device, blob);
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library.map(Mutex::new)
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Blob) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?.cast()
        }
    };
    if !blob.is_empty() {
        match make(blob.bytes()) {
            Ok(library) => {
                tracing::info!(
                    bytes = blob.bytes().len(),
                    mapped = matches!(blob, Blob::Mapped(_)),
                    "host pipeline library loaded"
                );
                return Some(Library { library, _blob: blob, added: 0 });
            }
            Err(e) => tracing::info!(error = %e, "host pipeline library rejected; starting an empty one"),
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    drop(blob);
    let library = make(&[]).ok()?;
    Some(Library { library, _blob: Blob::None, added: 0 })
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root].into_iter().chain(code.iter().copied()).chain([state.as_bytes()]) {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// Pipelines a driver's compiler crashed on (names, one per line).
const CRASHED_FILE: &str = "crashed.txt";

/// The pipeline named `name` is left out from the next run on.
fn note_crashed(name: &str) {
    use std::io::Write;
    let Some(dir) = DIR.get() else { return };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(CRASHED_FILE));
    if let Ok(mut f) = file {
        let _ = writeln!(f, "{name}");
    }
}

/// Whether the driver crashed making the pipeline named `name` before.
fn crashed(name: &str) -> bool {
    static NAMES: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    NAMES
        .get_or_init(|| {
            DIR.get()
                .and_then(|d| std::fs::read_to_string(d.join(CRASHED_FILE)).ok())
                .map(|t| t.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        })
        .contains(name)
}

thread_local! {
    /// The pipeline the driver makes on this thread.
    static MAKING: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// The driver makes a pipeline on this thread. If its compiler crashes there
/// (AMD's does on a few of the world's compute shaders), the pipeline is
/// noted and left out from then on, and the thread parked: the others, and
/// the title, go on.
struct Making;

impl Making {
    fn new(name: &str) -> Making {
        use windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler;
        static HANDLER: std::sync::Once = std::sync::Once::new();
        // SAFETY: a handler that lives as long as the process.
        HANDLER.call_once(|| unsafe {
            AddVectoredExceptionHandler(1, Some(compiler_crashed));
        });
        MAKING.with(|m| name.clone_into(&mut m.borrow_mut()));
        endless(name, true);
        Making
    }
}

/// The process's private bytes.
fn committed() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a structure of the size passed.
    unsafe {
        let mut info: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let size = size_of_val(&info) as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut info, size) == 0 {
            return 0;
        }
        info.PagefileUsage as u64
    }
}

/// Workers (threads) lost to compiles that crashed, not replaced yet.
static LOST: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Suspends thread `id`, at work in the driver's compiler (never in a heap
/// function, whose lock it would keep); false if it was not caught there.
fn suspend(id: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, HMODULE};
    use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, GetThreadContext};
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleHandleExW, GetModuleHandleW,
    };
    use windows::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_SUSPEND_RESUME,
    };
    const CONTEXT_CONTROL_AMD64: u32 = 0x10_0001;
    #[repr(align(16))]
    struct Aligned(CONTEXT);
    // SAFETY: plain calls on a thread of ours; the context is whole and
    // aligned; nothing that may need the suspended thread's locks is called
    // while it is suspended (the modules are asked about after it runs on).
    unsafe {
        let Ok(thread) = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, false, id) else {
            return false;
        };
        let module = |at: u64| {
            let mut m = HMODULE::default();
            let _ = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(at as *const u16),
                &mut m,
            );
            m.0 as usize
        };
        let named = |s: &str| GetModuleHandleW(&HSTRING::from(s)).map_or(0, |m| m.0 as usize);
        let (ntdll, ucrt) = (named("ntdll.dll"), named("ucrtbase.dll"));
        let mut caught = false;
        let mut last = Vec::new();
        for _ in 0..500 {
            if SuspendThread(thread) == u32::MAX {
                break;
            }
            let mut context: Aligned = std::mem::zeroed();
            context.0.ContextFlags = CONTEXT_CONTROL_AMD64;
            let known = GetThreadContext(thread.0, &mut context.0) != 0;
            let at = context.0.Rip >> 12;
            if known && last.contains(&at) {
                caught = true;
                break;
            }
            ResumeThread(thread);
            // A page of the driver's own code: caught there next time.
            let m = module(at << 12);
            if known && m != 0 && m != ntdll && m != ucrt {
                last.push(at);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _ = CloseHandle(thread);
        caught
    }
}

/// Notes pipelines the driver's compiler has worked on for long
/// ([`ENDLESS`]): they are left out from the next run on. AMD's never ends
/// some of the world's ray tracing programs, and takes memory all the while
/// (16 GB in two minutes), so their threads are stopped.
fn endless(name: &str, begins: bool) {
    use std::time::{Duration, Instant};
    // Some long programs take a while (the upscaler's 10,000 instructions);
    // what takes memory all along goes first.
    const ENDLESS: Duration = Duration::from_secs(20);
    const EVEN_SO: Duration = Duration::from_secs(150);
    const GROWTH: u64 = 3 << 30;
    static AT_WORK: Mutex<Vec<(String, Instant, u32)>> = Mutex::new(Vec::new());
    static WATCH: std::sync::Once = std::sync::Once::new();
    {
        let mut at_work = AT_WORK.lock().unwrap_or_else(|e| e.into_inner());
        if begins {
            // SAFETY: a plain call.
            let id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            at_work.push((name.to_owned(), Instant::now(), id));
        } else if let Some(i) = at_work.iter().position(|w| w.0 == name) {
            at_work.swap_remove(i);
        }
    }
    WATCH.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("victory-compiles".into())
            .spawn(|| {
                let mut noted: Vec<String> = Vec::new();
                // The process's commit when a compile was first seen late.
                let mut late: Option<u64> = None;
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    let lost = std::mem::take(&mut *LOST.lock().unwrap_or_else(|e| e.into_inner()));
                    for id in lost {
                        super::make::worker_lost(id);
                    }
                    let long: Vec<(String, u32)> = AT_WORK
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .filter(|w| w.1.elapsed() > ENDLESS && !noted.contains(&w.0))
                        .map(|w| (w.0.clone(), w.2))
                        .collect();
                    if long.is_empty() {
                        late = None;
                        continue;
                    }
                    let now = committed();
                    let grown = now.saturating_sub(*late.get_or_insert(now)) > GROWTH;
                    let long: Vec<(String, u32)> = if grown {
                        late = None;
                        long
                    } else {
                        AT_WORK
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .iter()
                            .filter(|w| w.1.elapsed() > EVEN_SO && !noted.contains(&w.0))
                            .map(|w| (w.0.clone(), w.2))
                            .collect()
                    };
                    for (name, id) in long {
                        let stopped = suspend(id);
                        if stopped {
                            super::make::worker_lost(id);
                        }
                        tracing::error!(
                            name,
                            stopped,
                            "the driver's compiler does not end a pipeline: it is left out from the next run on"
                        );
                        note_crashed(&name);
                        noted.push(name);
                    }
                }
            });
    });
}

impl Drop for Making {
    fn drop(&mut self) {
        let name = MAKING.with(|m| std::mem::take(&mut *m.borrow_mut()));
        endless(&name, false);
    }
}

unsafe extern "system" fn compiler_crashed(
    info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    const ACCESS_VIOLATION: i32 = 0xc000_0005_u32 as i32;
    // SAFETY: the system passes a live record.
    let record = unsafe { &*(*info).ExceptionRecord };
    let name = MAKING.try_with(|m| m.try_borrow().map(|m| m.clone()).unwrap_or_default());
    let name = name.unwrap_or_default();
    // Only reads or writes of the first pages: our own guards' faults are
    // elsewhere.
    if record.ExceptionCode != ACCESS_VIOLATION
        || name.is_empty()
        || record.ExceptionInformation[1] >= 0x10000
    {
        return 0; // not ours
    }
    tracing::error!(name, "the driver's compiler crashed making a pipeline: it is left out");
    note_crashed(&name);
    endless(&name, false);
    // SAFETY: a plain call.
    let id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    LOST.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    if crashed(name) {
        // E_FAIL.
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005_u32 as i32)));
    }
    let create = || {
        let _making = Making::new(name);
        create()
    };
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    if let Ok(pso) = load(&library.lock().unwrap_or_else(|e| e.into_inner()).library, PCWSTR(wide.as_ptr())) {
        LOADED.fetch_add(1, Ordering::Relaxed);
        return Ok(pso);
    }
    // Compiled outside the lock: compiles take up to seconds.
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { l.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        l.added += 1;
    }
    Ok(pso)
}

/// Writes the library and the early list when `min_added` new pipelines were
/// stored since the last save and `SAVE_EVERY` has passed (or immediately when
/// `now` is true).
pub(crate) fn save(now: bool) {
    save_with(now, if now { 1 } else { 1 }, std::time::Duration::from_secs(20));
}

/// Periodic save called from background workers: skips serializing the full
/// library until at least `PERIODIC_MIN_ADDED` pipelines accumulate so multi-hundred-MB
/// `Serialize` passes do not stall shader compilation workers every 20 seconds.
pub(crate) fn save_periodic() {
    const PERIODIC_MIN_ADDED: u64 = 64;
    const PERIODIC_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
    save_with(false, PERIODIC_MIN_ADDED, PERIODIC_EVERY);
}

fn save_with(now: bool, min_added: u64, every: std::time::Duration) {
    // Held over serialisation and both writes, so overlapping saves cannot
    // replace files under each other.
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let (Some(dir), Some(library)) = (DIR.get(), library()) else { return };
    let mut last = if now {
        LAST.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match LAST.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        }
    };
    if !now && last.is_some_and(|t| t.elapsed() < every) {
        return;
    }
    let (bytes_written, saved_additions) = {
        let l = library.lock().unwrap_or_else(|e| e.into_inner());
        let serialize_library = l.added >= min_added && l.added > 0;
        let used_dirty = USED_CHANGED.load(Ordering::Relaxed);
        if !serialize_library && !used_dirty {
            return;
        }
        *last = Some(std::time::Instant::now());
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(error = %e, "host pipeline cache dir not created");
            return;
        }
        if !serialize_library {
            (0, 0)
        } else {
            let tmp = dir.join(LIBRARY_FILE).with_extension("tmp");
            // SAFETY: a plain query.
            let size = unsafe { l.library.GetSerializedSize() };
            if let Err(e) = serialise(&l.library, &tmp, size) {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                let _ = std::fs::remove_file(&tmp);
                return;
            }
            (size, l.added)
        }
    };
    let tmp = dir.join(LIBRARY_FILE).with_extension("tmp");
    let used_changed = USED_CHANGED.swap(false, Ordering::Relaxed);
    let used: Vec<String> = USED.lock().unwrap_or_else(|e| e.into_inner()).iter().flatten().cloned().collect();
    let library_written = if bytes_written > 0 {
        replace_library(dir, &tmp)
    } else {
        Ok(())
    };
    let written = library_written
        .and_then(|()| write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()));
    match written {
        Ok(()) => {
            let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
            // Keep pipelines stored while the files were being written.
            l.added = l.added.saturating_sub(saved_additions);
            tracing::info!(
                bytes = bytes_written,
                early = used.len(),
                made = MADE.load(Ordering::Relaxed),
                loaded = LOADED.load(Ordering::Relaxed),
                started_early = EARLY.load(Ordering::Relaxed),
                "host pipeline library saved"
            );
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            // At least one cache file was not published. Keep the counters
            // dirty for the next save.
            if used_changed {
                USED_CHANGED.store(true, Ordering::Relaxed);
            }
            tracing::warn!(error = %e, "host pipeline library not saved");
        }
    }
}

fn serialise(
    library: &ID3D12PipelineLibrary1,
    tmp: &std::path::Path,
    size: usize,
) -> std::io::Result<()> {
    let failed = |e: windows::core::Error| std::io::Error::other(e.to_string());
    if mapped()
        && let Some(mut file) = Mapped::create(tmp, size)
    {
        // SAFETY: `file` is mapped for exactly `size` writable bytes.
        return unsafe { library.Serialize(file.bytes_mut()) }.map_err(failed);
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: a buffer of the size the library asks for.
    unsafe { library.Serialize(&mut bytes) }.map_err(failed)?;
    std::fs::write(tmp, &bytes)
}

fn replace_library(dir: &std::path::Path, tmp: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(tmp, dir.join(LIBRARY_FILE))
        .or_else(|_| std::fs::rename(tmp, dir.join(NEXT_FILE)))
}

/// Writes `bytes` to a unique temporary file beside `path`, then renames it
/// over `path`: a reader never sees a part, and overlapping saves do not
/// share a temporary file.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let extension = format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_extension(extension);
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Title pipeline keys drawn or dispatched with (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let library = open(&device, Blob::Held(b"not a pipeline library".repeat(16)))
            .expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(library._blob.is_empty());
    }

    #[test]
    fn mapped_library_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        let payload: Vec<u8> = (0..16 * 1024).map(|i| (i * 13 + 5) as u8).collect();
        {
            let mut writer = Mapped::create(&path, payload.len()).expect("mapped writer");
            writer.bytes_mut().copy_from_slice(&payload);
        }
        let reader = Mapped::open(&path).expect("mapped reader");
        assert_eq!(reader.bytes(), &payload[..]);
        drop(reader);
    }

    #[test]
    fn concurrent_atomic_writes_publish_one_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for thread in 0..8 {
            let path = path.clone();
            let start = start.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                let byte = thread as u8 + 1;
                for _ in 0..32 {
                    write_atomically(&path, &[byte; 64 * 1024]).unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let published = std::fs::read(&path).unwrap();
        assert_eq!(published.len(), 64 * 1024);
        assert!(published.iter().all(|&byte| byte == published[0]));
        assert!((1..=8).contains(&published[0]));
        assert!(!std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-")));
    }

    #[test]
    fn failed_atomic_writes_remove_their_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        std::fs::create_dir(&path).unwrap();

        assert!(write_atomically(&path, b"not published").is_err());
        assert!(!std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-")));
    }
}
