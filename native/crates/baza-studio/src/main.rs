#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;
mod connection_edit;
mod cstruct;
mod editor_text;
mod messages;
mod navigator;
mod renderer_qa;
mod self_test;
mod theme;
mod transfer;
mod widgets;
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let report = args
        .windows(2)
        .find(|w| w[0] == "--report")
        .map(|w| std::path::PathBuf::from(&w[1]));
    if args.iter().any(|a| a == "--self-test") {
        return self_test::run(report.as_deref());
    }
    let qa_auto = args.iter().any(|a| a == "--native-ui-self-test");
    let qa = qa_auto || args.iter().any(|a| a == "--qa-interactive");
    let qa_report = if qa_auto {
        Some(
            report
                .clone()
                .unwrap_or_else(|| "native-ui-report.json".into()),
        )
    } else {
        None
    };
    let qa_run = if qa { Some(uuid::Uuid::new_v4()) } else { None };
    let render_report = qa_report.clone();
    // QA only: what the self-test shows, and the size of its window.
    let mut qa_options = app::QaOptions::from_args(&args, qa);
    let size = qa_size(&args, qa).unwrap_or([1440.0, 900.0]);
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size(MIN_SIZE)
        .with_title("Baza Studio")
        .with_app_id(APP_ID)
        .with_icon(app_icon());
    if qa_options.scale.is_some() {
        // QA only (`--qa-scale`): the application keeps this size in points at
        // the chosen pixel density. A window with a title bar is shrunk to the
        // screen by the system; one without may be larger than the screen.
        qa_options.size = Some(size);
        viewport = viewport.with_decorations(false).with_position([0.0, 0.0]);
    }
    // QA only: held until the window is gone.
    let mut qa_window_slot = None;
    if qa_auto {
        // QA only: the self-test window opens above the other windows, where
        // the system paints it, without taking the keyboard from the
        // application the person at the computer is using. `--qa-foreground`
        // opens it like a normal launch instead.
        if !qa_options.foreground {
            viewport = viewport.with_active(false).with_always_on_top();
        }
        // Several self-tests may run at once. Each window is placed further
        // down and to the right than the ones already open, so none of them
        // is covered completely by another: see `QaWindowSlot`.
        qa_window_slot = QaWindowSlot::take();
        if let Some(slot) = &qa_window_slot {
            let base = if qa_options.scale.is_some() {
                0.0
            } else {
                48.0
            };
            let offset = base + slot.index as f32 * QaWindowSlot::STEP;
            viewport = viewport.with_position([offset, offset]);
        }
    }
    // QA only: a script may run longer than a plain self-test.
    let qa_time_limit = RENDER_TIME_LIMIT + qa_options.script_budget();
    if let Some(report) = &qa_report {
        // QA only: a window that is never painted would wait forever.
        app::qa_watchdog(report.clone(), &qa_options);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Baza Studio",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::StudioApp::new(
                cc,
                qa,
                qa_auto,
                render_report,
                qa_run,
                qa_options,
            )))
        }),
    )
    .map_err(|_| anyhow::anyhow!("native_window_start_failed"))?;
    app::qa_finished();
    drop(qa_window_slot);
    if let Some(path) = qa_report {
        renderer_qa::validate(
            &path,
            qa_run.ok_or_else(|| anyhow::anyhow!("native_qa_run_missing"))?,
            qa_time_limit,
        )?;
    }
    Ok(())
}

/// QA only: the place of a self-test window among the self-test windows that
/// are open at the same time, possibly started by other processes.
///
/// The system does not paint a window that other windows cover completely,
/// and a self-test that is not painted never gets its screenshot and ends in
/// a timeout. Windows only reach down and to the right of their corner, and a
/// new window opens above the older ones. So every new window takes a slot
/// after the highest one in use and is placed that many steps down and to the
/// right: the older windows keep a strip along their top and left edge in
/// view, whatever the sizes are. A slot is an exclusively locked file in the
/// temporary directory, so it is free again as soon as its process ends,
/// however it ends.
struct QaWindowSlot {
    index: u32,
    _lock: std::fs::File,
}

impl QaWindowSlot {
    /// Distance between the corners of neighbouring slots, in points.
    const STEP: f32 = 24.0;
    /// After this many windows at once the slots start again from the first.
    const COUNT: u32 = 16;

    fn take() -> Option<Self> {
        let directory = std::env::temp_dir().join("BazaStudio-qa-window-slots");
        std::fs::create_dir_all(&directory).ok()?;
        let open = |index: u32| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(directory.join(format!("{index}.lock")))
                .ok()
        };
        // Another self-test may start at the same moment and take the slot
        // seen as free; look again then.
        for _ in 0..8 {
            let mut highest = None;
            for index in 0..Self::COUNT {
                // A slot whose lock is refused belongs to a running window.
                if open(index)?.try_lock().is_err() {
                    highest = Some(index);
                }
            }
            let index = highest.map_or(0, |highest| (highest + 1) % Self::COUNT);
            let lock = open(index)?;
            if lock.try_lock().is_ok() {
                return Some(Self { index, _lock: lock });
            }
        }
        None
    }
}

/// Longest time a renderer self-test without a script may take to pass.
const RENDER_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(40);

/// Smallest inner size of the main window.
const MIN_SIZE: [f32; 2] = [960.0, 640.0];

/// QA only: `--qa-size <width>x<height>` opens the self-test window in another
/// size, never smaller than the minimum. Ignored outside the QA modes.
fn qa_size(args: &[String], qa: bool) -> Option<[f32; 2]> {
    let value = args.windows(2).find(|pair| pair[0] == "--qa-size")?;
    let (width, height) = value[1].split_once('x')?;
    let size = [width.parse::<f32>().ok()?, height.parse::<f32>().ok()?];
    (qa && size[0] <= 8192.0 && size[1] <= 8192.0)
        .then(|| [size[0].max(MIN_SIZE[0]), size[1].max(MIN_SIZE[1])])
}

/// Application identity outside the macOS bundle; `scripts/package-macos.sh` writes the same
/// value as `CFBundleIdentifier`, and Wayland matches the window with its desktop entry by it.
const APP_ID: &str = "pl.bazastudio.bazastudio";

/// Dock artwork on macOS, with the transparent margin of the system icon grid.
#[cfg(target_os = "macos")]
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/brand/baza-studio-dock-512.png");
/// Title bar and taskbar artwork elsewhere: the small-size drawing, because the toolkit
/// scales one image down to every size the window manager asks for.
#[cfg(not(target_os = "macos"))]
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/brand/baza-studio-window-256.png");

/// Window, taskbar and Dock icon.
///
/// A packaged macOS application keeps the icon of its bundle, which the system renders in
/// the current appearance; an empty icon tells eframe to leave it alone. Every other run,
/// including an unbundled development build, gets the embedded artwork.
fn app_icon() -> eframe::egui::IconData {
    let bundled = cfg!(target_os = "macos")
        && std::env::current_exe().is_ok_and(|executable| {
            executable
                .parent()
                .and_then(std::path::Path::parent)
                .is_some_and(|contents| contents.join("Info.plist").is_file())
        });
    if bundled {
        return eframe::egui::IconData::default();
    }
    eframe::icon_data::from_png_bytes(APP_ICON_PNG).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_app_icon_is_a_square_rgba_image() {
        let icon = eframe::icon_data::from_png_bytes(super::APP_ICON_PNG).unwrap();
        assert_eq!(icon.width, icon.height);
        assert!(icon.width >= 256);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }

    #[test]
    fn qa_size_needs_a_qa_mode_and_respects_the_minimum() {
        let args = ["BazaStudio", "--qa-size", "1000x700"].map(String::from);
        assert_eq!(super::qa_size(&args, true), Some([1000.0, 700.0]));
        assert_eq!(super::qa_size(&args, false), None);
        let small = ["BazaStudio", "--qa-size", "300x200"].map(String::from);
        assert_eq!(super::qa_size(&small, true), Some(super::MIN_SIZE));
        let broken = ["BazaStudio", "--qa-size", "wide"].map(String::from);
        assert_eq!(super::qa_size(&broken, true), None);
    }

    #[test]
    fn every_self_test_window_gets_a_slot_after_the_ones_in_use() {
        // Other self-tests may be running on this machine, so only the order
        // of the slots taken here is known.
        let first = super::QaWindowSlot::take().unwrap();
        let second = super::QaWindowSlot::take().unwrap();
        let third = super::QaWindowSlot::take().unwrap();
        let after = |earlier: u32, later: u32| {
            later > earlier || later < 3 // wrapped around after the last slot
        };
        assert!(after(first.index, second.index));
        assert!(after(second.index, third.index));
        assert_ne!(first.index, third.index);
        // A slot is free again when its window is gone, and the next window
        // still goes after the highest one in use.
        let highest = third.index;
        drop(second);
        let next = super::QaWindowSlot::take().unwrap();
        assert!(after(highest, next.index));
        assert_ne!(next.index, first.index);
        assert_ne!(next.index, third.index);
    }

    #[test]
    fn unbundled_run_gets_the_embedded_app_icon() {
        assert!(!super::app_icon().is_empty());
    }
}
