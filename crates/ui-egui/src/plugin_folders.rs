//! Setup › Plug-in Folders: the folders scanned for VST3 and CLAP plugins, the user's own ones
//! added and removed, rescans, and the files that would not load. Not in Pro Tools (its plug-ins
//! live in one fixed folder); REAPER keeps the same controls under Preferences › Plug-ins.
//!
//! Scans run on a background thread (see [`start`]) and are remembered in the settings folder,
//! so that only new and changed plugin files are loaded at each launch.

use crate::SoundApp;
use crate::theme::Tokens;
use egui::vec2;
use std::path::{Path, PathBuf};

/// The user's own plugin folders, scanned besides the standard ones (kept with the preferences).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PluginDirs {
    pub vst3: Vec<String>,
    pub clap: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Vst3,
    Clap,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Vst3 => "VST3",
            Format::Clap => "CLAP",
        }
    }
    fn dirs(self, all: &mut PluginDirs) -> &mut Vec<String> {
        match self {
            Format::Vst3 => &mut all.vst3,
            Format::Clap => &mut all.clap,
        }
    }
    fn search_dirs(self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        match self {
            Format::Vst3 => soundcraft_vst3_host::search_dirs(),
            Format::Clap => soundcraft_clap_host::search_dirs(),
        }
    }
    fn set_extra_dirs(self, dirs: &[String]) {
        let dirs = dirs.iter().map(PathBuf::from).collect();
        match self {
            Format::Vst3 => soundcraft_vst3_host::set_extra_dirs(dirs),
            Format::Clap => soundcraft_clap_host::set_extra_dirs(dirs),
        }
    }
    /// The number of plugins found, `None` while scanning or before the first scan.
    fn found(self) -> Option<usize> {
        match self {
            Format::Vst3 => soundcraft_vst3_host::cached().map(|l| l.len()),
            Format::Clap => soundcraft_clap_host::cached().map(|l| l.len()),
        }
    }
    fn scanning(self) -> bool {
        match self {
            Format::Vst3 => soundcraft_vst3_host::scanning(),
            Format::Clap => soundcraft_clap_host::scanning(),
        }
    }
    fn rescan(self, clear_cache: bool) {
        match self {
            Format::Vst3 => soundcraft_vst3_host::rescan_in_background(clear_cache),
            Format::Clap => soundcraft_clap_host::rescan_in_background(clear_cache),
        }
    }
    fn problems(self) -> Vec<(PathBuf, String)> {
        match self {
            Format::Vst3 => soundcraft_vst3_host::problems(),
            Format::Clap => soundcraft_clap_host::problems(),
        }
    }
}

/// At launch: remembers scans in `settings_dir` (when there is one), adds the user's folders and
/// starts scanning on a background thread, so that the plugin menus are ready when first opened.
pub fn start(dirs: &PluginDirs, settings_dir: Option<&Path>) {
    Format::Vst3.set_extra_dirs(&dirs.vst3);
    Format::Clap.set_extra_dirs(&dirs.clap);
    if let Some(dir) = settings_dir {
        soundcraft_vst3_host::set_cache_file(Some(dir.join("plugins-vst3.json")));
        soundcraft_clap_host::set_cache_file(Some(dir.join("plugins-clap.json")));
        soundcraft_vst3_host::scan_in_background();
        soundcraft_clap_host::scan_in_background();
    }
}

/// A "Scanning …" line with a spinner; keeps the UI redrawing until the scan ends.
pub(crate) fn scanning_label(ui: &mut egui::Ui, what: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.weak(format!("Scanning {what}…"));
    });
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
}

pub fn window(app: &mut SoundApp, ctx: &egui::Context) {
    let mut open = app.ui.show_plugin_folders;
    if !open {
        return;
    }
    let tab_id = egui::Id::new("plugin_folders_tab");
    let mut tab = ctx.data(|d| d.get_temp::<bool>(tab_id)).map_or(Format::Vst3, |clap| if clap { Format::Clap } else { Format::Vst3 });
    let mut changed = false;
    egui::Window::new("Plug-in Folders").open(&mut open).default_size(vec2(560.0, 380.0)).show(ctx, |ui| {
        let t = Tokens::current();
        ui.horizontal(|ui| {
            for f in [Format::Vst3, Format::Clap] {
                ui.selectable_value(&mut tab, f, f.name());
            }
        });
        ui.separator();
        // Folders change between scans only (a running scan would miss the change).
        let busy = tab.scanning();
        let (standard, _) = tab.search_dirs();
        ui.label("Standard folders");
        for d in &standard {
            ui.label(egui::RichText::new(d.display().to_string()).color(t.text_dim));
        }
        ui.add_space(6.0);
        ui.label("Your folders");
        let mine = tab.dirs(&mut app.ui.plugin_dirs);
        let mut remove = None;
        for (i, d) in mine.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Remove").small()).clicked() {
                    remove = Some(i);
                }
                ui.label(d);
            });
        }
        if mine.is_empty() {
            ui.weak("None");
        }
        if let Some(i) = remove {
            mine.remove(i);
            changed = true;
        }
        if let Some(pick) = &app.services.pick_folder
            && ui.add_enabled(!busy, egui::Button::new("Add Folder…")).clicked()
            && let Some(dir) = pick(mine.last().map_or("", String::as_str))
            && !mine.contains(&dir)
        {
            mine.push(dir);
            changed = true;
        }
        ui.separator();
        match tab.found() {
            Some(n) if !busy => {
                ui.label(format!("{n} {} plugin{} found", tab.name(), if n == 1 { "" } else { "s" }));
            }
            _ => scanning_label(ui, &format!("{} plugins", tab.name())),
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Rescan")).on_hover_text("Load the plugin files that are new or changed").clicked() {
                tab.rescan(false);
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Clear Cache & Rescan"))
                .on_hover_text("Load every plugin file again, including those that failed")
                .clicked()
            {
                tab.rescan(true);
            }
        });
        let problems = tab.problems();
        if !problems.is_empty() {
            ui.add_space(4.0);
            egui::CollapsingHeader::new(format!("{} file{} could not be loaded", problems.len(), if problems.len() == 1 { "" } else { "s" }))
                .id_salt("plugin_problems")
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                        for (path, why) in &problems {
                            ui.label(path.display().to_string()).on_hover_text(why);
                            ui.label(egui::RichText::new(why).small().color(t.text_dim));
                        }
                    });
                });
        }
    });
    if changed {
        tab.set_extra_dirs(tab.dirs(&mut app.ui.plugin_dirs));
        tab.rescan(false);
    }
    ctx.data_mut(|d| d.insert_temp(tab_id, tab == Format::Clap));
    app.ui.show_plugin_folders = open;
}
