use std::{env, fs, path::PathBuf, time::Duration};

use tokio::{process::Command, sync::watch, time::timeout};

use crate::stop::{StopNotifier, StopReason, StopSignal};

const PLUGIN: &str = "ig_reels_stop_key";
const ACTION: &str = "IGReelsScrollStop";

struct StopBridge {
    stop: StopNotifier,
    readiness: watch::Sender<Option<bool>>,
}

#[zbus::interface(name = "org.ig.ReelsScroll")]
impl StopBridge {
    #[zbus(name = "ready")]
    fn ready(&self, registered: bool) {
        self.readiness.send_replace(Some(registered));
    }

    #[zbus(name = "stop")]
    fn stop(&self) {
        self.stop.request_stop(StopReason::EmergencyKey);
    }
}

/// Owns the KDE shortcut registration and its D-Bus callback endpoint.
pub struct StopKey {
    _connection: zbus::Connection,
    script_path: PathBuf,
    cleaned: bool,
}

impl Drop for StopKey {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let script_path = self.script_path.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(cleanup_resources(script_path));
        } else {
            let _ = fs::remove_file(&self.script_path);
        }
    }
}

impl StopKey {
    pub async fn cleanup(&mut self) {
        if !self.cleaned {
            cleanup_resources(self.script_path.clone()).await;
            self.cleaned = true;
        }
    }

    pub async fn install() -> Result<(Self, StopSignal), String> {
        let (stop, signal) = StopSignal::channel();
        let (readiness, mut ready) = watch::channel(None);
        let connection = zbus::connection::Builder::session()
            .map_err(|e| e.to_string())?
            .name("org.ig.ReelsScroll")
            .map_err(|e| e.to_string())?
            .serve_at("/Stop", StopBridge { stop, readiness })
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
        // Clear stale registration left by a process that was forcibly killed.
        let _ = run_command(
            "qdbus6",
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.unloadScript",
                PLUGIN,
            ],
        )
        .await;
        let script_path = env::temp_dir().join(format!("{PLUGIN}_{}.js", std::process::id()));
        tokio::fs::write(&script_path, script)
            .await
            .map_err(|e| e.to_string())?;
        let mut guard = Self {
            _connection: connection,
            script_path,
            cleaned: false,
        };
        let installed = async {
            let script_id = run_command(
                "qdbus6",
                &[
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.loadScript",
                    guard.script_path.to_str().ok_or("invalid script path")?,
                    PLUGIN,
                ],
            )
            .await?;
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
            )
            .await?;
            timeout(Duration::from_secs(3), ready.wait_for(Option::is_some))
                .await
                .map_err(|_| "KWin did not acknowledge the X shortcut")?
                .map_err(|_| "stop shortcut disconnected")?;
            if !ready.borrow().unwrap_or(false) {
                return Err("KWin could not register X as a stop shortcut".into());
            }
            // registerShortcut may succeed even when X is already assigned.
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
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = installed {
            guard.cleanup().await;
            return Err(error);
        }
        Ok((guard, signal))
    }
}

async fn cleanup_resources(script_path: PathBuf) {
    let _ = run_command(
        "qdbus6",
        &[
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting.unloadScript",
            PLUGIN,
        ],
    )
    .await;
    let _ = run_command(
        "qdbus6",
        &[
            "org.kde.kglobalaccel",
            "/kglobalaccel",
            "org.kde.KGlobalAccel.unregister",
            "kwin",
            ACTION,
        ],
    )
    .await;
    let _ = tokio::fs::remove_file(script_path).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn command(args: &[&str]) -> String {
        run_command("qdbus6", args).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires a running KDE Wayland session"]
    async fn global_shortcut_calls_stop_and_unloads() {
        let (guard, mut signal) = StopKey::install().await.unwrap();
        command(&[
            "org.kde.kglobalaccel",
            "/component/kwin",
            "org.kde.kglobalaccel.Component.invokeShortcut",
            ACTION,
        ])
        .await;
        assert_eq!(
            timeout(Duration::from_secs(3), signal.wait())
                .await
                .unwrap(),
            StopReason::EmergencyKey
        );
        drop(guard);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if command(&[
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.isScriptLoaded",
                    PLUGIN,
                ])
                .await
                    == "false"
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            command(&[
                "org.kde.kglobalaccel",
                "/kglobalaccel",
                "org.kde.KGlobalAccel.isGlobalShortcutAvailable",
                "88",
                ""
            ])
            .await,
            "true"
        );
    }
}
