//! FilmCraft on Android.
//!
//! Runs the same [`filmcraft_ui_egui::FilmcraftApp`] as the desktop app inside a
//! `GameActivity` (android-activity's `game-activity` backend, which eframe needs for the soft
//! keyboard and accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then
//! packaged by the Gradle project in `android/`.
//!
//! Differences from the desktop app:
//! - no TCP control server, no native menus, no microphone (voice-over) input;
//! - no audio output yet (cpal has no Android backend here): playback runs on the wall clock;
//! - File › Import / Open ask `MainActivity.pickOpen()` (Storage Access Framework); the activity
//!   copies each picked file into the app's `files/Media/` folder and hands the path back on a
//!   Java thread through `nativeDeliverFile` into an inbox the app drains on its next frame,
//!   like the web build's asynchronous pickers;
//! - Save writes `files/Projects/<name>.fcproj`, exports render into `files/Exports/<name>`,
//!   and every finished file is published to `Downloads/FilmCraft/<name>` through
//!   `MainActivity.exportFile` (MediaStore, no dialog);
//! - preferences, auto-save, crash logs and the panel layout live in the app's private files
//!   directory.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use android_activity::AndroidApp;
use eframe::App as _;
use filmcraft_engine::autosave::AutosaveConfig;
use filmcraft_engine::{FsServices, Services, Session};
use filmcraft_ui_egui::{FilmcraftApp, RelinkHint};
use jni::objects::{JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};
use serde_json::{Value, json};

const LOG_TAG: &str = "filmcraft";

/// Files the Kotlin side delivers (display name, path of the copy); the app opens them on its
/// next frame.
static INBOX: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// The relink command waiting for the next picked file (Link Media ▸ Locate…, Attach Proxies…).
static RELINK: Mutex<Option<RelinkHint>> = Mutex::new(None);
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);

/// Where the app keeps its files (`<files dir>/…`).
#[derive(Clone)]
struct Dirs {
    /// Preferences, auto-save ring, crash logs.
    data: PathBuf,
    /// Copies of picked media (the Media Browser starts here).
    media: PathBuf,
    /// Saved projects (published to Downloads on every save).
    projects: PathBuf,
    /// Rendered exports (published to Downloads when the job finishes).
    exports: PathBuf,
}

impl Dirs {
    fn new(files: &Path) -> Self {
        let d = Self { data: files.join("FilmCraft"), media: files.join("Media"), projects: files.join("Projects"), exports: files.join("Exports") };
        for p in [&d.data, &d.media, &d.projects, &d.exports] {
            if let Err(e) = std::fs::create_dir_all(p) {
                log::warn!("creating {}: {e}", p.display());
            }
        }
        d
    }

    /// Whether a written file is one the user expects to find in Downloads.
    fn is_published(&self, path: &str) -> bool {
        let p = Path::new(path);
        p.starts_with(&self.projects) || p.starts_with(&self.exports)
    }
}

/// The activity's entry point, called by android-activity's GameActivity glue on its own thread.
/// It returns when the activity is destroyed.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    static LOGGER: OnceLock<()> = OnceLock::new();
    LOGGER.get_or_init(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag(LOG_TAG));
    });
    // SAFETY: `vm_as_ptr` is the process's JavaVM, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let _ = VM.set(vm);
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = app.activity_as_ptr() as usize;

    let files = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    let dirs = Dirs::new(&files);
    log::info!("FilmCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), dirs.data.display());
    // Panics anywhere go to <data dir>/Logs/crash-<day>.log with a backtrace; the UI pass and
    // frame workers catch them and keep running (see filmcraft_ui_egui::crash).
    filmcraft_ui_egui::crash::install(Some(dirs.data.join("Logs")));
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui panel/window sizes here on exit.
        persistence_path: Some(dirs.data.join("ui.ron")),
        ..Default::default()
    };
    let result = eframe::run_native(
        "FilmCraft",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            let app = build_app(cc, dirs);
            Ok(Box::new(app))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = result {
        log::error!("FilmCraft stopped: {e}");
        filmcraft_ui_egui::crash::record(&format!("FilmCraft could not start its window: {e}"));
        // winit allows one event loop per process: when Android recreates the activity in the
        // same process, end the process so the next launch starts clean instead of a blank window.
        std::process::exit(0);
    }
}

/// The session and app as the desktop shell builds them, with Android hooks.
fn build_app(cc: &eframe::CreationContext<'_>, dirs: Dirs) -> AndroidShell {
    let mut session = Session::new(Arc::new(AndroidServices { dirs: dirs.clone() }));
    if let Err(e) = session.start_autosave(AutosaveConfig::new(dirs.data.clone())) {
        log::warn!("auto-save and crash recovery unavailable: {e}");
    }
    // Settings ▸ General ▸ At Startup: Show Home (the demo project), Open Most Recent, or empty.
    match session.prefs.general.at_startup.as_str() {
        "emptyProject" => {}
        "openMostRecent" => {
            let recent = session.prefs.general.recent_projects.iter().find(|p| Path::new(p).exists()).cloned();
            let r = match recent {
                Some(p) => session.execute("file.open", json!({"path": p})),
                None => session.execute("file.openDemoProject", json!({})),
            };
            if let Err(e) = r {
                log::warn!("startup project: {e}");
            }
        }
        _ => {
            let _ = session.execute("file.openDemoProject", json!({}));
        }
    }
    let mut app = FilmcraftApp::new(session);
    if let Some(rs) = cc.wgpu_render_state.clone() {
        let info = rs.adapter.get_info();
        log::info!("wgpu backend {:?}, adapter {}", info.backend, info.name);
        app.set_wgpu(rs);
    }
    // Pickers are asynchronous: the hook starts the system picker and returns nothing; the files
    // are imported (or the relink command runs) when the user has chosen them.
    app.hooks.pick_files = Some(Box::new(|_exts: &[&str]| {
        pick_open(true);
        Vec::new()
    }));
    app.hooks.pick_open_project = Some(Box::new(|| {
        pick_open(false);
        None
    }));
    app.hooks.pick_file_for_relink = Some(Box::new(|_exts: &[&str], hint: Option<RelinkHint>| {
        if hint.is_some() {
            *RELINK.lock().unwrap_or_else(PoisonError::into_inner) = hint;
            pick_open(false);
        }
        None
    }));
    // No save dialog: the suggested name becomes the file name in Downloads/FilmCraft.
    let projects = dirs.projects.clone();
    app.hooks.pick_save = Some(Box::new(move |name: &str| Some(projects.join(file_name(name)).to_string_lossy().into_owned())));
    let exports = dirs.exports.clone();
    app.hooks.pick_save_as = Some(Box::new(move |_filter: &str, _exts: &[&str], name: &str| Some(exports.join(file_name(name)).to_string_lossy().into_owned())));
    app.hooks.open_path = Some(Box::new(|path: &str, _reveal: bool| Err(format!("{}: opening files in other apps is not available on Android", file_name(path)))));
    AndroidShell { app, dirs, published_jobs: HashSet::new() }
}

/// The eframe app: the shared FilmCraft UI plus the Android host's per-frame duties.
struct AndroidShell {
    app: FilmcraftApp,
    dirs: Dirs,
    /// Export jobs whose output has been published to Downloads.
    published_jobs: HashSet<u64>,
}

impl AndroidShell {
    /// Files the activity delivered since the last frame: a pending relink command takes the
    /// first one; `.fcproj` files open; everything else is imported in one go.
    fn drain_inbox(&mut self) {
        let files = std::mem::take(&mut *INBOX.lock().unwrap_or_else(PoisonError::into_inner));
        if files.is_empty() {
            return;
        }
        let mut files = files.into_iter();
        if let Some(hint) = RELINK.lock().unwrap_or_else(PoisonError::into_inner).take()
            && let Some((_, path)) = files.next()
        {
            let mut params = hint.params;
            if !params.is_object() {
                params = json!({});
            }
            params["path"] = json!(path);
            match self.app.session.execute(&hint.command, params) {
                Ok(v) => {
                    if let Some(n) = v.get("relinked").and_then(Value::as_array).map(Vec::len) {
                        self.app.status(format!("Linked {n} clip(s)"));
                    }
                }
                Err(e) => self.app.status(e.to_string()),
            }
        }
        let mut media = Vec::new();
        for (name, path) in files {
            if name.to_ascii_lowercase().ends_with(".fcproj") {
                if let Err(e) = self.app.session.execute("file.open", json!({"path": path})) {
                    self.app.status(e.to_string());
                }
            } else {
                media.push(path);
            }
        }
        if !media.is_empty() {
            match self.app.session.execute("file.import", json!({"paths": media})) {
                Ok(v) => {
                    let errors: Vec<&str> = v.get("errors").and_then(Value::as_array).map(|e| e.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    if !errors.is_empty() {
                        self.app.status(errors.join("; "));
                    }
                }
                Err(e) => self.app.status(e.to_string()),
            }
        }
    }

    /// Exports stream to `files/Exports/…` on a worker thread; once a job reports success its
    /// files go to Downloads/FilmCraft.
    fn publish_finished_exports(&mut self) {
        let mut outputs = Vec::new();
        for job in &self.app.session.jobs {
            if self.published_jobs.contains(&job.id) {
                continue;
            }
            let result = job.result.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(r) = result.as_ref() else { continue };
            self.published_jobs.insert(job.id);
            if let Ok(report) = r {
                outputs.push(report.path.clone());
                outputs.extend(report.extra_files.iter().cloned());
            }
        }
        for path in outputs {
            if !self.dirs.is_published(&path) || !Path::new(&path).is_file() {
                continue;
            }
            match export_file(&file_name(&path), &path) {
                Ok(()) => self.app.status(format!("Saved to Downloads/FilmCraft/{}", file_name(&path))),
                Err(e) => self.app.status(format!("Couldn't copy {} to Downloads: {e}", file_name(&path))),
            }
        }
    }
}

impl eframe::App for AndroidShell {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw_input);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain_inbox();
        self.app.logic(ctx, frame);
        self.publish_finished_exports();
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }

    fn on_exit(&mut self) {
        self.app.on_exit();
    }
}

/// The native file system, plus: files saved under `Projects/` or `Exports/` are also published
/// to Downloads, and the Media Browser starts in the folder of picked media.
struct AndroidServices {
    dirs: Dirs,
}

impl Services for AndroidServices {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        FsServices.read_file(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        FsServices.write_file(path, data)?;
        if self.dirs.is_published(path) {
            match export_file(&file_name(path), path) {
                Ok(()) => log::info!("published {path} to Downloads"),
                Err(e) => log::warn!("couldn't publish {path} to Downloads: {e}"),
            }
        }
        Ok(())
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        FsServices.file_size(path)
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        FsServices.read_range(path, offset, len)
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        FsServices.reader(path)
    }
    fn list_dir(&self, dir: &str) -> Option<std::io::Result<Vec<String>>> {
        FsServices.list_dir(dir)
    }
    fn file_loader(&self) -> Option<filmcraft_media::sequence::FrameLoader> {
        FsServices.file_loader()
    }
    fn list_entries(&self, dir: &str) -> Option<std::io::Result<Vec<filmcraft_engine::media_browser::DirEntry>>> {
        FsServices.list_entries(dir)
    }
    fn volumes(&self) -> Vec<filmcraft_engine::media_browser::Volume> {
        use filmcraft_engine::media_browser::{Volume, VolumeKind};
        [("Media", &self.dirs.media), ("Projects", &self.dirs.projects), ("Exports", &self.dirs.exports)]
            .into_iter()
            .map(|(name, path)| Volume { name: name.to_string(), path: path.to_string_lossy().into_owned(), kind: VolumeKind::Local })
            .collect()
    }
    fn home_dir(&self) -> Option<String> {
        Some(self.dirs.media.to_string_lossy().into_owned())
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

// ---- Calls into MainActivity (Kotlin) ----------------------------------------------------------

/// Run `f` with a JNI environment on this thread and the current activity.
fn with_activity<T>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Result<T, String> {
    let vm = VM.get().ok_or("the Java VM is not available")?;
    let raw = *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner);
    if raw == 0 {
        return Err("the activity is not running".to_string());
    }
    let raw = raw as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the reference comes from android-activity's `activity_as_ptr`, which keeps it
        // valid while the activity runs (ACTIVITY is cleared when `run_native` returns). `Cast`
        // neither owns nor deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// `MainActivity.pickOpen()` / `pickOpenMany()`: show the system file picker; the files come
/// through the inbox.
fn pick_open(multiple: bool) {
    let r = with_activity(|env, activity| {
        let method = if multiple { jni_str!("pickOpenMany") } else { jni_str!("pickOpen") };
        env.call_method(activity, method, jni_sig!("()V"), &[])?;
        Ok(())
    });
    if let Err(e) = r {
        log::error!("couldn't open the file picker: {e}");
    }
}

/// `MainActivity.exportFile(name, path)`: copy a file into `Downloads/FilmCraft/<name>`; `null`
/// on success, else the error message.
fn export_file(name: &str, path: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jname = JString::from_str(env, name)?;
        let jpath = JString::from_str(env, path)?;
        let ret = env
            .call_method(activity, jni_str!("exportFile"), jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"), &[(&jname).into(), (&jpath).into()])?
            .l()?;
        if ret.is_null() {
            return Ok(Ok(()));
        }
        let message = env.cast_local::<JString>(ret)?;
        Ok(Err(message.to_string()))
    })?
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// `MainActivity.nativeDeliverFile(name, path)`: a picked file, copied into the app's files, from
/// a Java thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_filmcraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    path: JString<'caller>,
) {
    let outcome = unowned_env.with_env(|_env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let path = path.to_string();
        log::info!("received {name} at {path}");
        INBOX.lock().unwrap_or_else(PoisonError::into_inner).push((name, path));
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
