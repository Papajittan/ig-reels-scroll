use std::{collections::HashSet, time::Duration};

use crate::{
    audio::AudioHandler,
    browser::BrowserAdapter,
    cli::Config,
    layout::{has_overlaps, GridSpec, LogicalRect},
    platform::{LayoutPass, Platform, WindowHandle},
};

const STARTUP_DELAY: Duration = Duration::from_secs(8);
const ACTIVATION_SETTLE: Duration = Duration::from_millis(100);
const AUDIO_SETTLE: Duration = Duration::from_millis(200);
const POST_SCROLL_AUDIO_SETTLE: Duration = Duration::from_millis(350);
const STAGGER_SETTLE: Duration = Duration::from_millis(900);
const LAYOUT_ATTEMPTS: usize = 20;
const LAYOUT_RETRY_EVERY: usize = 5;
const LAYOUT_RETRY_DELAY: Duration = Duration::from_millis(100);

pub async fn run<P, B, A>(
    config: &Config,
    platform: &P,
    browser: &B,
    audio: &A,
) -> Result<(), String>
where
    P: Platform,
    B: BrowserAdapter,
    A: AudioHandler<P>,
{
    let selected = browser
        .acquire_windows(platform, config.windows, config.open_browser)
        .await?;
    let windows = validate_selected_windows(selected, config.windows)?;
    let layout = GridSpec::for_count(windows.len())?;

    if config.open_browser {
        println!("Tiling the Reels windows to fit the screen...");
    }
    platform
        .arrange_windows(&windows, layout, LayoutPass::Restore)
        .await?;
    platform
        .arrange_windows(&windows, layout, LayoutPass::Place)
        .await?;
    confirm_layout(platform, &windows, layout).await?;

    println!(
        "Sign in and dismiss any Instagram popups. Scrolling begins in 8 seconds. Press X to stop."
    );
    tokio::time::sleep(STARTUP_DELAY).await;

    if audio.enabled() {
        println!("Auto-unmute enabled for all selected windows.");
        for window in &windows {
            platform.activate_window(window).await?;
            tokio::time::sleep(AUDIO_SETTLE).await;
            let geometry = get_geometry(platform, window).await?;
            audio.unmute(platform, window, &geometry).await;
        }
    }

    for (index, window) in windows.iter().enumerate().skip(1) {
        let offset = starting_offset(index, config.stagger);
        if offset > 0 {
            println!(
                "Moving window {} ahead by {offset} scroll steps...",
                index + 1
            );
            for _ in 0..offset {
                scroll_window(platform, window, audio).await?;
                // Give Instagram time to complete each reel transition.
                tokio::time::sleep(STAGGER_SETTLE).await;
            }
        }
    }

    for step in 1..=config.count {
        for window in &windows {
            scroll_window(platform, window, audio).await?;
        }
        println!(
            "Scroll pass {step}/{} across {} window(s)",
            config.count,
            windows.len()
        );
        if step < config.count {
            tokio::time::sleep(config.interval).await;
        }
    }
    println!("Done.");
    Ok(())
}

/// Browser adapters must return exactly the requested number of distinct,
/// non-empty window handles before the engine sends any desktop input.
fn validate_selected_windows<M>(
    selected: Vec<crate::browser::BrowserWindow<M>>,
    expected_count: usize,
) -> Result<Vec<WindowHandle>, String> {
    if selected.len() != expected_count {
        return Err(format!(
            "browser adapter returned {} window(s), expected {expected_count}",
            selected.len()
        ));
    }

    let mut identities = HashSet::with_capacity(selected.len());
    for window in &selected {
        let identity = window.handle.as_str();
        if identity.trim().is_empty() {
            return Err("browser adapter returned an empty window handle".into());
        }
        if !identities.insert(identity) {
            return Err("browser adapter returned duplicate window handles".into());
        }
    }

    Ok(selected.into_iter().map(|window| window.handle).collect())
}

pub fn starting_offset(index: usize, stagger: u32) -> u32 {
    index as u32 * stagger
}

async fn confirm_layout<P: Platform>(
    platform: &P,
    windows: &[WindowHandle],
    layout: GridSpec,
) -> Result<(), String> {
    for attempt in 0..LAYOUT_ATTEMPTS {
        let mut geometry = Vec::with_capacity(windows.len());
        for window in windows {
            geometry.push(get_geometry(platform, window).await?);
        }
        if geometry.iter().all(|rect| rect.is_valid()) && !has_overlaps(&geometry) {
            for (index, rect) in geometry.iter().enumerate() {
                println!(
                    "Window {}: {}x{} at {},{}",
                    index + 1,
                    rect.width,
                    rect.height,
                    rect.x,
                    rect.y
                );
            }
            return Ok(());
        }
        if attempt % LAYOUT_RETRY_EVERY == LAYOUT_RETRY_EVERY - 1 {
            // A late window restore can replace an earlier resize.
            platform
                .arrange_windows(windows, layout, LayoutPass::Place)
                .await?;
        }
        tokio::time::sleep(LAYOUT_RETRY_DELAY).await;
    }
    Err("Selected windows still overlap after layout retries. Scrolling stopped.".into())
}

async fn get_geometry<P: Platform>(
    platform: &P,
    window: &WindowHandle,
) -> Result<LogicalRect, String> {
    platform
        .window_geometry(window)
        .await?
        .ok_or_else(|| "Could not read selected window geometry".into())
}

async fn scroll_window<P, A>(platform: &P, window: &WindowHandle, audio: &A) -> Result<(), String>
where
    P: Platform,
    A: AudioHandler<P>,
{
    platform.activate_window(window).await?;
    tokio::time::sleep(ACTIVATION_SETTLE).await;
    let geometry = get_geometry(platform, window).await?;
    let (x, y) = geometry
        .center()
        .ok_or("Selected window center is outside the supported coordinate range")?;
    platform.move_pointer(x, y).await?;
    // Scroll roughly one content viewport so a step advances the feed naturally.
    platform.scroll(0.0, geometry.scroll_distance()).await?;
    if audio.enabled() {
        tokio::time::sleep(POST_SCROLL_AUDIO_SETTLE).await;
        audio.unmute(platform, window, &geometry).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::{sync::watch, time::Instant};

    use super::*;
    use crate::{
        audio::NoAudio,
        browser::BrowserWindow,
        layout::LogicalRect,
        platform::{ButtonState, PlatformWindow, PointerButton},
    };

    #[derive(Clone, Debug)]
    struct TestMetadata;

    #[derive(Clone)]
    struct FakeBrowser {
        windows: Vec<BrowserWindow<TestMetadata>>,
        events: Arc<Mutex<Vec<String>>>,
        acquisition_error: Option<&'static str>,
        return_all: bool,
    }

    impl BrowserAdapter for FakeBrowser {
        type Metadata = TestMetadata;

        fn identifies(&self, _app_id: Option<&str>) -> bool {
            true
        }

        fn metadata(&self, _app_id: Option<String>, _title: String) -> Self::Metadata {
            TestMetadata
        }

        fn launch_new_window(&self) -> Result<(), String> {
            self.events.lock().unwrap().push("launch".into());
            Ok(())
        }

        async fn acquire_windows<P: Platform>(
            &self,
            _platform: &P,
            count: usize,
            _open_browser: bool,
        ) -> Result<Vec<BrowserWindow<Self::Metadata>>, String> {
            self.events.lock().unwrap().push("acquire".into());
            if let Some(error) = self.acquisition_error {
                return Err(error.into());
            }
            Ok(if self.return_all {
                self.windows.clone()
            } else {
                self.windows.iter().take(count).cloned().collect()
            })
        }
    }

    struct FakePlatform {
        windows: Vec<PlatformWindow>,
        events: Arc<Mutex<Vec<String>>>,
        overlap: bool,
        fail: Option<&'static str>,
    }

    impl FakePlatform {
        fn record(&self, event: impl Into<String>, operation: &'static str) -> Result<(), String> {
            if self.fail == Some(operation) {
                return Err(format!("fake {operation} failure"));
            }
            self.events.lock().unwrap().push(event.into());
            Ok(())
        }
    }

    impl Platform for FakePlatform {
        async fn list_windows(&self) -> Result<Vec<PlatformWindow>, String> {
            self.record("list".to_owned(), "list")?;
            Ok(self.windows.clone())
        }

        async fn window_geometry(
            &self,
            window: &WindowHandle,
        ) -> Result<Option<LogicalRect>, String> {
            self.record(format!("geometry:{}", window.as_str()), "geometry")?;
            let index = self
                .windows
                .iter()
                .position(|item| item.handle == *window)
                .unwrap_or(0);
            Ok(Some(LogicalRect {
                x: if self.overlap { 0 } else { index as i32 * 1000 },
                y: 0,
                width: 900,
                height: 1000,
            }))
        }

        async fn activate_window(&self, window: &WindowHandle) -> Result<(), String> {
            self.record(format!("activate:{}", window.as_str()), "activate")
        }

        async fn arrange_windows(
            &self,
            _windows: &[WindowHandle],
            _layout: GridSpec,
            pass: LayoutPass,
        ) -> Result<(), String> {
            self.record(format!("layout:{pass:?}"), "arrange")
        }

        async fn move_pointer(&self, x: i32, y: i32) -> Result<(), String> {
            self.record(format!("move:{x},{y}"), "move")
        }

        async fn scroll(&self, _horizontal: f64, vertical: f64) -> Result<(), String> {
            self.record(format!("scroll:{vertical}"), "scroll")
        }

        async fn pointer_button(
            &self,
            _button: PointerButton,
            _state: ButtonState,
        ) -> Result<(), String> {
            self.record("button".to_owned(), "button")
        }
    }

    #[derive(Clone)]
    struct FakeAudio {
        events: Arc<Mutex<Vec<String>>>,
        enabled: bool,
    }

    impl<P: Platform> AudioHandler<P> for FakeAudio {
        fn enabled(&self) -> bool {
            self.enabled
        }

        async fn unmute(&self, _platform: &P, window: &WindowHandle, _bounds: &LogicalRect) {
            self.events
                .lock()
                .unwrap()
                .push(format!("audio:{}", window.as_str()));
        }
    }

    fn fixtures(
        count: usize,
        overlap: bool,
        fail: Option<&'static str>,
    ) -> (FakePlatform, FakeBrowser, Arc<Mutex<Vec<String>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let platform_windows: Vec<_> = (0..count)
            .map(|index| PlatformWindow {
                handle: WindowHandle::new(format!("window-{index}")),
                app_id: Some("firefox".into()),
                title: format!("Reels {index}"),
            })
            .collect();
        let browser_windows = platform_windows
            .iter()
            .map(|window| BrowserWindow {
                handle: window.handle.clone(),
                metadata: TestMetadata,
            })
            .collect();
        (
            FakePlatform {
                windows: platform_windows,
                events: events.clone(),
                overlap,
                fail,
            },
            FakeBrowser {
                windows: browser_windows,
                events: events.clone(),
                acquisition_error: None,
                return_all: false,
            },
            events,
        )
    }

    fn config(windows: usize, count: u32, interval: Duration, stagger: u32) -> Config {
        Config {
            windows,
            count,
            interval,
            open_browser: false,
            stagger,
            unmute: false,
        }
    }

    async fn finish_paused<T>(task: tokio::task::JoinHandle<T>) -> T {
        for _ in 0..200 {
            tokio::task::yield_now().await;
            if task.is_finished() {
                return task.await.unwrap();
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        panic!("paused engine task did not finish")
    }

    #[test]
    fn starting_stagger_offsets_are_linear_and_zero_is_supported() {
        assert_eq!(
            (0..4)
                .map(|index| starting_offset(index, 5))
                .collect::<Vec<_>>(),
            vec![0, 5, 10, 15]
        );
        assert_eq!(starting_offset(15, 0), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn acquisition_precedes_layout_and_layout_verification_precedes_startup() {
        let (platform, browser, events) = fixtures(2, false, None);
        let task = tokio::spawn(async move {
            run(
                &config(2, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await
        });
        let result = finish_paused(task).await;
        assert!(result.is_ok());
        let events = events.lock().unwrap();
        let acquire = events.iter().position(|event| event == "acquire").unwrap();
        let restore = events
            .iter()
            .position(|event| event == "layout:Restore")
            .unwrap();
        let place = events
            .iter()
            .position(|event| event == "layout:Place")
            .unwrap();
        let geometry = events
            .iter()
            .position(|event| event.starts_with("geometry:"))
            .unwrap();
        let first_scroll = events
            .iter()
            .position(|event| event.starts_with("scroll:"))
            .unwrap();
        assert!(
            acquire < restore && restore < place && place < geometry && geometry < first_scroll
        );
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_window_acquisition_stops_before_platform_manipulation() {
        async fn assert_rejected(
            platform: FakePlatform,
            browser: FakeBrowser,
            expected_error: &str,
        ) {
            let events = Arc::clone(&platform.events);
            let result = run(
                &config(2, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await;
            assert_eq!(result.unwrap_err(), expected_error);
            assert_eq!(*events.lock().unwrap(), ["acquire"]);
        }

        let (platform, mut browser, _) = fixtures(2, false, None);
        browser.windows.truncate(1);
        assert_rejected(
            platform,
            browser,
            "browser adapter returned 1 window(s), expected 2",
        )
        .await;

        let (platform, mut browser, _) = fixtures(2, false, None);
        browser.windows.clear();
        assert_rejected(
            platform,
            browser,
            "browser adapter returned 0 window(s), expected 2",
        )
        .await;

        let (platform, mut browser, _) = fixtures(2, false, None);
        browser.windows[1].handle = browser.windows[0].handle.clone();
        assert_rejected(
            platform,
            browser,
            "browser adapter returned duplicate window handles",
        )
        .await;

        let (platform, mut browser, _) = fixtures(2, false, None);
        browser.windows[0].handle = WindowHandle::new("  ");
        assert_rejected(
            platform,
            browser,
            "browser adapter returned an empty window handle",
        )
        .await;

        let (platform, mut browser, _) = fixtures(2, false, None);
        browser.windows.push(BrowserWindow {
            handle: WindowHandle::new("extra-window"),
            metadata: TestMetadata,
        });
        browser.return_all = true;
        assert_rejected(
            platform,
            browser,
            "browser adapter returned 3 window(s), expected 2",
        )
        .await;

        let (platform, mut browser, _) = fixtures(2, false, None);
        browser.acquisition_error = Some("adapter acquisition failed");
        assert_rejected(platform, browser, "adapter acquisition failed").await;
    }

    #[tokio::test(start_paused = true)]
    async fn pass_schedule_waits_between_passes_and_skips_the_final_wait() {
        let (platform, browser, events) = fixtures(1, false, None);
        let start = Instant::now();
        let task = tokio::spawn(async move {
            run(
                &config(1, 2, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await
        });
        assert!(finish_paused(task).await.is_ok());
        assert!(Instant::now() - start >= Duration::from_secs(11));
        assert!(Instant::now() - start < Duration::from_secs(12));
        let scrolls = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.starts_with("scroll:"))
            .count();
        assert_eq!(scrolls, 2);

        let (platform, browser, _) = fixtures(1, false, None);
        let start = Instant::now();
        let task = tokio::spawn(async move {
            run(
                &config(1, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await
        });
        assert!(finish_paused(task).await.is_ok());
        assert!(Instant::now() - start >= Duration::from_secs(8));
        assert!(Instant::now() - start < Duration::from_secs(9));
    }

    #[tokio::test(start_paused = true)]
    async fn activation_geometry_pointer_and_scroll_are_ordered_per_window() {
        let (platform, browser, events) = fixtures(1, false, None);
        let task = tokio::spawn(async move {
            run(
                &config(1, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await
        });
        assert!(finish_paused(task).await.is_ok());
        let events = events.lock().unwrap();
        let activate = events
            .iter()
            .position(|event| event == "activate:window-0")
            .unwrap();
        let geometry = events
            .iter()
            .skip(activate + 1)
            .position(|event| event == "geometry:window-0")
            .map(|index| index + activate + 1)
            .unwrap();
        let pointer = events
            .iter()
            .position(|event| event == "move:450,500")
            .unwrap();
        let scroll = events
            .iter()
            .position(|event| event == "scroll:840")
            .unwrap();
        assert!(activate < geometry && geometry < pointer && pointer < scroll);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_startup_delay_does_not_send_input() {
        let (platform, browser, events) = fixtures(1, false, None);
        let (cancel, mut receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            let settings = config(1, 1, Duration::from_secs(3), 0);
            tokio::select! {
                biased;
                _ = receiver.wait_for(|cancelled| *cancelled) => Ok(()),
                result = run(&settings, &platform, &browser, &NoAudio) => result,
            }
        });
        tokio::task::yield_now().await;
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "layout:Place"));
        cancel.send_replace(true);
        assert!(task.await.unwrap().is_ok());
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("scroll:")));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_in_interpass_delay_stops_before_the_next_pass() {
        let (platform, browser, events) = fixtures(1, false, None);
        let (cancel, mut receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            let settings = config(1, 2, Duration::from_secs(3), 0);
            tokio::select! {
                biased;
                _ = receiver.wait_for(|cancelled| *cancelled) => Ok(()),
                result = run(&settings, &platform, &browser, &NoAudio) => result,
            }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(8)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("scroll:"))
                .count(),
            1
        );
        cancel.send_replace(true);
        assert!(task.await.unwrap().is_ok());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("scroll:"))
                .count(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_stagger_wait_stops_before_regular_passes() {
        let (platform, browser, events) = fixtures(2, false, None);
        let (cancel, mut receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            let settings = config(2, 1, Duration::from_secs(3), 1);
            tokio::select! {
                biased;
                _ = receiver.wait_for(|cancelled| *cancelled) => Ok(()),
                result = run(&settings, &platform, &browser, &NoAudio) => result,
            }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(8)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(100)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("scroll:"))
                .count(),
            1,
            "one starting offset should have completed"
        );
        cancel.send_replace(true);
        assert!(task.await.unwrap().is_ok());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("scroll:"))
                .count(),
            1,
            "cancellation during the stagger wait must prevent more scrolling"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backend_failures_propagate_and_overlapping_windows_are_never_scrolled() {
        let (platform, browser, events) = fixtures(1, false, Some("activate"));
        let task = tokio::spawn(async move {
            run(
                &config(1, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await
        });
        assert_eq!(
            finish_paused(task).await.unwrap_err(),
            "fake activate failure"
        );
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("scroll:")));

        let (platform, browser, events) = fixtures(2, true, None);
        let task = tokio::spawn(async move {
            run(
                &config(2, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await
        });
        let error = finish_paused(task).await.unwrap_err();
        assert!(error.contains("still overlap"));
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("scroll:")));
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| *event == "layout:Place")
                .count(),
            5
        );
    }

    #[tokio::test(start_paused = true)]
    async fn audio_disabled_is_inert_and_enabled_audio_runs_at_both_hook_points() {
        let (platform, browser, events) = fixtures(1, false, None);
        let task = tokio::spawn(async move {
            run(
                &config(1, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &NoAudio,
            )
            .await
        });
        assert!(finish_paused(task).await.is_ok());
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("audio:")));

        let (platform, browser, events) = fixtures(1, false, None);
        let audio = FakeAudio {
            events: events.clone(),
            enabled: true,
        };
        let task = tokio::spawn(async move {
            run(
                &config(1, 1, Duration::from_secs(3), 0),
                &platform,
                &browser,
                &audio,
            )
            .await
        });
        assert!(finish_paused(task).await.is_ok());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("audio:"))
                .count(),
            2
        );
    }
}
