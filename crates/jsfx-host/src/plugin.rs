//! A JSFX effect as a [`soundcraft_dsp::Plugin`].

use crate::header::{JsfxSource, SliderDef, parse_source};
use crate::parse::{self, MAX_CH, Names, compile};
use crate::vm::Machine;
use crate::{ID_PREFIX, JsfxError};
use soundcraft_dsp::{Category, ParamInfo, Plugin, PluginInfo, Taper, Unit};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Default memory size in `f64` slots (REAPER's default is 8M; the OS commits pages lazily).
const DEFAULT_MEM: usize = 1 << 22;
const MAX_MEM: usize = 1 << 24;
/// Loop-iteration budgets per section run.
const FUEL_INIT: u64 = 50_000_000;
const FUEL_SLIDER: u64 = 5_000_000;
const FUEL_BLOCK: u64 = 5_000_000;
const FUEL_SAMPLE: u64 = 200_000;
/// Largest source file we load.
const MAX_FILE: u64 = 2 * 1024 * 1024;
/// JSFX cannot report its tail; assume a few seconds so delays and reverbs ring out.
const TAIL_SECONDS: f32 = 3.0;

struct Sections {
    init: Option<crate::ast::Expr>,
    slider: Option<crate::ast::Expr>,
    block: Option<crate::ast::Expr>,
    sample: Option<crate::ast::Expr>,
}

pub struct JsfxPlugin {
    info: &'static PluginInfo,
    code: Sections,
    m: Machine,
    /// Slider variable slot per parameter, parallel to `info.params`.
    slots: Vec<usize>,
    values: Vec<f32>,
    dirty: bool,
    faulted: bool,
    sr: f32,
    max_block: usize,
    channels: usize,
    latency: usize,
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn param_info(def: &SliderDef) -> ParamInfo {
    let choices: &'static [&'static str] =
        if def.choices.is_empty() { &[] } else { Box::leak(def.choices.iter().map(|c| leak_str(c)).collect::<Vec<_>>().into_boxed_slice()) };
    let id = format!("slider{}", def.index);
    let name = if def.label.is_empty() { id.clone() } else { def.label.clone() };
    ParamInfo {
        id: leak_str(&id),
        name: leak_str(&name),
        min: def.min as f32,
        max: def.max as f32,
        default: def.default as f32,
        unit: if choices.is_empty() { Unit::None } else { Unit::Choice },
        taper: if def.log && def.min > 0.0 { Taper::Log } else { Taper::Linear },
        choices,
    }
}

/// One leaked `PluginInfo` per distinct (id, source); instantiating the same effect again reuses it.
fn info_for(id: &str, text: &str, src: &JsfxSource) -> &'static PluginInfo {
    static CACHE: OnceLock<Mutex<HashMap<u64, &'static PluginInfo>>> = OnceLock::new();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    text.hash(&mut h);
    let key = h.finish();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = guard.get(&key) {
        return i;
    }
    let desc = src.desc.trim();
    let name = if desc.is_empty() { id.strip_prefix(ID_PREFIX).unwrap_or(id) } else { desc };
    let short: String = name.chars().filter(|c| c.is_alphanumeric()).take(8).collect();
    let params: Vec<ParamInfo> = src.sliders.iter().map(param_info).collect();
    let info: &'static PluginInfo = Box::leak(Box::new(PluginInfo {
        id: leak_str(id),
        name: leak_str(name),
        short_name: leak_str(if short.is_empty() { "JSFX" } else { &short }),
        category: Category::Other,
        params: Box::leak(params.into_boxed_slice()),
        is_instrument: false,
        audiosuite: true,
    }));
    guard.insert(key, info);
    info
}

impl JsfxPlugin {
    /// Compiles `text` as a JSFX file. `id` is the full SoundCraft id (`jsfx:<relative path>`).
    pub fn from_source(id: &str, text: &str) -> Result<Self, JsfxError> {
        let mut src = parse_source(text)?;
        src.sliders.sort_by_key(|s| s.index);
        if let Some(first) = src.ignored.iter().find(|s| s.as_str() == "import") {
            // Imports only matter for user functions, which fail loudly below if they are used.
            log::debug!("jsfx {id}: `{first}` lines are ignored");
        }
        let mut names = Names::new();
        for s in &src.sliders {
            if let Some(v) = &s.var {
                names.alias(v, parse::SLIDER1 + s.index - 1);
            }
        }
        let mut sec = |s: &crate::header::Section| -> Result<Option<crate::ast::Expr>, JsfxError> {
            if s.code.trim().is_empty() { Ok(None) } else { compile(&s.code, s.line, &mut names).map(Some) }
        };
        let code = Sections { init: sec(&src.init)?, slider: sec(&src.slider)?, block: sec(&src.block)?, sample: sec(&src.sample)? };
        let mem = src.maxmem.map_or(DEFAULT_MEM, |m| m.clamp(1 << 10, MAX_MEM));
        let info = info_for(id, text, &src);
        let m = Machine::new(names.len(), mem);
        Ok(Self {
            info,
            code,
            m,
            slots: src.sliders.iter().map(|s| parse::SLIDER1 + s.index - 1).collect(),
            values: info.params.iter().map(|p| p.default).collect(),
            dirty: true,
            faulted: false,
            sr: 44_100.0,
            max_block: 512,
            channels: 2,
            latency: 0,
        })
    }

    /// Loads a JSFX file; `root` is the Effects folder the plugin id is made relative to.
    pub fn from_file(root: &Path, path: &Path) -> Result<Self, JsfxError> {
        let io = |e: std::io::Error| JsfxError::Io(format!("{}: {e}", path.display()));
        if std::fs::metadata(path).map_err(io)?.len() > MAX_FILE {
            return Err(JsfxError::Io(format!("{}: file too large", path.display())));
        }
        let bytes = std::fs::read(path).map_err(io)?;
        let text = String::from_utf8_lossy(&bytes);
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        Self::from_source(&format!("{ID_PREFIX}{rel}"), &text)
    }

    /// `true` once a section ran out of its budget; the effect then passes audio through untouched.
    pub fn is_faulted(&self) -> bool {
        self.faulted
    }

    fn param_index(&self, id: &str) -> Option<usize> {
        self.info.params.iter().position(|p| p.id == id)
    }

    fn push_sliders(&mut self) {
        for (slot, v) in self.slots.iter().zip(&self.values) {
            self.m.set_var(*slot, f64::from(*v));
        }
    }

    fn run(&mut self, which: fn(&Sections) -> &Option<crate::ast::Expr>, fuel: u64) {
        if let Some(e) = which(&self.code)
            && !self.m.run(e, fuel)
        {
            log::warn!("jsfx {}: section ran out of its budget; bypassing", self.info.id);
            self.faulted = true;
        }
    }

    fn read_latency(&mut self) {
        let d = self.m.var(parse::PDC_DELAY);
        self.latency = if d.is_finite() && d > 0.0 { (d as usize).min(self.sr as usize * 4) } else { 0 };
    }

    /// (Re)starts the effect: clears state, publishes host variables, runs `@init` then `@slider`.
    fn start(&mut self) {
        self.m.reset();
        self.faulted = false;
        self.m.set_var(parse::SRATE, f64::from(self.sr));
        self.m.set_var(parse::NUM_CH, self.channels as f64);
        self.m.set_var(parse::SAMPLESBLOCK, self.max_block as f64);
        self.m.set_var(parse::TEMPO, 120.0);
        self.m.set_var(parse::TS_NUM, 4.0);
        self.m.set_var(parse::TS_DENOM, 4.0);
        self.push_sliders();
        self.run(|c| &c.init, FUEL_INIT);
        self.run(|c| &c.slider, FUEL_SLIDER);
        self.dirty = false;
        self.read_latency();
    }
}

impl Plugin for JsfxPlugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        self.sr = if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 44_100.0 };
        self.max_block = max_block.max(1);
        self.channels = channels.clamp(1, MAX_CH);
        self.start();
    }

    fn reset(&mut self) {
        self.start();
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.param_index(id) else { return false };
        let Some(info) = self.info.params.get(i) else { return false };
        let v = info.clamp(value);
        if let Some(slot) = self.values.get_mut(i) {
            *slot = v;
        }
        self.dirty = true;
        true
    }

    fn param(&self, id: &str) -> Option<f32> {
        self.param_index(id).and_then(|i| self.values.get(i)).copied()
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if self.faulted {
            return;
        }
        if self.dirty {
            self.push_sliders();
            self.run(|c| &c.slider, FUEL_SLIDER);
            self.dirty = false;
            self.read_latency();
        }
        self.run(|c| &c.block, FUEL_BLOCK);
        if self.code.sample.is_none() || self.faulted {
            return;
        }
        let ch = io.len().min(MAX_CH);
        let n = frames.min(io.iter().take(ch).map(Vec::len).min().unwrap_or(0));
        for i in 0..n {
            for (c, buf) in io.iter().take(ch).enumerate() {
                self.m.set_var(parse::SPL0 + c, f64::from(buf.get(i).copied().unwrap_or(0.0)));
            }
            if let Some(e) = &self.code.sample
                && !self.m.run(e, FUEL_SAMPLE)
            {
                log::warn!("jsfx {}: @sample ran out of its budget; bypassing", self.info.id);
                self.faulted = true;
                return;
            }
            for (c, buf) in io.iter_mut().take(ch).enumerate() {
                let v = self.m.var(parse::SPL0 + c);
                if let Some(s) = buf.get_mut(i) {
                    *s = if v.is_finite() { v as f32 } else { 0.0 };
                }
            }
        }
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn tail_samples(&self) -> usize {
        if self.code.sample.is_some() { (self.sr * TAIL_SECONDS) as usize } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(src: &str) -> JsfxPlugin {
        let mut p = JsfxPlugin::from_source("jsfx:test", src).unwrap();
        p.prepare(48_000.0, 64, 2);
        p
    }

    fn run(p: &mut JsfxPlugin, l: f32, r: f32, frames: usize) -> Vec<Vec<f32>> {
        let mut io = vec![vec![l; frames], vec![r; frames]];
        p.process(&mut io, frames);
        io
    }

    const GAIN: &str = "desc:Gain\nslider1:0<-60,12,0.1>Gain dB\n@init\ng=1;\n@slider\ng=10^(slider1/20);\n@sample\nspl0*=g;\nspl1*=g;\n";

    #[test]
    fn gain_follows_its_slider() {
        let mut p = load(GAIN);
        assert_eq!(p.info().name, "Gain");
        assert_eq!(p.info().params.len(), 1);
        let io = run(&mut p, 1.0, -1.0, 4);
        assert!((io[0][0] - 1.0).abs() < 1e-6 && (io[1][3] + 1.0).abs() < 1e-6);
        assert!(p.set_param("slider1", -6.0206));
        let io = run(&mut p, 1.0, 1.0, 4);
        assert!((io[0][0] - 0.5).abs() < 1e-3, "{}", io[0][0]);
        assert!(!p.set_param("nope", 1.0));
        assert_eq!(p.param("slider1"), Some(-6.0206));
    }

    #[test]
    fn params_are_clamped_and_nan_safe() {
        let mut p = load(GAIN);
        p.set_param("slider1", 1e9);
        assert_eq!(p.param("slider1"), Some(12.0));
        p.set_param("slider1", f32::NAN);
        assert_eq!(p.param("slider1"), Some(0.0));
    }

    #[test]
    fn state_persists_across_blocks_and_memory_works() {
        // One-sample delay through a memory cell.
        let mut p = load("desc:D\n@init\nbuf=0;\n@sample\nout=buf[0];\nbuf[0]=spl0;\nspl0=out;\n");
        let mut io = vec![vec![1.0, 2.0, 3.0], vec![0.0; 3]];
        p.process(&mut io, 3);
        assert_eq!(io[0], [0.0, 1.0, 2.0]);
        let mut io = vec![vec![9.0], vec![0.0]];
        p.process(&mut io, 1);
        assert_eq!(io[0], [3.0]);
        p.reset();
        let mut io = vec![vec![5.0], vec![0.0]];
        p.process(&mut io, 1);
        assert_eq!(io[0], [0.0]);
    }

    #[test]
    fn host_variables_and_latency() {
        let mut p = load("desc:V\n@init\npdc_delay=64;\n@sample\nspl0=srate;\nspl1=num_ch;\n");
        let io = run(&mut p, 0.0, 0.0, 1);
        assert_eq!((io[0][0], io[1][0]), (48_000.0, 2.0));
        assert_eq!(p.latency(), 64);
    }

    #[test]
    fn runaway_scripts_fault_and_pass_through() {
        let mut p = load("desc:Hang\n@sample\nwhile(1) ( x += 1; );\n");
        let io = run(&mut p, 0.25, 0.25, 8);
        assert!(p.is_faulted());
        assert!(io[0].iter().all(|v| v.is_finite()));
        let io = run(&mut p, 0.5, 0.5, 2);
        assert_eq!(io[0], [0.5, 0.5], "bypassed after the fault");
    }

    #[test]
    fn non_finite_output_becomes_silence() {
        let mut p = load("desc:Nan\n@sample\nspl0=log(-1);\nspl1=1/0;\n");
        let io = run(&mut p, 1.0, 1.0, 2);
        assert_eq!(io[0], [0.0, 0.0]);
        assert_eq!(io[1], [0.0, 0.0]);
    }

    #[test]
    fn mono_and_short_buffers_are_handled() {
        let mut p = load(GAIN);
        let mut io = vec![vec![1.0; 4]];
        p.process(&mut io, 100);
        let mut none: Vec<Vec<f32>> = Vec::new();
        p.process(&mut none, 10);
        p.prepare(0.0, 0, 0);
        assert_eq!(p.info().params[0].id, "slider1");
    }

    #[test]
    fn choice_sliders_and_aliases() {
        let mut p = load("desc:M\nslider1:mode=1<0,2,1{A,B,C}>Mode\n@sample\nspl0=mode;\n");
        assert_eq!(p.info().params[0].choices, ["A", "B", "C"]);
        assert_eq!(run(&mut p, 0.0, 0.0, 1)[0][0], 1.0);
        p.set_param("slider1", 2.2);
        assert_eq!(run(&mut p, 0.0, 0.0, 1)[0][0], 2.0);
    }

    #[test]
    fn unsupported_features_fail_to_load_with_a_reason() {
        let Err(e) = JsfxPlugin::from_source("jsfx:x", "desc:x\n@init\nfunction f(a) ( a );\n") else { panic!("should not load") };
        assert!(matches!(e, JsfxError::Unsupported(_)), "{e}");
        assert!(JsfxPlugin::from_source("jsfx:x", "desc:x\n@sample\nspl0 = ;\n").is_err());
    }

    #[test]
    fn same_source_shares_one_plugin_info() {
        let a = load(GAIN);
        let b = load(GAIN);
        assert!(std::ptr::eq(a.info(), b.info()));
    }

    #[test]
    fn sections_without_sample_code_pass_audio_through() {
        let mut p = load("desc:Meter\n@block\nx+=1;\n");
        assert_eq!(run(&mut p, 0.3, 0.3, 2)[0], [0.3, 0.3]);
        assert_eq!(p.tail_samples(), 0);
    }
}
