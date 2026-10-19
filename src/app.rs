use std::{env, process::ExitCode};

use crate::{
    audio::AudioMode,
    browser::firefox::FirefoxAdapter,
    cli::{parse_args, Config, ParseOutcome},
    engine,
    platform::kde_wayland::{KdeCleanupQueue, KdeEnvironment, KdeWaylandPlatform},
    stop::StopReason,
    stop_key::StopKey,
};

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
            stop_key.cleanup().await;
            return Err(format!("could not initialize auto-unmute: {error}"));
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
    if let Err(error) = audio.restore().await {
        eprintln!("Could not restore desktop accessibility setting: {error}");
    }
    cleanup.finish().await;
    stop_key.cleanup().await;
    result
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
