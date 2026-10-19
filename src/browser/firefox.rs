use std::{collections::HashSet, sync::Arc, time::Duration};

use tokio::time::sleep;

use super::{BrowserAdapter, BrowserWindow};
use crate::platform::{Platform, PlatformWindow};

const REELS_URL: &str = "https://www.instagram.com/reels/";
const DISCOVERY_ATTEMPTS: usize = 300;
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirefoxMetadata {
    pub application_id: Option<String>,
    pub title: String,
}

#[derive(Clone)]
pub struct FirefoxAdapter {
    launch: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

impl Default for FirefoxAdapter {
    fn default() -> Self {
        Self {
            launch: Arc::new(launch_firefox_window),
        }
    }
}

impl FirefoxAdapter {
    async fn firefox_windows<P: Platform>(
        &self,
        platform: &P,
    ) -> Result<Vec<BrowserWindow<FirefoxMetadata>>, String> {
        let windows = platform.list_windows().await.map_err(|error| {
            format!(
                "KWin could not list Firefox windows: {}",
                error.trim_start()
            )
        })?;
        Ok(windows
            .into_iter()
            .filter(|window| self.identifies(window.app_id.as_deref()))
            .map(|window| self.adapt_window(window))
            .collect())
    }

    fn adapt_window(&self, window: PlatformWindow) -> BrowserWindow<FirefoxMetadata> {
        BrowserWindow {
            handle: window.handle,
            metadata: self.metadata(window.app_id, window.title),
        }
    }

    async fn wait_for_new_window<P: Platform>(
        &self,
        platform: &P,
        existing: &HashSet<crate::platform::WindowHandle>,
    ) -> Result<BrowserWindow<FirefoxMetadata>, String> {
        for _ in 0..DISCOVERY_ATTEMPTS {
            if let Some(window) = self
                .firefox_windows(platform)
                .await?
                .into_iter()
                .find(|window| !existing.contains(&window.handle))
            {
                return Ok(window);
            }
            sleep(DISCOVERY_INTERVAL).await;
        }
        Err("Firefox did not create a new Wayland window in 30 seconds".into())
    }
}

impl BrowserAdapter for FirefoxAdapter {
    type Metadata = FirefoxMetadata;

    fn identifies(&self, app_id: Option<&str>) -> bool {
        app_id
            .unwrap_or_default()
            .to_lowercase()
            .contains("firefox")
    }

    fn metadata(&self, app_id: Option<String>, title: String) -> Self::Metadata {
        FirefoxMetadata {
            application_id: app_id,
            title,
        }
    }

    fn launch_new_window(&self) -> Result<(), String> {
        (self.launch)()
    }

    async fn acquire_windows<P: Platform>(
        &self,
        platform: &P,
        count: usize,
        open_browser: bool,
    ) -> Result<Vec<BrowserWindow<Self::Metadata>>, String> {
        if open_browser {
            println!("Opening {count} Firefox Reels window(s)...");
            let mut opened = Vec::with_capacity(count);
            for _ in 0..count {
                let existing: HashSet<_> = self
                    .firefox_windows(platform)
                    .await?
                    .into_iter()
                    .map(|window| window.handle)
                    .collect();
                self.launch_new_window()?;
                opened.push(self.wait_for_new_window(platform, &existing).await?);
            }
            Ok(opened)
        } else {
            let windows = self.firefox_windows(platform).await?;
            if windows.len() < count {
                return Err(format!(
                    "--no-open requested {count} Firefox windows, found {}",
                    windows.len()
                ));
            }
            Ok(windows.into_iter().take(count).collect())
        }
    }
}

fn launch_firefox_window() -> Result<(), String> {
    std::process::Command::new("firefox")
        .args(["--new-window", REELS_URL])
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open Firefox: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::{
        layout::{GridSpec, LogicalRect},
        platform::{ButtonState, LayoutPass, PlatformWindow, PointerButton, WindowHandle},
    };

    struct FakePlatform {
        windows: Arc<Mutex<Vec<PlatformWindow>>>,
        list_calls: Arc<Mutex<usize>>,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl Platform for FakePlatform {
        async fn list_windows(&self) -> Result<Vec<PlatformWindow>, String> {
            *self.list_calls.lock().unwrap() += 1;
            self.events.lock().unwrap().push("list".into());
            Ok(self.windows.lock().unwrap().clone())
        }

        async fn window_geometry(
            &self,
            _window: &WindowHandle,
        ) -> Result<Option<LogicalRect>, String> {
            unreachable!("window discovery does not request geometry")
        }

        async fn activate_window(&self, _window: &WindowHandle) -> Result<(), String> {
            unreachable!("window discovery does not activate windows")
        }

        async fn arrange_windows(
            &self,
            _windows: &[WindowHandle],
            _layout: GridSpec,
            _pass: LayoutPass,
        ) -> Result<(), String> {
            unreachable!("window discovery does not arrange windows")
        }

        async fn move_pointer(&self, _x: i32, _y: i32) -> Result<(), String> {
            unreachable!("window discovery does not move the pointer")
        }

        async fn scroll(&self, _horizontal: f64, _vertical: f64) -> Result<(), String> {
            unreachable!("window discovery does not scroll")
        }

        async fn pointer_button(
            &self,
            _button: PointerButton,
            _state: ButtonState,
        ) -> Result<(), String> {
            unreachable!("window discovery does not click")
        }
    }

    #[test]
    fn discovery_matches_firefox_app_ids_and_retains_metadata() {
        let adapter = FirefoxAdapter::default();
        assert!(adapter.identifies(Some("org.mozilla.firefox")));
        assert!(adapter.identifies(Some("Firefox")));
        assert!(!adapter.identifies(Some("org.mozilla.thunderbird")));
        assert!(!adapter.identifies(None));
        let metadata = adapter.metadata(Some("firefox".into()), "Instagram".into());
        assert_eq!(metadata.application_id.as_deref(), Some("firefox"));
        assert_eq!(metadata.title, "Instagram");
    }

    #[tokio::test]
    async fn no_open_selection_filters_and_preserves_platform_enumeration_order() {
        let list_calls = Arc::new(Mutex::new(0));
        let platform = FakePlatform {
            windows: Arc::new(Mutex::new(vec![
                PlatformWindow {
                    handle: WindowHandle::new("mail"),
                    app_id: Some("thunderbird".into()),
                    title: "Mail".into(),
                },
                PlatformWindow {
                    handle: WindowHandle::new("first-firefox"),
                    app_id: Some("org.mozilla.firefox".into()),
                    title: "Instagram".into(),
                },
                PlatformWindow {
                    handle: WindowHandle::new("second-firefox"),
                    app_id: Some("Firefox".into()),
                    title: "Other page".into(),
                },
            ])),
            list_calls: list_calls.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
        };
        let adapter = FirefoxAdapter::default();
        let selected = adapter.acquire_windows(&platform, 1, false).await.unwrap();
        assert_eq!(selected[0].handle.as_str(), "first-firefox");
        let selected = adapter.acquire_windows(&platform, 2, false).await.unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|window| window.handle.as_str())
                .collect::<Vec<_>>(),
            vec!["first-firefox", "second-firefox"]
        );
        assert_eq!(
            adapter
                .acquire_windows(&platform, 3, false)
                .await
                .unwrap_err(),
            "--no-open requested 3 Firefox windows, found 2"
        );
        assert_eq!(*list_calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn new_windows_are_snapshotted_launched_and_discovered_sequentially() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let windows = Arc::new(Mutex::new(vec![PlatformWindow {
            handle: WindowHandle::new("existing"),
            app_id: Some("firefox".into()),
            title: "Existing".into(),
        }]));
        let platform = FakePlatform {
            windows: windows.clone(),
            list_calls: Arc::new(Mutex::new(0)),
            events: events.clone(),
        };
        let launches = Arc::new(Mutex::new(0_usize));
        let adapter = FirefoxAdapter {
            launch: Arc::new({
                let events = events.clone();
                let windows = windows.clone();
                let launches = launches.clone();
                move || {
                    events.lock().unwrap().push("launch".into());
                    let mut launches = launches.lock().unwrap();
                    *launches += 1;
                    windows.lock().unwrap().push(PlatformWindow {
                        handle: WindowHandle::new(format!("opened-{}", *launches)),
                        app_id: Some("org.mozilla.firefox".into()),
                        title: "Instagram".into(),
                    });
                    Ok(())
                }
            }),
        };
        let selected = adapter.acquire_windows(&platform, 2, true).await.unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|window| window.handle.as_str())
                .collect::<Vec<_>>(),
            vec!["opened-1", "opened-2"]
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec!["list", "launch", "list", "list", "launch", "list"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn new_window_readiness_keeps_the_thirty_second_timeout() {
        let platform = FakePlatform {
            windows: Arc::new(Mutex::new(vec![PlatformWindow {
                handle: WindowHandle::new("existing"),
                app_id: Some("firefox".into()),
                title: "Existing".into(),
            }])),
            list_calls: Arc::new(Mutex::new(0)),
            events: Arc::new(Mutex::new(Vec::new())),
        };
        let adapter = FirefoxAdapter {
            launch: Arc::new(|| Ok(())),
        };
        let task = tokio::spawn(async move { adapter.acquire_windows(&platform, 1, true).await });
        tokio::task::yield_now().await;
        for _ in 0..DISCOVERY_ATTEMPTS {
            tokio::time::advance(DISCOVERY_INTERVAL).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            "Firefox did not create a new Wayland window in 30 seconds"
        );
    }
}
