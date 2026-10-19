use std::{
    env,
    future::Future,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::oneshot,
    task::JoinHandle,
    time::{sleep, timeout},
};
use wdotool_core::{
    backend::detector::{self, BackendKind, Environment},
    DynBackend,
};

use crate::{
    layout::{GridSpec, LogicalRect},
    platform::{ButtonState, LayoutPass, Platform, PlatformWindow, PointerButton, WindowHandle},
};

const LAYOUT_SETTLE: Duration = Duration::from_millis(500);
const QDBUS_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_TERMINATION_TIMEOUT: Duration = Duration::from_secs(1);
const SCRIPT_OPERATION_TIMEOUT: Duration = Duration::from_secs(7);
const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const ABORTED_TASK_WAIT: Duration = Duration::from_secs(1);
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

    pub async fn finish(&self) -> Result<(), String> {
        self.finish_with_timeout(CLEANUP_TIMEOUT).await
    }

    async fn finish_with_timeout(&self, limit: Duration) -> Result<(), String> {
        let mut tracked = std::mem::take(&mut *self.tasks.lock().unwrap());
        let mut next = 0;
        let mut errors = Vec::new();
        let drain = async {
            loop {
                while next < tracked.len() {
                    match (&mut tracked[next]).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => errors.push(error),
                        Err(error) => errors.push(format!("KWin cleanup task failed: {error}")),
                    }
                    next += 1;
                }

                let queued = std::mem::take(&mut *self.tasks.lock().unwrap());
                if queued.is_empty() {
                    break;
                }
                tracked.extend(queued);
            }
        };

        if timeout(limit, drain).await.is_err() {
            tracked.extend(std::mem::take(&mut *self.tasks.lock().unwrap()));
            let mut pending = Vec::new();
            for (index, task) in tracked.iter_mut().enumerate().skip(next) {
                if task.is_finished() {
                    match (&mut *task).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => errors.push(error),
                        Err(error) if error.is_cancelled() => {}
                        Err(error) => errors.push(format!("KWin cleanup task failed: {error}")),
                    }
                } else {
                    task.abort();
                    pending.push(index);
                }
            }
            let wait_aborted = async {
                for index in pending {
                    match (&mut tracked[index]).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => errors.push(error),
                        Err(error) if error.is_cancelled() => {}
                        Err(error) => errors.push(format!("KWin cleanup task failed: {error}")),
                    }
                }
            };
            let _ = timeout(ABORTED_TASK_WAIT, wait_aborted).await;
            errors.push(
                "timed out waiting for KWin cleanup; remote script state is unconfirmed".into(),
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("KWin cleanup failed: {}", errors.join("; ")))
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
            .map_err(|error| format!("could not read window geometry: {error}"))
    }

    async fn activate_window(&self, window: &WindowHandle) -> Result<(), String> {
        let id = wdotool_core::WindowId(window.as_str().to_owned());
        self.backend
            .activate_window(&id)
            .await
            .map_err(|error| format!("could not focus window: {error}"))
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
    let x_start = js_partition("column", "area.width", grid.columns);
    let x_end = js_partition("(column + 1)", "area.width", grid.columns);
    let y_start = js_partition("row", "area.height", grid.rows);
    let y_end = js_partition("(row + 1)", "area.height", grid.rows);
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

fn js_partition(boundary: &str, dimension: &str, divisions: u32) -> String {
    format!("Math.floor({boundary} * {dimension} / {divisions})")
}

pub(crate) async fn run_qdbus_command(program: &str, args: &[&str]) -> Result<String, String> {
    run_command_with_timeout(program, args, QDBUS_COMMAND_TIMEOUT).await
}

async fn run_command_with_timeout(
    program: &str,
    args: &[&str],
    limit: Duration,
) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout is piped for every owned command");
    let stderr = child
        .stderr
        .take()
        .expect("stderr is piped for every owned command");
    let (status, stdout, stderr) = match timeout(
        limit,
        wait_and_capture(&mut child, stdout, stderr),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(wait_error)) => {
            let termination = terminate_and_reap(&mut child).await;
            return Err(match termination {
                Ok(()) => format!(
                    "could not read {program} output or wait for completion: {wait_error}; child was terminated and reaped"
                ),
                Err(termination_error) => {
                    format!("could not read {program} output or wait for completion: {wait_error}; {termination_error}")
                }
            });
        }
        Err(_) => {
            let detail = match child.try_wait() {
                Ok(Some(_)) => {
                    "child had exited and was reaped, but its output pipes did not close before the deadline"
                        .to_owned()
                }
                Ok(None) => match terminate_and_reap(&mut child).await {
                    Ok(()) => "child was terminated and reaped".to_owned(),
                    Err(error) => error,
                },
                Err(error) => match terminate_and_reap(&mut child).await {
                    Ok(()) => format!(
                        "child was terminated and reaped after checking its state failed: {error}"
                    ),
                    Err(termination) => {
                        format!("could not check child state: {error}; {termination}")
                    }
                },
            };
            return Err(format!(
                "{program} {} timed out after {:.1}s; {detail}; remote operation outcome is uncertain",
                args.join(" "),
                limit.as_secs_f64()
            ));
        }
    };

    if stdout.truncated || stderr.truncated {
        return Err(format!(
            "{program} output exceeded the {} byte capture limit",
            MAX_CAPTURED_OUTPUT_BYTES
        ));
    }
    let output = std::process::Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    };
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

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn wait_and_capture(
    child: &mut Child,
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
) -> std::io::Result<(std::process::ExitStatus, CapturedOutput, CapturedOutput)> {
    let mut stdout_output = CapturedOutput::new();
    let mut stderr_output = CapturedOutput::new();
    let mut stdout_buffer = [0; 4096];
    let mut stderr_buffer = [0; 4096];
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    let mut status = None;

    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some() && stdout_closed && stderr_closed {
            return Ok((
                status.take().expect("child exit status was checked above"),
                stdout_output,
                stderr_output,
            ));
        }

        tokio::select! {
            result = stdout.read(&mut stdout_buffer), if !stdout_closed => {
                match result? {
                    0 => stdout_closed = true,
                    count => stdout_output.append(&stdout_buffer[..count]),
                }
            }
            result = stderr.read(&mut stderr_buffer), if !stderr_closed => {
                match result? {
                    0 => stderr_closed = true,
                    count => stderr_output.append(&stderr_buffer[..count]),
                }
            }
            _ = sleep(Duration::from_millis(10)), if status.is_none() => {}
        }
    }
}

impl CapturedOutput {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(MAX_CAPTURED_OUTPUT_BYTES),
            truncated: false,
        }
    }

    fn append(&mut self, input: &[u8]) {
        let available = MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        let captured = input.len().min(available);
        self.bytes.extend_from_slice(&input[..captured]);
        self.truncated |= captured < input.len();
    }
}

async fn terminate_and_reap(child: &mut Child) -> Result<(), String> {
    let kill_error = child.start_kill().err();
    match timeout(CHILD_TERMINATION_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("could not reap the owned child process: {error}")),
        Err(_) => Err(match kill_error {
            Some(error) => format!("could not confirm owned child termination or reaping: {error}"),
            None => "timed out confirming owned child termination or reaping".into(),
        }),
    }
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
            run_qdbus_command(
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
            run_qdbus_command(
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
            run_qdbus_command(
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
    cancel: oneshot::Receiver<()>,
) -> Result<(), String> {
    run_script_lifecycle_with_timeout(
        commands,
        plugin,
        script_path,
        cancel,
        SCRIPT_OPERATION_TIMEOUT,
    )
    .await
}

async fn run_script_lifecycle_with_timeout<C: KWinScriptCommands>(
    commands: C,
    plugin: String,
    script_path: PathBuf,
    mut cancel: oneshot::Receiver<()>,
    operation_timeout: Duration,
) -> Result<(), String> {
    if cancellation_requested(&mut cancel) {
        return remove_temporary_script(&script_path);
    }

    let result = match bounded_script_command(
        "load",
        commands.load_script(&script_path, &plugin),
        operation_timeout,
    )
    .await
    {
        Ok(script_id) if !cancellation_requested(&mut cancel) => {
            bounded_script_command(
                "execution",
                commands.run_script(&script_id),
                operation_timeout,
            )
            .await
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
    let unload =
        bounded_script_command("unload", commands.unload_script(&plugin), operation_timeout)
            .await
            .map_err(|error| {
                format!(
                    "KWin script {plugin} unload failed; remote cleanup is unconfirmed: {error}"
                )
            });
    let remove = remove_temporary_script(&script_path);
    match (result, unload, remove) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (operation, unload, remove) => {
            let errors = [operation.err(), unload.err(), remove.err()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            Err(errors.join("; "))
        }
    }
}

async fn bounded_script_command<T, F>(
    operation: &str,
    command: F,
    limit: Duration,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    match timeout(limit, command).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "KWin script {operation} timed out after {:.1}s; remote script state is unconfirmed",
            limit.as_secs_f64()
        )),
    }
}

fn remove_temporary_script(path: &std::path::Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|error| {
        format!(
            "could not remove temporary KWin script {}: {error}",
            path.display()
        )
    })
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

    struct DelayedRunCommands {
        events: Arc<Mutex<Vec<&'static str>>>,
        run_started: Mutex<Option<oneshot::Sender<()>>>,
        release_run: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl DelayedRunCommands {
        fn new(run_started: oneshot::Sender<()>, release_run: oneshot::Receiver<()>) -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
                run_started: Mutex::new(Some(run_started)),
                release_run: tokio::sync::Mutex::new(Some(release_run)),
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl KWinScriptCommands for DelayedRunCommands {
        fn load_script<'a>(
            &'a self,
            _path: &'a std::path::Path,
            _plugin: &'a str,
        ) -> impl Future<Output = Result<String, String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("load");
                Ok("42".into())
            }
        }

        fn run_script<'a>(
            &'a self,
            _script_id: &'a str,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("run-start");
                if let Some(started) = self.run_started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                let release = self.release_run.lock().await.take().unwrap();
                let _ = release.await;
                self.events.lock().unwrap().push("run-finished");
                Ok(())
            }
        }

        fn unload_script<'a>(
            &'a self,
            _plugin: &'a str,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("unload");
                Ok(())
            }
        }
    }

    struct ImmediateCommands {
        events: Arc<Mutex<Vec<&'static str>>>,
        fail_load: bool,
        fail_unload: bool,
        never_load: bool,
        never_unload: bool,
    }

    #[allow(clippy::manual_async_fn)]
    impl KWinScriptCommands for ImmediateCommands {
        fn load_script<'a>(
            &'a self,
            _path: &'a std::path::Path,
            _plugin: &'a str,
        ) -> impl Future<Output = Result<String, String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("load");
                if self.never_load {
                    std::future::pending().await
                } else if self.fail_load {
                    Err("simulated load failure".into())
                } else {
                    Ok("42".into())
                }
            }
        }

        fn run_script<'a>(
            &'a self,
            _script_id: &'a str,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("run");
                Ok(())
            }
        }

        fn unload_script<'a>(
            &'a self,
            _plugin: &'a str,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.events.lock().unwrap().push("unload");
                if self.never_unload {
                    std::future::pending().await
                } else if self.fail_unload {
                    Err("simulated D-Bus failure".into())
                } else {
                    Ok(())
                }
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
            .unwrap()
            .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            ["load-start", "load-finished", "unload"]
        );
        assert!(!script_path.exists(), "script file survives until unload");
    }

    #[tokio::test]
    async fn canceled_layout_drains_script_execution_before_unload() {
        let script_path = test_script_path();
        std::fs::write(&script_path, "test script").unwrap();
        let (run_started_tx, run_started_rx) = oneshot::channel();
        let (release_run_tx, release_run_rx) = oneshot::channel();
        let commands = DelayedRunCommands::new(run_started_tx, release_run_rx);
        let events = Arc::clone(&commands.events);
        let cleanup = KdeCleanupQueue::default();
        let task = KWinLayoutTask::spawn(
            commands,
            "test_plugin_run_cancel".into(),
            script_path.clone(),
            cleanup.clone(),
        );

        run_started_rx.await.unwrap();
        drop(task);
        let mut finish = tokio::spawn(async move { cleanup.finish().await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut finish)
            .await
            .is_err());
        release_run_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), &mut finish)
            .await
            .expect("cleanup must drain script execution")
            .unwrap()
            .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            ["load", "run-start", "run-finished", "unload"]
        );
        assert!(!script_path.exists());
    }

    #[tokio::test]
    async fn successful_lifecycle_unloads_and_removes_its_temporary_script() {
        let script_path = test_script_path();
        std::fs::write(&script_path, "test script").unwrap();
        let commands = ImmediateCommands {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_load: false,
            fail_unload: false,
            never_load: false,
            never_unload: false,
        };
        let events = Arc::clone(&commands.events);
        let (_cancel, cancelled) = oneshot::channel();
        let result = run_script_lifecycle(
            commands,
            "test_plugin_success".into(),
            script_path.clone(),
            cancelled,
        )
        .await;
        assert_eq!(result, Ok(()));
        assert_eq!(*events.lock().unwrap(), ["load", "run", "unload"]);
        assert!(!script_path.exists());
    }

    #[tokio::test]
    async fn unload_failure_is_returned_and_temporary_file_is_removed() {
        let script_path = test_script_path();
        std::fs::write(&script_path, "test script").unwrap();
        let commands = ImmediateCommands {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_load: false,
            fail_unload: true,
            never_load: false,
            never_unload: false,
        };
        let (_cancel, cancelled) = oneshot::channel();
        let error = run_script_lifecycle(
            commands,
            "test_plugin_unload_failure".into(),
            script_path.clone(),
            cancelled,
        )
        .await
        .unwrap_err();
        assert!(error.contains("unload failed; remote cleanup is unconfirmed"));
        assert!(error.contains("simulated D-Bus failure"));
        assert!(!script_path.exists());
    }

    #[tokio::test]
    async fn primary_lifecycle_failure_is_preserved_with_unload_failure() {
        let script_path = test_script_path();
        std::fs::write(&script_path, "test script").unwrap();
        let commands = ImmediateCommands {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_load: true,
            fail_unload: true,
            never_load: false,
            never_unload: false,
        };
        let events = Arc::clone(&commands.events);
        let (_cancel, cancelled) = oneshot::channel();

        let error = run_script_lifecycle(
            commands,
            "test_plugin_primary_and_cleanup_failure".into(),
            script_path.clone(),
            cancelled,
        )
        .await
        .unwrap_err();

        assert!(error.contains("simulated load failure"));
        assert!(error.contains("simulated D-Bus failure"));
        assert_eq!(*events.lock().unwrap(), ["load", "unload"]);
        assert!(!script_path.exists());
    }

    #[tokio::test]
    async fn never_completing_load_times_out_then_attempts_unload() {
        let script_path = test_script_path();
        std::fs::write(&script_path, "test script").unwrap();
        let commands = ImmediateCommands {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_load: false,
            fail_unload: false,
            never_load: true,
            never_unload: false,
        };
        let events = Arc::clone(&commands.events);
        let (_cancel, cancelled) = oneshot::channel();

        let error = run_script_lifecycle_with_timeout(
            commands,
            "test_plugin_stalled_load".into(),
            script_path.clone(),
            cancelled,
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();

        assert!(error.contains("KWin script load timed out"));
        assert_eq!(*events.lock().unwrap(), ["load", "unload"]);
        assert!(!script_path.exists());
    }

    #[tokio::test]
    async fn never_completing_unload_times_out_and_reports_remote_uncertainty() {
        let script_path = test_script_path();
        std::fs::write(&script_path, "test script").unwrap();
        let commands = ImmediateCommands {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_load: false,
            fail_unload: false,
            never_load: false,
            never_unload: true,
        };
        let events = Arc::clone(&commands.events);
        let (_cancel, cancelled) = oneshot::channel();

        let error = run_script_lifecycle_with_timeout(
            commands,
            "test_plugin_stalled_unload".into(),
            script_path.clone(),
            cancelled,
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();

        assert!(error.contains("unload failed; remote cleanup is unconfirmed"));
        assert!(error.contains("unload timed out"));
        assert_eq!(*events.lock().unwrap(), ["load", "run", "unload"]);
        assert!(!script_path.exists());
    }

    #[tokio::test]
    async fn cleanup_queue_finishes_multiple_operations_and_reports_failures() {
        let cleanup = KdeCleanupQueue::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        for (name, fail) in [("first", false), ("second", true), ("third", false)] {
            let events = Arc::clone(&events);
            cleanup.track(tokio::spawn(async move {
                events.lock().unwrap().push(name);
                if fail {
                    Err("simulated cleanup failure".into())
                } else {
                    Ok(())
                }
            }));
        }

        let error = cleanup.finish().await.unwrap_err();
        assert!(error.contains("simulated cleanup failure"));
        let mut completed = events.lock().unwrap().clone();
        completed.sort_unstable();
        assert_eq!(completed, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn cleanup_queue_aborts_pending_tasks_after_its_deadline() {
        use std::sync::atomic::AtomicUsize;

        struct DropMark(Arc<AtomicUsize>);
        impl Drop for DropMark {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let cleanup = KdeCleanupQueue::default();
        let drops = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let mark = DropMark(Arc::clone(&drops));
            cleanup.track(tokio::spawn(async move {
                let _mark = mark;
                std::future::pending::<Result<(), String>>().await
            }));
        }

        let error = cleanup
            .finish_with_timeout(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("timed out waiting for KWin cleanup"));
        assert!(error.contains("remote script state is unconfirmed"));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cleanup_queue_deadline_keeps_completed_results_around_pending_tasks() {
        use std::sync::atomic::AtomicUsize;

        struct DropMark(Arc<AtomicUsize>);
        impl Drop for DropMark {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let cleanup = KdeCleanupQueue::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let pending_mark = DropMark(Arc::clone(&drops));
        cleanup.track(tokio::spawn(async move {
            let _mark = pending_mark;
            std::future::pending::<Result<(), String>>().await
        }));
        cleanup.track(tokio::spawn(async {
            Err("simulated unload failure".into())
        }));
        cleanup.track(tokio::spawn(async { Ok(()) }));

        let error = cleanup
            .finish_with_timeout(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("simulated unload failure"));
        assert!(error.contains("timed out waiting for KWin cleanup"));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn subprocess_timeout_terminates_and_reaps_owned_child() {
        let pid_path = test_script_path().with_extension("pid");
        let pid_path_str = pid_path.to_str().unwrap();
        let command = format!("echo $$ > '{pid_path_str}'; exec sleep 60");
        let error = run_command_with_timeout("sh", &["-c", &command], Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(error.contains("child was terminated and reaped"));

        let pid = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .to_owned();
        let process_path = PathBuf::from(format!("/proc/{pid}"));
        assert!(
            !process_path.exists(),
            "timed-out child was still present when the command runner returned"
        );
        let _ = std::fs::remove_file(pid_path);
    }

    #[tokio::test]
    async fn subprocess_runner_captures_output_on_success() {
        let output = run_command_with_timeout(
            "sh",
            &["-c", "printf 'captured'; printf 'diagnostic' >&2"],
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(output, "captured");
    }
}
