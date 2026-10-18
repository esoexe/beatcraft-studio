//! Headless CLAP 1.x plugin host (effects slice).
//!
//! Implements the official CLAP C ABI directly (spec: github.com/free-audio/clap,
//! headers v1.2.x) so we depend on nothing but `libloading`. Scope of this
//! milestone, deliberately: **stereo audio effects** on the master buss.
//! Instruments, parameter automation, GUI embedding and plugin-delay
//! compensation are the following milestones — headless effects are the
//! 90%-value path.
//!
//! Parity note: `ClapFx` tiles *every* buffer it receives onto a global,
//! absolute 512-frame grid (`world_pos`), so offline render (one giant call)
//! and the streaming engine (bar-sized calls) make the exact same sequence of
//! `process()` calls with the exact same `steady_time` values. Bit-parity is
//! preserved with a plugin inserted, provided the plugin itself is
//! deterministic.

use anyhow::{Context, Result};
use std::{
    ffi::{c_void, CStr, CString},
    os::raw::c_char,
    path::{Path, PathBuf},
};

/// Internal processing grid; also the max_frames we declare at activate().
const TILE: usize = 512;

/* ─────────────────────────── ABI surface (spec-verified) ─────────────────────────── */

pub const CLAP_VERSION: ClapVersion = ClapVersion {
    major: 1,
    minor: 2,
    revision: 10,
};
const CLAP_PLUGIN_FACTORY_ID: &[u8] = b"clap.plugin-factory\0";
const CLAP_EXT_AUDIO_PORTS: &[u8] = b"clap.audio-ports\0";
/// plugin-features.h: an effect that transforms audio.
const CLAP_FEATURE_AUDIO_EFFECT: &str = "audio-effect";
/// plugin-features.h: a note event-driven sound generator.
const CLAP_FEATURE_INSTRUMENT: &str = "instrument";
const CLAP_EXT_NOTE_PORTS: &[u8] = b"clap.note-ports\0";
const CLAP_EVENT_NOTE_ON: u16 = 0;
const CLAP_EVENT_NOTE_OFF: u16 = 1;
const CLAP_NAME_SIZE: usize = 256;
const CLAP_INVALID_ID: u32 = u32::MAX;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClapVersion {
    pub major: u32,
    pub minor: u32,
    pub revision: u32,
}

#[repr(C)]
struct ClapPluginEntry {
    clap_version: ClapVersion,
    init: extern "C" fn(*const c_char) -> bool,
    deinit: extern "C" fn(),
    get_factory: extern "C" fn(*const c_char) -> *const c_void,
}

#[repr(C)]
struct ClapPluginFactory {
    get_plugin_count: extern "C" fn(*const ClapPluginFactory) -> u32,
    get_plugin_descriptor:
        extern "C" fn(*const ClapPluginFactory, u32) -> *const ClapPluginDescriptor,
    create_plugin: extern "C" fn(
        *const ClapPluginFactory,
        *const ClapHost,
        *const c_char,
    ) -> *const ClapPlugin,
}

#[repr(C)]
struct ClapPluginDescriptor {
    clap_version: ClapVersion,
    id: *const c_char,
    name: *const c_char,
    vendor: *const c_char,
    url: *const c_char,
    manual_url: *const c_char,
    support_url: *const c_char,
    version: *const c_char,
    description: *const c_char,
    /// Null-terminated array of C strings.
    features: *const *const c_char,
}

#[repr(C)]
struct ClapPlugin {
    desc: *const ClapPluginDescriptor,
    plugin_data: *mut c_void,
    init: extern "C" fn(*const ClapPlugin) -> bool,
    destroy: extern "C" fn(*const ClapPlugin),
    activate: extern "C" fn(*const ClapPlugin, f64, u32, u32) -> bool,
    deactivate: extern "C" fn(*const ClapPlugin),
    start_processing: extern "C" fn(*const ClapPlugin) -> bool,
    stop_processing: extern "C" fn(*const ClapPlugin),
    reset: extern "C" fn(*const ClapPlugin),
    process: extern "C" fn(*const ClapPlugin, *const ClapProcess) -> i32,
    get_extension: extern "C" fn(*const ClapPlugin, *const c_char) -> *const c_void,
    on_main_thread: extern "C" fn(*const ClapPlugin),
}

#[repr(C)]
struct ClapHost {
    clap_version: ClapVersion,
    host_data: *mut c_void,
    name: *const c_char,
    vendor: *const c_char,
    url: *const c_char,
    version: *const c_char,
    get_extension: extern "C" fn(*const ClapHost, *const c_char) -> *const c_void,
    request_restart: extern "C" fn(*const ClapHost),
    request_process: extern "C" fn(*const ClapHost),
    request_callback: extern "C" fn(*const ClapHost),
}

#[repr(C)]
struct ClapAudioPortInfo {
    id: u32,
    name: [c_char; CLAP_NAME_SIZE],
    flags: u32,
    channel_count: u32,
    port_type: *const c_char,
    in_place_pair: u32,
}

impl ClapAudioPortInfo {
    fn zeroed() -> Self {
        Self {
            id: 0,
            name: [0; CLAP_NAME_SIZE],
            flags: 0,
            channel_count: 0,
            port_type: std::ptr::null(),
            in_place_pair: CLAP_INVALID_ID,
        }
    }
}

#[repr(C)]
struct ClapPluginAudioPorts {
    count: extern "C" fn(*const ClapPlugin, bool) -> u32,
    get: extern "C" fn(*const ClapPlugin, u32, bool, *mut ClapAudioPortInfo) -> bool,
}

#[repr(C)]
struct ClapEventHeader {
    size: u32,
    time: u32,
    space_id: u16,
    type_: u16,
    flags: u32,
}

#[repr(C)]
struct ClapInputEvents {
    ctx: *mut c_void,
    size: extern "C" fn(*const ClapInputEvents) -> u32,
    get: extern "C" fn(*const ClapInputEvents, u32) -> *const ClapEventHeader,
}

#[repr(C)]
struct ClapOutputEvents {
    ctx: *mut c_void,
    try_push: extern "C" fn(*const ClapOutputEvents, *const ClapEventHeader) -> bool,
}

#[repr(C)]
struct ClapAudioBuffer {
    data32: *mut *mut f32,
    data64: *mut *mut f64,
    channel_count: u32,
    latency: u32,
    constant_mask: u64,
}

#[repr(C)]
struct ClapProcess {
    steady_time: i64,
    frames_count: u32,
    /// clap_event_transport* — free-running (null) for now.
    transport: *const c_void,
    audio_inputs: *const ClapAudioBuffer,
    audio_outputs: *mut ClapAudioBuffer,
    audio_inputs_count: u32,
    audio_outputs_count: u32,
    in_events: *const ClapInputEvents,
    out_events: *const ClapOutputEvents,
}

/// Holder that asserts thread-safety for `static`s containing raw pointers.
/// Used only for immutable statics; the pointers inside are never mutated.
struct SyncStatic<T>(T);
unsafe impl<T> Sync for SyncStatic<T> {}
unsafe impl<T> Send for SyncStatic<T> {}

/* ─────────────────────────── host registration ─────────────────────────── */

extern "C" fn host_get_extension(_host: *const ClapHost, _id: *const c_char) -> *const c_void {
    // No host extensions in this milestone. Optional extensions must be
    // gracefully absent per spec, and plugins cope with null here.
    std::ptr::null()
}
extern "C" fn host_noop(_host: *const ClapHost) {}

fn host_struct() -> &'static ClapHost {
    static HOST: std::sync::OnceLock<SyncStatic<ClapHost>> = std::sync::OnceLock::new();
    &HOST
        .get_or_init(|| {
            // Static strings are null-terminated literals; the version is leaked once.
            let version: &'static [u8] = Box::leak(
                CString::new(env!("CARGO_PKG_VERSION"))
                    .unwrap()
                    .into_bytes_with_nul()
                    .into_boxed_slice(),
            );
            SyncStatic(ClapHost {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: c"BeatCraft Studio".as_ptr(),
                vendor: c"esoexe".as_ptr(),
                url: c"https://github.com/esoexe/beatcraft-studio".as_ptr(),
                version: version.as_ptr() as *const c_char,
                get_extension: host_get_extension,
                request_restart: host_noop,
                request_process: host_noop,
                request_callback: host_noop,
            })
        })
        .0
}

/* ─────────────────────────── scanning (spec search paths) ─────────────────────────── */

/// Default OS search locations plus CLAP_PATH, recursively walked for `*.clap`.
pub fn scan_paths() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Ok(cp) = std::env::var("COMMONPROGRAMFILES") {
            roots.push(PathBuf::from(cp).join("CLAP"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            roots.push(
                PathBuf::from(local)
                    .join("Programs")
                    .join("Common")
                    .join("CLAP"),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        roots.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/CLAP"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".clap"));
        }
        roots.push(PathBuf::from("/usr/lib/clap"));
    }
    let sep = if cfg!(windows) { ';' } else { ':' };
    if let Ok(extra) = std::env::var("CLAP_PATH") {
        roots.extend(
            extra
                .split(sep)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    find_clap_candidates(&roots)
}

/// Recursively collect `.clap` files/bundles under the given roots (pure, testable).
pub fn find_clap_candidates(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        visited += 1;
        if visited > 10_000 {
            break; // paranoia: pathological directory trees
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("clap"))
            {
                found.push(path);
            } else if path.is_dir() {
                stack.push(path);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/* ─────────────────────────── descriptors ─────────────────────────── */

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub is_effect: bool,
    pub is_instrument: bool,
}

fn read_cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_string)
}

fn read_features(ptr: *const *const c_char) -> Vec<String> {
    let mut out = Vec::new();
    if ptr.is_null() {
        return out;
    }
    for i in 0..64 {
        let item = unsafe { *ptr.add(i) };
        if item.is_null() {
            break;
        }
        if let Some(s) = read_cstr(item) {
            out.push(s);
        }
    }
    out
}

fn descriptor_info(d: &ClapPluginDescriptor) -> Option<PluginInfo> {
    if d.clap_version.major < 1 {
        return None;
    }
    let features = read_features(d.features);
    Some(PluginInfo {
        id: read_cstr(d.id)?,
        name: read_cstr(d.name).unwrap_or_else(|| "Unnamed CLAP".into()),
        vendor: read_cstr(d.vendor).unwrap_or_default(),
        version: read_cstr(d.version).unwrap_or_default(),
        is_effect: features.iter().any(|f| f == CLAP_FEATURE_AUDIO_EFFECT),
        is_instrument: features.iter().any(|f| f == CLAP_FEATURE_INSTRUMENT),
    })
}

struct OwnedBundle {
    // Dropped first (declaration order): symbols must not outlive the library.
    _lib: libloading::Library,
    entry: &'static ClapPluginEntry,
}

/// # Safety
/// All function pointers used afterwards must stay within `.1`'s lifetime.
unsafe fn open_bundle(path: &Path) -> Result<OwnedBundle> {
    let lib = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("Could not load {}", path.display()))?;
    let entry: &'static ClapPluginEntry = unsafe {
        let sym: libloading::Symbol<'_, *const ClapPluginEntry> = lib.get(b"clap_entry")?;
        // SAFETY: `lib` is moved into OwnedBundle together with this reference,
        // and the library field is dropped before the reference ever could be,
        // so the pointee stays mapped for the reference's effective lifetime.
        (*sym).as_ref().context("clap_entry symbol is null")?
    };
    if entry.clap_version.major < 1 {
        anyhow::bail!("Unsupported CLAP ABI version {}", entry.clap_version.major);
    }
    Ok(OwnedBundle { _lib: lib, entry })
}

/// Load a bundle long enough to enumerate its plugin descriptors.
pub fn inspect(path: &Path) -> Result<Vec<PluginInfo>> {
    let bundle = unsafe { open_bundle(path)? };
    let path_c = CString::new(path.to_string_lossy().as_ref())?;
    if !(bundle.entry.init)(path_c.as_ptr()) {
        anyhow::bail!("Plugin entry init() failed for {}", path.display());
    }
    struct DeinitGuard<'e> {
        entry: &'e ClapPluginEntry,
    }
    impl Drop for DeinitGuard<'_> {
        fn drop(&mut self) {
            (self.entry.deinit)();
        }
    }
    let _guard = DeinitGuard {
        entry: bundle.entry,
    };
    let factory = (bundle.entry.get_factory)(CLAP_PLUGIN_FACTORY_ID.as_ptr() as *const c_char)
        as *const ClapPluginFactory;
    let Some(factory) = (unsafe { factory.as_ref() }) else {
        return Ok(Vec::new());
    };
    let count = (factory.get_plugin_count)(factory);
    let mut infos = Vec::new();
    for i in 0..count.min(1024) {
        let desc = (factory.get_plugin_descriptor)(factory, i);
        let Some(desc) = (unsafe { desc.as_ref() }) else {
            continue;
        };
        if let Some(info) = descriptor_info(desc) {
            infos.push(info);
        }
    }
    Ok(infos)
}

/// Scan all spec paths and return every *effect* plugin found, as
/// `(bundle path, descriptor)` pairs. Bundles that fail to load are skipped.
pub fn scan_effects() -> Vec<(PathBuf, PluginInfo)> {
    scan_paths()
        .into_iter()
        .filter_map(|p| inspect(&p).ok().map(|infos| (p, infos)))
        .flat_map(|(p, infos)| {
            infos
                .into_iter()
                .filter(|i| i.is_effect)
                .map(move |i| (p.clone(), i))
        })
        .collect()
}

/// Scan all spec paths and return every *instrument* plugin found.
pub fn scan_instruments() -> Vec<(PathBuf, PluginInfo)> {
    scan_paths()
        .into_iter()
        .filter_map(|p| inspect(&p).ok().map(|infos| (p, infos)))
        .flat_map(|(p, infos)| {
            infos
                .into_iter()
                .filter(|i| i.is_instrument)
                .map(move |i| (p.clone(), i))
        })
        .collect()
}

/* ─────────────────────────── instance: headless stereo FX ─────────────────────────── */

extern "C" fn empty_events_size(_list: *const ClapInputEvents) -> u32 {
    0
}
extern "C" fn empty_events_get(_l: *const ClapInputEvents, _idx: u32) -> *const ClapEventHeader {
    std::ptr::null()
}
extern "C" fn sink_events_try_push(
    _l: *const ClapOutputEvents,
    _e: *const ClapEventHeader,
) -> bool {
    true
}
static EMPTY_INPUT_EVENTS: SyncStatic<ClapInputEvents> = SyncStatic(ClapInputEvents {
    ctx: std::ptr::null_mut(),
    size: empty_events_size,
    get: empty_events_get,
});
static SINK_OUTPUT_EVENTS: SyncStatic<ClapOutputEvents> = SyncStatic(ClapOutputEvents {
    ctx: std::ptr::null_mut(),
    try_push: sink_events_try_push,
});

/// A live, activated CLAP effect instance on a stereo master buss.
///
/// All audio fed to [`ClapFx::process_chunk`] is interleaved f32 stereo; the
/// instance keeps an absolute frame counter so calls tile identically no
/// matter how the caller chunks the timeline (see module docs).
pub struct ClapFx {
    // Field order matters: destroyed plugin (Drop) first, then the bundle.
    plugin: *const ClapPlugin,
    stopped: bool,
    out_channels: usize,
    in_channels: usize,
    world_pos: usize,
    _bundle: OwnedBundle,
}
// SAFETY: ClapFx is created, driven and dropped on one thread at a time
// (the bounce thread or the streaming worker). The raw plugin pointer is
// only ever dereferenced from `process_chunk`/`Drop`, which take `&mut self`.
unsafe impl Send for ClapFx {}

impl ClapFx {
    /// Load `plugin_id` from the bundle at `path`, activate at `sr`.
    pub fn load(path: &Path, plugin_id: &str, sr: u32) -> Result<Self> {
        let bundle = unsafe { open_bundle(path)? };
        let path_c = CString::new(path.to_string_lossy().as_ref())?;
        if !(bundle.entry.init)(path_c.as_ptr()) {
            anyhow::bail!("entry init() failed for {}", path.display());
        }
        let factory = (bundle.entry.get_factory)(CLAP_PLUGIN_FACTORY_ID.as_ptr() as *const c_char)
            as *const ClapPluginFactory;
        let factory = unsafe { factory.as_ref() }.context("no plugin factory")?;
        let id_c = CString::new(plugin_id)?;
        let plugin = (factory.create_plugin)(factory, host_struct(), id_c.as_ptr());
        let plugin_ref = unsafe { plugin.as_ref() }.context("create_plugin returned null")?;
        if !(plugin_ref.init)(plugin_ref) {
            anyhow::bail!("plugin init failed for {}", plugin_id);
        }
        let (in_channels, out_channels) = inspect_ports(plugin_ref);
        if out_channels == 0 {
            (plugin_ref.destroy)(plugin_ref);
            anyhow::bail!("{} has no audio outputs", plugin_id);
        }
        if !(plugin_ref.activate)(plugin_ref, sr as f64, 32, TILE as u32) {
            (plugin_ref.destroy)(plugin_ref);
            anyhow::bail!("activate failed for {}", plugin_id);
        }
        if !(plugin_ref.start_processing)(plugin_ref) {
            (plugin_ref.deactivate)(plugin_ref);
            (plugin_ref.destroy)(plugin_ref);
            anyhow::bail!("start_processing failed for {}", plugin_id);
        }
        Ok(ClapFx {
            plugin,
            stopped: false,
            out_channels,
            in_channels,
            world_pos: 0,
            _bundle: bundle,
        })
    }

    /// Run `buf` (interleaved stereo f32) through the plugin in place.
    pub fn process_chunk(&mut self, buf: &mut [f32]) {
        if self.stopped {
            return;
        }
        let total = buf.len() / 2;
        let mut off = 0usize;
        while off < total {
            match self.sub_render(buf, off, total) {
                Some(n) => off += n,
                None => return, // poisoned — leave remaining audio dry
            }
        }
    }

    /// Render one tile starting at frame-offset `off` inside `buf`.
    /// Returns the tile width, or `None` if the plugin poisoned itself.
    fn sub_render(&mut self, buf: &mut [f32], off: usize, total: usize) -> Option<usize> {
        let grid = TILE - (self.world_pos % TILE);
        let n = grid.min(total - off);
        let plugin = unsafe { &*self.plugin };

        // planar inputs
        let mut in_l: Vec<f32> = Vec::new();
        let mut in_r: Vec<f32> = Vec::new();
        if self.in_channels > 0 {
            in_l.reserve(n);
            in_r.reserve(n);
            for i in 0..n {
                in_l.push(buf[(off + i) * 2]);
                in_r.push(buf[(off + i) * 2 + 1]);
            }
        }
        let mut out_bufs = vec![vec![0.0_f32; n]; self.out_channels.max(1)];
        let mut in_ptrs: Vec<*mut f32> = match self.in_channels {
            0 => Vec::new(),
            1 => vec![in_l.as_mut_ptr()],
            _ => vec![in_l.as_mut_ptr(), in_r.as_mut_ptr()],
        };
        let mut out_ptrs: Vec<*mut f32> = out_bufs.iter_mut().map(|b| b.as_mut_ptr()).collect();

        let in_buf = ClapAudioBuffer {
            data32: if in_ptrs.is_empty() {
                std::ptr::null_mut()
            } else {
                in_ptrs.as_mut_ptr()
            },
            data64: std::ptr::null_mut(),
            channel_count: self.in_channels as u32,
            latency: 0,
            constant_mask: 0,
        };
        let mut out_buf = ClapAudioBuffer {
            data32: out_ptrs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: self.out_channels.max(1) as u32,
            latency: 0,
            constant_mask: 0,
        };
        let process = ClapProcess {
            steady_time: self.world_pos as i64,
            frames_count: n as u32,
            transport: std::ptr::null(),
            audio_inputs: if self.in_channels > 0 {
                &in_buf
            } else {
                std::ptr::null()
            },
            audio_outputs: &mut out_buf,
            audio_inputs_count: u32::from(self.in_channels > 0),
            audio_outputs_count: 1,
            in_events: &EMPTY_INPUT_EVENTS.0,
            out_events: &SINK_OUTPUT_EVENTS.0,
        };
        let status = (plugin.process)(plugin, &process);
        self.world_pos += n;
        if status == 0 {
            // CLAP_PROCESS_ERROR: poison the instance, pass audio dry.
            self.stopped = true;
            eprintln!("CLAP plugin reported CLAP_PROCESS_ERROR; bypassing");
            return None;
        }
        // TAIL/SLEEP are ignored — we always have fresh input next call.
        let lch = &out_bufs[0];
        let rch = out_bufs.get(1).unwrap_or(lch);
        for (i, (&l, &r)) in lch.iter().zip(rch.iter()).enumerate() {
            buf[(off + i) * 2] = l;
            buf[(off + i) * 2 + 1] = r;
        }
        Some(n)
    }
}

impl Drop for ClapFx {
    fn drop(&mut self) {
        let plugin = unsafe { &*self.plugin };
        (plugin.stop_processing)(plugin);
        (plugin.deactivate)(plugin);
        (plugin.destroy)(plugin);
    }
}

/* ─────────────────────────── instruments: note events + ClapSynth ─────────────────────────── */

#[repr(C)]
struct ClapEventNote {
    header: ClapEventHeader,
    note_id: i32,
    port_index: i16,
    channel: i16,
    key: i16,
    velocity: f64,
}

#[repr(C)]
struct ClapNotePortInfo {
    id: u32,
    supported_dialects: u32,
    preferred_dialect: u32,
    name: [c_char; CLAP_NAME_SIZE],
}

#[repr(C)]
struct ClapPluginNotePorts {
    count: extern "C" fn(*const ClapPlugin, bool) -> u32,
    get: extern "C" fn(*const ClapPlugin, u32, bool, *mut ClapNotePortInfo) -> bool,
}

/// A sample-accurate note on the *absolute* frame timeline of a bounce.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteEvent {
    pub abs_frame: usize,
    pub on: bool,
    pub key: u8,
    /// 0.0..=1.0 (mapped from MIDI velocity by the caller).
    pub velocity: f32,
}

struct EventsSlice {
    data: *const ClapEventNote,
    len: usize,
}
extern "C" fn note_events_size(list: *const ClapInputEvents) -> u32 {
    let s = unsafe { &*((*list).ctx as *const EventsSlice) };
    s.len as u32
}
extern "C" fn note_events_get(list: *const ClapInputEvents, index: u32) -> *const ClapEventHeader {
    let s = unsafe { &*((*list).ctx as *const EventsSlice) };
    if (index as usize) >= s.len {
        return std::ptr::null();
    }
    unsafe { s.data.add(index as usize) as *const ClapEventHeader }
}

/// Filter the absolute-timeline events down to one tile and convert to the
/// spec's `clap_event_note`, sorted by sample time (spec requirement). Events
/// carry note_id -1 / port 0 / channel 0 per the lowest-common-denominator
/// CLAP dialect.
fn notes_as_process_events(
    tile_abs: usize,
    tile_frames: usize,
    events: &[NoteEvent],
) -> Vec<ClapEventNote> {
    let mut out: Vec<ClapEventNote> = events
        .iter()
        .filter(|e| e.abs_frame >= tile_abs && e.abs_frame < tile_abs + tile_frames)
        .map(|e| ClapEventNote {
            header: ClapEventHeader {
                size: std::mem::size_of::<ClapEventNote>() as u32,
                time: (e.abs_frame - tile_abs) as u32,
                space_id: 0,
                type_: if e.on {
                    CLAP_EVENT_NOTE_ON
                } else {
                    CLAP_EVENT_NOTE_OFF
                },
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: e.key as i16,
            velocity: if e.on {
                e.velocity.clamp(0.0, 1.0) as f64
            } else {
                0.0
            },
        })
        .collect();
    out.sort_by_key(|e| e.header.time); // stable: same-sample events keep caller order
    out
}

/// The testable heart of [`ClapSynth`]: everything except dynamic loading.
/// Kept separate so unit tests can drive the production rendering machinery
/// against an in-process plugin.
struct SynthCore {
    plugin: *const ClapPlugin,
    out_channels: usize,
    world_pos: usize,
    stopped: bool,
}
// SAFETY: identical reasoning as ClapFx — single-threaded ownership at a time.
unsafe impl Send for SynthCore {}

impl SynthCore {
    fn new(plugin: *const ClapPlugin, out_channels: usize) -> Self {
        Self {
            plugin,
            out_channels,
            world_pos: 0,
            stopped: false,
        }
    }

    /// Advance the synth over the absolute window
    /// `[abs_start, abs_start + dst.len()/2)` starting from frame `abs_start`,
    /// mixing the fresh audio into `dst` (interleaved stereo).
    fn render_window(&mut self, dst: &mut [f32], abs_start: usize, events: &[NoteEvent]) {
        if abs_start != self.world_pos {
            // Callers must stream contiguously; resync deterministically rather
            // than emitting garbage. The two renderers never trigger this.
            debug_assert_eq!(abs_start, self.world_pos);
            self.world_pos = abs_start;
        }
        if self.stopped {
            return;
        }
        let total = dst.len() / 2;
        let mut off = 0usize;
        while off < total {
            match self.sub_render(dst, off, total, events) {
                Some(n) => off += n,
                None => return,
            }
        }
    }

    fn sub_render(
        &mut self,
        dst: &mut [f32],
        off: usize,
        total: usize,
        events: &[NoteEvent],
    ) -> Option<usize> {
        let grid = TILE - (self.world_pos % TILE);
        let n = grid.min(total - off);
        let tile_abs = self.world_pos;
        let plugin = unsafe { &*self.plugin };

        let evs = notes_as_process_events(tile_abs, n, events);
        let event_slice = EventsSlice {
            data: evs.as_ptr(),
            len: evs.len(),
        };
        let in_events = ClapInputEvents {
            ctx: &event_slice as *const EventsSlice as *mut c_void,
            size: note_events_size,
            get: note_events_get,
        };

        let mut out_bufs = vec![vec![0.0_f32; n]; self.out_channels.max(1)];
        let mut out_ptrs: Vec<*mut f32> = out_bufs.iter_mut().map(|b| b.as_mut_ptr()).collect();
        let mut out_buf = ClapAudioBuffer {
            data32: out_ptrs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: self.out_channels.max(1) as u32,
            latency: 0,
            constant_mask: 0,
        };
        let process = ClapProcess {
            steady_time: tile_abs as i64,
            frames_count: n as u32,
            transport: std::ptr::null(),
            audio_inputs: std::ptr::null(),
            audio_outputs: &mut out_buf,
            audio_inputs_count: 0,
            audio_outputs_count: 1,
            in_events: &in_events,
            out_events: &SINK_OUTPUT_EVENTS.0,
        };
        let status = (plugin.process)(plugin, &process);
        self.world_pos += n;
        if status == 0 {
            self.stopped = true;
            eprintln!("CLAP instrument reported CLAP_PROCESS_ERROR; muting");
            return None;
        }
        let lch = &out_bufs[0];
        let rch = out_bufs.get(1).unwrap_or(lch);
        for (i, (&l, &r)) in lch.iter().zip(rch.iter()).enumerate() {
            dst[(off + i) * 2] += l;
            dst[(off + i) * 2 + 1] += r;
        }
        Some(n)
    }
}

/// A live CLAP instrument driven by the project's note lanes. Shares the
/// FX instance's guarantees: global 512-frame tiling, absolute event
/// filtering, persistent across chunks — so offline and streaming bounces make
/// identical plugin calls and stay bit-identical.
pub struct ClapSynth {
    core: SynthCore,
    _bundle: OwnedBundle,
}
// SAFETY: same single-threaded discipline as ClapFx.
unsafe impl Send for ClapSynth {}

impl ClapSynth {
    /// Load and activate an instrument plugin at `sr`. Note ports are
    /// optional in the spec; when present they must accept the CLAP dialect,
    /// otherwise loading fails loudly instead of producing silence.
    pub fn load(path: &Path, plugin_id: &str, sr: u32) -> Result<Self> {
        let bundle = unsafe { open_bundle(path)? };
        let path_c = CString::new(path.to_string_lossy().as_ref())?;
        if !(bundle.entry.init)(path_c.as_ptr()) {
            anyhow::bail!("entry init() failed for {}", path.display());
        }
        let factory = (bundle.entry.get_factory)(CLAP_PLUGIN_FACTORY_ID.as_ptr() as *const c_char)
            as *const ClapPluginFactory;
        let factory = unsafe { factory.as_ref() }.context("no plugin factory")?;
        let id_c = CString::new(plugin_id)?;
        let plugin = (factory.create_plugin)(factory, host_struct(), id_c.as_ptr());
        let plugin_ref = unsafe { plugin.as_ref() }.context("create_plugin returned null")?;
        if !(plugin_ref.init)(plugin_ref) {
            anyhow::bail!("plugin init failed for {}", plugin_id);
        }

        // note port validation
        let note_ext =
            (plugin_ref.get_extension)(plugin_ref, CLAP_EXT_NOTE_PORTS.as_ptr() as *const c_char)
                as *const ClapPluginNotePorts;
        if let Some(ext) = unsafe { note_ext.as_ref() } {
            let count = (ext.count)(plugin_ref, true);
            if count > 0 {
                let mut info = ClapNotePortInfo {
                    id: 0,
                    supported_dialects: 0,
                    preferred_dialect: 0,
                    name: [0; CLAP_NAME_SIZE],
                };
                if (ext.get)(plugin_ref, 0, true, &mut info) && info.supported_dialects & 1 == 0 {
                    (plugin_ref.destroy)(plugin_ref);
                    anyhow::bail!("{} accepts no CLAP-dialect note events", plugin_id);
                }
            }
        }

        let (_in_channels, out_channels) = inspect_ports(plugin_ref);
        if out_channels == 0 {
            (plugin_ref.destroy)(plugin_ref);
            anyhow::bail!("{} has no audio outputs", plugin_id);
        }
        if !(plugin_ref.activate)(plugin_ref, sr as f64, 32, TILE as u32) {
            (plugin_ref.destroy)(plugin_ref);
            anyhow::bail!("activate failed for {}", plugin_id);
        }
        if !(plugin_ref.start_processing)(plugin_ref) {
            (plugin_ref.deactivate)(plugin_ref);
            (plugin_ref.destroy)(plugin_ref);
            anyhow::bail!("start_processing failed for {}", plugin_id);
        }
        Ok(ClapSynth {
            core: SynthCore::new(plugin, out_channels),
            _bundle: bundle,
        })
    }

    /// See [`SynthCore::render_window`].
    pub fn render_window(&mut self, dst: &mut [f32], abs_start: usize, events: &[NoteEvent]) {
        self.core.render_window(dst, abs_start, events);
    }
}

impl Drop for ClapSynth {
    fn drop(&mut self) {
        let plugin = unsafe { &*self.core.plugin };
        (plugin.stop_processing)(plugin);
        (plugin.deactivate)(plugin);
        (plugin.destroy)(plugin);
    }
}

fn inspect_ports(plugin: &ClapPlugin) -> (usize, usize) {
    let ext = (plugin.get_extension)(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr() as *const c_char)
        as *const ClapPluginAudioPorts;
    let Some(ext) = (unsafe { ext.as_ref() }) else {
        return (2, 2); // no extension: assume plain stereo in/out effect
    };
    let mut in_channels = 0usize;
    let mut out_channels = 0usize;
    let mut info = ClapAudioPortInfo::zeroed();
    if (ext.count)(plugin, true) > 0 && (ext.get)(plugin, 0, true, &mut info) {
        in_channels = (info.channel_count as usize).clamp(1, 2);
    }
    let mut info = ClapAudioPortInfo::zeroed();
    if (ext.count)(plugin, false) > 0 && (ext.get)(plugin, 0, false, &mut info) {
        out_channels = usize::min(8, info.channel_count as usize);
    }
    (in_channels, out_channels.max(1))
}

/* ─────────────────────────── tests: scanner + in-process ABI double ─────────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanning_finds_bundles_recursively_and_skips_non_plugins() {
        let root = std::env::temp_dir().join(format!("beatcraft-clap-scan-{}", std::process::id()));
        let nested = root.join("A").join("deep").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Fuzz Unit.clap"), b"fake").unwrap();
        std::fs::write(root.join("Noisy Neighbor.CLAP"), b"fake").unwrap();
        std::fs::write(root.join("not-a-plugin.dll"), b"fake").unwrap();
        let found = find_clap_candidates(std::slice::from_ref(&root));
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|p| p.ends_with("Fuzz Unit.clap")));
        assert!(found.iter().any(|p| p.ends_with("Noisy Neighbor.CLAP")));
        std::fs::remove_dir_all(&root).ok();
    }

    // ── A tiny but spec-honest plugin living inside the test binary ──

    extern "C" fn p_init(_p: *const ClapPlugin) -> bool {
        true
    }
    extern "C" fn p_destroy(_p: *const ClapPlugin) {}
    extern "C" fn p_activate(_p: *const ClapPlugin, sr: f64, mn: u32, mx: u32) -> bool {
        sr > 8_000.0 && mn <= TILE as u32 && mx >= TILE as u32
    }
    extern "C" fn p_deactivate(_p: *const ClapPlugin) {}
    extern "C" fn p_start(_p: *const ClapPlugin) -> bool {
        true
    }
    extern "C" fn p_stop(_p: *const ClapPlugin) {}
    extern "C" fn p_reset(_p: *const ClapPlugin) {}
    extern "C" fn p_on_main(_p: *const ClapPlugin) {}
    extern "C" fn p_get_extension(_p: *const ClapPlugin, id: *const c_char) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) }.to_bytes();
        if id == b"clap.audio-ports" {
            static PORTS: SyncStatic<ClapPluginAudioPorts> = SyncStatic(ClapPluginAudioPorts {
                count: ports_count,
                get: ports_get,
            });
            &PORTS.0 as *const ClapPluginAudioPorts as *const c_void
        } else {
            std::ptr::null()
        }
    }
    extern "C" fn ports_count(_p: *const ClapPlugin, _is_input: bool) -> u32 {
        1
    }
    extern "C" fn ports_get(
        _p: *const ClapPlugin,
        _index: u32,
        _is_input: bool,
        info: *mut ClapAudioPortInfo,
    ) -> bool {
        let info = unsafe { &mut *info };
        info.channel_count = 2;
        info.port_type = c"stereo".as_ptr();
        true
    }
    extern "C" fn p_process(_p: *const ClapPlugin, proc: *const ClapProcess) -> i32 {
        let proc = unsafe { &*proc };
        // Input event list must read as empty; that is the host contract.
        let in_events = unsafe { &*proc.in_events };
        assert_eq!((in_events.size)(in_events), 0);
        let n = proc.frames_count as usize;
        let inputs = unsafe { &*proc.audio_inputs };
        let outputs = unsafe { &mut *proc.audio_outputs };
        let in_l = unsafe { std::slice::from_raw_parts(*inputs.data32, n) };
        let in_r = unsafe { std::slice::from_raw_parts(*inputs.data32.add(1), n) };
        let out_l = unsafe { std::slice::from_raw_parts_mut(*outputs.data32, n) };
        let out_r = unsafe { std::slice::from_raw_parts_mut(*outputs.data32.add(1), n) };
        for ((ol, orr), (&l, &r)) in out_l
            .iter_mut()
            .zip(out_r.iter_mut())
            .zip(in_l.iter().zip(in_r.iter()))
        {
            *ol = l * 2.0;
            *orr = r * 2.0;
        }
        1 // CLAP_PROCESS_CONTINUE
    }

    /// Build the plugin + descriptor lazily (statics cannot self-reference).
    struct FakePlugin {
        _features: Box<[*const c_char; 2]>,
        _desc: Box<ClapPluginDescriptor>,
        plugin: Box<ClapPlugin>,
    }
    static FAKE: std::sync::OnceLock<SyncStatic<FakePlugin>> = std::sync::OnceLock::new();
    fn fake() -> &'static FakePlugin {
        &FAKE
            .get_or_init(|| {
                let features = Box::new([c"audio-effect".as_ptr(), std::ptr::null()]);
                let desc = Box::new(ClapPluginDescriptor {
                    clap_version: ClapVersion {
                        major: 1,
                        minor: 0,
                        revision: 0,
                    },
                    id: c"com.beatcraft.test-doubler".as_ptr(),
                    name: c"BeatCraft Test Doubler".as_ptr(),
                    vendor: std::ptr::null(),
                    url: std::ptr::null(),
                    manual_url: std::ptr::null(),
                    support_url: std::ptr::null(),
                    version: std::ptr::null(),
                    description: std::ptr::null(),
                    features: features.as_ptr(),
                });
                let plugin = Box::new(ClapPlugin {
                    desc: &*desc,
                    plugin_data: std::ptr::null_mut(),
                    init: p_init,
                    destroy: p_destroy,
                    activate: p_activate,
                    deactivate: p_deactivate,
                    start_processing: p_start,
                    stop_processing: p_stop,
                    reset: p_reset,
                    process: p_process,
                    get_extension: p_get_extension,
                    on_main_thread: p_on_main,
                });
                SyncStatic(FakePlugin {
                    _features: features,
                    _desc: desc,
                    plugin,
                })
            })
            .0
    }

    extern "C" fn factory_count(_f: *const ClapPluginFactory) -> u32 {
        1
    }
    extern "C" fn factory_descriptor(
        _f: *const ClapPluginFactory,
        index: u32,
    ) -> *const ClapPluginDescriptor {
        if index == 0 {
            &*fake()._desc
        } else {
            std::ptr::null()
        }
    }
    extern "C" fn factory_create(
        _f: *const ClapPluginFactory,
        host: *const ClapHost,
        id: *const c_char,
    ) -> *const ClapPlugin {
        let host = unsafe { &*host };
        assert_eq!(host.clap_version.major, 1);
        assert_eq!(
            unsafe { CStr::from_ptr(host.name) }.to_bytes(),
            b"BeatCraft Studio"
        );
        let requested = unsafe { CStr::from_ptr(id) }.to_bytes();
        assert_eq!(requested, b"com.beatcraft.test-doubler");
        // Host extensions are deliberately absent; plugin must see null.
        assert!((host.get_extension)(host, c"clap.log".as_ptr()).is_null());
        &*fake().plugin
    }
    static FACTORY: SyncStatic<ClapPluginFactory> = SyncStatic(ClapPluginFactory {
        get_plugin_count: factory_count,
        get_plugin_descriptor: factory_descriptor,
        create_plugin: factory_create,
    });

    extern "C" fn entry_init(_path: *const c_char) -> bool {
        true
    }
    extern "C" fn entry_deinit() {}
    extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) }.to_bytes();
        if id == b"clap.plugin-factory" {
            &FACTORY.0 as *const ClapPluginFactory as *const c_void
        } else {
            std::ptr::null()
        }
    }
    static ENTRY: SyncStatic<ClapPluginEntry> = SyncStatic(ClapPluginEntry {
        clap_version: ClapVersion {
            major: 1,
            minor: 2,
            revision: 0,
        },
        init: entry_init,
        deinit: entry_deinit,
        get_factory: entry_get_factory,
    });

    #[test]
    fn descriptor_round_trip_and_classification() {
        let info = descriptor_info(&fake()._desc).unwrap();
        assert_eq!(info.id, "com.beatcraft.test-doubler");
        assert_eq!(info.name, "BeatCraft Test Doubler");
        assert!(info.is_effect);
        let factory = (ENTRY.0.get_factory)(CLAP_PLUGIN_FACTORY_ID.as_ptr() as *const c_char);
        assert_eq!(
            factory as usize,
            &FACTORY.0 as *const ClapPluginFactory as usize
        );
    }

    /// Drives the fake plugin through the exact ClapProcess assembly ClapFx
    /// builds, including odd call sizes, and verifies the *global* tiling
    /// property: chunked and unchunked calls produce bit-identical output.
    #[test]
    fn host_process_assembly_is_chunk_invariant() {
        let plugin = &*fake().plugin;
        run(plugin, 44_100.0);
    }

    fn run(plugin: &ClapPlugin, sr: f64) {
        assert!((plugin.init)(plugin));
        assert!((plugin.activate)(plugin, sr, 32, TILE as u32));
        assert!((plugin.start_processing)(plugin));

        let frames = 2_000usize;
        let src: Vec<f32> = (0..frames * 2).map(|i| (i as f32).sin() * 0.25).collect();

        // Path A: one call of the whole buffer through ClapFx-equivalent tiling.
        let world_tiled = emulate_fx_tiles(plugin, &src, frames, frames);
        // Path B: caller chunks of 700 frames plus a remainder (bar-like).
        let mut caller_chunked = Vec::new();
        let mut off = 0usize;
        let mut chopper = FakeCallerFx {
            plugin,
            world_pos: 0,
        };
        while off < frames {
            let take = (frames - off).min(700);
            let mut seg = src[off * 2..(off + take) * 2].to_vec();
            chopper.process(&mut seg);
            caller_chunked.extend_from_slice(&seg);
            off += take;
        }
        assert_eq!(
            world_tiled.len(),
            caller_chunked.len(),
            "lengths must match"
        );
        assert_eq!(
            world_tiled, caller_chunked,
            "global tiling must be bit-identical"
        );
        // and the fake plugin did actually double the input
        for (i, v) in world_tiled.iter().enumerate() {
            assert!((v - src[i] * 2.0).abs() < 1e-6);
        }

        (plugin.stop_processing)(plugin);
        (plugin.deactivate)(plugin);
        (plugin.destroy)(plugin);
    }

    /// Mirroring ClapFx::process_chunk exactly: tiles onto the global 512 grid.
    fn emulate_fx_tiles(
        plugin: &ClapPlugin,
        src: &[f32],
        _frames: usize,
        _call: usize,
    ) -> Vec<f32> {
        let mut chopper = FakeCallerFx {
            plugin,
            world_pos: 0,
        };
        let mut out = src.to_vec();
        chopper.process(&mut out);
        out
    }

    struct FakeCallerFx<'p> {
        plugin: &'p ClapPlugin,
        world_pos: usize,
    }
    impl FakeCallerFx<'_> {
        fn process(&mut self, buf: &mut [f32]) {
            let total = buf.len() / 2;
            let mut off = 0usize;
            while off < total {
                let grid = TILE - (self.world_pos % TILE);
                let n = grid.min(total - off);
                self.one_tile(buf, off, n);
                self.world_pos += n;
                off += n;
            }
        }
        fn one_tile(&mut self, buf: &mut [f32], off: usize, n: usize) {
            let mut in_l: Vec<f32> = Vec::with_capacity(n);
            let mut in_r: Vec<f32> = Vec::with_capacity(n);
            for i in 0..n {
                in_l.push(buf[(off + i) * 2]);
                in_r.push(buf[(off + i) * 2 + 1]);
            }
            let mut out_l = vec![0.0_f32; n];
            let mut out_r = vec![0.0_f32; n];
            let mut in_ptrs = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
            let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
            let in_buf = ClapAudioBuffer {
                data32: in_ptrs.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut out_buf = ClapAudioBuffer {
                data32: out_ptrs.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let process = ClapProcess {
                steady_time: self.world_pos as i64,
                frames_count: n as u32,
                transport: std::ptr::null(),
                audio_inputs: &in_buf,
                audio_outputs: &mut out_buf,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: &EMPTY_INPUT_EVENTS.0,
                out_events: &SINK_OUTPUT_EVENTS.0,
            };
            assert_eq!((self.plugin.process)(self.plugin, &process), 1);
            for (i, (&l, &r)) in out_l.iter().zip(out_r.iter()).enumerate() {
                buf[(off + i) * 2] = l;
                buf[(off + i) * 2 + 1] = r;
            }
        }
    }

    // ── A spec-honest instrument: 128 sine voices, event-driven ──

    struct SynthState {
        log: Vec<usize>,
        voices: [bool; 128],
        phases: [f64; 128],
        freqs: [f64; 128],
        sr: f64,
    }
    static SYNTH_STATE: std::sync::Mutex<SynthState> = std::sync::Mutex::new(SynthState {
        log: Vec::new(),
        voices: [false; 128],
        phases: [0.0; 128],
        freqs: [0.0; 128],
        sr: 44_100.0,
    });
    /// Serializes the synth fixtures (tests share one global fake instance).
    static SYNTH_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    extern "C" fn syn_init(_p: *const ClapPlugin) -> bool {
        true
    }
    extern "C" fn syn_destroy(_p: *const ClapPlugin) {}
    extern "C" fn syn_activate(_p: *const ClapPlugin, sr: f64, _mn: u32, mx: u32) -> bool {
        *SYNTH_STATE.lock().unwrap() = SynthState {
            log: Vec::new(),
            voices: [false; 128],
            phases: [0.0; 128],
            freqs: [0.0; 128],
            sr,
        };
        sr > 8_000.0 && mx >= TILE as u32
    }
    extern "C" fn syn_deactivate(_p: *const ClapPlugin) {}
    extern "C" fn syn_start(_p: *const ClapPlugin) -> bool {
        true
    }
    extern "C" fn syn_stop(_p: *const ClapPlugin) {}
    extern "C" fn syn_reset(_p: *const ClapPlugin) {}
    extern "C" fn syn_on_main(_p: *const ClapPlugin) {}
    extern "C" fn syn_get_extension(_p: *const ClapPlugin, id: *const c_char) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) }.to_bytes();
        if id == b"clap.audio-ports" {
            static PORTS: SyncStatic<ClapPluginAudioPorts> = SyncStatic(ClapPluginAudioPorts {
                count: syn_audio_count,
                get: syn_audio_get,
            });
            &PORTS.0 as *const ClapPluginAudioPorts as *const c_void
        } else if id == b"clap.note-ports" {
            static PORTS: SyncStatic<ClapPluginNotePorts> = SyncStatic(ClapPluginNotePorts {
                count: syn_note_count,
                get: syn_note_get,
            });
            &PORTS.0 as *const ClapPluginNotePorts as *const c_void
        } else {
            std::ptr::null()
        }
    }
    extern "C" fn syn_audio_count(_p: *const ClapPlugin, is_input: bool) -> u32 {
        if is_input {
            0
        } else {
            1
        }
    }
    extern "C" fn syn_audio_get(
        _p: *const ClapPlugin,
        _i: u32,
        is_input: bool,
        info: *mut ClapAudioPortInfo,
    ) -> bool {
        if is_input {
            return false;
        }
        let info = unsafe { &mut *info };
        info.channel_count = 2;
        info.port_type = c"stereo".as_ptr();
        true
    }
    extern "C" fn syn_note_count(_p: *const ClapPlugin, is_input: bool) -> u32 {
        if is_input {
            1
        } else {
            0
        }
    }
    extern "C" fn syn_note_get(
        _p: *const ClapPlugin,
        _i: u32,
        is_input: bool,
        info: *mut ClapNotePortInfo,
    ) -> bool {
        if !is_input {
            return false;
        }
        let info = unsafe { &mut *info };
        info.supported_dialects = 1; // CLAP dialect only
        info.preferred_dialect = 1;
        true
    }
    fn synth_freq(key: i16) -> f64 {
        440.0 * 2f64.powf((key as f64 - 69.0) / 12.0)
    }
    extern "C" fn syn_process(_p: *const ClapPlugin, proc: *const ClapProcess) -> i32 {
        let proc = unsafe { &*proc };
        let n = proc.frames_count as usize;
        let inputs_ok = proc.audio_inputs_count == 0;
        assert!(inputs_ok, "synth must not receive audio inputs");
        let in_events = unsafe { &*proc.in_events };
        let ev_count = (in_events.size)(in_events) as usize;
        let outputs = unsafe { &mut *proc.audio_outputs };
        let out_l = unsafe { std::slice::from_raw_parts_mut(*outputs.data32, n) };
        let out_r = unsafe { std::slice::from_raw_parts_mut(*outputs.data32.add(1), n) };
        let steady = proc.steady_time as usize;
        let mut state = SYNTH_STATE.lock().unwrap();
        let mut cursor = 0usize;
        for (i, (ol, orr)) in out_l.iter_mut().zip(out_r.iter_mut()).enumerate() {
            while cursor < ev_count {
                let hdr = (in_events.get)(in_events, cursor as u32);
                let ev = unsafe { &*(hdr as *const ClapEventNote) };
                // spec: events are sample-sorted; stop at the first future event
                if ev.header.time as usize > i {
                    break;
                }
                state.log.push(steady + ev.header.time as usize);
                let key = ev.key as usize;
                if ev.header.type_ == CLAP_EVENT_NOTE_ON {
                    state.voices[key] = true;
                    state.phases[key] = 0.0;
                    state.freqs[key] = synth_freq(ev.key);
                } else if ev.header.type_ == CLAP_EVENT_NOTE_OFF {
                    state.voices[key] = false;
                }
                cursor += 1;
            }
            let mut acc = 0.0_f64;
            for k in 0..128 {
                if state.voices[k] {
                    acc += (state.phases[k] * std::f64::consts::TAU).sin() * 0.25;
                    state.phases[k] += state.freqs[k] / state.sr;
                }
            }
            *ol = acc as f32;
            *orr = acc as f32;
        }
        1 // CLAP_PROCESS_CONTINUE
    }

    struct FakeSynth {
        _features: Box<[*const c_char; 2]>,
        _desc: Box<ClapPluginDescriptor>,
        plugin: Box<ClapPlugin>,
    }
    static FAKE_SYNTH: std::sync::OnceLock<SyncStatic<FakeSynth>> = std::sync::OnceLock::new();
    fn fake_synth() -> &'static FakeSynth {
        &FAKE_SYNTH
            .get_or_init(|| {
                let features = Box::new([c"instrument".as_ptr(), std::ptr::null()]);
                let desc = Box::new(ClapPluginDescriptor {
                    clap_version: ClapVersion {
                        major: 1,
                        minor: 0,
                        revision: 0,
                    },
                    id: c"com.beatcraft.test-synth".as_ptr(),
                    name: c"BeatCraft Test Synth".as_ptr(),
                    vendor: std::ptr::null(),
                    url: std::ptr::null(),
                    manual_url: std::ptr::null(),
                    support_url: std::ptr::null(),
                    version: std::ptr::null(),
                    description: std::ptr::null(),
                    features: features.as_ptr(),
                });
                let plugin = Box::new(ClapPlugin {
                    desc: &*desc,
                    plugin_data: std::ptr::null_mut(),
                    init: syn_init,
                    destroy: syn_destroy,
                    activate: syn_activate,
                    deactivate: syn_deactivate,
                    start_processing: syn_start,
                    stop_processing: syn_stop,
                    reset: syn_reset,
                    process: syn_process,
                    get_extension: syn_get_extension,
                    on_main_thread: syn_on_main,
                });
                SyncStatic(FakeSynth {
                    _features: features,
                    _desc: desc,
                    plugin,
                })
            })
            .0
    }

    fn active_fake_synth() -> &'static ClapPlugin {
        let p = &*fake_synth().plugin;
        assert!((p.init)(p));
        assert!((p.activate)(p, 44_100.0, 32, TILE as u32));
        assert!((p.start_processing)(p));
        p
    }

    #[test]
    fn note_events_arrive_sample_accurately() {
        let _g = SYNTH_TEST.lock().unwrap();
        let plugin = active_fake_synth();
        let mut core = SynthCore::new(plugin, 2);
        let mut buf = vec![0.0_f32; 1000 * 2];
        core.render_window(
            &mut buf,
            0,
            &[
                NoteEvent {
                    abs_frame: 100,
                    on: true,
                    key: 60,
                    velocity: 0.8,
                },
                NoteEvent {
                    abs_frame: 500,
                    on: false,
                    key: 60,
                    velocity: 0.0,
                },
                NoteEvent {
                    abs_frame: 700,
                    on: true,
                    key: 64,
                    velocity: 1.0,
                },
            ],
        );
        let log = SYNTH_STATE.lock().unwrap().log.clone();
        assert_eq!(
            log,
            vec![100, 500, 700],
            "absolute event times must survive tiling"
        );
        assert!(
            buf[..100 * 2].iter().all(|v| *v == 0.0),
            "silence before first ON"
        );
        assert!(
            buf[100 * 2..500 * 2].iter().any(|v| v.abs() > 1e-3),
            "voice sounds in gate"
        );
        assert!(
            buf[500 * 2..700 * 2].iter().all(|v| v.abs() < 1e-7),
            "gate is silent between OFF and next ON"
        );
        assert!(
            buf[700 * 2..].iter().any(|v| v.abs() > 1e-3),
            "second note rings to end"
        );
        (plugin.stop_processing)(plugin);
        (plugin.deactivate)(plugin);
    }

    #[test]
    fn synth_output_is_chunk_invariant() {
        let _g = SYNTH_TEST.lock().unwrap();
        let events = vec![
            NoteEvent {
                abs_frame: 50,
                on: true,
                key: 48,
                velocity: 0.9,
            },
            NoteEvent {
                abs_frame: 900,
                on: true,
                key: 55,
                velocity: 0.9,
            },
            NoteEvent {
                abs_frame: 1_400,
                on: false,
                key: 48,
                velocity: 0.0,
            },
            NoteEvent {
                abs_frame: 2_600,
                on: true,
                key: 60,
                velocity: 0.7,
            },
            NoteEvent {
                abs_frame: 2_900,
                on: false,
                key: 55,
                velocity: 0.0,
            },
            NoteEvent {
                abs_frame: 3_500,
                on: false,
                key: 60,
                velocity: 0.0,
            },
        ];
        let total = 4_000usize;

        // Whole-timeline render through the production machinery.
        let plugin_a = active_fake_synth();
        let mut core_a = SynthCore::new(plugin_a, 2);
        let mut whole = vec![0.0_f32; total * 2];
        core_a.render_window(&mut whole, 0, &events);
        (plugin_a.stop_processing)(plugin_a);
        (plugin_a.deactivate)(plugin_a);

        // Same stream in four bar-like windows with the SAME events.
        let plugin_b = active_fake_synth();
        let mut core_b = SynthCore::new(plugin_b, 2);
        let mut chunked = vec![0.0_f32; total * 2];
        let mut abs = 0usize;
        for w in [900usize, 700, 1_100, total - 900 - 700 - 1_100] {
            core_b.render_window(&mut chunked[abs * 2..(abs + w) * 2], abs, &events);
            abs += w;
        }
        (plugin_b.stop_processing)(plugin_b);
        (plugin_b.deactivate)(plugin_b);

        assert_eq!(
            whole, chunked,
            "persistent instance + absolute events must be bit-identical across chunking"
        );
        assert!(whole.iter().any(|v| v.abs() > 1e-3));
    }
}
