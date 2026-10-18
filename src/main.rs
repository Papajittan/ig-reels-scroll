use std::{
    collections::HashSet,
    env, fs,
    process::{Command, ExitCode},
    time::Duration,
};

use wdotool_core::{
    backend::detector::{self, BackendKind, Environment},
    DynBackend, WindowGeometry, WindowId, WindowInfo,
};

const REELS_URL: &str = "https://www.instagram.com/reels/";

mod audio;
mod stop_key;

struct Config {
    windows: usize,
    count: u32,
    interval: Duration,
    open_browser: bool,
    stagger: u32,
    unmute: bool,
}

fn usage(program: &str) {
    eprintln!(
        "Usage: {program} [--windows N] [--count N] [--interval SECONDS] [--stagger N] [--no-open] [--unmute | --no-unmute]\n\
         KDE Plasma Wayland: tile N Firefox Reels windows and scroll each one.\n\
         Defaults: 1 window, 20 scroll passes, 3 seconds apart, 5 extra starting scrolls per window.\n\
         Press X anywhere or Ctrl-C in the terminal to stop. --stagger 0 disables starting offsets.\n\
         --unmute enables automatic unmuting; otherwise audio is left unchanged."
    );
}

fn parse_args() -> Result<Config, String> {
    let mut windows = 1_usize;
    let mut count = 20_u32;
    let mut interval_secs = 3.0_f64;
    let mut open_browser = true;
    let mut stagger = 5_u32;
    let mut unmute = false;
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "ig-reels-scroll".into());

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--windows" => {
                windows = args
                    .next()
                    .ok_or("--windows needs a number")?
                    .parse()
                    .map_err(|_| "--windows must be from 1 to 16")?;
                if !(1..=16).contains(&windows) {
                    return Err("--windows must be from 1 to 16".into());
                }
            }
            "--count" => {
                count = args
                    .next()
                    .ok_or("--count needs a number")?
                    .parse()
                    .map_err(|_| "--count must be a positive integer")?;
                if count == 0 {
                    return Err("--count must be greater than zero".into());
                }
            }
            "--interval" => {
                interval_secs = args
                    .next()
                    .ok_or("--interval needs seconds")?
                    .parse()
                    .map_err(|_| "--interval must be a number")?;
                if !interval_secs.is_finite() || interval_secs < 0.2 {
                    return Err("--interval must be at least 0.2 seconds".into());
                }
            }
            "--no-open" => open_browser = false,
            "--unmute" => unmute = true,
            "--no-unmute" => unmute = false,
            "--stagger" => {
                stagger = args
                    .next()
                    .ok_or("--stagger needs a number")?
                    .parse()
                    .map_err(|_| "--stagger must be from 0 to 100")?;
                if stagger > 100 {
                    return Err("--stagger must be from 0 to 100".into());
                }
            }
            "-h" | "--help" => {
                usage(&program);
                std::process::exit(0);
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }

    Ok(Config {
        windows,
        count,
        interval: Duration::from_secs_f64(interval_secs),
        open_browser,
        stagger,
        unmute,
    })
}

fn is_firefox(window: &WindowInfo) -> bool {
    window
        .app_id
        .as_deref()
        .unwrap_or_default()
        .to_lowercase()
        .contains("firefox")
}

async fn firefox_windows(backend: &DynBackend) -> Result<Vec<WindowInfo>, String> {
    backend
        .list_windows()
        .await
        .map(|windows| windows.into_iter().filter(is_firefox).collect())
        .map_err(|error| format!("KWin could not list Firefox windows: {error}"))
}

fn open_firefox_window() -> Result<(), String> {
    Command::new("firefox")
        .args(["--new-window", REELS_URL])
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open Firefox: {error}"))
}

async fn wait_for_new_window(
    backend: &DynBackend,
    existing: &HashSet<WindowId>,
) -> Result<WindowInfo, String> {
    for _ in 0..300 {
        if let Some(window) = firefox_windows(backend)
            .await?
            .into_iter()
            .find(|window| !existing.contains(&window.id))
        {
            return Ok(window);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("Firefox did not create a new Wayland window in 30 seconds".into())
}

fn tile_firefox_windows(ids: &[WindowId]) -> Result<(), String> {
    // Wayland clients acknowledge size changes asynchronously. Allow Firefox
    // to finish restoring its saved size before requesting the tile geometry.
    apply_window_layout(ids, true)?;
    apply_window_layout(ids, false)
}

fn apply_window_layout(ids: &[WindowId], restore_only: bool) -> Result<(), String> {
    let plugin = format!("ig_reels_scroll_{}", std::process::id());
    let script_path = env::temp_dir().join(format!("{plugin}.js"));
    let target_ids = ids
        .iter()
        .map(|id| format!("{:?}", id.0))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        r#"
const count = {count};
const restoreOnly = {restore_only};
const targetIds = new Set([{target_ids}]);
const targets = workspace.windowList().filter(w =>
    targetIds.has((w.internalId || w.windowId || "").toString())
);
if (targets.length === {count}) {{
    const area = workspace.clientArea(KWin.MaximizeArea, targets[0]);
    const columns = Math.ceil(Math.sqrt(count));
    const rows = Math.ceil(count / columns);
    targets.forEach((window, index) => {{
        if (restoreOnly) {{
            window.minimized = false;
            window.fullScreen = false;
            window.setMaximize(false, false);
            return;
        }}
        const column = index % columns;
        const row = Math.floor(index / columns);
        const x = area.x + Math.floor(column * area.width / columns);
        const y = area.y + Math.floor(row * area.height / rows);
        const width = Math.floor((column + 1) * area.width / columns) - Math.floor(column * area.width / columns);
        const height = Math.floor((row + 1) * area.height / rows) - Math.floor(row * area.height / rows);
        window.frameGeometry = {{ x, y, width, height }};
    }});
}}
"#,
        count = ids.len()
    );
    fs::write(&script_path, script)
        .map_err(|error| format!("could not write temporary KWin script: {error}"))?;

    let result = (|| {
        let path = script_path
            .to_str()
            .ok_or("invalid temporary script path")?;
        let script_id = run_command(
            "qdbus6",
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.loadScript",
                path,
                &plugin,
            ],
        )?;
        let script_object = format!("/Scripting/Script{}", script_id.trim());
        run_command(
            "qdbus6",
            &["org.kde.KWin", &script_object, "org.kde.kwin.Script.run"],
        )?;
        std::thread::sleep(Duration::from_millis(500));
        let _ = run_command(
            "qdbus6",
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.unloadScript",
                &plugin,
            ],
        );
        Ok(())
    })();
    let _ = fs::remove_file(script_path);
    result
}

fn run_command(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
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

async fn window_geometry(backend: &DynBackend, id: &WindowId) -> Result<WindowGeometry, String> {
    backend
        .window_geometry(id)
        .await
        .map_err(|error| format!("could not read Firefox window geometry: {error}"))?
        .ok_or_else(|| "KWin did not provide Firefox window geometry".into())
}

async fn confirm_tiling(backend: &DynBackend, ids: &[WindowId]) -> Result<(), String> {
    for attempt in 0..20 {
        let mut geometries = Vec::new();
        for id in ids {
            geometries.push(window_geometry(backend, id).await?);
        }
        let mut overlaps = false;
        for (index, a) in geometries.iter().enumerate() {
            for b in &geometries[index + 1..] {
                overlaps |= (a.x as i64) < b.x as i64 + b.width as i64
                    && (b.x as i64) < a.x as i64 + a.width as i64
                    && (a.y as i64) < b.y as i64 + b.height as i64
                    && (b.y as i64) < a.y as i64 + a.height as i64;
            }
        }
        if !overlaps && geometries.iter().all(|g| g.width > 0 && g.height > 0) {
            for (index, g) in geometries.iter().enumerate() {
                println!(
                    "Window {}: {}x{} at {},{}",
                    index + 1,
                    g.width,
                    g.height,
                    g.x,
                    g.y
                );
            }
            return Ok(());
        }
        if attempt % 5 == 4 {
            // A late Firefox restore response may have replaced our first
            // resize. Reapply geometry without changing maximize state again.
            apply_window_layout(ids, false)?;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("KWin did not tile the Firefox windows: they still overlap. Scrolling stopped.".into())
}

async fn scroll_window(
    backend: &DynBackend,
    id: &WindowId,
    audio: Option<&audio::AudioControl>,
) -> Result<(), String> {
    backend
        .activate_window(id)
        .await
        .map_err(|error| format!("could not focus Firefox window: {error}"))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let geometry = window_geometry(backend, id).await?;
    backend
        .mouse_move(
            geometry.x + geometry.width as i32 / 2,
            geometry.y + geometry.height as i32 / 2,
            true,
        )
        .await
        .map_err(|error| format!("could not move the pointer: {error}"))?;
    backend
        // libei uses logical pixels, not mouse-wheel click counts. Three
        // pixels barely moves the feed; use approximately one content viewport.
        .scroll(0.0, (geometry.height as f64 - 160.0).max(240.0))
        .await
        .map_err(|error| format!("could not scroll Reels: {error}"))?;
    if let Some(audio) = audio {
        tokio::time::sleep(Duration::from_millis(350)).await;
        audio.unmute(backend, &id.0, &geometry).await;
    }
    Ok(())
}

async fn run() -> Result<(), String> {
    let config = parse_args()?;
    let environment = Environment::detect();
    if !environment.is_wayland() || !environment.desktop_is("KDE") {
        return Err("This version is for KDE Plasma on Wayland.".into());
    }

    let (_stop_key, mut stop) = stop_key::StopKey::install().await?;
    println!("Global X stop key is ready. Ctrl-C also stops the script.");
    let audio = if config.unmute {
        Some(
            audio::AudioControl::new()
                .await
                .map_err(|e| format!("could not initialize auto-unmute: {e}"))?,
        )
    } else {
        None
    };
    let result = tokio::select! {
        biased;
        _ = stop.wait_for(|value| *value == 3) => {
            println!("Stopped by X.");
            Ok(())
        }
        result = tokio::signal::ctrl_c() => {
            println!("Stopped by Ctrl-C.");
            result.map_err(|e| format!("could not listen for Ctrl-C: {e}"))
        }
        result = scroll_reels(config, environment, audio.as_ref()) => result,
    };
    if let Some(audio) = audio {
        if let Err(error) = audio.restore().await {
            eprintln!("Could not restore desktop accessibility setting: {error}");
        }
    }
    result
}

async fn scroll_reels(
    config: Config,
    environment: Environment,
    audio: Option<&audio::AudioControl>,
) -> Result<(), String> {
    // Keep one portal connection alive for the whole run. Spawning a separate
    // wdotool process for every mouse action starts a new Remote Desktop session
    // each time, which produces a stack of KDE Portal Integration notifications.
    let backend = detector::build(&environment, Some(BackendKind::KdeDBus))
        .await
        .map_err(|error| format!("could not start one KDE input session: {error}"))?;

    let ids: Vec<WindowId> = if config.open_browser {
        println!("Opening {} Firefox Reels window(s)...", config.windows);
        let mut opened = Vec::new();
        for _ in 0..config.windows {
            let existing: HashSet<WindowId> = firefox_windows(&backend)
                .await?
                .into_iter()
                .map(|window| window.id)
                .collect();
            open_firefox_window()?;
            opened.push(wait_for_new_window(&backend, &existing).await?.id);
        }
        println!("Tiling the Reels windows to fit the screen...");
        tile_firefox_windows(&opened)?;
        opened
    } else {
        let windows = firefox_windows(&backend).await?;
        if windows.len() < config.windows {
            return Err(format!(
                "--no-open requested {} Firefox windows, found {}",
                config.windows,
                windows.len()
            ));
        }
        let ids: Vec<WindowId> = windows
            .into_iter()
            .take(config.windows)
            .map(|window| window.id)
            .collect();
        tile_firefox_windows(&ids)?;
        ids
    };

    confirm_tiling(&backend, &ids).await?;
    println!(
        "Sign in and dismiss any Instagram popups. Scrolling begins in 8 seconds. Press X to stop."
    );
    tokio::time::sleep(Duration::from_secs(8)).await;

    if let Some(audio) = audio {
        println!("Auto-unmute enabled for all selected windows.");
        for id in &ids {
            backend
                .activate_window(id)
                .await
                .map_err(|e| e.to_string())?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let geometry = window_geometry(&backend, id).await?;
            audio.unmute(&backend, &id.0, &geometry).await;
        }
    }

    for (index, id) in ids.iter().enumerate().skip(1) {
        let offset = starting_offset(index, config.stagger);
        if offset > 0 {
            println!(
                "Moving window {} ahead by {offset} scroll steps...",
                index + 1
            );
            for _ in 0..offset {
                scroll_window(&backend, id, audio).await?;
                // Allow Instagram's reel transition to finish between inputs.
                tokio::time::sleep(Duration::from_millis(900)).await;
            }
        }
    }

    for step in 1..=config.count {
        for id in &ids {
            scroll_window(&backend, id, audio).await?;
        }
        println!(
            "Scroll pass {step}/{} across {} window(s)",
            config.count,
            ids.len()
        );
        if step < config.count {
            tokio::time::sleep(config.interval).await;
        }
    }
    println!("Done.");
    Ok(())
}

fn starting_offset(index: usize, stagger: u32) -> u32 {
    index as u32 * stagger
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_offsets_separate_windows_and_allow_opt_out() {
        assert_eq!(
            (0..4).map(|i| starting_offset(i, 5)).collect::<Vec<_>>(),
            vec![0, 5, 10, 15]
        );
        assert_eq!(starting_offset(15, 0), 0);
    }
}
