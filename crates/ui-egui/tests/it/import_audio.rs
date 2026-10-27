//! Importing audio: files dropped on the Edit window land where they are dropped (a track from
//! the time under the pointer, new tracks, or the Clip List only), and File › Import › Audio
//! imports any number of files to the chosen destination.

use egui::{DroppedFile, Event, Pos2, pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use soundcraft_ui_egui::dialogs::Dialog;
use soundcraft_ui_egui::edit_window::rect;
use soundcraft_ui_egui::import_audio::{Destination, DropTarget, ImportAudio, drop_target};
use soundcraft_ui_egui::{Services, SoundApp};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
struct Dropped(PathBuf);

impl DroppedFile for Dropped {
    fn path(&self) -> &Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

/// Half a second of tone at 48 kHz (24 000 frames).
fn wav(dir: &Path, name: &str) -> PathBuf {
    let one: Vec<f32> = (0..24_000).map(|i| 0.3 * (std::f32::consts::TAU * 330.0 * i as f32 / 48_000.0).sin()).collect();
    let buf = AudioBuffer { sample_rate: 48_000, channels: vec![one] };
    let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Int16, dither: false, bwf: None };
    let path = dir.join(name);
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &opts).unwrap()).unwrap();
    path
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sc-import-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn step(h: &mut Harness<'_, SoundApp>) {
    let mut raw = std::mem::take(h.input_mut());
    let ctx = h.ctx.clone();
    h.state_mut().raw_input_hook(&ctx, &mut raw);
    *h.input_mut() = raw;
    h.step();
}

fn harness() -> Harness<'static, SoundApp> {
    let app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    let mut h = Harness::builder().with_size(vec2(1600.0, 2000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            let _ = app.take_dropped_files();
            app.ui(ui);
        },
        app,
    );
    for _ in 0..6 {
        step(&mut h);
    }
    h
}

fn drop_at(h: &mut Harness<'_, SoundApp>, p: Pos2, files: &[PathBuf]) {
    h.state_mut().synthetic.push(Event::PointerMoved(p));
    for _ in 0..3 {
        step(h);
    }
    for f in files {
        h.input_mut().dropped_files.push(Arc::new(Dropped(f.clone())));
    }
    for _ in 0..4 {
        step(h);
    }
}

fn track_named(h: &Harness<'_, SoundApp>, name: &str) -> soundcraft_model::TrackId {
    h.state().engine.session().tracks.iter().find(|t| t.name == name).unwrap().id
}

#[test]
fn files_dropped_on_a_track_go_there_one_after_another() {
    let d = dir("track");
    let files = [wav(&d, "one.wav"), wav(&d, "two.wav")];
    let mut h = harness();
    let bass = track_named(&h, "Bass");
    let row = h.state().edit_layout.rows.iter().find(|(id, _)| *id == bass.0).map(|(_, r)| rect(*r)).unwrap();
    let tl = rect(h.state().edit_layout.timeline);
    let p = pos2(tl.min.x + 300.0, row.center().y);
    let Some(DropTarget::Track(t, at)) = drop_target(h.state(), Some(p)) else { panic!("an audio track's row takes the files") };
    assert_eq!(t, bass);
    let (tracks, clips) = {
        let s = h.state().engine.session();
        (s.tracks.len(), s.track(bass).unwrap().clips().len())
    };
    drop_at(&mut h, p, &files);
    let s = h.state().engine.session();
    assert_eq!(s.tracks.len(), tracks, "no new tracks");
    let mut starts: Vec<i64> = s.track(bass).unwrap().clips().iter().filter(|c| c.name == "one" || c.name == "two").map(|c| c.start).collect();
    starts.sort_unstable();
    assert_eq!(starts, [at, at + 24_000]);
    assert!(h.state().ui.status.starts_with("Imported 2 files onto Bass"), "{}", h.state().ui.status);
    // One undo step takes the whole drop back.
    assert!(h.state_mut().engine.undo());
    assert_eq!(h.state().engine.session().track(bass).unwrap().clips().len(), clips);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn files_dropped_below_the_tracks_make_new_tracks_and_on_the_clip_list_stay_there() {
    let d = dir("new");
    let files = [wav(&d, "kick.wav"), wav(&d, "snare.wav")];
    let mut h = harness();
    let tl = rect(h.state().edit_layout.timeline);
    let below = pos2(tl.min.x + 200.0, tl.max.y - 10.0);
    assert!(matches!(drop_target(h.state(), Some(below)), Some(DropTarget::NewTracks(_))), "empty timeline space makes new tracks");
    let tracks = h.state().engine.session().tracks.len();
    drop_at(&mut h, below, &files);
    assert_eq!(h.state().engine.session().tracks.len(), tracks + 2);

    let list = h.state().clip_list_rect.expect("the Clip List is shown");
    assert_eq!(drop_target(h.state(), Some(list.center())), Some(DropTarget::ClipList));
    let (tracks, sources) = (h.state().engine.session().tracks.len(), h.state().engine.session().sources.len());
    drop_at(&mut h, list.center(), &[wav(&d, "pad.wav")]);
    let s = h.state().engine.session();
    assert_eq!((s.tracks.len(), s.sources.len()), (tracks, sources + 1), "a source, no track");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn the_import_dialog_imports_its_list_to_the_chosen_destination() {
    let d = dir("dialog");
    let files: Vec<String> = [wav(&d, "a.wav"), wav(&d, "b.wav")].iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let mut h = harness();
    let (tracks, sources) = (h.state().engine.session().tracks.len(), h.state().engine.session().sources.len());
    let mut imp = ImportAudio { destination: Destination::ClipList, ..ImportAudio::default() };
    imp.add(files);
    h.state_mut().dialogs.open = Some(Dialog::ImportAudio(imp));
    h.run_steps(3);
    let _ = h.get_by_label("Clip List only");
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Import").click();
    h.run_steps(3);
    let s = h.state().engine.session();
    assert_eq!((s.tracks.len(), s.sources.len()), (tracks, sources + 2));
    assert!(h.state().dialogs.open.is_none(), "the dialog closes");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn import_with_an_empty_list_does_nothing() {
    let mut h = harness();
    let tracks = h.state().engine.session().tracks.len();
    h.state_mut().dialogs.open = Some(Dialog::ImportAudio(ImportAudio::default()));
    h.run_steps(3);
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Import").click();
    h.run_steps(3);
    assert_eq!(h.state().engine.session().tracks.len(), tracks);
    assert!(h.state().dialogs.open.is_some(), "still open, waiting for files");
}

/// Half a second of tone at `rate` with `channels` channels.
fn wav_at(dir: &Path, name: &str, rate: u32, channels: usize) -> PathBuf {
    let frames = rate / 2;
    let chans: Vec<Vec<f32>> = (0..channels)
        .map(|c| {
            let gain = 0.3 / (c as f32 + 1.0);
            (0..frames).map(|i| gain * (std::f32::consts::TAU * 330.0 * i as f32 / rate as f32).sin()).collect()
        })
        .collect();
    let buf = AudioBuffer { sample_rate: rate, channels: chans };
    let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Int16, dither: false, bwf: None };
    let path = dir.join(name);
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &opts).unwrap()).unwrap();
    path
}

/// The Bass row's point `dx` pixels into the timeline, and what dropping there would do.
fn bass_drop(h: &Harness<'_, SoundApp>, dx: f32) -> (Pos2, DropTarget) {
    let bass = track_named(h, "Bass");
    let row = h.state().edit_layout.rows.iter().find(|(id, _)| *id == bass.0).map(|(_, r)| rect(*r)).unwrap();
    let tl = rect(h.state().edit_layout.timeline);
    let p = pos2(tl.min.x + dx, row.center().y);
    let target = drop_target(h.state(), Some(p)).expect("the Edit window takes the drop");
    assert!(matches!(target, DropTarget::Track(t, _) if t == bass), "{target:?}");
    (p, target)
}

#[test]
fn a_drop_onto_a_clip_lands_on_it_and_leaves_the_rest_of_it_in_place() {
    let d = dir("onto-clip");
    let file = wav(&d, "hit.wav");
    let mut h = harness();
    let (p, target) = bass_drop(&h, 300.0);
    let DropTarget::Track(_, at) = target else { panic!() };
    let before: Vec<(i64, i64)> = {
        let s = h.state().engine.session();
        s.track(track_named(&h, "Bass")).unwrap().clips().iter().map(|c| (c.start, c.length)).collect()
    };
    drop_at(&mut h, p, &[file]);
    let s = h.state().engine.session();
    let clips = s.track(track_named(&h, "Bass")).unwrap().clips();
    let new = clips.iter().find(|c| c.name == "hit").expect("the dropped clip");
    assert_eq!((new.start, new.length), (at, 24_000), "it lands where it was dropped");
    // The clip it landed on is cut either side: its material stays before and after the drop.
    let cut = before.iter().find(|(start, len)| *start <= at && start + len > at).map(|(start, len)| (*start, *len));
    let (c_start, c_len) = cut.expect("the drop lands on a clip");
    assert_eq!(clips.len(), before.len() + 2, "the drop adds a clip and splits the one under it");
    assert!(clips.iter().any(|c| c.name == "Bass" && c.start == c_start && c.start + c.length == at), "the part before the drop stays");
    assert!(clips.iter().any(|c| c.name == "Bass" && c.start == at + 24_000 && c.start + c.length == c_start + c_len), "and the part after");
    for (start, len) in before.iter().filter(|(start, len)| start + len <= at || *start > at) {
        assert!(clips.iter().any(|c| c.start == *start && c.length == *len), "the clip at {start} is untouched");
    }
    assert!(h.state_mut().engine.undo(), "one undo step");
    let after: Vec<(i64, i64)> =
        h.state().engine.session().track(track_named(&h, "Bass")).unwrap().clips().iter().map(|c| (c.start, c.length)).collect();
    assert_eq!(after, before, "one undo puts the clip back together");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_44k_file_in_a_48k_session_takes_its_resampled_length() {
    let d = dir("rate");
    let files = [wav_at(&d, "a.wav", 44_100, 1), wav_at(&d, "b.wav", 44_100, 1)];
    let mut h = harness();
    let (p, target) = bass_drop(&h, 300.0);
    let DropTarget::Track(_, at) = target else { panic!() };
    drop_at(&mut h, p, &files);
    let s = h.state().engine.session();
    let mut starts: Vec<i64> =
        s.track(track_named(&h, "Bass")).unwrap().clips().iter().filter(|c| c.name == "a" || c.name == "b").map(|c| c.start).collect();
    starts.sort_unstable();
    // 22 050 frames at 44.1 kHz are 24 000 at 48 kHz.
    assert_eq!(starts, [at, at + 24_000], "the second file follows the resampled length");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_stereo_file_is_refused_on_a_mono_track_with_a_hint() {
    let d = dir("width");
    let wide = wav_at(&d, "wide.wav", 48_000, 2);
    let mut h = harness();
    let (p, _) = bass_drop(&h, 300.0);
    let before = h.state().engine.session().track(track_named(&h, "Bass")).unwrap().clips().len();
    drop_at(&mut h, p, &[wide]);
    let s = h.state().engine.session();
    assert_eq!(s.track(track_named(&h, "Bass")).unwrap().clips().len(), before, "nothing lands on the mono track");
    assert!(h.state().ui.status.contains("2 channels on the 1-channel track Bass"), "{}", h.state().ui.status);
    assert!(h.state().ui.status.contains("new track of its width"), "{}", h.state().ui.status);
    let _ = std::fs::remove_dir_all(d);
}
