use std::{env, fs, future::Future, path::PathBuf, time::Duration};

use tokio::{sync::watch, time::timeout};

use crate::{
    platform::kde_wayland::run_qdbus_command,
    stop::{StopNotifier, StopReason, StopSignal},
};

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
        eprintln!("Stop-key cleanup was skipped; KWin shortcut state may remain registered.");
        let _ = fs::remove_file(&self.script_path);
    }
}

impl StopKey {
    pub async fn cleanup(&mut self) -> Result<(), String> {
        if !self.cleaned {
            let result = cleanup_resources_with(
                &self._connection,
                &QdbusStopKeyCommands,
                self.script_path.clone(),
            )
            .await;
            self.cleaned = true;
            result?;
        }
        Ok(())
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
        let _ = run_qdbus_command(
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
            let script_id = run_qdbus_command(
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
            run_qdbus_command(
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
            return match guard.cleanup().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; stop-key cleanup also failed: {cleanup}")),
            };
        }
        Ok((guard, signal))
    }
}

#[allow(clippy::manual_async_fn)]
trait StopKeyCleanupCommands: Send + Sync {
    fn unload(&self) -> impl Future<Output = Result<(), String>> + Send + '_;
    fn unregister(&self) -> impl Future<Output = Result<(), String>> + Send + '_;
}

struct QdbusStopKeyCommands;

#[allow(clippy::manual_async_fn)]
impl StopKeyCleanupCommands for QdbusStopKeyCommands {
    fn unload(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
        async {
            run_qdbus_command(
                "qdbus6",
                &[
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.unloadScript",
                    PLUGIN,
                ],
            )
            .await
            .map(|_| ())
        }
    }

    fn unregister(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
        async {
            run_qdbus_command(
                "qdbus6",
                &[
                    "org.kde.kglobalaccel",
                    "/kglobalaccel",
                    "org.kde.KGlobalAccel.unregister",
                    "kwin",
                    ACTION,
                ],
            )
            .await
            .map(|_| ())
        }
    }
}

async fn cleanup_resources_with<K, C>(
    callback_owner: &K,
    commands: &C,
    script_path: PathBuf,
) -> Result<(), String>
where
    C: StopKeyCleanupCommands,
{
    let mut errors = Vec::new();
    if let Err(error) = commands.unload().await {
        errors.push(format!(
            "could not unload the KWin stop-key script; remote cleanup is unconfirmed: {error}"
        ));
    }
    if let Err(error) = commands.unregister().await {
        errors.push(format!(
            "could not unregister the KWin stop shortcut: {error}"
        ));
    }
    if let Err(error) = tokio::fs::remove_file(&script_path).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            errors.push(format!(
                "could not remove stop-key script {}: {error}",
                script_path.display()
            ));
        }
    }
    // Keep the owning StopKey's callback endpoint borrowed through both remote
    // cleanup requests so KWin can still call it while the script is removed.
    let _ = callback_owner;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    struct MockCleanupCommands {
        callback_live: Arc<AtomicBool>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    #[allow(clippy::manual_async_fn)]
    impl StopKeyCleanupCommands for MockCleanupCommands {
        fn unload(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
            async {
                assert!(self.callback_live.load(Ordering::SeqCst));
                self.events.lock().unwrap().push("unload");
                Err("simulated unload failure".into())
            }
        }

        fn unregister(&self) -> impl Future<Output = Result<(), String>> + Send + '_ {
            async {
                assert!(
                    self.callback_live.load(Ordering::SeqCst),
                    "callback owner must remain alive until after unregister"
                );
                self.events.lock().unwrap().push("unregister");
                Ok(())
            }
        }
    }

    async fn command(args: &[&str]) -> String {
        run_qdbus_command("qdbus6", args).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires a running KDE Wayland session"]
    async fn global_shortcut_calls_stop_and_unloads() {
        let (mut guard, mut signal) = StopKey::install().await.unwrap();
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
        guard.cleanup().await.unwrap();
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

    #[tokio::test]
    async fn unload_failure_still_unregisters_while_callback_owner_is_alive() {
        struct CallbackOwner(Arc<AtomicBool>);
        impl Drop for CallbackOwner {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }

        let callback_live = Arc::new(AtomicBool::new(true));
        let owner = CallbackOwner(Arc::clone(&callback_live));
        let events = Arc::new(Mutex::new(Vec::new()));
        let commands = MockCleanupCommands {
            callback_live: Arc::clone(&callback_live),
            events: Arc::clone(&events),
        };
        let script_path = env::temp_dir().join(format!(
            "ig_reels_stop_key_cleanup_{}.js",
            std::process::id()
        ));
        fs::write(&script_path, "test script").unwrap();

        let error = cleanup_resources_with(&owner, &commands, script_path.clone())
            .await
            .unwrap_err();

        assert!(error.contains("remote cleanup is unconfirmed"));
        assert!(error.contains("simulated unload failure"));
        assert_eq!(*events.lock().unwrap(), ["unload", "unregister"]);
        assert!(!script_path.exists());
        assert!(callback_live.load(Ordering::SeqCst));
        drop(owner);
        assert!(!callback_live.load(Ordering::SeqCst));
    }
}
