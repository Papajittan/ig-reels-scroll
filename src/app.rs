use std::{env, future::Future, process::ExitCode, time::Duration};

use tokio::time::timeout;

use crate::{
    audio::AudioMode,
    browser::firefox::FirefoxAdapter,
    cli::{parse_args, Config, ParseOutcome},
    engine,
    platform::kde_wayland::{KdeCleanupQueue, KdeEnvironment, KdeWaylandPlatform},
    stop::StopReason,
    stop_key::StopKey,
};

const ACCESSIBILITY_RESTORE_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run() -> ExitCode {
    let config = match parse_args(env::args()) {
        Ok(ParseOutcome::Run(config)) => config,
        Ok(ParseOutcome::Help(help)) => {
            eprintln!("{help}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match run_config(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_config(config: Config) -> Result<(), String> {
    let environment = KdeEnvironment::detect()?;

    let (mut stop_key, mut stop) = StopKey::install().await?;
    println!("Global X stop key is ready. Ctrl-C also stops the script.");
    let audio = match AudioMode::initialize(config.unmute).await {
        Ok(audio) => audio,
        Err(error) => {
            return match stop_key.cleanup().await {
                Ok(()) => Err(format!("could not initialize auto-unmute: {error}")),
                Err(cleanup) => Err(format!(
                    "could not initialize auto-unmute: {error}; stop-key cleanup also failed: {cleanup}"
                )),
            };
        }
    };
    let cleanup = KdeCleanupQueue::default();
    let result = tokio::select! {
        biased;
        reason = stop.wait() => {
            match reason {
                StopReason::EmergencyKey => println!("Stopped by X."),
            }
            Ok(())
        }
        result = tokio::signal::ctrl_c() => {
            println!("Stopped by Ctrl-C.");
            result.map_err(|error| format!("could not listen for Ctrl-C: {error}"))
        }
        result = run_engine(config, &environment, &audio, cleanup.clone()) => result,
    };
    let cleanup_result = finish_run_cleanup(
        audio.restore(),
        cleanup.finish(),
        stop_key.cleanup(),
        ACCESSIBILITY_RESTORE_TIMEOUT,
    )
    .await;

    match (result, cleanup_result) {
        (result, Ok(())) => result,
        (Ok(()), Err(cleanup_error)) => Err(format!(
            "run completed, but cleanup failed: {cleanup_error}"
        )),
        (Err(run_error), Err(cleanup_error)) => {
            Err(format!("{run_error}; cleanup also failed: {cleanup_error}"))
        }
    }
}

async fn finish_run_cleanup<R, L, S>(
    restore: R,
    layout: L,
    stop_key: S,
    restore_limit: Duration,
) -> Result<(), String>
where
    R: Future<Output = Result<(), String>>,
    L: Future<Output = Result<(), String>>,
    S: Future<Output = Result<(), String>>,
{
    let mut errors = Vec::new();
    if let Err(error) = restore_accessibility_with_timeout(restore, restore_limit).await {
        errors.push(error);
    }
    if let Err(error) = layout.await {
        errors.push(error);
    }
    if let Err(error) = stop_key.await {
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

async fn restore_accessibility_with_timeout<F>(restore: F, limit: Duration) -> Result<(), String>
where
    F: Future<Output = Result<(), String>>,
{
    match timeout(limit, restore).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "could not restore the desktop accessibility setting: {error}"
        )),
        Err(_) => Err(
            "timed out restoring the desktop accessibility setting; its final state is unconfirmed"
                .into(),
        ),
    }
}

async fn run_engine(
    config: Config,
    environment: &KdeEnvironment,
    audio: &AudioMode,
    cleanup: KdeCleanupQueue,
) -> Result<(), String> {
    // The platform value owns one input/portal session for the entire engine run.
    let platform = KdeWaylandPlatform::connect(environment, cleanup).await?;
    let browser = FirefoxAdapter::default();
    engine::run(&config, &platform, &browser, audio).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn accessibility_restore_timeout_is_reported_as_unconfirmed() {
        let error =
            restore_accessibility_with_timeout(std::future::pending(), Duration::from_millis(10))
                .await
                .unwrap_err();
        assert!(error.contains("timed out restoring the desktop accessibility setting"));
        assert!(error.contains("final state is unconfirmed"));
    }

    #[tokio::test]
    async fn restore_failure_does_not_skip_layout_or_stop_key_cleanup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let restore_events = Arc::clone(&events);
        let layout_events = Arc::clone(&events);
        let stop_key_events = Arc::clone(&events);
        let result = finish_run_cleanup(
            async move {
                restore_events.lock().unwrap().push("restore");
                Err("simulated restore failure".to_owned())
            },
            async move {
                layout_events.lock().unwrap().push("layout");
                Ok(())
            },
            async move {
                stop_key_events.lock().unwrap().push("stop-key");
                Ok(())
            },
            Duration::from_millis(20),
        )
        .await;

        let error = result.unwrap_err();
        assert!(error.contains("simulated restore failure"));
        assert_eq!(*events.lock().unwrap(), ["restore", "layout", "stop-key"]);
    }

    #[tokio::test]
    async fn restore_timeout_preserves_primary_error_and_runs_later_cleanup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let layout_events = Arc::clone(&events);
        let stop_key_events = Arc::clone(&events);
        let result = finish_run_cleanup(
            std::future::pending::<Result<(), String>>(),
            async move {
                layout_events.lock().unwrap().push("layout");
                Err("simulated layout cleanup failure".to_owned())
            },
            async move {
                stop_key_events.lock().unwrap().push("stop-key");
                Ok(())
            },
            Duration::from_millis(10),
        )
        .await;

        let error = result.unwrap_err();
        assert!(error.contains("timed out restoring the desktop accessibility setting"));
        assert!(error.contains("simulated layout cleanup failure"));
        assert_eq!(*events.lock().unwrap(), ["layout", "stop-key"]);
    }
}
