use std::{
    collections::{HashSet, VecDeque},
    sync::Mutex,
    time::Duration,
};

use wdotool_core::{DynBackend, KeyDirection, MouseButton, WindowGeometry};
use zbus::{zvariant::OwnedObjectPath, Connection, Proxy};

type Node = (String, OwnedObjectPath);
type Rect = (i32, i32, i32, i32);
type VolumeIcon = (Node, Rect);
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";

/// Optional, state-aware activation of Instagram's visible unmute button.
pub struct AudioControl {
    session: Connection,
    bus: Connection,
    restore_disabled: bool,
    warned: Mutex<HashSet<String>>,
}

impl AudioControl {
    pub async fn new() -> Result<Self, String> {
        let session = Connection::session().await.map_err(|e| e.to_string())?;
        let address: String = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .await
            .map_err(|e| e.to_string())?
            .call("GetAddress", &())
            .await
            .map_err(|e| e.to_string())?;
        let bus = zbus::connection::Builder::address(address.as_str())
            .map_err(|e| e.to_string())?
            .build()
            .await
            .map_err(|e| e.to_string())?;
        let status = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status")
            .await
            .map_err(|e| e.to_string())?;
        let enabled: bool = status
            .get_property("IsEnabled")
            .await
            .map_err(|e| e.to_string())?;
        if !enabled {
            status
                .set_property("IsEnabled", true)
                .await
                .map_err(|e| e.to_string())?;
        }
        drop(status);
        Ok(Self {
            session,
            bus,
            restore_disabled: !enabled,
            warned: Mutex::new(HashSet::new()),
        })
    }

    /// Restore the desktop accessibility setting after normal exit, X, or Ctrl-C.
    pub async fn restore(&self) -> Result<(), String> {
        if self.restore_disabled {
            Proxy::new(
                &self.session,
                "org.a11y.Bus",
                "/org/a11y/bus",
                "org.a11y.Status",
            )
            .await
            .map_err(|e| e.to_string())?
            .set_property("IsEnabled", false)
            .await
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn accessible<'a>(&'a self, node: &'a Node) -> Result<Proxy<'a>, String> {
        Proxy::new(&self.bus, node.0.as_str(), node.1.as_str(), ACCESSIBLE)
            .await
            .map_err(|e| e.to_string())
    }

    async fn children(&self, node: &Node) -> Result<Vec<Node>, String> {
        self.accessible(node)
            .await?
            .call("GetChildren", &())
            .await
            .map_err(|e| e.to_string())
    }

    async fn active_frame(&self) -> Result<Node, String> {
        let root = (
            "org.a11y.atspi.Registry".into(),
            OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root").unwrap(),
        );
        for app in self.children(&root).await? {
            let proxy = self.accessible(&app).await?;
            let name: String = proxy.get_property("Name").await.unwrap_or_default();
            if !name.to_lowercase().contains("firefox") {
                continue;
            }
            for frame in self.children(&app).await? {
                let proxy = self.accessible(&frame).await?;
                let name: String = proxy.get_property("Name").await.unwrap_or_default();
                let states: Vec<u32> = proxy.call("GetState", &()).await.unwrap_or_default();
                if name.to_lowercase().contains("instagram") && has_state(&states, 1) {
                    return Ok(frame);
                }
            }
        }
        Err("no active Instagram window in Firefox's accessibility tree".into())
    }

    async fn ensure_unmuted(
        &self,
        backend: &DynBackend,
        bounds: &WindowGeometry,
    ) -> Result<(), String> {
        let frame = self.active_frame().await?;
        if let Some((slider, (x, y, width, height))) = self.volume_icon(&frame, bounds).await? {
            if !self.volume_is_zero(&slider).await? {
                return Ok(());
            }
            // Firefox exposes Instagram's speaker icon as a tiny volume slider.
            // Read its state, but send an ordinary mouse click: changing the
            // native accessibility Value property can crash Firefox.
            backend
                .mouse_move(x + width / 2, y + height / 2, true)
                .await
                .map_err(|e| e.to_string())?;
            tokio::time::sleep(Duration::from_millis(80)).await;
            backend
                .mouse_button(MouseButton::Left, KeyDirection::Press)
                .await
                .map_err(|e| e.to_string())?;
            backend
                .mouse_button(MouseButton::Left, KeyDirection::Release)
                .await
                .map_err(|e| e.to_string())?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            if self.volume_is_zero(&slider).await? {
                return Err("the speaker was clicked but still reports muted audio".into());
            }
            println!("Reel speaker unmuted.");
            return Ok(());
        }
        self.unmute_in_frame(frame).await
    }

    async fn volume_is_zero(&self, node: &Node) -> Result<bool, String> {
        let value = Proxy::new(
            &self.bus,
            node.0.as_str(),
            node.1.as_str(),
            "org.a11y.atspi.Value",
        )
        .await
        .map_err(|e| e.to_string())?;
        let current: f64 = value
            .get_property("CurrentValue")
            .await
            .map_err(|e| e.to_string())?;
        let minimum: f64 = value
            .get_property("MinimumValue")
            .await
            .map_err(|e| e.to_string())?;
        if !current.is_finite() || !minimum.is_finite() {
            return Err("the speaker returned an invalid volume".into());
        }
        Ok(current <= minimum)
    }

    async fn volume_icon(
        &self,
        frame: &Node,
        bounds: &WindowGeometry,
    ) -> Result<Option<VolumeIcon>, String> {
        let frame_component = Proxy::new(
            &self.bus,
            frame.0.as_str(),
            frame.1.as_str(),
            "org.a11y.atspi.Component",
        )
        .await
        .map_err(|e| e.to_string())?;
        let (frame_rect,): (Rect,) = frame_component
            .call("GetExtents", &(0_u32,))
            .await
            .map_err(|e| e.to_string())?;
        let mut queue = VecDeque::from([frame.clone()]);
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        for _ in 0..6000 {
            let Some(node) = queue.pop_front() else {
                break;
            };
            if !seen.insert(node.clone()) {
                continue;
            }
            let proxy = self.accessible(&node).await?;
            let role: String = proxy.call("GetRoleName", &()).await.unwrap_or_default();
            // Firefox exposes hidden browser menus and background documents in
            // the same tree. Their controls must never become click targets.
            if matches!(role.as_str(), "menu" | "document web") {
                let states: Vec<u32> = proxy.call("GetState", &()).await.unwrap_or_default();
                if !has_state(&states, 25) {
                    continue;
                }
            }
            if role == "slider" {
                let name: String = proxy.get_property("Name").await.unwrap_or_default();
                let states: Vec<u32> = proxy.call("GetState", &()).await.unwrap_or_default();
                if name.eq_ignore_ascii_case("Adjust volume")
                    && has_state(&states, 25)
                    && has_state(&states, 30)
                {
                    let component = Proxy::new(
                        &self.bus,
                        node.0.as_str(),
                        node.1.as_str(),
                        "org.a11y.atspi.Component",
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    let (local,): (Rect,) = component
                        .call("GetExtents", &(0_u32,))
                        .await
                        .map_err(|e| e.to_string())?;
                    if let Some(rect) = screen_icon_rect(local, frame_rect, bounds) {
                        found.push((node.clone(), rect));
                    }
                }
            }
            if let Ok(children) = self.children(&node).await {
                queue.extend(children);
            }
        }
        if !queue.is_empty() {
            return Err("Firefox's accessibility tree is too large to inspect safely".into());
        }
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => Err("multiple visible speaker icons; skipping an ambiguous click".into()),
        }
    }

    async fn unmute_in_frame(&self, frame: Node) -> Result<(), String> {
        let mut queue = VecDeque::from([(frame, None::<Node>)]);
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            let Some((node, ancestor_button)) = queue.pop_front() else {
                break;
            };
            if !seen.insert(node.clone()) {
                continue;
            }
            let proxy = self.accessible(&node).await?;
            let role: String = proxy.call("GetRoleName", &()).await.unwrap_or_default();
            let button = if matches!(role.as_str(), "push button" | "toggle button" | "button") {
                Some(node.clone())
            } else {
                ancestor_button
            };
            let name: String = proxy.get_property("Name").await.unwrap_or_default();
            if let Some(muted) = audio_label(&name) {
                let states: Vec<u32> = proxy.call("GetState", &()).await.unwrap_or_default();
                if has_state(&states, 25) && has_state(&states, 30) {
                    if let Some(ref action) = button {
                        if !candidates.iter().any(|(existing, _)| existing == action) {
                            candidates.push((action.clone(), muted));
                        }
                    }
                }
            }
            if let Ok(children) = self.children(&node).await {
                queue.extend(children.into_iter().map(|child| (child, button.clone())));
            }
        }
        if !queue.is_empty() {
            return Err("Firefox's accessibility tree is too large to inspect safely".into());
        }
        match candidates.as_slice() {
            [(_, false)] => Ok(()), // Already audible: never toggle it off.
            [(node, true)] => {
                let action = Proxy::new(
                    &self.bus,
                    node.0.as_str(),
                    node.1.as_str(),
                    "org.a11y.atspi.Action",
                )
                .await
                .map_err(|e| e.to_string())?;
                let activated: bool = action
                    .call("DoAction", &(0_i32,))
                    .await
                    .map_err(|e| e.to_string())?;
                if activated {
                    Ok(())
                } else {
                    Err("Instagram's unmute button could not be activated".into())
                }
            }
            [] => Err(
                "no visible audio control found (English Instagram labels are currently required)"
                    .into(),
            ),
            _ => Err("multiple visible audio controls; skipping an ambiguous unmute".into()),
        }
    }

    pub async fn unmute(&self, backend: &DynBackend, window: &str, bounds: &WindowGeometry) {
        let result =
            tokio::time::timeout(Duration::from_secs(5), self.ensure_unmuted(backend, bounds))
                .await
                .unwrap_or_else(|_| Err("Firefox's audio control lookup timed out".into()));
        if let Err(error) = result {
            if self.warned.lock().unwrap().insert(window.to_owned()) {
                eprintln!(
                    "Auto-unmute skipped for window {window}: {error}. Scrolling will continue."
                );
            }
        }
    }
}

fn window_icon_rect((x, y, width, height): Rect, bounds: &WindowGeometry) -> Option<Rect> {
    // Translate a validated window-relative icon into compositor coordinates.
    let valid = (1..=96).contains(&width)
        && (1..=96).contains(&height)
        && x >= 0
        && y >= 0
        && x as i64 + width as i64 <= bounds.width as i64
        && y as i64 + height as i64 <= bounds.height as i64;
    if valid {
        Some((
            bounds.x.saturating_add(x),
            bounds.y.saturating_add(y),
            width,
            height,
        ))
    } else {
        None
    }
}

fn screen_icon_rect(
    (x, y, width, height): Rect,
    (frame_x, frame_y, _, _): Rect,
    bounds: &WindowGeometry,
) -> Option<Rect> {
    // Firefox retains its own window origin on Wayland. Normalize within that
    // coordinate system, then translate to KWin's actual compositor position.
    window_icon_rect(
        (
            x.saturating_sub(frame_x),
            y.saturating_sub(frame_y),
            width,
            height,
        ),
        bounds,
    )
}

fn has_state(states: &[u32], state: usize) -> bool {
    states
        .get(state / 32)
        .is_some_and(|word| word & (1 << (state % 32)) != 0)
}

fn audio_label(name: &str) -> Option<bool> {
    match name.trim().to_lowercase().as_str() {
        "unmute" | "unmute audio" | "unmute video" | "turn audio on" | "turn sound on"
        | "audio is muted" => Some(true),
        "mute" | "mute audio" | "mute video" | "turn audio off" | "turn sound off"
        | "audio is playing" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "physically clicks the last open Firefox Instagram window; requires KDE Wayland"]
    async fn live_reels_speaker_unmutes_with_pointer_input() {
        use wdotool_core::backend::detector::{self, BackendKind, Environment};
        let environment = Environment::detect();
        let backend = detector::build(&environment, Some(BackendKind::KdeDBus))
            .await
            .unwrap();
        let window = backend
            .list_windows()
            .await
            .unwrap()
            .into_iter()
            .rfind(|w| {
                w.app_id
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("firefox")
                    && w.title.to_lowercase().contains("instagram")
            })
            .expect("open a Reels page before running this test");
        let audio = AudioControl::new().await.unwrap();
        backend.activate_window(&window.id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let geometry = backend.window_geometry(&window.id).await.unwrap().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            audio.ensure_unmuted(&backend, &geometry),
        )
        .await;
        audio.restore().await.unwrap();
        assert!(result.is_ok(), "speaker lookup timed out");
        result.unwrap().unwrap();
    }

    #[test]
    fn speaker_coordinates_include_kde_panel_and_tiled_window_origin() {
        let bounds = WindowGeometry {
            x: 0,
            y: 36,
            width: 1920,
            height: 1044,
        };
        assert_eq!(
            window_icon_rect((1175, 984, 28, 28), &bounds),
            Some((1175, 1020, 28, 28))
        );
        let tiled = WindowGeometry {
            x: 960,
            y: 36,
            width: 960,
            height: 1044,
        };
        assert_eq!(
            window_icon_rect((800, 900, 28, 28), &tiled),
            Some((1760, 936, 28, 28))
        );
        assert_eq!(window_icon_rect((800, 1942, 28, 28), &tiled), None);
        assert_eq!(window_icon_rect((-1, -1, -1, -1), &tiled), None);
        assert_eq!(window_icon_rect((0, 0, 518, 926), &bounds), None);
        // Regression: Firefox retains (45,45) while KWin moves the window
        // to (0,36). The speaker center must land at (1189,1034).
        assert_eq!(
            screen_icon_rect((1220, 1029, 28, 28), (45, 45, 1920, 1044), &bounds),
            Some((1175, 1020, 28, 28))
        );
        assert_eq!(
            screen_icon_rect((845, 945, 28, 28), (45, 45, 960, 1044), &tiled),
            Some((1760, 936, 28, 28))
        );
    }

    struct VolumeFixture(Arc<AtomicBool>);

    #[zbus::interface(name = "org.a11y.atspi.Value")]
    impl VolumeFixture {
        #[zbus(property)]
        fn current_value(&self) -> f64 {
            if self.0.load(Ordering::SeqCst) {
                0.0
            } else {
                25.0
            }
        }
        #[zbus(property)]
        fn minimum_value(&self) -> f64 {
            0.0
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn volume_state_is_read_without_setting_native_accessibility_values() {
        let server = Connection::session().await.unwrap();
        let node = (
            server.unique_name().unwrap().to_string(),
            OwnedObjectPath::try_from("/test/volume").unwrap(),
        );
        let muted = Arc::new(AtomicBool::new(true));
        server
            .object_server()
            .at(node.1.clone(), VolumeFixture(muted.clone()))
            .await
            .unwrap();
        let client = Connection::session().await.unwrap();
        let audio = AudioControl {
            session: client.clone(),
            bus: client,
            restore_disabled: false,
            warned: Mutex::new(HashSet::new()),
        };
        assert!(audio.volume_is_zero(&node).await.unwrap());
        muted.store(false, Ordering::SeqCst);
        assert!(!audio.volume_is_zero(&node).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires a live desktop accessibility bus"]
    async fn desktop_accessibility_setting_is_restored() {
        let session = Connection::session().await.unwrap();
        let status = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status")
            .await
            .unwrap();
        let before: bool = status.get_property("IsEnabled").await.unwrap();
        let audio = AudioControl::new().await.unwrap();
        let enabled: bool = status.get_property("IsEnabled").await.unwrap();
        audio.restore().await.unwrap();
        let after: bool = status.get_property("IsEnabled").await.unwrap();
        assert!(enabled);
        assert_eq!(after, before);
    }

    struct AccessibleFixture {
        frame: bool,
        muted: Arc<AtomicBool>,
        showing: Arc<AtomicBool>,
        children: Vec<Node>,
    }

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl AccessibleFixture {
        #[zbus(property)]
        fn name(&self) -> &str {
            if self.frame {
                "Instagram"
            } else if self.muted.load(Ordering::SeqCst) {
                "Unmute"
            } else {
                "Mute"
            }
        }

        #[zbus(name = "GetChildren")]
        fn get_children(&self) -> Vec<Node> {
            self.children.clone()
        }

        #[zbus(name = "GetRoleName")]
        fn get_role_name(&self) -> &str {
            if self.frame {
                "frame"
            } else {
                "push button"
            }
        }

        #[zbus(name = "GetState")]
        fn get_state(&self) -> Vec<u32> {
            vec![
                (1 << 30)
                    | if self.showing.load(Ordering::SeqCst) {
                        1 << 25
                    } else {
                        0
                    },
                0,
            ]
        }
    }

    struct ActionFixture {
        muted: Arc<AtomicBool>,
        clicks: Arc<AtomicUsize>,
    }

    #[zbus::interface(name = "org.a11y.atspi.Action")]
    impl ActionFixture {
        #[zbus(name = "DoAction")]
        fn do_action(&self, index: i32) -> bool {
            assert_eq!(index, 0);
            self.muted.fetch_xor(true, Ordering::SeqCst);
            self.clicks.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unmute_is_idempotent_and_ignores_hidden_controls_over_dbus() {
        let bus = Connection::session().await.unwrap();
        let address = bus.unique_name().unwrap().to_string();
        let frame = (
            address.clone(),
            OwnedObjectPath::try_from("/test/frame").unwrap(),
        );
        let button = (address, OwnedObjectPath::try_from("/test/button").unwrap());
        let muted = Arc::new(AtomicBool::new(true));
        let showing = Arc::new(AtomicBool::new(true));
        let clicks = Arc::new(AtomicUsize::new(0));
        for (node, is_frame, children) in [
            (&frame, true, vec![button.clone()]),
            (&button, false, vec![]),
        ] {
            bus.object_server()
                .at(
                    node.1.clone(),
                    AccessibleFixture {
                        frame: is_frame,
                        muted: muted.clone(),
                        showing: showing.clone(),
                        children,
                    },
                )
                .await
                .unwrap();
        }
        bus.object_server()
            .at(
                button.1.clone(),
                ActionFixture {
                    muted: muted.clone(),
                    clicks: clicks.clone(),
                },
            )
            .await
            .unwrap();
        let client = Connection::session().await.unwrap();
        let audio = AudioControl {
            session: client.clone(),
            bus: client,
            restore_disabled: false,
            warned: Mutex::new(HashSet::new()),
        };
        tokio::time::timeout(Duration::from_secs(5), audio.unmute_in_frame(frame.clone()))
            .await
            .unwrap()
            .unwrap();
        assert!(!muted.load(Ordering::SeqCst));
        audio.unmute_in_frame(frame.clone()).await.unwrap();
        assert_eq!(
            clicks.load(Ordering::SeqCst),
            1,
            "an already unmuted reel must not be toggled again"
        );
        showing.store(false, Ordering::SeqCst);
        muted.store(true, Ordering::SeqCst);
        assert!(audio.unmute_in_frame(frame).await.is_err());
        assert_eq!(
            clicks.load(Ordering::SeqCst),
            1,
            "hidden audio controls must not be activated"
        );
    }

    #[test]
    fn distinguish_unmute_actions_from_already_audible_controls() {
        assert_eq!(audio_label("Audio is muted"), Some(true));
        assert_eq!(audio_label("Turn audio on"), Some(true));
        assert_eq!(audio_label(" Unmute "), Some(true));
        assert_eq!(audio_label("Mute"), Some(false));
        assert_eq!(audio_label("Audio is playing"), Some(false));
        assert_eq!(audio_label("Unmute notifications"), None);
        assert_eq!(audio_label("Play"), None);
    }
}
