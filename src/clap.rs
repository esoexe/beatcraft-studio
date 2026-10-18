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
const CLAP_EXT_PARAMS: &[u8] = b"clap.params\0";
const CLAP_EXT_LATENCY: &[u8] = b"clap.latency\0";
const CLAP_EVENT_NOTE_ON: u16 = 0;
const CLAP_EVENT_NOTE_OFF: u16 = 1;
const CLAP_EVENT_PARAM_VALUE: u16 = 5;
const CLAP_EVENT_MIDI: u16 = 10;
const CLAP_NOTE_DIALECT_CLAP: u32 = 1;
const CLAP_NOTE_DIALECT_MIDI: u32 = 2;
/// Max plugin latency we'll honour; beyond this something is pathological.
const MAX_LATENCY_PRIME: usize = 8192;
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

#[cfg(test)]
extern "C" fn empty_events_size(_list: *const ClapInputEvents) -> u32 {
    0
}
#[cfg(test)]
extern "C" fn empty_events_get(_l: *const ClapInputEvents, _idx: u32) -> *const ClapEventHeader {
    std::ptr::null()
}
extern "C" fn sink_events_try_push(
    _l: *const ClapOutputEvents,
    _e: *const ClapEventHeader,
) -> bool {
    true
}
#[cfg(test)]
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
    core: FxCore,
    _bundle: OwnedBundle,
}
// SAFETY: single-threaded ownership at a time (bounce or streaming worker).
unsafe impl Send for ClapFx {}

/// Backlinks shared by FX and instrument cores.
fn plugin_latency(plugin: &ClapPlugin) -> usize {
    let ext = (plugin.get_extension)(plugin, CLAP_EXT_LATENCY.as_ptr() as *const c_char)
        as *const ClapPluginLatency;
    unsafe { ext.as_ref() }
        .map(|l| (l.get)(plugin) as usize)
        .unwrap_or(0)
        .min(MAX_LATENCY_PRIME)
}

/// The testable heart of [`ClapFx`]; exists so unit tests can drive the
/// production rendering machinery with an in-process plugin.
struct FxCore {
    plugin: *const ClapPlugin,
    in_channels: usize,
    out_channels: usize,
    world_pos: usize,
    /// Declared plugin latency in frames; the master insert deliberately does
    /// *not* compensate (a master-bus delay shifts the whole bounce uniformly,
    /// which changes nothing audible). It is reported for diagnostics.
    latency: usize,
    stopped: bool,
    pending_params: Vec<(u32, f64)>,
}
// SAFETY: same single-threaded discipline as ClapFx.
unsafe impl Send for FxCore {}

impl FxCore {
    fn new(plugin: *const ClapPlugin, in_channels: usize, out_channels: usize) -> Self {
        Self {
            plugin,
            in_channels,
            out_channels,
            world_pos: 0,
            latency: 0,
            stopped: false,
            pending_params: Vec::new(),
        }
    }

    fn with_latency(
        plugin: *const ClapPlugin,
        in_channels: usize,
        out_channels: usize,
        latency: usize,
    ) -> Self {
        let mut core = Self::new(plugin, in_channels, out_channels);
        core.latency = latency.min(MAX_LATENCY_PRIME);
        core
    }

    fn set_param(&mut self, id: u32, value: f64) {
        self.pending_params.push((id, value));
    }

    fn process_chunk(&mut self, buf: &mut [f32]) {
        if self.stopped {
            return;
        }
        self.run_span(buf);
    }

    fn run_span(&mut self, buf: &mut [f32]) {
        let total = buf.len() / 2;
        let mut off = 0usize;
        while off < total {
            match self.sub_render(buf, off, total) {
                Some(n) => off += n,
                None => return,
            }
        }
    }

    fn sub_render(&mut self, buf: &mut [f32], off: usize, total: usize) -> Option<usize> {
        let grid = TILE - (self.world_pos % TILE);
        let n = grid.min(total - off);
        let plugin = unsafe { &*self.plugin };

        let mut set = EventSet::default();
        self.drain_params_into(&mut set);
        set.sort_and_index();
        let in_events = set.as_input();

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
            in_events: &in_events,
            out_events: &SINK_OUTPUT_EVENTS.0,
        };
        let status = (plugin.process)(plugin, &process);
        self.world_pos += n;
        if status == 0 {
            self.stopped = true;
            eprintln!("CLAP plugin reported CLAP_PROCESS_ERROR; bypassing");
            return None;
        }
        let lch = &out_bufs[0];
        let rch = out_bufs.get(1).unwrap_or(lch);
        for (i, (&l, &r)) in lch.iter().zip(rch.iter()).enumerate() {
            buf[(off + i) * 2] = l;
            buf[(off + i) * 2 + 1] = r;
        }
        Some(n)
    }

    fn drain_params_into(&mut self, set: &mut EventSet) {
        if !self.pending_params.is_empty() {
            self.pending_params.sort_by_key(|p| p.0);
            for (id, v) in self.pending_params.drain(..) {
                set.push_param(0, id, v);
            }
        }
    }
}

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
        let latency = plugin_latency(plugin_ref);
        Ok(ClapFx {
            core: FxCore::with_latency(plugin, in_channels, out_channels, latency),
            _bundle: bundle,
        })
    }

    /// Run `buf` (interleaved stereo f32) through the plugin in place.
    pub fn process_chunk(&mut self, buf: &mut [f32]) {
        self.core.process_chunk(buf);
    }

    /// Queue an initial parameter batch, delivered as time-0 PARAM_VALUE
    /// events on the first processed tile (deterministic, spec automation).
    pub fn apply_initial(&mut self, params: &[(u32, f64)]) {
        for &(id, v) in params {
            self.core.set_param(id, v);
        }
    }

    /// Queue a parameter change for the next processed tile.
    pub fn set_param(&mut self, id: u32, value: f64) {
        self.core.set_param(id, value);
    }

    /// Enumerate the plugin's user-facing parameters.
    pub fn list_params(&self) -> Vec<ParamInfo> {
        read_params(unsafe { &*self.core.plugin })
    }

    /// Declared plugin latency in frames (uncompensated on the master buss).
    pub fn latency(&self) -> usize {
        self.core.latency
    }
}

impl Drop for ClapFx {
    fn drop(&mut self) {
        let plugin = unsafe { &*self.core.plugin };
        (plugin.stop_processing)(plugin);
        (plugin.deactivate)(plugin);
        (plugin.destroy)(plugin);
    }
}

/* ─────────────────── instruments: unified events + ClapSynth ─────────────────── */

#[repr(C)]
struct ClapEventParamValue {
    header: ClapEventHeader,
    param_id: u32,
    cookie: *mut c_void,
    note_id: i32,
    port_index: i16,
    channel: i16,
    key: i16,
    value: f64,
}

#[repr(C)]
struct ClapEventMidi {
    header: ClapEventHeader,
    port_index: u16,
    data: [u8; 3],
}

#[repr(C)]
struct ClapPluginLatency {
    get: extern "C" fn(*const ClapPlugin) -> u32,
}

#[repr(C)]
struct ClapParamInfo {
    id: u32,
    flags: u32,
    cookie: *mut c_void,
    name: [c_char; CLAP_NAME_SIZE],
    module: [c_char; 1024], // CLAP_PATH_SIZE
    min_value: f64,
    max_value: f64,
    default_value: f64,
}

impl ClapParamInfo {
    fn zeroed() -> Self {
        Self {
            id: 0,
            flags: 0,
            cookie: std::ptr::null_mut(),
            name: [0; CLAP_NAME_SIZE],
            module: [0; 1024],
            min_value: 0.0,
            max_value: 0.0,
            default_value: 0.0,
        }
    }
}

#[repr(C)]
struct ClapPluginParams {
    count: extern "C" fn(*const ClapPlugin) -> u32,
    get_info: extern "C" fn(*const ClapPlugin, u32, *mut ClapParamInfo) -> bool,
    get_value: extern "C" fn(*const ClapPlugin, u32, *mut f64) -> bool,
    value_to_text: extern "C" fn(*const ClapPlugin, u32, f64, *mut c_char, u32) -> bool,
    text_to_value: extern "C" fn(*const ClapPlugin, u32, *const c_char, *mut f64) -> bool,
    flush: extern "C" fn(*const ClapPlugin, *const ClapInputEvents, *const ClapOutputEvents),
}

/// Backing storage for one tile's input event list: notes, parameter values,
/// and bridged MIDI, spec-sorted. Rebuilt per tile on the stack.
enum StoredEvent {
    Note(ClapEventNote),
    Param(ClapEventParamValue),
    Midi(ClapEventMidi),
}

impl StoredEvent {
    fn time(&self) -> u32 {
        match self {
            StoredEvent::Note(e) => e.header.time,
            StoredEvent::Param(e) => e.header.time,
            StoredEvent::Midi(e) => e.header.time,
        }
    }
}

#[derive(Default)]
struct EventSet {
    storage: Vec<StoredEvent>,
    // Rebuilt by sort_and_index(); raw pointers valid until the next mutation.
    headers: Vec<*const ClapEventHeader>,
}

impl EventSet {
    fn push_note(&mut self, time: u32, on: bool, key: u8, velocity: f64) {
        self.storage.push(StoredEvent::Note(ClapEventNote {
            header: ClapEventHeader {
                size: std::mem::size_of::<ClapEventNote>() as u32,
                time,
                space_id: 0,
                type_: if on {
                    CLAP_EVENT_NOTE_ON
                } else {
                    CLAP_EVENT_NOTE_OFF
                },
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: key as i16,
            velocity: if on { velocity } else { 0.0 },
        }));
    }

    fn push_param(&mut self, time: u32, param_id: u32, value: f64) {
        self.storage.push(StoredEvent::Param(ClapEventParamValue {
            header: ClapEventHeader {
                size: std::mem::size_of::<ClapEventParamValue>() as u32,
                time,
                space_id: 0,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id,
            cookie: std::ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        }));
    }

    fn push_midi(&mut self, time: u32, data: [u8; 3]) {
        self.storage.push(StoredEvent::Midi(ClapEventMidi {
            header: ClapEventHeader {
                size: std::mem::size_of::<ClapEventMidi>() as u32,
                time,
                space_id: 0,
                type_: CLAP_EVENT_MIDI,
                flags: 0,
            },
            port_index: 0,
            data,
        }));
    }

    /// Spec: input events must be sample-sorted. Stable, so events pushed
    /// earlier at the same sample keep their order (params before notes).
    fn sort_and_index(&mut self) {
        self.storage.sort_by_key(StoredEvent::time);
        self.headers = self
            .storage
            .iter()
            .map(|e| match e {
                StoredEvent::Note(e) => &e.header as *const ClapEventHeader,
                StoredEvent::Param(e) => &e.header as *const ClapEventHeader,
                StoredEvent::Midi(e) => &e.header as *const ClapEventHeader,
            })
            .collect();
    }

    fn as_input(&mut self) -> ClapInputEvents {
        ClapInputEvents {
            ctx: self as *mut EventSet as *mut c_void,
            size: event_set_size,
            get: event_set_get,
        }
    }
}

extern "C" fn event_set_size(list: *const ClapInputEvents) -> u32 {
    let set = unsafe { &*((*list).ctx as *const EventSet) };
    set.headers.len() as u32
}
extern "C" fn event_set_get(list: *const ClapInputEvents, index: u32) -> *const ClapEventHeader {
    let set = unsafe { &*((*list).ctx as *const EventSet) };
    set.headers
        .get(index as usize)
        .copied()
        .unwrap_or(std::ptr::null())
}

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

/// Which note dialect the instance speaks to its instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteDialect {
    Clap,
    Midi,
}

/// User-facing parameter descriptor (hidden/readonly/bypass params filtered).
#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
}

fn read_clap_string(buf: &[c_char]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let bytes: Vec<u8> = buf[..end].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Enumerate a plugin's user-facing parameters, or empty when the plugin
/// exports no params extension.
fn read_params(plugin: &ClapPlugin) -> Vec<ParamInfo> {
    const HIDE_FLAGS: u32 = (1 << 2) | (1 << 3) | (1 << 4); // hidden | readonly | bypass
    let ext = (plugin.get_extension)(plugin, CLAP_EXT_PARAMS.as_ptr() as *const c_char)
        as *const ClapPluginParams;
    let Some(ext) = (unsafe { ext.as_ref() }) else {
        return Vec::new();
    };
    let count = (ext.count)(plugin).min(4096);
    let mut out = Vec::new();
    for i in 0..count {
        let mut info = ClapParamInfo::zeroed();
        if !(ext.get_info)(plugin, i, &mut info) {
            continue;
        }
        if info.flags & HIDE_FLAGS != 0 {
            continue;
        }
        out.push(ParamInfo {
            id: info.id,
            name: read_clap_string(&info.name),
            module: read_clap_string(&info.module),
            min_value: info.min_value,
            max_value: info.max_value,
            default_value: info.default_value,
        });
    }
    out
}

/// Load a plugin just long enough to enumerate its parameters (params are
/// queryable while deactivated per spec).
pub fn inspect_params(path: &Path, plugin_id: &str) -> Result<Vec<ParamInfo>> {
    let bundle = unsafe { open_bundle(path)? };
    let path_c = CString::new(path.to_string_lossy().as_ref())?;
    if !(bundle.entry.init)(path_c.as_ptr()) {
        anyhow::bail!("entry init() failed for {}", path.display());
    }
    struct DeinitGuard<'e>(&'e ClapPluginEntry);
    impl Drop for DeinitGuard<'_> {
        fn drop(&mut self) {
            (self.0.deinit)();
        }
    }
    let _g = DeinitGuard(bundle.entry);
    let factory = (bundle.entry.get_factory)(CLAP_PLUGIN_FACTORY_ID.as_ptr() as *const c_char)
        as *const ClapPluginFactory;
    let factory = unsafe { factory.as_ref() }.context("no plugin factory")?;
    let id_c = CString::new(plugin_id)?;
    let plugin = (factory.create_plugin)(factory, host_struct(), id_c.as_ptr());
    let plugin_ref = unsafe { plugin.as_ref() }.context("create_plugin returned null")?;
    if !(plugin_ref.init)(plugin_ref) {
        anyhow::bail!("plugin init failed for {}", plugin_id);
    }
    struct DestroyGuard(*const ClapPlugin);
    impl Drop for DestroyGuard {
        fn drop(&mut self) {
            let p = unsafe { &*self.0 };
            (p.destroy)(p);
        }
    }
    let _pg = DestroyGuard(plugin);
    Ok(read_params(plugin_ref))
}

/// Filter the absolute-timeline notes into `set` for one tile, in whichever
/// dialect the instance negotiated.
fn push_notes(
    set: &mut EventSet,
    tile_abs: usize,
    tile_frames: usize,
    events: &[NoteEvent],
    dialect: NoteDialect,
) {
    for e in events
        .iter()
        .filter(|e| e.abs_frame >= tile_abs && e.abs_frame < tile_abs + tile_frames)
    {
        let time = (e.abs_frame - tile_abs) as u32;
        match dialect {
            NoteDialect::Clap => {
                let vel = if e.on {
                    e.velocity.clamp(0.0, 1.0) as f64
                } else {
                    0.0
                };
                set.push_note(time, e.on, e.key, vel);
            }
            NoteDialect::Midi => {
                let vel127 = if e.on {
                    (e.velocity.clamp(0.0, 1.0) * 127.0)
                        .round()
                        .clamp(1.0, 127.0) as u8
                } else {
                    0
                };
                set.push_midi(time, [if e.on { 0x90 } else { 0x80 }, e.key, vel127]);
            }
        }
    }
}

/// The testable heart of [`ClapSynth`]: everything except dynamic loading.
struct SynthCore {
    plugin: *const ClapPlugin,
    out_channels: usize,
    world_pos: usize,
    /// Declared latency in frames. Per-track latencies *are* compensated by
    /// the host (every track is shifted to the slowest plugin track — the
    /// classic PDC pattern), so tracks stay sample-aligned against each other.
    latency: usize,
    stopped: bool,
    pending_params: Vec<(u32, f64)>,
    dialect: NoteDialect,
}
// SAFETY: identical reasoning as ClapFx — single-threaded ownership at a time.
unsafe impl Send for SynthCore {}

impl SynthCore {
    fn new(plugin: *const ClapPlugin, out_channels: usize) -> Self {
        Self {
            plugin,
            out_channels,
            world_pos: 0,
            latency: 0,
            stopped: false,
            pending_params: Vec::new(),
            dialect: NoteDialect::Clap,
        }
    }

    fn set_param(&mut self, id: u32, value: f64) {
        self.pending_params.push((id, value));
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
        self.run_span(dst, events);
    }

    fn run_span(&mut self, dst: &mut [f32], events: &[NoteEvent]) {
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

        let mut set = EventSet::default();
        if !self.pending_params.is_empty() {
            self.pending_params.sort_by_key(|p| p.0);
            for (id, v) in self.pending_params.drain(..) {
                set.push_param(0, id, v);
            }
        }
        push_notes(&mut set, tile_abs, n, events, self.dialect);
        set.sort_and_index();
        let in_events = set.as_input();

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
    /// Load and activate an instrument plugin at `sr`. The note-port dialect
    /// is negotiated as CLAP first, bridged MIDI second; anything else fails
    /// loudly instead of producing silence.
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

        let mut dialect = NoteDialect::Clap;
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
                if (ext.get)(plugin_ref, 0, true, &mut info) {
                    if info.supported_dialects & CLAP_NOTE_DIALECT_CLAP != 0 {
                        dialect = NoteDialect::Clap;
                    } else if info.supported_dialects & CLAP_NOTE_DIALECT_MIDI != 0 {
                        dialect = NoteDialect::Midi;
                    } else {
                        (plugin_ref.destroy)(plugin_ref);
                        anyhow::bail!("{} speaks neither CLAP nor MIDI note dialects", plugin_id);
                    }
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
        let mut core = SynthCore::new(plugin, out_channels);
        core.dialect = dialect;
        core.latency = plugin_latency(plugin_ref).min(MAX_LATENCY_PRIME);
        Ok(ClapSynth {
            core,
            _bundle: bundle,
        })
    }

    /// See [`SynthCore::render_window`].
    pub fn render_window(&mut self, dst: &mut [f32], abs_start: usize, events: &[NoteEvent]) {
        self.core.render_window(dst, abs_start, events);
    }

    /// Queue an initial parameter batch, delivered as time-0 PARAM_VALUE
    /// events on the first processed tile.
    pub fn apply_initial(&mut self, params: &[(u32, f64)]) {
        for &(id, v) in params {
            self.core.set_param(id, v);
        }
    }

    /// Queue a parameter change for the next processed tile.
    pub fn set_param(&mut self, id: u32, value: f64) {
        self.core.set_param(id, value);
    }

    /// Enumerate the plugin's user-facing parameters.
    pub fn list_params(&self) -> Vec<ParamInfo> {
        read_params(unsafe { &*self.core.plugin })
    }

    /// The negotiated note dialect.
    pub fn dialect(&self) -> NoteDialect {
        self.core.dialect
    }

    /// Declared plugin latency in frames, for the track-alignment PDC pass.
    pub fn latency(&self) -> usize {
        self.core.latency
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

    // ── Fixture: latency-reporting delay FX (for PDC) ──

    const LAT_SAMPLES: usize = 64;
    struct LatState {
        l: [f32; LAT_SAMPLES],
        r: [f32; LAT_SAMPLES],
        pos: usize,
    }
    static LAT_STATE: std::sync::Mutex<LatState> = std::sync::Mutex::new(LatState {
        l: [0.0; LAT_SAMPLES],
        r: [0.0; LAT_SAMPLES],
        pos: 0,
    });

    extern "C" fn lat_get_extension(_p: *const ClapPlugin, id: *const c_char) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) }.to_bytes();
        if id == b"clap.audio-ports" {
            static PORTS: SyncStatic<ClapPluginAudioPorts> = SyncStatic(ClapPluginAudioPorts {
                count: ports_count,
                get: ports_get,
            });
            &PORTS.0 as *const ClapPluginAudioPorts as *const c_void
        } else if id == b"clap.latency" {
            static LAT: SyncStatic<ClapPluginLatency> = SyncStatic(ClapPluginLatency {
                get: lat_latency_get,
            });
            &LAT.0 as *const ClapPluginLatency as *const c_void
        } else {
            std::ptr::null()
        }
    }
    extern "C" fn lat_latency_get(_p: *const ClapPlugin) -> u32 {
        LAT_SAMPLES as u32
    }
    extern "C" fn lat_process(_p: *const ClapPlugin, proc: *const ClapProcess) -> i32 {
        let proc = unsafe { &*proc };
        let n = proc.frames_count as usize;
        let inputs = unsafe { &*proc.audio_inputs };
        let outputs = unsafe { &mut *proc.audio_outputs };
        let in_l = unsafe { std::slice::from_raw_parts(*inputs.data32, n) };
        let in_r = unsafe { std::slice::from_raw_parts(*inputs.data32.add(1), n) };
        let out_l = unsafe { std::slice::from_raw_parts_mut(*outputs.data32, n) };
        let out_r = unsafe { std::slice::from_raw_parts_mut(*outputs.data32.add(1), n) };
        let mut st = LAT_STATE.lock().unwrap();
        for i in 0..n {
            let pos = st.pos;
            let ol = st.l[pos];
            let orr = st.r[pos];
            st.l[pos] = in_l[i];
            st.r[pos] = in_r[i];
            st.pos = (pos + 1) % LAT_SAMPLES;
            out_l[i] = ol;
            out_r[i] = orr;
        }
        1
    }
    fn make_lat_plugin() -> Box<ClapPlugin> {
        Box::new(ClapPlugin {
            desc: std::ptr::null(),
            plugin_data: std::ptr::null_mut(),
            init: p_init,
            destroy: p_destroy,
            activate: p_activate,
            deactivate: p_deactivate,
            start_processing: p_start,
            stop_processing: p_stop,
            reset: p_reset,
            process: lat_process,
            get_extension: lat_get_extension,
            on_main_thread: p_on_main,
        })
    }

    /// Master-bus latency is deliberately *not* compensated: the declared
    /// latency is reported, and the (unaffected, whole-mix) shift passes
    /// through bit-identically in both render paths. Track-level latencies
    /// are what the PDC shift machinery aligns (see audio::tests).
    #[test]
    fn master_latency_is_reported_and_uniform() {
        let _g = SYNTH_TEST.lock().unwrap();
        *LAT_STATE.lock().unwrap() = LatState {
            l: [0.0; LAT_SAMPLES],
            r: [0.0; LAT_SAMPLES],
            pos: 0,
        };
        let plugin = make_lat_plugin();
        let plugin_ref: &'static ClapPlugin = Box::leak(plugin);
        let (in_ch, out_ch) = inspect_ports(plugin_ref);
        assert_eq!((in_ch, out_ch), (2, 2));
        let mut core = FxCore::with_latency(plugin_ref, in_ch, out_ch, plugin_latency(plugin_ref));
        assert_eq!(core.latency, LAT_SAMPLES);

        let mut buf = vec![0.0_f32; 300 * 2];
        buf[0] = 1.0;
        core.process_chunk(&mut buf);
        assert_eq!(buf[LAT_SAMPLES * 2], 1.0, "impulse rides the uniform shift");
        assert!(buf[..LAT_SAMPLES * 2].iter().all(|v| *v == 0.0));
    }

    /// Unprimed, the same plugin must delay by its declared latency — proves
    /// the fixture actually delays (and the prime is what does the aligning).
    #[test]
    fn unprimed_fixture_delays_by_declared_latency() {
        let _g = SYNTH_TEST.lock().unwrap();
        *LAT_STATE.lock().unwrap() = LatState {
            l: [0.0; LAT_SAMPLES],
            r: [0.0; LAT_SAMPLES],
            pos: 0,
        };
        let plugin = make_lat_plugin();
        let plugin_ref: &'static ClapPlugin = Box::leak(plugin);
        let mut core = FxCore::new(plugin_ref, 2, 2); // no priming
        let mut buf = vec![0.0_f32; 300 * 2];
        buf[0] = 1.0;
        core.process_chunk(&mut buf);
        assert_eq!(buf[LAT_SAMPLES * 2], 1.0);
        assert!(buf[..LAT_SAMPLES * 2].iter().all(|v| *v == 0.0));
    }

    // ── Fixture: parameterized gain FX (for param hosting) ──

    struct ParamState {
        log: Vec<(u32, usize, f64)>,
        gain: f64,
    }
    static PARAM_STATE: std::sync::Mutex<ParamState> = std::sync::Mutex::new(ParamState {
        log: Vec::new(),
        gain: 1.0,
    });

    extern "C" fn par_get_extension(_p: *const ClapPlugin, id: *const c_char) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) }.to_bytes();
        if id == b"clap.audio-ports" {
            static PORTS: SyncStatic<ClapPluginAudioPorts> = SyncStatic(ClapPluginAudioPorts {
                count: ports_count,
                get: ports_get,
            });
            &PORTS.0 as *const ClapPluginAudioPorts as *const c_void
        } else if id == b"clap.params" {
            static PARAMS: SyncStatic<ClapPluginParams> = SyncStatic(ClapPluginParams {
                count: par_count,
                get_info: par_get_info,
                get_value: par_get_value,
                value_to_text: par_value_to_text,
                text_to_value: par_text_to_value,
                flush: par_flush,
            });
            &PARAMS.0 as *const ClapPluginParams as *const c_void
        } else {
            std::ptr::null()
        }
    }
    extern "C" fn par_count(_p: *const ClapPlugin) -> u32 {
        3
    }
    fn write_cstr(buf: &mut [c_char], text: &str) {
        for (i, b) in text.as_bytes().iter().enumerate() {
            buf[i] = *b as c_char;
        }
        buf[text.len()] = 0;
    }
    extern "C" fn par_get_info(
        _p: *const ClapPlugin,
        index: u32,
        info: *mut ClapParamInfo,
    ) -> bool {
        let info = unsafe { &mut *info };
        match index {
            0 => {
                info.id = 7;
                info.flags = 0;
                write_cstr(&mut info.name, "Gain");
                write_cstr(&mut info.module, "Master");
                info.min_value = 0.0;
                info.max_value = 2.0;
                info.default_value = 1.0;
                true
            }
            1 => {
                info.id = 42;
                info.flags = 0;
                write_cstr(&mut info.name, "Cutoff");
                write_cstr(&mut info.module, "Filter");
                info.min_value = 20.0;
                info.max_value = 20_000.0;
                info.default_value = 1_000.0;
                true
            }
            2 => {
                info.id = 99;
                info.flags = 1 << 2; // hidden — must be filtered by the host
                write_cstr(&mut info.name, "Hidden");
                info.min_value = 0.0;
                info.max_value = 1.0;
                info.default_value = 0.0;
                true
            }
            _ => false,
        }
    }
    extern "C" fn par_get_value(_p: *const ClapPlugin, id: u32, out: *mut f64) -> bool {
        if id == 7 {
            unsafe { *out = PARAM_STATE.lock().unwrap().gain };
            true
        } else {
            false
        }
    }
    extern "C" fn par_value_to_text(
        _p: *const ClapPlugin,
        _id: u32,
        _v: f64,
        _b: *mut c_char,
        _c: u32,
    ) -> bool {
        false
    }
    extern "C" fn par_text_to_value(
        _p: *const ClapPlugin,
        _id: u32,
        _t: *const c_char,
        _o: *mut f64,
    ) -> bool {
        false
    }
    extern "C" fn par_flush(
        _p: *const ClapPlugin,
        _i: *const ClapInputEvents,
        _o: *const ClapOutputEvents,
    ) {
    }
    extern "C" fn par_process(_p: *const ClapPlugin, proc: *const ClapProcess) -> i32 {
        let proc = unsafe { &*proc };
        let n = proc.frames_count as usize;
        let in_events = unsafe { &*proc.in_events };
        let ev_count = (in_events.size)(in_events) as usize;
        let mut state = PARAM_STATE.lock().unwrap();
        for i in 0..ev_count {
            let hdr = (in_events.get)(in_events, i as u32);
            let ev = unsafe { &*(hdr as *const ClapEventParamValue) };
            if ev.header.type_ == CLAP_EVENT_PARAM_VALUE {
                let abs = proc.steady_time as usize + ev.header.time as usize;
                state.log.push((ev.param_id, abs, ev.value));
                if ev.param_id == 7 {
                    state.gain = ev.value;
                }
            }
        }
        drop(state);
        let gain = PARAM_STATE.lock().unwrap().gain;
        let inputs = unsafe { &*proc.audio_inputs };
        let outputs = unsafe { &mut *proc.audio_outputs };
        let in_l = unsafe { std::slice::from_raw_parts(*inputs.data32, n) };
        let in_r = unsafe { std::slice::from_raw_parts(*inputs.data32.add(1), n) };
        let out_l = unsafe { std::slice::from_raw_parts_mut(*outputs.data32, n) };
        let out_r = unsafe { std::slice::from_raw_parts_mut(*outputs.data32.add(1), n) };
        for i in 0..n {
            out_l[i] = in_l[i] * gain as f32;
            out_r[i] = in_r[i] * gain as f32;
        }
        1
    }
    fn make_param_plugin() -> Box<ClapPlugin> {
        Box::new(ClapPlugin {
            desc: std::ptr::null(),
            plugin_data: std::ptr::null_mut(),
            init: p_init,
            destroy: p_destroy,
            activate: p_activate,
            deactivate: p_deactivate,
            start_processing: p_start,
            stop_processing: p_stop,
            reset: p_reset,
            process: par_process,
            get_extension: par_get_extension,
            on_main_thread: p_on_main,
        })
    }

    /// Parameter hosting: enumeration filters hidden params, events arrive at
    /// sample 0 of the next tile, and the plugin hears the exact value.
    #[test]
    fn params_enumerate_and_flow_sample_accurately() {
        let _g = SYNTH_TEST.lock().unwrap();
        *PARAM_STATE.lock().unwrap() = ParamState {
            log: Vec::new(),
            gain: 1.0,
        };
        let plugin = make_param_plugin();
        let plugin_ref: &'static ClapPlugin = Box::leak(plugin);

        let params = read_params(plugin_ref);
        assert_eq!(params.len(), 2, "hidden param must be filtered");
        assert_eq!(params[0].id, 7);
        assert_eq!(params[0].name, "Gain");
        assert_eq!(params[0].module, "Master");
        assert_eq!(
            (
                params[0].min_value,
                params[0].max_value,
                params[0].default_value
            ),
            (0.0, 2.0, 1.0)
        );
        assert_eq!(params[1].id, 42);

        let mut core = FxCore::new(plugin_ref, 2, 2);
        core.set_param(7, 0.5);
        let mut buf = vec![0.25_f32; 256 * 2];
        core.process_chunk(&mut buf);
        let log = PARAM_STATE.lock().unwrap().log.clone();
        assert_eq!(log, vec![(7u32, 0usize, 0.5f64)]);
        assert!(
            buf.iter().all(|v| (*v - 0.125).abs() < 1e-6),
            "gain applied at sample 0"
        );
    }

    // ── Fixture: MIDI-dialect instrument (for dialect bridging) ──

    struct MidiState {
        log: Vec<[u8; 3]>,
        on: [bool; 128],
    }
    static MIDI_STATE: std::sync::Mutex<MidiState> = std::sync::Mutex::new(MidiState {
        log: Vec::new(),
        on: [false; 128],
    });

    extern "C" fn midi_note_get(
        _p: *const ClapPlugin,
        _i: u32,
        is_input: bool,
        info: *mut ClapNotePortInfo,
    ) -> bool {
        if !is_input {
            return false;
        }
        let info = unsafe { &mut *info };
        info.supported_dialects = 2; // MIDI only — no CLAP dialect here
        info.preferred_dialect = 2;
        true
    }
    extern "C" fn midi_get_extension(_p: *const ClapPlugin, id: *const c_char) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) }.to_bytes();
        if id == b"clap.note-ports" {
            static PORTS: SyncStatic<ClapPluginNotePorts> = SyncStatic(ClapPluginNotePorts {
                count: syn_note_count,
                get: midi_note_get,
            });
            &PORTS.0 as *const ClapPluginNotePorts as *const c_void
        } else if id == b"clap.audio-ports" {
            static PORTS: SyncStatic<ClapPluginAudioPorts> = SyncStatic(ClapPluginAudioPorts {
                count: syn_audio_count,
                get: syn_audio_get,
            });
            &PORTS.0 as *const ClapPluginAudioPorts as *const c_void
        } else {
            std::ptr::null()
        }
    }
    extern "C" fn midi_process(_p: *const ClapPlugin, proc: *const ClapProcess) -> i32 {
        let proc = unsafe { &*proc };
        let n = proc.frames_count as usize;
        let in_events = unsafe { &*proc.in_events };
        let ev_count = (in_events.size)(in_events) as usize;
        let outputs = unsafe { &mut *proc.audio_outputs };
        let out_l = unsafe { std::slice::from_raw_parts_mut(*outputs.data32, n) };
        let out_r = unsafe { std::slice::from_raw_parts_mut(*outputs.data32.add(1), n) };
        let mut state = MIDI_STATE.lock().unwrap();
        let mut cursor = 0usize;
        for i in 0..n {
            while cursor < ev_count {
                let hdr = (in_events.get)(in_events, cursor as u32);
                let ev = unsafe { &*(hdr as *const ClapEventMidi) };
                if ev.header.time as usize > i {
                    break;
                }
                assert_eq!(ev.header.type_, CLAP_EVENT_MIDI);
                state.log.push(ev.data);
                let status = ev.data[0] & 0xF0;
                let key = ev.data[1] as usize;
                if status == 0x90 && ev.data[2] > 0 {
                    state.on[key] = true;
                } else if status == 0x80 || (status == 0x90 && ev.data[2] == 0) {
                    state.on[key] = false;
                }
                cursor += 1;
            }
            let acc: f32 = if state.on.iter().any(|v| *v) {
                0.2
            } else {
                0.0
            };
            out_l[i] = acc;
            out_r[i] = acc;
        }
        1
    }
    fn make_midi_synth() -> Box<ClapPlugin> {
        Box::new(ClapPlugin {
            desc: std::ptr::null(),
            plugin_data: std::ptr::null_mut(),
            init: p_init,
            destroy: p_destroy,
            activate: p_activate,
            deactivate: p_deactivate,
            start_processing: p_start,
            stop_processing: p_stop,
            reset: p_reset,
            process: midi_process,
            get_extension: midi_get_extension,
            on_main_thread: p_on_main,
        })
    }

    /// MIDI-dialect bridging: a MIDI-only instrument receives 0x90/0x80 frames
    /// with velocity mapped to 1..127, and its gate behaves accordingly.
    #[test]
    fn midi_dialect_is_bridged() {
        let _g = SYNTH_TEST.lock().unwrap();
        *MIDI_STATE.lock().unwrap() = MidiState {
            log: Vec::new(),
            on: [false; 128],
        };
        let plugin = make_midi_synth();
        let plugin_ref: &'static ClapPlugin = Box::leak(plugin);

        // Host-side negotiation: MIDI-only port must select Midi.
        // (ClapSynth::load does this; here we exercise the core directly.)
        let mut core = SynthCore::new(plugin_ref, 2);
        core.dialect = NoteDialect::Midi;

        let mut buf = vec![0.0_f32; 400 * 2];
        core.render_window(
            &mut buf,
            0,
            &[
                NoteEvent {
                    abs_frame: 10,
                    on: true,
                    key: 60,
                    velocity: 0.5,
                },
                NoteEvent {
                    abs_frame: 200,
                    on: false,
                    key: 60,
                    velocity: 0.0,
                },
            ],
        );
        let log = MIDI_STATE.lock().unwrap().log.clone();
        assert_eq!(log, vec![[0x90, 60, 64], [0x80, 60, 0]]);
        assert!(buf[..10 * 2].iter().all(|v| *v == 0.0));
        assert!(buf[10 * 2..200 * 2].iter().all(|v| (*v - 0.2).abs() < 1e-7));
        assert!(buf[200 * 2..].iter().all(|v| *v == 0.0));
    }
}
