use std::{env, fs, path::PathBuf, time::Duration};

use tokio::sync::watch;

use crate::run_command;

const PLUGIN: &str = "ig_reels_stop_key";
const ACTION: &str = "IGReelsScrollStop";

struct StopBridge {
    events: watch::Sender<u8>,
}

#[zbus::interface(name = "org.ig.ReelsScroll")]
impl StopBridge {
    #[zbus(name = "ready")]
    fn ready(&self, registered: bool) {
        self.events.send_replace(if registered { 1 } else { 2 });
    }

    #[zbus(name = "stop")]
    fn stop(&self) {
        self.events.send_replace(3);
    }
}

/// Owns both the D-Bus receiver and KWin shortcut for this process's lifetime.
pub struct StopKey {
    _connection: zbus::Connection,
    script_path: PathBuf,
}

impl Drop for StopKey {
    fn drop(&mut self) {
        let _ = run_command(
            "qdbus6",
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.unloadScript",
                PLUGIN,
            ],
        );
        let _ = run_command(
            "qdbus6",
            &[
                "org.kde.kglobalaccel",
                "/kglobalaccel",
                "org.kde.KGlobalAccel.unregister",
                "kwin",
                ACTION,
            ],
        );
        let _ = fs::remove_file(&self.script_path);
    }
}

impl StopKey {
    pub async fn install() -> Result<(Self, watch::Receiver<u8>), String> {
        let (events, mut receiver) = watch::channel(0);
        let connection = zbus::connection::Builder::session()
            .map_err(|e| e.to_string())?
            .name("org.ig.ReelsScroll")
            .map_err(|e| e.to_string())?
            .serve_at("/Stop", StopBridge { events })
            .map_err(|e| e.to_string())?
            .build()
            .await
            .map_err(|e| {
                format!("could not install stop key (another scroller may be running): {e}")
            })?;
        let address = connection.unique_name().ok_or("missing D-Bus address")?;
        let script = format!(
            r#"
const registered = registerShortcut("{ACTION}", "Stop Instagram Reels scroller", "X", function () {{
    callDBus("{address}", "/Stop", "org.ig.ReelsScroll", "stop");
}});
callDBus("{address}", "/Stop", "org.ig.ReelsScroll", "ready", registered);
"#
        );
        // Remove a stale script left by a forcibly killed earlier process.
        let _ = run_command(
            "qdbus6",
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.unloadScript",
                PLUGIN,
            ],
        );
        let script_path = env::temp_dir().join(format!("{PLUGIN}_{}.js", std::process::id()));
        fs::write(&script_path, script).map_err(|e| e.to_string())?;
        let guard = Self {
            _connection: connection,
            script_path,
        };
        let script_id = run_command(
            "qdbus6",
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.loadScript",
                guard.script_path.to_str().ok_or("invalid script path")?,
                PLUGIN,
            ],
        )?;
        let script_id: u32 = script_id
            .parse()
            .map_err(|_| "KWin could not load the stop shortcut")?;
        run_command(
            "qdbus6",
            &[
                "org.kde.KWin",
                &format!("/Scripting/Script{script_id}"),
                "org.kde.kwin.Script.run",
            ],
        )?;
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .map_err(|_| "KWin did not acknowledge the X shortcut")?
            .map_err(|_| "stop shortcut disconnected")?;
        if *receiver.borrow() != 1 {
            return Err("KWin could not register X as a stop shortcut".into());
        }
        // registerShortcut may succeed even when the key is already assigned.
        let proxy = zbus::Proxy::new(
            &guard._connection,
            "org.kde.kglobalaccel",
            "/kglobalaccel",
            "org.kde.KGlobalAccel",
        )
        .await
        .map_err(|e| e.to_string())?;
        let action = vec!["kwin", ACTION, "KWin", "Stop Instagram Reels scroller"];
        let keys: Vec<i32> = proxy
            .call("shortcut", &(action,))
            .await
            .map_err(|e| e.to_string())?;
        if !keys.contains(&88) {
            return Err("X is unavailable as a global shortcut; check KDE System Settings → Keyboard → Shortcuts → KWin".into());
        }
        Ok((guard, receiver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a running KDE Wayland session"]
    async fn global_shortcut_calls_stop_and_unloads() {
        let (guard, mut receiver) = StopKey::install().await.unwrap();
        run_command(
            "qdbus6",
            &[
                "org.kde.kglobalaccel",
                "/component/kwin",
                "org.kde.kglobalaccel.Component.invokeShortcut",
                ACTION,
            ],
        )
        .unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            receiver.wait_for(|value| *value == 3),
        )
        .await
        .unwrap()
        .unwrap();
        drop(guard);
        assert_eq!(
            run_command(
                "qdbus6",
                &[
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.isScriptLoaded",
                    PLUGIN
                ]
            )
            .unwrap(),
            "false"
        );
        assert_eq!(
            run_command(
                "qdbus6",
                &[
                    "org.kde.kglobalaccel",
                    "/kglobalaccel",
                    "org.kde.KGlobalAccel.isGlobalShortcutAvailable",
                    "88",
                    ""
                ]
            )
            .unwrap(),
            "true"
        );
    }
}
