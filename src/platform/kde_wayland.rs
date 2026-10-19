use std::{
    env,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{process::Command, sync::oneshot, task::JoinHandle, time::sleep};
use wdotool_core::{
    backend::detector::{self, BackendKind, Environment},
    DynBackend,
};

use crate::{
    layout::{GridAxis, GridSpec, LogicalRect},
    platform::{ButtonState, LayoutPass, Platform, PlatformWindow, PointerButton, WindowHandle},
};

const LAYOUT_SETTLE: Duration = Duration::from_millis(500);
type KWinTask = JoinHandle<Result<(), String>>;

fn ensure_supported_environment(environment: &Environment) -> Result<(), String> {
    if !environment.is_wayland() || !environment.desktop_is("KDE") {
        return Err("This version is for KDE Plasma on Wayland.".into());
    }
    Ok(())
}

/// Environment probe result kept private to the KDE backend boundary.
pub struct KdeEnvironment(Environment);

impl KdeEnvironment {
    pub fn detect() -> Result<Self, String> {
        let environment = Environment::detect();
        ensure_supported_environment(&environment)?;
        Ok(Self(environment))
    }
}

/// Owns the single KDE Remote Desktop input backend used for a complete run.
pub struct KdeWaylandPlatform {
    backend: DynBackend,
    cleanup: KdeCleanupQueue,
}

/// Tracks KWin script lifecycles that must finish before application shutdown.
#[derive(Clone, Default)]
pub struct KdeCleanupQueue {
    tasks: Arc<Mutex<Vec<KWinTask>>>,
}

impl KdeCleanupQueue {
    fn track(&self, task: JoinHandle<Result<(), String>>) {
        self.tasks.lock().unwrap().push(task);
    }

    pub async fn finish(&self) {
        loop {
            let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
            if tasks.is_empty() {
                break;
            }
            for task in tasks {
                let _ = task.await;
            }
        }
    }
}

impl KdeWaylandPlatform {
    pub async fn connect(
        environment: &KdeEnvironment,
        cleanup: KdeCleanupQueue,
    ) -> Result<Self, String> {
        let backend = detector::build(&environment.0, Some(BackendKind::KdeDBus))
            .await
            .map_err(|error| format!("could not start one KDE input session: {error}"))?;
        Ok(Self { backend, cleanup })
    }
}

impl Platform for KdeWaylandPlatform {
    async fn list_windows(&self) -> Result<Vec<PlatformWindow>, String> {
        self.backend
            .list_windows()
            .await
            .map(|windows| {
                windows
                    .into_iter()
                    .map(|window| PlatformWindow {
                        handle: WindowHandle::new(window.id.0),
                        app_id: window.app_id,
                        title: window.title,
                    })
                    .collect()
            })
            .map_err(|error| error.to_string())
    }

    async fn window_geometry(&self, window: &WindowHandle) -> Result<Option<LogicalRect>, String> {
        let id = wdotool_core::WindowId(window.as_str().to_owned());
        self.backend
            .window_geometry(&id)
            .await
            .map(|geometry| {
                geometry.map(|geometry| LogicalRect {
                    x: geometry.x,
                    y: geometry.y,
                    width: geometry.width,
                    height: geometry.height,
                })
            })
            .map_err(|error| format!("could not read Firefox window geometry: {error}"))
    }

    async fn activate_window(&self, window: &WindowHandle) -> Result<(), String> {
        let id = wdotool_core::WindowId(window.as_str().to_owned());
        self.backend
            .activate_window(&id)
            .await
            .map_err(|error| format!("could not focus Firefox window: {error}"))
    }

    async fn arrange_windows(
        &self,
        windows: &[WindowHandle],
        layout: GridSpec,
        pass: LayoutPass,
    ) -> Result<(), String> {
        apply_window_layout(windows, layout, pass, &self.cleanup).await
    }

    async fn move_pointer(&self, x: i32, y: i32) -> Result<(), String> {
        self.backend
            .mouse_move(x, y, true)
            .await
            .map_err(|error| format!("could not move the pointer: {error}"))
    }

    async fn scroll(&self, horizontal: f64, vertical: f64) -> Result<(), String> {
        self.backend
            .scroll(horizontal, vertical)
            .await
            .map_err(|error| format!("could not scroll Reels: {error}"))
    }

    async fn pointer_button(
        &self,
        button: PointerButton,
        state: ButtonState,
    ) -> Result<(), String> {
        let button = match button {
            PointerButton::Left => wdotool_core::MouseButton::Left,
        };
        let state = match state {
            ButtonState::Press => wdotool_core::KeyDirection::Press,
            ButtonState::Release => wdotool_core::KeyDirection::Release,
        };
        self.backend
            .mouse_button(button, state)
            .await
            .map_err(|error| format!("could not send pointer button: {error}"))
    }
}

async fn apply_window_layout(
    windows: &[WindowHandle],
    layout: GridSpec,
    pass: LayoutPass,
    cleanup: &KdeCleanupQueue,
) -> Result<(), String> {
    let plugin = format!("ig_reels_scroll_{}", std::process::id());
    let script_path = env::temp_dir().join(format!("{plugin}.js"));
    let script = layout_script(windows, layout, pass);
    if let Err(error) = std::fs::write(&script_path, script) {
        let _ = std::fs::remove_file(&script_path);
        return Err(format!("could not write temporary KWin script: {error}"));
    }
    let task = KWinLayoutTask::spawn(QdbusScriptCommands, plugin, script_path, cleanup.clone());
    task.wait().await
}

fn layout_script(windows: &[WindowHandle], grid: GridSpec, pass: LayoutPass) -> String {
    let target_ids = windows
        .iter()
        .map(|window| format!("{:?}", window.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let count = windows.len();
    let restore_only = pass == LayoutPass::Restore;
    let x_start = grid.js_partition(GridAxis::Horizontal, "column");
    let x_end = grid.js_partition(GridAxis::Horizontal, "(column + 1)");
    let y_start = grid.js_partition(GridAxis::Vertical, "row");
    let y_end = grid.js_partition(GridAxis::Vertical, "(row + 1)");
    format!(
        r#"
const count = {count};
const restoreOnly = {restore_only};
const columns = {columns};
const rows = {rows};
const targetIds = new Set([{target_ids}]);
const targets = workspace.windowList().filter(w =>
    targetIds.has((w.internalId || w.windowId || "").toString())
);
if (targets.length === count) {{
    const area = workspace.clientArea(KWin.MaximizeArea, targets[0]);
    targets.forEach((window, index) => {{
        if (restoreOnly) {{
            window.minimized = false;
            window.fullScreen = false;
            window.setMaximize(false, false);
            return;
        }}
        const column = index % columns;
        const row = Math.floor(index / columns);
        const x = area.x + {x_start};
        const y = area.y + {y_start};
        const width = {x_end} - {x_start};
        const height = {y_end} - {y_start};
        window.frameGeometry = {{ x, y, width, height }};
    }});
}}
"#,
        columns = grid.columns,
        rows = grid.rows,
    )
}

async fn run_command(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("{program} {} failed", args.join(" "))
        } else {
            format!("{program}: {stderr}")
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[allow(clippy::manual_async_fn)]
trait KWinScriptCommands: Send + Sync + 'static {
    fn load_script<'a>(
        &'a self,
        path: &'a std::path::Path,
        plugin: &'a str,
    ) -> impl Future<Output = Result<String, String>> + Send + 'a;
    fn run_script<'a>(
        &'a self,
        script_id: &'a str,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;
    fn unload_script<'a>(
        &'a self,
        plugin: &'a str,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;
}

struct QdbusScriptCommands;

#[allow(clippy::manual_async_fn)]
impl KWinScriptCommands for QdbusScriptCommands {
    fn load_script<'a>(
        &'a self,
        path: &'a std::path::Path,
        plugin: &'a str,
    ) -> impl Future<Output = Result<String, String>> + Send + 'a {
        async move {
            let path = path.to_str().ok_or("invalid temporary script path")?;
            run_command(
                "qdbus6",
                &[
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.loadScript",
                    path,
                    plugin,
                ],
            )
            .await
        }
    }

    fn run_script<'a>(
        &'a self,
        script_id: &'a str,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a {
        async move {
            let script_object = format!("/Scripting/Script{}", script_id.trim());
            run_command(
                "qdbus6",
                &["org.kde.KWin", &script_object, "org.kde.kwin.Script.run"],
            )
            .await
            .map(|_| ())
        }
    }

    fn unload_script<'a>(
        &'a self,
        plugin: &'a str,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a {
        async move {
            run_command(
                "qdbus6",
                &[
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.unloadScript",
                    plugin,
                ],
            )
            .await
            .map(|_| ())
        }
    }
}

async fn run_script_lifecycle<C: KWinScriptCommands>(
    commands: C,
    plugin: String,
    script_path: PathBuf,
    mut cancel: oneshot::Receiver<()>,
) -> Result<(), String> {
    if cancellation_requested(&mut cancel) {
        let _ = std::fs::remove_file(script_path);
        return Ok(());
    }

    let result = match commands.load_script(&script_path, &plugin).await {
        Ok(script_id) if !cancellation_requested(&mut cancel) => {
            commands.run_script(&script_id).await
        }
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    };

    if result.is_ok() && !cancellation_requested(&mut cancel) {
        tokio::select! {
            biased;
            _ = &mut cancel => {},
            _ = sleep(LAYOUT_SETTLE) => {},
        }
    }

    // Drain KWin's load/run request before unloading, then remove the script.
    // This worker remains alive when its caller is canceled.
    let _ = commands.unload_script(&plugin).await;
    let _ = std::fs::remove_file(script_path);
    result
}

fn cancellation_requested(cancel: &mut oneshot::Receiver<()>) -> bool {
    match cancel.try_recv() {
        Ok(()) | Err(oneshot::error::TryRecvError::Closed) => true,
        Err(oneshot::error::TryRecvError::Empty) => false,
    }
}

struct KWinLayoutTask {
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), String>>>,
    cleanup: KdeCleanupQueue,
}

impl KWinLayoutTask {
    fn spawn<C: KWinScriptCommands>(
        commands: C,
        plugin: String,
        script_path: PathBuf,
        cleanup: KdeCleanupQueue,
    ) -> Self {
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(run_script_lifecycle(
            commands,
            plugin,
            script_path,
            cancelled,
        ));
        Self {
            cancel: Some(cancel),
            task: Some(task),
            cleanup,
        }
    }

    async fn wait(mut self) -> Result<(), String> {
        let result = self
            .task
            .as_mut()
            .expect("layout task exists until it completes")
            .await
            .map_err(|error| format!("KWin layout task failed: {error}"))?;
        self.task.take();
        self.cancel.take();
        result
    }
}

impl Drop for KWinLayoutTask {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            self.cleanup.track(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    struct DelayedLoadCommands {
        events: Arc<Mutex<Vec<&'static str>>>,
        load_started: Mutex<Option<oneshot::Sender<()>>>,
        release_load: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl DelayedLoadCommands {
        fn new(load_started: oneshot::Sender<()>, release_load: oneshot::Receiver<()>) -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
                load_started: Mutex::new(Some(load_started)),
                release_load: tokio::sync::Mutex::new(Some(release_load)),
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl KWinScriptCommands for DelayedLoadCommands {
        fn load_script<'a>(
            &'a self,
            _path: &std::path::Path,
            _plugin: &str,
        ) -> impl Future<Output = Result<String, String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("load-start");
                if let Some(started) = self.load_started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                let release = self.release_load.lock().await.take().unwrap();
                let _ = release.await;
                self.events.lock().unwrap().push("load-finished");
                Ok("42".into())
            }
        }

        fn run_script<'a>(
            &'a self,
            _script_id: &str,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("run");
                Ok(())
            }
        }

        fn unload_script<'a>(
            &'a self,
            _plugin: &str,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("unload");
                Ok(())
            }
        }
    }

    fn test_script_path() -> PathBuf {
        let sequence = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "ig_reels_scroll_cancellation_{}_{}.js",
            std::process::id(),
            sequence
        ))
    }

    #[test]
    fn kwin_script_targets_only_selected_ids_and_uses_common_grid() {
        let windows = [WindowHandle::new("0x1"), WindowHandle::new("0x2")];
        let grid = GridSpec::for_count(windows.len()).unwrap();
        let script = layout_script(&windows, grid, LayoutPass::Place);
        assert!(script.contains("const columns = 2;"));
        assert!(script.contains("const rows = 1;"));
        assert!(script.contains("new Set([\"0x1\", \"0x2\"])"));
        assert!(script.contains("workspace.clientArea(KWin.MaximizeArea, targets[0])"));
        assert!(script.contains("Math.floor(column * area.width / 2)"));
        assert!(script.contains("Math.floor((column + 1) * area.width / 2)"));
        assert!(script.contains("const restoreOnly = false;"));
        let restore = layout_script(&windows, grid, LayoutPass::Restore);
        assert!(restore.contains("const restoreOnly = true;"));
        assert!(restore.contains("window.setMaximize(false, false)"));
    }

    #[tokio::test]
    async fn canceled_layout_drains_load_before_unload_and_file_removal() {
        let script_path = test_script_path();
        std::fs::write(&script_path, "test script").unwrap();
        let (load_started_tx, load_started_rx) = oneshot::channel();
        let (release_load_tx, release_load_rx) = oneshot::channel();
        let commands = DelayedLoadCommands::new(load_started_tx, release_load_rx);
        let events = Arc::clone(&commands.events);
        let cleanup = KdeCleanupQueue::default();
        let task = KWinLayoutTask::spawn(
            commands,
            "test_plugin".into(),
            script_path.clone(),
            cleanup.clone(),
        );

        load_started_rx.await.unwrap();
        drop(task);

        let mut finish = tokio::spawn(async move { cleanup.finish().await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut finish)
            .await
            .is_err());
        release_load_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), &mut finish)
            .await
            .expect("cleanup must drain the in-flight KWin load")
            .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            ["load-start", "load-finished", "unload"]
        );
        assert!(!script_path.exists(), "script file survives until unload");
    }
}
