use crate::{
    audio,
    engine::{AudioEngine, Recorder},
    midi,
    model::{
        self, AutomationLane, AutomationPoint, AutomationTarget, FxChain, Note, Project, SampleRef,
        StudioSpec, Track, TrackKind, STEPS,
    },
};
use eframe::egui::{self, Align, Color32, FontId, Layout, RichText, Sense, Stroke, Vec2};
use std::{collections::HashMap, path::PathBuf, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorTab {
    Sequencer,
    PianoRoll,
    Mixer,
    Automation,
}
impl EditorTab {
    fn label(self) -> &'static str {
        match self {
            Self::Sequencer => "Sequencer",
            Self::PianoRoll => "Piano roll",
            Self::Mixer => "Mixer / effects",
            Self::Automation => "Automation",
        }
    }
}

#[derive(Default)]
struct UiActions {
    changed: bool,
    save: bool,
    open: bool,
    export_wav: bool,
    export_midi: bool,
    import_midi: bool,
    import_sample: bool,
    new_sample_track: bool,
    play_pause: bool,
    stop: bool,
    record: bool,
    back: bool,
    undo: bool,
    redo: bool,
    launch: Option<String>,
}

pub struct BeatCraftApp {
    launch_mode: Option<String>,
    current_studio: Option<String>,
    project: Option<Project>,
    project_path: Option<PathBuf>,
    tab: EditorTab,
    textures: HashMap<String, egui::TextureHandle>,
    status: String,
    engine: Option<AudioEngine>,
    audio_error: Option<String>,
    recorder: Option<Recorder>,
    paused: bool,
    selected_track: String,
    automation_track: String,
    automation_target: AutomationTarget,
    note_velocity: u8,
    note_length: u16,
    piano_roll_scroll_initialized: bool,
    selected_note: Option<(String, u16, u8)>,
    undo_stack: Vec<Project>,
    redo_stack: Vec<Project>,
    last_autosave: std::time::Instant,
    dirty_since_save: bool,
    /// Background render in flight (streaming pipeline stage 1): UI never blocks.
    pending_render: Option<std::sync::mpsc::Receiver<audio::RenderMsg>>,
    render_progress: Option<(usize, usize)>,
    /// Hardware MIDI-in: active connection, playhead-quantized recording arm.
    midi_session: Option<crate::midi::MidiInputSession>,
    midi_armed: bool,
    midi_session_ping: Option<String>,
    last_midi_write: std::time::Instant,
    recovery_offer: Option<PathBuf>,
    autosave_written: Option<PathBuf>,
    /// CLAP effects found on the system (scanned on first open of the mixer).
    clap_effects: Option<std::rc::Rc<Vec<(PathBuf, crate::clap::PluginInfo)>>>,
}

impl BeatCraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>, studio_id: Option<String>) -> Self {
        configure_theme(&cc.egui_ctx);
        let engine_result = AudioEngine::new();
        let (engine, audio_error) = match engine_result {
            Ok(engine) => (Some(engine), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let mut textures = HashMap::new();
        for (key, bytes) in [
            (
                "launcher",
                include_bytes!("../assets/beatcraft-studio.png").as_slice(),
            ),
            (
                "nyx",
                include_bytes!("../assets/drift-portrait.png").as_slice(),
            ),
            (
                "hermes",
                include_bytes!("../assets/boogie-portrait.png").as_slice(),
            ),
            (
                "alexander",
                include_bytes!("../assets/rhythm-portrait.png").as_slice(),
            ),
        ] {
            if let Ok(image) = image::load_from_memory(bytes) {
                let rgba = image.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let source = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                textures.insert(
                    key.into(),
                    cc.egui_ctx
                        .load_texture(key, source, egui::TextureOptions::LINEAR),
                );
            }
        }
        let current_studio = studio_id.clone();
        let project = current_studio.as_deref().map(model::default_project);
        let mut app = Self {
            launch_mode: studio_id,
            current_studio,
            project,
            project_path: None,
            tab: EditorTab::Sequencer,
            textures,
            status: "Rust desktop engine ready · offline-first".into(),
            engine,
            audio_error,
            recorder: None,
            paused: false,
            selected_track: "sub".into(),
            automation_track: "kick".into(),
            automation_target: AutomationTarget::Volume,
            note_velocity: 96,
            note_length: 2,
            piano_roll_scroll_initialized: false,
            selected_note: None,
            clap_effects: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_autosave: std::time::Instant::now(),
            dirty_since_save: false,
            pending_render: None,
            render_progress: None,
            midi_session: None,
            midi_armed: false,
            midi_session_ping: None,
            last_midi_write: std::time::Instant::now(),
            recovery_offer: None,
            autosave_written: None,
        };
        if app.project.is_some()
            && std::fs::metadata(Self::autosave_path())
                .map(|m| m.is_file())
                .unwrap_or(false)
        {
            app.recovery_offer = Some(Self::autosave_path());
        }
        app
    }

    /// Crash-recovery sidecar beside the system temp dir (local-first, no servers).
    fn autosave_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push("beatcraft-studio-autosave.json");
        p
    }

    /// Writes a sidecar JSON every ~25s while the project has unsaved edits.
    fn autosave_tick(&mut self) {
        if !self.dirty_since_save
            || self.last_autosave.elapsed() < std::time::Duration::from_secs(25)
        {
            return;
        }
        if let Some(project) = &self.project {
            let path = Self::autosave_path();
            if let Ok(text) = serde_json::to_string_pretty(project) {
                if std::fs::write(&path, text).is_ok() {
                    self.autosave_written = Some(path);
                    self.dirty_since_save = false; // revised only when a fresh edit lands
                    self.last_autosave = std::time::Instant::now();
                } else {
                    // give it another probe soon instead of silently giving up
                    self.last_autosave =
                        std::time::Instant::now() - std::time::Duration::from_secs(20);
                }
            }
        }
    }

    fn launcher_ui(&mut self, ctx: &egui::Context) {
        let animation_time = ctx.input(|input| input.time) as f32;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(color("#0C1017")).inner_margin(egui::Margin::same(28))).show(ctx,|ui|{
            ui.horizontal(|ui|{
                ui.vertical(|ui|{ui.heading(RichText::new("BEATCRAFT").size(29.0).strong().color(color("#F0F2E9")));ui.label(RichText::new("NATIVE RUST DAW  ·  THREE STUDIOS  ·  LOCAL FIRST").small().color(color("#9AA5B5")));});
                ui.with_layout(Layout::right_to_left(Align::Center),|ui|{ui.label(RichText::new("VERSION 1.0  ·  GPL-3.0+").small().color(color("#AEB9C8")));});
            });
            ui.add_space(22.0);
            ui.heading(RichText::new("Choose your room.").size(25.0).strong().color(color("#F0F2E9")));
            ui.label(RichText::new("A native Rust workspace for building phonk, funk, and hybrid beats.").color(color("#A1AABB")));
            ui.add_space(15.0);
            let available=ui.available_width();let gap=14.0;let card_width=((available-gap*2.0)/3.0).max(250.0);let card_height=(ui.available_height()-42.0).max(520.0);
            ui.horizontal_top(|ui|{
                for (index, spec) in model::STUDIOS.into_iter().enumerate() {
                    let phase = animation_time * 1.35 + index as f32 * 0.85;
                    let pulse = 0.22 + 0.20 * (0.5 + 0.5 * phase.sin());
                    let border = color(spec.color).gamma_multiply(pulse);
                    ui.allocate_ui_with_layout(Vec2::new(card_width,card_height),Layout::top_down(Align::Min),|ui|{
                        egui::Frame::group(ui.style()).fill(color("#151B25")).stroke(Stroke::new(1.0_f32,border)).corner_radius(18.0).inner_margin(16.0).show(ui,|ui|{
                            ui.set_min_height(590.0);
                            if let Some(texture)=self.textures.get(spec.id){ui.add(egui::Image::new(texture).fit_to_exact_size(Vec2::new(card_width-66.0,205.0)).corner_radius(12.0));}
                            ui.add_space(13.0);
                            ui.label(RichText::new(format!("{} ROOM",spec.genre)).small().strong().color(color(spec.color)));
                            ui.heading(RichText::new(spec.name).size(25.0).strong().color(color("#F0F2E9")));
                            ui.label(RichText::new(spec.title).strong().color(color(spec.color)));
                            ui.label(RichText::new(spec.tagline).color(color("#C5CDD7")));
                            ui.add_space(10.0);ui.label(RichText::new(spec.description).color(color("#9AA5B5")));
                            ui.with_layout(Layout::bottom_up(Align::Min),|ui|{
                                if ui.add_sized(Vec2::new(ui.available_width(),38.0),egui::Button::new(RichText::new("ENTER STUDIO  >").strong().color(Color32::BLACK)).fill(color(spec.color))).clicked(){self.launcher_open(spec.id);}
                                ui.add_space(8.0);
                            });
                        });
                    });
                    if spec.id!="alexander" {ui.add_space(gap);}
                }
            });
            ui.add_space(12.0);
            ui.horizontal(|ui|{ui.label(RichText::new("No account · no subscription · projects and samples stay on your computer").small().color(color("#8792A2")));ui.with_layout(Layout::right_to_left(Align::Center),|ui|{ui.label(RichText::new("BeatCraft Studio 1.0").small().color(color("#8792A2")));});});
        });
    }

    fn launcher_open(&mut self, studio_id: &str) {
        self.current_studio = Some(studio_id.to_owned());
        self.project = Some(model::default_project(studio_id));
        self.project_path = None;
        self.tab = EditorTab::Sequencer;
        self.status = format!("{} is ready", studio_spec(studio_id).name);
    }

    fn draw_header(&mut self, ctx: &egui::Context, project: &mut Project, actions: &mut UiActions) {
        let spec = studio_spec(&project.studio_id);
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(color("#101620"))
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if self.launch_mode.is_none() && ui.button("< Launcher").clicked() {
                        actions.back = true;
                    }
                    if let Some(texture) = self.textures.get(spec.id) {
                        ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(Vec2::splat(42.0))
                                .corner_radius(8.0),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(format!("{}  /  {}", spec.name, spec.title))
                                .size(18.0)
                                .strong()
                                .color(color("#F0F2E9")),
                        );
                        ui.label(RichText::new(spec.tagline).small().color(color("#9AA5B5")));
                    });
                    ui.separator();
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut project.name)
                                .desired_width(150.0)
                                .hint_text("Session name"),
                        )
                        .changed()
                    {
                        actions.changed = true;
                    }
                    if ui
                        .button("Open")
                        .on_hover_text("Open local BeatCraft project")
                        .clicked()
                    {
                        actions.open = true;
                    }
                    if ui
                        .button("Save")
                        .on_hover_text("Save project and copy its samples beside it")
                        .clicked()
                    {
                        actions.save = true;
                    }
                    if ui
                        .button("MIDI In")
                        .on_hover_text("Import a Standard MIDI File")
                        .clicked()
                    {
                        actions.import_midi = true;
                        actions.changed = true;
                    }
                    if ui.button("MIDI Out").clicked() {
                        actions.export_midi = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Export WAV").strong().color(Color32::BLACK),
                            )
                            .fill(color("#B8E58A")),
                        )
                        .clicked()
                    {
                        actions.export_wav = true;
                    }
                    egui::ComboBox::from_id_salt("export-depth")
                        .width(66.0)
                        .selected_text(match project.export_bit_depth {
                            model::WavBitDepth::Sixteen => "16",
                            model::WavBitDepth::TwentyFour => "24",
                            model::WavBitDepth::Float32 => "32f",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut project.export_bit_depth,
                                model::WavBitDepth::Sixteen,
                                "16-bit",
                            );
                            ui.selectable_value(
                                &mut project.export_bit_depth,
                                model::WavBitDepth::TwentyFour,
                                "24-bit",
                            );
                            ui.selectable_value(
                                &mut project.export_bit_depth,
                                model::WavBitDepth::Float32,
                                "32-bit float",
                            );
                        });
                    egui::ComboBox::from_id_salt("export-rate")
                        .width(58.0)
                        .selected_text(format!(
                            "{}k",
                            project.export_sample_rate.max(8_000) / 1_000
                        ))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut project.export_sample_rate,
                                44_100,
                                "44.1 kHz",
                            );
                            ui.selectable_value(&mut project.export_sample_rate, 48_000, "48 kHz");
                        });
                    if ui.button("Undo").on_hover_text("Ctrl+Z").clicked() {
                        actions.undo = true;
                    }
                    if ui.button("Redo").on_hover_text("Ctrl+Y").clicked() {
                        actions.redo = true;
                    }
                });
            });
    }

    fn draw_transport(
        &mut self,
        ctx: &egui::Context,
        project: &mut Project,
        actions: &mut UiActions,
    ) {
        egui::TopBottomPanel::top("transport").frame(egui::Frame::new().fill(color("#151B25")).inner_margin(egui::Margin::symmetric(14,7))).show(ctx,|ui|{
            ui.horizontal(|ui|{
                let play_text=if self.engine.as_ref().is_some_and(|e|e.is_playing()){"Pause"}else if self.paused{"Resume"}else{"Play"};
                if ui.add(egui::Button::new(RichText::new(play_text).strong().color(Color32::BLACK)).fill(color("#B8E58A"))).clicked(){actions.play_pause=true;}
                if ui.button("Stop").clicked(){actions.stop=true;}
                if self.recorder.is_some(){if ui.button(RichText::new("Stop recording").color(color("#F08D79"))).clicked(){actions.record=true;}}
                else if ui.button("Record input").on_hover_text("Record from the default audio input, then add the take as a sample track").clicked(){actions.record=true;}
                ui.separator();
                let midi_label = self.midi_session.as_ref().map(|m| format!("MIDI-in: {}", m.device_name)).unwrap_or_else(|| "MIDI in".to_string());
                ui.menu_button(midi_label, |ui| {
                    ui.set_min_width(230.0);
                    let devices = crate::midi::list_input_devices();
                    if devices.is_empty() {
                        ui.label(RichText::new("No MIDI inputs detected — plug in a keyboard/controller.").small().color(color("#8792A2")));
                    }
                    for name in devices {
                        if ui.button(&name).clicked() {
                            match crate::midi::connect_input(&name) {
                                Some(session) => {
                                    self.status = format!("MIDI-in connected: {}", session.device_name);
                                    self.midi_session = Some(session);
                                }
                                None => self.status = "Could not open that MIDI input.".into(),
                            }
                        }
                    }
                    if self.midi_session.is_some() && ui.button("Disconnect").clicked() {
                        self.midi_session = None;
                        self.status = "MIDI-in disconnected".into();
                    }
                    ui.label(RichText::new("Arm MIDI REC below, press Play, and play your controller — notes quantize onto the selected pattern.").small().color(color("#8792A2")));
                });
                if self.midi_session.is_some() {
                    let armed_color = color(if self.midi_armed { "#F0625D" } else { "#7A8494" });
                    if ui.button(RichText::new(if self.midi_armed { "● MIDI REC" } else { "○ MIDI REC" }).color(armed_color))
                        .on_hover_text("Arm hardware-MIDI recording: while playing, controller notes land on the grid at the playhead")
                        .clicked()
                    {
                        self.midi_armed = !self.midi_armed;
                        self.status = if self.midi_armed {
                            "MIDI REC armed — press Play and perform".into()
                        } else {
                            "MIDI REC disarmed".into()
                        };
                    }
                }
                ui.separator();
                ui.label(RichText::new("TEMPO").small().strong().color(color("#9AA5B5")));
                let mut tempo=project.bpm as i32;
                if ui.add(egui::DragValue::new(&mut tempo).range(50..=200).suffix(" BPM")).changed(){project.bpm=tempo as u16;let bpm=project.bpm;if let Some(p)=project.selected_pattern_mut(){p.bpm=bpm;}actions.changed=true;}
                ui.add_space(6.0);
                ui.label(RichText::new("SWING").small().strong().color(color("#9AA5B5")));
                let mut swing=project.swing as i32;
                if ui.add(egui::Slider::new(&mut swing,0..=60).suffix("%").show_value(true)).changed(){project.swing=swing as u8;actions.changed=true;}
                ui.separator();
                for tab in [EditorTab::Sequencer,EditorTab::PianoRoll,EditorTab::Mixer,EditorTab::Automation]{if ui.selectable_label(self.tab==tab,tab.label()).clicked(){self.tab=tab;}}
                ui.with_layout(Layout::right_to_left(Align::Center),|ui|{
                    if let Some(engine)=&self.engine {let total=project.arrangement.len().max(1)*STEPS;let step_duration=60.0/f32::from(project.bpm)/4.0;let step=((engine.position_frames() as f32/engine.sample_rate().max(1) as f32/step_duration) as usize).min(total.saturating_sub(1));let bar=step/STEPS;let sub=step%STEPS;ui.label(RichText::new(format!("BAR {:02}  ·  STEP {:02}",bar+1,sub+1)).monospace().color(color("#BAC4D0")));}
                    else {ui.label(RichText::new("AUDIO DEVICE OFFLINE").small().strong().color(color("#F08D79")));}
                });
            });
        });
    }

    fn draw_sequencer(
        &mut self,
        ctx: &egui::Context,
        project: &mut Project,
        actions: &mut UiActions,
    ) {
        let playhead = self
            .engine
            .as_ref()
            .filter(|engine| engine.is_playing())
            .and_then(|engine| {
                let step_duration = 60.0 / f64::from(project.bpm) / 4.0;
                let step = (engine.position_frames() as f64
                    / f64::from(engine.sample_rate().max(1))
                    / step_duration) as usize;
                let bar = step / STEPS;
                (project.arrangement.get(bar).and_then(Option::as_deref)
                    == Some(project.selected_pattern_id.as_str()))
                .then_some(step % STEPS)
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(color("#0C1017")).inner_margin(egui::Margin::same(14))).show(ctx,|ui|{
            let height=ui.available_height();
            ui.horizontal_top(|ui|{
                ui.allocate_ui_with_layout(Vec2::new(230.0,height),Layout::top_down(Align::Min),|ui|{
                    panel_frame(ui,|ui|{
                        ui.label(RichText::new("PATTERN DECK").small().strong().color(color("#9AA5B5")));
                        ui.heading(RichText::new(format!("{:02} grooves",project.patterns.len())).size(17.0).color(color("#F0F2E9")));
                        egui::ScrollArea::vertical().id_salt("pattern-deck").max_height((height-195.0).max(140.0)).show(ui,|ui|{
                            let patterns=project.patterns.clone();
                            for pattern in patterns {
                                let selected=pattern.id==project.selected_pattern_id;
                                if ui.add_sized(Vec2::new(ui.available_width(),52.0),egui::SelectableLabel::new(selected,format!("{}\n{} · {} BPM",pattern.name,pattern.genre,pattern.bpm))).clicked(){self.selected_note=None;project.selected_pattern_id=pattern.id;project.bpm=pattern.bpm;actions.changed=true;}
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button("+ New").clicked() {
                                project.new_pattern(false);
                                actions.changed = true;
                            }
                            if ui.button("Duplicate").clicked() {
                                project.new_pattern(true);
                                actions.changed = true;
                            }
                        });
                        if ui.button("Clear selected pattern").clicked(){if let Some(p)=project.selected_pattern_mut(){for row in p.rows.values_mut(){row.fill(false);}for notes in p.notes.values_mut(){notes.clear();}}actions.changed=true;}
                        if ui.button("+ Import WAV sample").clicked(){actions.import_sample=true;actions.new_sample_track=true;actions.changed=true;}
                        ui.add_space(6.0);ui.label(RichText::new("Patterns, note events, and sample references are stored locally in your project.").small().color(color("#8792A2")));
                    });
                });
                ui.add_space(10.0);
                ui.vertical(|ui|{
                    panel_frame(ui,|ui|{
                        ui.horizontal(|ui|{
                            ui.vertical(|ui|{ui.label(RichText::new("STEP SEQUENCER  /  16 STEPS").small().strong().color(color("#9AA5B5")));if let Some(pattern)=project.selected_pattern_mut(){if ui.add(egui::TextEdit::singleline(&mut pattern.name).desired_width(230.0)).changed(){actions.changed=true;}}});
                            ui.with_layout(Layout::right_to_left(Align::Center),|ui|{let count=project.selected_pattern().map(|p|p.rows.values().map(|r|r.iter().filter(|v|**v).count()).sum::<usize>()).unwrap_or(0);ui.label(RichText::new(format!("{count} active steps")).color(color("#A1AABB")));});
                        });
                        ui.separator();draw_step_grid(ui,project,actions,playhead);
                        ui.label(RichText::new("Click a cell to toggle it · M = mute · S = solo · use the piano roll for note pitch and length").small().color(color("#8E99A9")));
                    });
                    ui.add_space(10.0);draw_arrangement(ui,project,actions);
                });
            });
        });
    }

    fn draw_piano_roll(
        &mut self,
        ctx: &egui::Context,
        project: &mut Project,
        actions: &mut UiActions,
    ) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(color("#0C1017")).inner_margin(egui::Margin::same(14))).show(ctx,|ui|{
            panel_frame(ui,|ui|{
                ui.horizontal(|ui|{
                    ui.label(RichText::new("PIANO ROLL").small().strong().color(color("#9AA5B5")));
                    egui::ComboBox::from_id_salt("piano-track").selected_text(track_name(project,&self.selected_track)).show_ui(ui,|ui|{for track in project.tracks.iter().filter(|t|matches!(t.kind,TrackKind::Sub|TrackKind::Bass|TrackKind::Keys)){ui.selectable_value(&mut self.selected_track,track.id.clone(),&track.name);}});
                    ui.label(RichText::new("New note length").color(color("#E5E8E0")));ui.add(egui::DragValue::new(&mut self.note_length).range(1..=16).suffix(" steps"));
                    ui.label(RichText::new("Velocity").color(color("#E5E8E0")));ui.add(egui::Slider::new(&mut self.note_velocity,1..=127).show_value(true));
                    if ui.button("Clear track notes").clicked() {
                        if let Some(pattern) = project.selected_pattern_mut() {
                            if let Some(notes) = pattern.notes.get_mut(&self.selected_track) {
                                notes.clear();
                            }
                            if let Some(row) = pattern.rows.get_mut(&self.selected_track) {
                                row.fill(false);
                            }
                        }
                        actions.changed = true;
                    }
                });
                ui.label(RichText::new("MIDI 0–127 (C-1 to G9) · click an empty cell to add a note; select notes to edit length, velocity, or delete.").small().color(color("#A1AABB")));
                ui.separator();
                let mut piano_scroll=egui::ScrollArea::both().id_salt("piano-roll").auto_shrink([false,false]).max_height(ui.available_height()-75.0);
                if !self.piano_roll_scroll_initialized {
                    piano_scroll=piano_scroll.vertical_scroll_offset(1120.0);
                    self.piano_roll_scroll_initialized=true;
                }
                piano_scroll.show(ui,|ui|{
                    egui::Grid::new("piano-roll-grid").spacing(Vec2::new(2.0,1.0)).striped(false).show(ui,|ui|{
                        ui.label(RichText::new("PITCH").small().strong());for step in 0..STEPS{ui.add_sized(Vec2::new(27.0,20.0),egui::Label::new(RichText::new(format!("{:02}",step+1)).small()).halign(Align::Center));}ui.end_row();
                        for pitch in (0_u8..=127_u8).rev(){
                            let name=note_name(pitch);let black=matches!(pitch%12,1|3|6|8|10);
                            ui.add_sized(Vec2::new(36.0,17.0),egui::Label::new(RichText::new(name).small().color(if pitch%12==0{color("#D6DCE4")}else{color("#8894A4")})));
                            for step in 0..STEPS{
                                let active=project.selected_pattern().and_then(|p|p.notes.get(&self.selected_track)).is_some_and(|events|events.iter().any(|n|n.pitch==pitch&&n.step as usize<=step&&step<(n.step as usize+n.duration as usize)));
                                let exists_start=project.selected_pattern().and_then(|p|p.notes.get(&self.selected_track)).is_some_and(|events|events.iter().any(|n|n.pitch==pitch&&n.step as usize==step));
                                let selected=self.selected_note==Some((self.selected_track.clone(),step as u16,pitch));
                                let base=if active{color(&track_color(project,&self.selected_track))}else if black{color("#171E28")}else{color("#202832")};let fill=if selected{base.gamma_multiply(1.45)}else{base};
                                let (rect,response)=ui.allocate_exact_size(Vec2::new(27.0,16.0),Sense::click());ui.painter().rect_filled(rect,2.0,fill);ui.painter().rect_stroke(rect,2.0,Stroke::new(if step%4==0{1.2_f32}else{0.5_f32},if step%4==0{color("#657286")}else{color("#384454")}),egui::StrokeKind::Inside);
                                if active&&exists_start{ui.painter().text(rect.left_center()+Vec2::new(2.0,0.0),egui::Align2::LEFT_CENTER,note_name(pitch),FontId::proportional(8.0),Color32::BLACK);}
                                if response.clicked(){if active{let start=project.selected_pattern().and_then(|p|p.notes.get(&self.selected_track)).and_then(|events|events.iter().find(|n|n.pitch==pitch&&n.step as usize<=step&&step<(n.step as usize+n.duration as usize))).map(|n|n.step).unwrap_or(step as u16);self.selected_note=Some((self.selected_track.clone(),start,pitch));if let Some(note)=project.selected_pattern().and_then(|p|p.notes.get(&self.selected_track)).and_then(|events|events.iter().find(|n|n.pitch==pitch&&n.step==start)){self.note_velocity=note.velocity;self.note_length=note.duration.min(16);}}
                                    else{let note=Note{step:step as u16,pitch,duration:self.note_length.clamp(1,16),velocity:self.note_velocity};if let Some(p)=project.selected_pattern_mut(){p.notes.entry(self.selected_track.clone()).or_default().push(note);sync_pitched_row(p,&self.selected_track);}self.selected_note=Some((self.selected_track.clone(),step as u16,pitch));actions.changed=true;}}
                            }ui.end_row();
                        }
                    });
                });
                if let Some((track_id,start,pitch))=self.selected_note.clone(){ui.separator();ui.horizontal(|ui|{
                    ui.label(RichText::new(format!("Selected note · {} · step {}",note_name(pitch),start+1)).color(color("#E5E8E0")));let mut length=self.note_length;let mut velocity=self.note_velocity;
                    if ui.add(egui::Slider::new(&mut length,1..=16).text("Length")).changed(){self.note_length=length;if let Some(note)=find_note_mut(project,&track_id,start,pitch){note.duration=length;actions.changed=true;}}
                    if ui.add(egui::Slider::new(&mut velocity,1..=127).text("Velocity")).changed(){self.note_velocity=velocity;if let Some(note)=find_note_mut(project,&track_id,start,pitch){note.velocity=velocity;actions.changed=true;}}
                    if ui.button("Delete note").clicked(){if let Some(p)=project.selected_pattern_mut(){if let Some(events)=p.notes.get_mut(&track_id){events.retain(|n|!(n.step==start&&n.pitch==pitch));}sync_pitched_row(p,&track_id);}self.selected_note=None;actions.changed=true;}
                });}
            });
        });
    }

    fn draw_mixer(&mut self, ctx: &egui::Context, project: &mut Project, actions: &mut UiActions) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(color("#0C1017"))
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                {
                    ui.group(|ui| {
                        ui.heading(RichText::new("MASTER SUITE").color(color("#E7C779")));
                        ui.label(RichText::new("3-band EQ → stereo width → compressor → limiter, always the final buss.").small().color(color("#8792A2")));
                        let suite = &mut project.master_suite;
                        egui::Grid::new("master-suite-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                            ui.checkbox(&mut suite.eq_enabled, "EQ");
                            ui.horizontal(|ui| {
                                if ui.add(egui::Slider::new(&mut suite.eq_low_db, -12.0..=12.0).text("Low dB")).changed() { actions.changed = true; }
                                if ui.add(egui::Slider::new(&mut suite.eq_mid_db, -12.0..=12.0).text("Mid dB")).changed() { actions.changed = true; }
                                if ui.add(egui::Slider::new(&mut suite.eq_high_db, -12.0..=12.0).text("High dB")).changed() { actions.changed = true; }
                            });
                            ui.end_row();
                            ui.checkbox(&mut suite.comp_enabled, "Compressor");
                            ui.horizontal(|ui| {
                                if ui.add(egui::Slider::new(&mut suite.comp_threshold_db, -40.0..=0.0).text("Thresh dB")).changed() { actions.changed = true; }
                                if ui.add(egui::Slider::new(&mut suite.comp_ratio, 1.0..=12.0).text("Ratio")).changed() { actions.changed = true; }
                                if ui.add(egui::Slider::new(&mut suite.comp_makeup_db, 0.0..=18.0).text("Makeup dB")).changed() { actions.changed = true; }
                            });
                            ui.end_row();
                            ui.checkbox(&mut suite.limiter_enabled, "Limiter");
                            ui.horizontal(|ui| {
                                if ui.add(egui::Slider::new(&mut suite.limiter_ceiling, 0.5..=1.0).text("Ceiling")).changed() { actions.changed = true; }
                                if ui.add(egui::Slider::new(&mut suite.stereo_width, 0.0..=2.0).text("Width")).changed() { actions.changed = true; }
                            });
                            ui.end_row();
                        });
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("CLAP FX").color(color("#E7C779")).small());
                            if self.clap_effects.is_none() {
                                self.clap_effects = Some(std::rc::Rc::new(crate::clap::scan_effects()));
                            }
                            let effects = self.clap_effects.clone().unwrap_or_default();
                            let current = project
                                .master_clap
                                .as_ref()
                                .map(|i| i.name.clone())
                                .unwrap_or_else(|| "None".to_string());
                            egui::ComboBox::from_id_salt("master-clap-fx")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    if ui.selectable_label(project.master_clap.is_none(), "None").clicked() {
                                        project.master_clap = None;
                                        actions.changed = true;
                                    }
                                    if effects.is_empty() {
                                        ui.label(RichText::new("No CLAP effects found on this system").small().color(color("#8792A2")));
                                    }
                                    for (path, info) in effects.iter() {
                                        let selected = project.master_clap.as_ref().is_some_and(|i| i.plugin_id == info.id);
                                        if ui.selectable_label(selected, format!("{}{}", info.name, if info.vendor.is_empty() { String::new() } else { format!(" — {}", info.vendor) })).clicked() {
                                            project.master_clap = Some(crate::model::ClapInsert {
                                                path: path.to_string_lossy().to_string(),
                                                plugin_id: info.id.clone(),
                                                name: info.name.clone(),
                                                enabled: true,
                                            });
                                            actions.changed = true;
                                        }
                                    }
                                });
                            if let Some(insert) = &mut project.master_clap {
                                if ui.checkbox(&mut insert.enabled, "enabled").changed() {
                                    actions.changed = true;
                                }
                            }
                        });
                    });
                    ui.add_space(8.0);
                }
                egui::ScrollArea::vertical()
                    .id_salt("mixer")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.heading(RichText::new("MIXER & EFFECTS").color(color("#F0F2E9")));
                            if ui.button("+ Add sample track").clicked() {
                                actions.import_sample = true;
                                actions.new_sample_track = true;
                                actions.changed = true;
                            }
                            ui.label(
                            RichText::new(
                                "Per-track level, pan, mute/solo, filter, drive, delay, and reverb",
                            )
                            .small()
                            .color(color("#9AA5B5")),
                        );
                        });
                        for index in 0..project.tracks.len() {
                            let track_name = project.tracks[index].name.clone();
                            let track_color = color(&project.tracks[index].color);
                            egui::Frame::group(ui.style())
                                .fill(color("#151B25"))
                                .stroke(Stroke::new(1.0_f32, color("#2A3444")))
                                .inner_margin(12.0)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.colored_label(
                                            track_color,
                                            RichText::new(&track_name).strong(),
                                        );
                                        let mut mute = project.tracks[index].mute;
                                        let mut solo = project.tracks[index].solo;
                                        if ui.toggle_value(&mut mute, "M").clicked() {
                                            project.tracks[index].mute = mute;
                                            actions.changed = true;
                                        }
                                        if ui.toggle_value(&mut solo, "S").clicked() {
                                            project.tracks[index].solo = solo;
                                            actions.changed = true;
                                        }
                                        let mut volume = project.tracks[index].volume;
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut volume, 0.0..=1.0)
                                                    .text("Level")
                                                    .show_value(true),
                                            )
                                            .changed()
                                        {
                                            project.tracks[index].volume = volume;
                                            actions.changed = true;
                                        }
                                        let mut pan = project.tracks[index].pan;
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut pan, -1.0..=1.0)
                                                    .text("Pan")
                                                    .show_value(true),
                                            )
                                            .changed()
                                        {
                                            project.tracks[index].pan = pan;
                                            actions.changed = true;
                                        }
                                    });
                                    ui.horizontal_wrapped(|ui| {
                                        let fx = &mut project.tracks[index].fx;
                                        if ui
                                            .add(
                                                egui::Slider::new(
                                                    &mut fx.lowpass_hz,
                                                    40.0..=20_000.0,
                                                )
                                                .logarithmic(true)
                                                .text("Low-pass Hz"),
                                            )
                                            .changed()
                                        {
                                            actions.changed = true;
                                        }
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut fx.drive, 0.0..=1.0)
                                                    .text("Drive"),
                                            )
                                            .changed()
                                        {
                                            actions.changed = true;
                                        }
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut fx.delay_mix, 0.0..=0.8)
                                                    .text("Delay"),
                                            )
                                            .changed()
                                        {
                                            actions.changed = true;
                                        }
                                        if ui
                                            .add(
                                                egui::Slider::new(
                                                    &mut fx.delay_feedback,
                                                    0.0..=0.9,
                                                )
                                                .text("Feedback"),
                                            )
                                            .changed()
                                        {
                                            actions.changed = true;
                                        }
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut fx.delay_ms, 20.0..=1200.0)
                                                    .text("Delay ms"),
                                            )
                                            .changed()
                                        {
                                            actions.changed = true;
                                        }
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut fx.reverb_mix, 0.0..=0.8)
                                                    .text("Reverb"),
                                            )
                                            .changed()
                                        {
                                            actions.changed = true;
                                        }
                                    });
                                    if project.tracks[index].kind == TrackKind::Sample {
                                        ui.horizontal(|ui| {
                                            if let Some(sample) =
                                                project.tracks[index].sample.as_mut()
                                            {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "WAV · {}",
                                                        PathBuf::from(&sample.path)
                                                            .file_name()
                                                            .unwrap_or_default()
                                                            .to_string_lossy()
                                                    ))
                                                    .small()
                                                    .color(color("#A1AABB")),
                                                );
                                                if ui
                                                    .add(
                                                        egui::Slider::new(
                                                            &mut sample.trim_start,
                                                            0.0..=0.98,
                                                        )
                                                        .text("Start"),
                                                    )
                                                    .changed()
                                                {
                                                    actions.changed = true;
                                                }
                                                if ui
                                                    .add(
                                                        egui::Slider::new(
                                                            &mut sample.trim_end,
                                                            0.02..=1.0,
                                                        )
                                                        .text("End"),
                                                    )
                                                    .changed()
                                                {
                                                    actions.changed = true;
                                                }
                                                if ui.checkbox(&mut sample.looped, "Loop").changed()
                                                {
                                                    actions.changed = true;
                                                }
                                                ui.label(RichText::new("Fit").small().color(color("#8792A2")));
                                                if ui
                                                    .add(
                                                        egui::DragValue::new(&mut sample.fit_steps)
                                                            .range(0..=64)
                                                            .speed(1),
                                                    )
                                                    .on_hover_text("Varispeed warp: force the take to exactly N 16ths (0 = natural)")
                                                    .changed()
                                                {
                                                    actions.changed = true;
                                                }
                                            } else {
                                                ui.label(
                                                    RichText::new("No WAV assigned")
                                                        .color(color("#A1AABB")),
                                                );
                                            }
                                            if ui.button("Assign WAV").clicked() {
                                                actions.import_sample = true;
                                                actions.changed = true;
                                            }
                                        });
                                    }
                                });
                            ui.add_space(8.0);
                        }
                        panel_frame(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("MASTER BUS").strong());
                                if ui
                                    .add(
                                        egui::Slider::new(&mut project.master, 0.0..=1.0)
                                            .text("Master"),
                                    )
                                    .changed()
                                {
                                    actions.changed = true;
                                }
                                if ui
                                    .add(
                                        egui::Slider::new(&mut project.grit, 0.0..=1.0)
                                            .text("Tape grit"),
                                    )
                                    .changed()
                                {
                                    actions.changed = true;
                                }
                            });
                        });
                    });
            });
    }

    fn draw_automation(
        &mut self,
        ctx: &egui::Context,
        project: &mut Project,
        actions: &mut UiActions,
    ) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(color("#0C1017")).inner_margin(egui::Margin::same(14))).show(ctx,|ui|{
            panel_frame(ui,|ui|{
                ui.heading(RichText::new("AUTOMATION LANES").color(color("#F0F2E9")));ui.label(RichText::new("Edit 16 points across the arrangement; the renderer interpolates between them.").small().color(color("#A1AABB")));
                ui.horizontal(|ui|{
                    egui::ComboBox::from_id_salt("automation-track").selected_text(track_name(project,&self.automation_track)).show_ui(ui,|ui|{for t in &project.tracks{ui.selectable_value(&mut self.automation_track,t.id.clone(),&t.name);}});
                    egui::ComboBox::from_id_salt("automation-target").selected_text(match self.automation_target{AutomationTarget::Volume=>"Volume",AutomationTarget::Pan=>"Pan"}).show_ui(ui,|ui|{ui.selectable_value(&mut self.automation_target,AutomationTarget::Volume,"Volume");ui.selectable_value(&mut self.automation_target,AutomationTarget::Pan,"Pan");});
                    if ui.button("Remove lane").clicked(){project.automation.retain(|l|!(l.track_id==self.automation_track&&l.target==self.automation_target));actions.changed=true;}
                });
                let range=if self.automation_target==AutomationTarget::Volume{0.0..=1.0}else{-1.0..=1.0};
                let mut values=[if self.automation_target==AutomationTarget::Volume{1.0}else{0.0};STEPS];
                let beat_span=project.arrangement.len().max(1) as f32*4.0;
                if let Some(lane)=project.automation.iter().find(|l|l.track_id==self.automation_track&&l.target==self.automation_target){for point in &lane.points{let step=((point.beat/beat_span)*STEPS as f32).round() as usize;if step<STEPS{values[step]=point.value;}}}
                ui.separator();let mut lane_changed=false;
                ui.horizontal(|ui| {
                    for (step, value) in values.iter_mut().enumerate() {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(format!("{:02}", step + 1)).small().color(color("#9AA5B5")));
                            if ui.add(egui::Slider::new(value, range.clone()).vertical().show_value(false).trailing_fill(true).step_by(0.01)).changed() {
                                lane_changed = true;
                                actions.changed = true;
                            }
                        });
                    }
                });
                if lane_changed {
                    let point_spacing=beat_span/STEPS as f32;
                    let points=(0..STEPS).map(|step|AutomationPoint{beat:step as f32*point_spacing,value:values[step]}).collect();
                    if let Some(lane)=project.automation.iter_mut().find(|l|l.track_id==self.automation_track&&l.target==self.automation_target){lane.points=points;}
                    else{project.automation.push(AutomationLane{id:format!("{}-{:?}",self.automation_track,self.automation_target),track_id:self.automation_track.clone(),target:self.automation_target,points});}
                }
                ui.label(RichText::new("Automation is stored with the session and applied during WAV render/playback.").small().color(color("#8792A2")));
            });
        });
    }

    fn perform_actions(&mut self, actions: UiActions) {
        if actions.back && self.launch_mode.is_none() {
            self.current_studio = None;
            self.project = None;
            self.project_path = None;
            return;
        }
        if actions.undo {
            if let Some(previous) = self.undo_stack.pop() {
                if let Some(current) = self.project.replace(previous) {
                    self.redo_stack.push(current);
                }
                self.status = "Undo".into();
            }
        }
        if actions.redo {
            if let Some(next) = self.redo_stack.pop() {
                if let Some(current) = self.project.replace(next) {
                    self.undo_stack.push(current);
                }
                self.status = "Redo".into();
            }
        }
        if actions.open {
            self.open_project();
        }
        if actions.save {
            self.save_project();
        }
        if actions.import_midi {
            self.import_midi_file();
        }
        if actions.import_sample {
            self.import_sample_file(actions.new_sample_track);
        }
        if actions.export_wav {
            self.export_wav();
        }
        if actions.export_midi {
            self.export_midi();
        }
        if actions.play_pause {
            self.toggle_playback();
        }
        if actions.stop {
            if let Some(engine) = &self.engine {
                engine.stop();
            }
            self.paused = false;
            self.status = "Stopped".into();
        }
        if actions.record {
            self.toggle_recording();
        }
        if actions.changed {
            self.dirty_since_save = true;
        }
        if let Some(id) = actions.launch {
            self.launcher_open(&id);
        }
        self.autosave_tick();
    }
    fn save_project(&mut self) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let path = self.project_path.clone().or_else(|| {
            rfd::FileDialog::new()
                .add_filter("BeatCraft project", &["json"])
                .set_file_name(format!("{}.bcproj.json", safe_filename(&project.name)))
                .save_file()
        });
        if let Some(path) = path {
            match crate::project::save(&path, project) {
                Ok(()) => {
                    self.project_path = Some(path.clone());
                    self.dirty_since_save = false;
                    self.recovery_offer = None;
                    self.status = format!("Saved project and sample bundle: {}", path.display());
                }
                Err(e) => self.status = format!("Save failed: {e}"),
            }
        }
    }
    fn open_project(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("BeatCraft project", &["json"])
            .pick_file()
        {
            match crate::project::load(&path) {
                Ok(project) => {
                    if self
                        .launch_mode
                        .as_deref()
                        .is_some_and(|id| id != project.studio_id)
                    {
                        self.status="This session belongs to another studio; open it from the BeatCraft launcher.".into();
                        return;
                    }
                    self.current_studio = Some(project.studio_id.clone());
                    self.project = Some(project);
                    self.project_path = Some(path.clone());
                    self.undo_stack.clear();
                    self.redo_stack.clear();
                    self.status = format!(
                        "Opened {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
                Err(e) => self.status = format!("Could not open project: {e}"),
            }
        }
    }
    fn import_midi_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MIDI files", &["mid", "midi"])
            .pick_file()
        else {
            return;
        };
        if let Some(project) = self.project.as_mut() {
            match midi::import_midi(&path, project) {
                Ok(count) => {
                    self.status = format!("Imported {count} MIDI notes");
                    self.project_path = None;
                }
                Err(e) => self.status = format!("MIDI import failed: {e}"),
            }
        }
    }
    fn import_sample_file(&mut self, new_sample_track: bool) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV audio", &["wav"])
            .pick_file()
        else {
            return;
        };
        if let Err(e) = audio::load_wav(&path) {
            self.status = format!("WAV sample could not be decoded: {e}");
            return;
        }
        if let Some(project) = self.project.as_mut() {
            let selected = self.selected_track.clone();
            let target = if new_sample_track {
                None
            } else {
                project
                    .tracks
                    .iter_mut()
                    .find(|t| t.id == selected && t.kind == TrackKind::Sample)
            };
            if let Some(track) = target {
                track.sample = Some(SampleRef::new(path.to_string_lossy().into_owned()));
                self.status = format!("Sample assigned to {}", track.name);
            } else {
                let n = project
                    .tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Sample)
                    .count()
                    + 1;
                let id = format!("sample-{n}");
                let track = Track {
                    id: id.clone(),
                    name: format!("Sample {n}"),
                    kind: TrackKind::Sample,
                    color: "#F2A65A".into(),
                    volume: 0.82,
                    pan: 0.0,
                    mute: false,
                    solo: false,
                    fx: FxChain::default(),
                    sample: Some(SampleRef::new(path.to_string_lossy().into_owned())),
                };
                for pattern in &mut project.patterns {
                    pattern.rows.insert(id.clone(), vec![false; STEPS]);
                }
                project.tracks.push(track);
                self.selected_track = id.clone();
                if let Some(pattern) = project.selected_pattern_mut() {
                    pattern.rows.entry(id).or_insert_with(|| vec![false; STEPS]);
                }
                self.status = format!(
                    "Imported {} as a new sample track",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
    }
    fn export_wav(&mut self) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Stereo WAV", &["wav"])
            .set_file_name(format!("{}.wav", safe_filename(&project.name)))
            .save_file()
        else {
            return;
        };
        let sample_rate = match project.export_sample_rate {
            48_000 => 48_000,
            _ => 44_100,
        };
        match audio::render(project, sample_rate).and_then(|buffer| {
            audio::write_wav_depth(&path, &buffer, sample_rate, project.export_bit_depth)
                .map(|_| buffer.len() / 2)
        }) {
            Ok(frames) => {
                self.status = format!(
                    "Rendered {:.1} seconds to {}",
                    frames as f32 / sample_rate as f32,
                    path.display()
                )
            }
            Err(e) => self.status = format!("WAV export failed: {e}"),
        }
    }
    fn export_midi(&mut self) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Standard MIDI", &["mid"])
            .set_file_name(format!("{}.mid", safe_filename(&project.name)))
            .save_file()
        else {
            return;
        };
        match midi::export_midi(project, &path) {
            Ok(n) => self.status = format!("Exported MIDI · {n} events · {}", path.display()),
            Err(e) => self.status = format!("MIDI export failed: {e}"),
        }
    }
    /// Drain the background render: show progress live, hand off when complete.
    fn service_pending_render(&mut self) {
        let Some(rx) = &self.pending_render else {
            return;
        };
        let mut done = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                audio::RenderMsg::Progress { bar, bars } => {
                    self.render_progress = Some((bar, bars));
                }
                audio::RenderMsg::Done(result) => done = Some(result),
            }
        }
        if let Some((bar, bars)) = self.render_progress {
            if done.is_none() {
                self.status = format!("Rendering bar {}/{bars} on background thread…", bar + 1);
            }
        }
        if let Some(result) = done {
            self.pending_render = None;
            self.render_progress = None;
            match result {
                Ok(buffer) => {
                    if buffer.is_empty() {
                        // streaming mode: audio already flowed through the ring
                        self.status = "Stream render finished — tail playing out".into();
                    } else if let Some(engine) = &self.engine {
                        engine.play(buffer);
                        self.paused = false;
                        self.status = "Playing rendered arrangement".into();
                    }
                }
                Err(e) => self.status = format!("Playback render failed: {e}"),
            }
        }
    }

    /// Consume hardware controller events from MIDI-in. When armed and playing,
    /// notes are quantized onto the selected pattern's 16-step grid at the
    /// playhead (GM mapping on drum floors, pitched notes on Drift/Boogie).
    fn service_midi_in(&mut self) {
        let Some(session) = &self.midi_session else {
            return;
        };
        let mut events = Vec::new();
        while let Ok((_at, event)) = session.rx.try_recv() {
            events.push(event);
        }
        if !events.is_empty() {
            if let Some(session) = &self.midi_session {
                self.midi_session_ping = Some(session.device_name.clone());
            }
        }
        for event in events {
            if let crate::midi::LiveMidiEvent::NoteOn {
                pitch, velocity, ..
            } = event
            {
                self.handle_midi_note_on(pitch, velocity);
            }
        }
    }

    fn handle_midi_note_on(&mut self, pitch: u8, velocity: u8) {
        let name = midi_note_name(pitch);
        let armed = self.midi_armed;
        let playing = self.engine.as_ref().is_some_and(AudioEngine::is_playing);
        if !armed || !playing {
            self.status = format!(
                "MIDI-in ♪ {name} (vel {velocity}) — arm MIDI REC and press play to record it to the grid"
            );
            return;
        }
        let (engine_rate, frames) = match &self.engine {
            Some(e) => (e.sample_rate() as f32, e.position_frames()),
            None => return,
        };
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let step_seconds = 60.0 / (project.bpm.clamp(50, 200) as f32) / 4.0;
        let frames_per_step = (step_seconds * engine_rate).max(1.0);
        let step = ((frames as f32 / frames_per_step).round() as u16) % STEPS as u16;
        // throttle undo spam: one undo snapshot per ~500 ms of performance
        if self.last_midi_write.elapsed() > std::time::Duration::from_millis(500) {
            self.push_undo(project.clone());
        }
        self.last_midi_write = std::time::Instant::now();
        // chosen lane decided before we borrow the project mutably
        let preferred_lane = self.selected_note.as_ref().map(|(lane, _, _)| lane.clone());
        let Some(project) = self.project.as_mut() else {
            return;
        };
        let Some(pattern) = project.selected_pattern_mut() else {
            return;
        };
        if pattern.rows.is_empty() {
            // Pitched floor: insert a grid note in the current lane.
            let lane = preferred_lane
                .or_else(|| pattern.notes.keys().next().cloned())
                .unwrap_or_else(|| "keys".to_string());
            let velocity = velocity.max(1);
            pattern
                .notes
                .entry(lane.clone())
                .or_default()
                .push(model::Note {
                    step,
                    pitch,
                    duration: 1,
                    velocity,
                });
            self.dirty_since_save = true;
            self.status = format!("MIDI REC ♪ {name} → {lane} · step {}", step + 1);
        } else {
            // Drum floor: GM row mapping, set the step hot.
            let row = crate::midi::drum_row_for_pitch(pitch).to_string();
            let row = if pattern.rows.contains_key(&row) {
                row
            } else {
                pattern.rows.keys().next().cloned().unwrap_or_default()
            };
            if let Some(steps) = pattern.rows.get_mut(&row) {
                if let Some(cell) = steps.get_mut(step as usize) {
                    *cell = true;
                }
            }
            self.dirty_since_save = true;
            self.status = format!("MIDI REC ♪ {name} → {row} · step {}", step + 1);
        }
    }

    fn toggle_playback(&mut self) {
        let Some(engine) = &self.engine else {
            self.status = self
                .audio_error
                .clone()
                .unwrap_or_else(|| "Audio output unavailable; WAV export still works.".into());
            return;
        };
        if engine.is_playing() {
            engine.pause();
            self.paused = true;
            self.status = "Playback paused".into();
        } else if self.paused && engine.position_frames() > 0 {
            engine.resume();
            self.paused = false;
            self.status = "Playback resumed".into();
        } else if let Some(project) = &self.project {
            // Streaming pipeline: bounce on a worker thread; the UI (transport,
            // MIDI-in, mixer) stays fully live while audio is being produced.
            // True streaming: the render thread stays just ahead of the playhead;
            // playback starts with the first bar, memory stays flat forever.
            let (rx, consumer, total, _handle) =
                audio::spawn_stream(project.clone(), engine.sample_rate());
            engine.play_stream(consumer, total);
            self.paused = false;
            self.pending_render = Some(rx);
            self.render_progress = Some((0, project.arrangement.len().max(1) + 1));
            self.status = "Streaming playback — rendering just ahead of the playhead".into();
        }
    }
    fn toggle_recording(&mut self) {
        if let Some(recorder) = self.recorder.take() {
            let sample_rate = recorder.sample_rate;
            let samples = recorder.finish();
            if samples.is_empty() {
                self.status = "Recording contained no input samples".into();
                return;
            }
            let Some(path) = rfd::FileDialog::new()
                .add_filter("Stereo WAV", &["wav"])
                .set_file_name("BeatCraft-Recording.wav")
                .save_file()
            else {
                self.status = "Recording discarded".into();
                return;
            };
            match audio::write_wav(&path, &samples, sample_rate) {
                Ok(()) => {
                    self.status = format!("Recorded take saved to {}", path.display());
                    self.add_recorded_sample(path);
                }
                Err(e) => self.status = format!("Could not save recording: {e}"),
            }
        } else {
            match Recorder::start() {
                Ok(recorder) => {
                    self.recorder = Some(recorder);
                    self.status =
                        "Recording from default audio input · up to 90 seconds per take".into();
                }
                Err(e) => self.status = format!("Input unavailable: {e}"),
            }
        }
    }
    fn add_recorded_sample(&mut self, path: PathBuf) {
        if let Some(project) = self.project.as_mut() {
            let n = project
                .tracks
                .iter()
                .filter(|t| t.kind == TrackKind::Sample)
                .count()
                + 1;
            let id = format!("recording-{n}");
            let track = Track {
                id: id.clone(),
                name: format!("Recording {n}"),
                kind: TrackKind::Sample,
                color: "#F2A65A".into(),
                volume: 0.82,
                pan: 0.0,
                mute: false,
                solo: false,
                fx: FxChain::default(),
                sample: Some(SampleRef::new(path.to_string_lossy().into_owned())),
            };
            for p in &mut project.patterns {
                p.rows.insert(id.clone(), vec![false; STEPS]);
            }
            if let Some(p) = project.selected_pattern_mut() {
                p.rows
                    .entry(id.clone())
                    .or_insert_with(|| vec![false; STEPS])[0] = true;
            }
            project.tracks.push(track);
            self.selected_track = id;
        }
    }
    fn push_undo(&mut self, before: Project) {
        self.dirty_since_save = true;
        self.undo_stack.push(before);
        if self.undo_stack.len() > 60 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }
}

impl eframe::App for BeatCraftApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(40));
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Z)) {
            if let Some(previous) = self.undo_stack.pop() {
                if let Some(current) = self.project.replace(previous) {
                    self.redo_stack.push(current);
                }
                self.status = "Undo".into();
            }
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Y)) {
            if let Some(next) = self.redo_stack.pop() {
                if let Some(current) = self.project.replace(next) {
                    self.undo_stack.push(current);
                }
                self.status = "Redo".into();
            }
        }
        if self.current_studio.is_none() {
            self.launcher_ui(ctx);
            return;
        }
        let Some(mut project) = self.project.take() else {
            self.current_studio = None;
            self.launcher_ui(ctx);
            return;
        };
        let before = project.clone();
        let mut actions = UiActions::default();
        self.draw_header(ctx, &mut project, &mut actions);
        // Crash-recovery banner: a sidecar autosave survived an interrupted session.
        if let Some(path) = self.recovery_offer.clone() {
            egui::TopBottomPanel::top("recovery-banner").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        color("#E0B34E"),
                        "Unsaved work was recovered from an interrupted session.",
                    );
                    if ui.button("Open autosaved session").clicked() {
                        match crate::project::load(&path) {
                            Ok(loaded) => {
                                project = loaded;
                                self.status =
                                    "Recovered autosaved session — review and Save.".into();
                            }
                            Err(err) => self.status = format!("Recovery failed: {err}"),
                        }
                        self.recovery_offer = None;
                    }
                    if ui.button("Dismiss").clicked() {
                        self.recovery_offer = None;
                    }
                });
            });
        }
        self.draw_transport(ctx, &mut project, &mut actions);
        match self.tab {
            EditorTab::Sequencer => self.draw_sequencer(ctx, &mut project, &mut actions),
            EditorTab::PianoRoll => self.draw_piano_roll(ctx, &mut project, &mut actions),
            EditorTab::Mixer => self.draw_mixer(ctx, &mut project, &mut actions),
            EditorTab::Automation => self.draw_automation(ctx, &mut project, &mut actions),
        }
        let is_playing = self.engine.as_ref().is_some_and(AudioEngine::is_playing);
        let pulse_phase = ctx.input(|input| input.time) as f32 * 5.5;
        let playback_color =
            color("#B8E58A").gamma_multiply(0.58 + 0.42 * (0.5 + 0.5 * pulse_phase.sin()));
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(color("#101620"))
                    .inner_margin(egui::Margin::symmetric(12, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} · Rust native · local-first",
                            studio_spec(&project.studio_id).name
                        ))
                        .small()
                        .color(color("#9AA5B5")),
                    );
                    if is_playing {
                        ui.colored_label(playback_color, "PLAYING");
                    } else if self.paused {
                        ui.colored_label(color("#E7C779"), "PAUSED");
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(&self.status).small().color(color("#A8B2C0")));
                    });
                });
            });
        if actions.changed {
            let _ = project.validate();
        }
        self.project = Some(project);
        if actions.changed {
            self.push_undo(before);
        }
        self.perform_actions(actions);
        self.service_pending_render();
        self.service_midi_in();
    }
}

fn configure_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = color("#151B25");
    visuals.panel_fill = color("#0C1017");
    visuals.extreme_bg_color = color("#0A0E14");
    visuals.faint_bg_color = color("#151B25");
    visuals.override_text_color = Some(color("#E5E8E0"));
    visuals.selection.bg_fill = color("#374A3B");
    visuals.selection.stroke = Stroke::new(1.0_f32, color("#B8E58A"));
    visuals.widgets.noninteractive.bg_fill = color("#151B25");
    visuals.widgets.inactive.bg_fill = color("#1D2531");
    visuals.widgets.hovered.bg_fill = color("#273243");
    visuals.widgets.active.bg_fill = color("#B8E58A");
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, color("#E5E8E0"));
    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    ctx.set_style(style);
}
fn color(value: &str) -> Color32 {
    let s = value.trim_start_matches('#');
    if s.len() == 6 {
        if let Ok(v) = u32::from_str_radix(s, 16) {
            return Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        }
    }
    Color32::WHITE
}
fn studio_spec(id: &str) -> StudioSpec {
    *model::STUDIOS
        .iter()
        .find(|s| s.id == id)
        .unwrap_or(&model::STUDIOS[0])
}
fn panel_frame(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(color("#151B25"))
        .stroke(Stroke::new(1.0_f32, color("#2A3444")))
        .corner_radius(14.0)
        .inner_margin(12.0)
        .show(ui, add);
}
fn safe_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    s.trim_matches('-')
        .chars()
        .take(48)
        .collect::<String>()
        .to_lowercase()
}
fn track_name(project: &Project, id: &str) -> String {
    project
        .tracks
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| "Select track".into())
}
fn track_color(project: &Project, id: &str) -> String {
    project
        .tracks
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.color.clone())
        .unwrap_or_else(|| "#B8E58A".into())
}
fn note_name(pitch: u8) -> String {
    let names = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        names[(pitch % 12) as usize],
        i16::from(pitch) / 12 - 1
    )
}
fn find_note_mut<'a>(
    project: &'a mut Project,
    track: &str,
    step: u16,
    pitch: u8,
) -> Option<&'a mut Note> {
    project
        .selected_pattern_mut()?
        .notes
        .get_mut(track)?
        .iter_mut()
        .find(|n| n.step == step && n.pitch == pitch)
}
fn sync_pitched_row(pattern: &mut model::Pattern, track_id: &str) {
    if let Some(row) = pattern.rows.get_mut(track_id) {
        row.fill(false);
        if let Some(notes) = pattern.notes.get(track_id) {
            for note in notes {
                if let Some(v) = row.get_mut(note.step as usize) {
                    *v = true;
                }
            }
        }
    }
}
fn toggle_step(project: &mut Project, track_id: &str, step: usize) {
    let Some(pattern) = project.selected_pattern_mut() else {
        return;
    };
    let row = pattern
        .rows
        .entry(track_id.into())
        .or_insert_with(|| vec![false; STEPS]);
    if step >= row.len() {
        return;
    }
    let active = row[step];
    row[step] = !active;
    if matches!(track_id, "sub" | "bass" | "keys") {
        let events = pattern.notes.entry(track_id.into()).or_default();
        if active {
            events.retain(|n| n.step as usize != step);
        } else {
            const FUNK: [i8; 16] = [0, 7, 0, 3, 0, 5, 7, 3, 0, 7, 10, 7, 0, 3, 5, 7];
            const PHONK: [i8; 16] = [0, 0, 3, 0, 7, 3, 0, -2, 0, 3, 7, 3, 0, -2, 3, 0];
            let degree = if pattern.genre == "FUNK" {
                FUNK[step]
            } else {
                PHONK[step]
            };
            let root = match (track_id, pattern.genre.as_str()) {
                ("sub", "FUNK") => 33,
                ("sub", "HYBRID") => 41,
                ("sub", _) => 38,
                ("bass", "FUNK") => 45,
                ("bass", "HYBRID") => 48,
                ("bass", _) => 38,
                ("keys", "FUNK") => 57,
                ("keys", "HYBRID") => 53,
                _ => 50,
            };
            let intervals: &[u8] = if track_id == "keys" { &[0, 3, 7] } else { &[0] };
            for interval in intervals {
                events.push(Note {
                    step: step as u16,
                    pitch: (root + degree as i16).clamp(0, 127) as u8 + *interval,
                    duration: if track_id == "keys" { 3 } else { 2 },
                    velocity: 96,
                });
            }
        }
    }
}
fn midi_note_name(pitch: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let note = NAMES[(pitch % 12) as usize];
    format!("{note}{}", pitch as i32 / 12 - 1)
}

fn draw_step_grid(
    ui: &mut egui::Ui,
    project: &mut Project,
    actions: &mut UiActions,
    playhead: Option<usize>,
) {
    let tracks = project.tracks.clone();
    egui::ScrollArea::both()
        .id_salt("sequencer-grid")
        .auto_shrink([false, false])
        .max_height((ui.available_height() - 290.0).clamp(220.0, 340.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    Vec2::new(235.0, 22.0),
                    egui::Label::new(
                        RichText::new("TRACK / MIX")
                            .small()
                            .strong()
                            .color(color("#9AA5B5")),
                    ),
                );
                for step in 0..STEPS {
                    ui.add_sized(
                        Vec2::new(25.0, 22.0),
                        egui::Label::new(
                            RichText::new(if step % 4 == 0 {
                                format!("{}", step / 4 + 1)
                            } else {
                                "·".into()
                            })
                            .small()
                            .color(color("#95A0AF")),
                        )
                        .halign(Align::Center),
                    );
                }
            });
            for track in tracks {
                let active = project
                    .selected_pattern()
                    .and_then(|p| p.rows.get(&track.id))
                    .cloned()
                    .unwrap_or_else(|| vec![false; STEPS]);
                ui.horizontal(|ui| {
                    ui.add_sized(
                        Vec2::new(78.0, 28.0),
                        egui::Label::new(
                            RichText::new(&track.name)
                                .strong()
                                .color(color(&track.color)),
                        ),
                    );
                    let mut mute = track.mute;
                    if ui
                        .toggle_value(&mut mute, "M")
                        .on_hover_text("Mute track")
                        .clicked()
                    {
                        if let Some(t) = project.tracks.iter_mut().find(|t| t.id == track.id) {
                            t.mute = mute;
                        }
                        actions.changed = true;
                    }
                    let mut solo = track.solo;
                    if ui
                        .toggle_value(&mut solo, "S")
                        .on_hover_text("Solo track")
                        .clicked()
                    {
                        if let Some(t) = project.tracks.iter_mut().find(|t| t.id == track.id) {
                            t.solo = solo;
                        }
                        actions.changed = true;
                    }
                    let mut volume = track.volume;
                    if ui
                        .add_sized(
                            Vec2::new(70.0, 24.0),
                            egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                        )
                        .changed()
                    {
                        if let Some(t) = project.tracks.iter_mut().find(|t| t.id == track.id) {
                            t.volume = volume;
                        }
                        actions.changed = true;
                    }
                    for step in 0..STEPS {
                        let fill = if active.get(step).copied().unwrap_or(false) {
                            color(&track.color)
                        } else {
                            color("#252E3B")
                        };
                        let stroke = if playhead == Some(step) {
                            Stroke::new(2.2_f32, color("#F4F1DC"))
                        } else {
                            Stroke::new(
                                if step % 4 == 0 { 1.3_f32 } else { 0.7_f32 },
                                if step % 4 == 0 {
                                    color("#647286")
                                } else {
                                    color("#394455")
                                },
                            )
                        };
                        let button = egui::Button::new("").fill(fill).stroke(stroke);
                        if ui
                            .add_sized(Vec2::new(25.0, 25.0), button)
                            .on_hover_text(format!("{} · step {}", track.name, step + 1))
                            .clicked()
                        {
                            toggle_step(project, &track.id, step);
                            actions.changed = true;
                        }
                    }
                });
                ui.add_space(3.0);
            }
        });
}
fn draw_arrangement(ui: &mut egui::Ui, project: &mut Project, actions: &mut UiActions) {
    panel_frame(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("SONG ARRANGEMENT")
                        .small()
                        .strong()
                        .color(color("#9AA5B5")),
                );
                ui.label(
                    RichText::new("Choose a pattern for each bar")
                        .small()
                        .color(color("#A1AABB")),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    project.arrangement.fill(None);
                    actions.changed = true;
                }
                if ui.button("Fill selected").clicked() {
                    project
                        .arrangement
                        .fill(Some(project.selected_pattern_id.clone()));
                    actions.changed = true;
                }
                egui::ComboBox::from_id_salt("bar-count")
                    .selected_text(format!("{} bars", project.arrangement.len()))
                    .show_ui(ui, |ui| {
                        for count in [4, 8, 16, 32] {
                            if ui
                                .selectable_label(
                                    project.arrangement.len() == count,
                                    format!("{count} bars"),
                                )
                                .clicked()
                            {
                                project
                                    .arrangement
                                    .resize(count, Some(project.selected_pattern_id.clone()));
                                actions.changed = true;
                            }
                        }
                    });
            });
        });
        ui.add_space(6.0);
        let patterns: Vec<(String, String)> = project
            .patterns
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        egui::ScrollArea::horizontal()
            .id_salt("arrangement")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (index, slot) in project.arrangement.iter_mut().enumerate() {
                        let selected = slot.clone().unwrap_or_default();
                        let current = patterns
                            .iter()
                            .find(|p| p.0 == selected)
                            .map(|p| p.1.as_str())
                            .unwrap_or("REST");
                        egui::ComboBox::from_id_salt(("arrangement-bar", index))
                            .selected_text(format!("{:02} · {}", index + 1, current))
                            .width(100.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(slot.is_none(), "REST").clicked() {
                                    *slot = None;
                                    actions.changed = true;
                                }
                                for (id, name) in &patterns {
                                    if ui
                                        .selectable_label(
                                            slot.as_deref() == Some(id.as_str()),
                                            name,
                                        )
                                        .clicked()
                                    {
                                        *slot = Some(id.clone());
                                        actions.changed = true;
                                    }
                                }
                            });
                    }
                })
            });
    });
}

pub fn run(studio_id: Option<&str>) -> eframe::Result<()> {
    let selected = studio_id.map(str::to_owned);
    let window_title = selected
        .as_deref()
        .map(|id| {
            let spec = studio_spec(id);
            format!("BeatCraft {} — {}", spec.name, spec.title)
        })
        .unwrap_or_else(|| "BeatCraft Studio".into());
    let icon = image::load_from_memory(include_bytes!("../assets/beatcraft-studio.png"))
        .ok()
        .map(|image| {
            let rgba = image.to_rgba8();
            let width = rgba.width();
            let height = rgba.height();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        })
        .map(std::sync::Arc::new);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1360.0, 900.0])
        .with_min_inner_size([960.0, 640.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        &window_title,
        options,
        Box::new(move |cc| Ok(Box::new(BeatCraftApp::new(cc, selected.clone())))),
    )
}
