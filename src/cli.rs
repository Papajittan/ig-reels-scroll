use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub windows: usize,
    pub count: u32,
    pub interval: Duration,
    pub open_browser: bool,
    pub stagger: u32,
    pub unmute: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseOutcome {
    Run(Config),
    Help(String),
}

pub fn usage(program: &str) -> String {
    format!(
        "Usage: {program} [--windows N] [--count N] [--interval SECONDS] [--stagger N] [--no-open] [--unmute | --no-unmute]\n\
         KDE Plasma Wayland: tile N Firefox Reels windows and scroll each one.\n\
         Defaults: 1 window, 20 scroll passes, 3 seconds apart, 5 extra starting scrolls per window.\n\
         Press X anywhere or Ctrl-C in the terminal to stop. --stagger 0 disables starting offsets.\n\
         --unmute enables automatic unmuting; otherwise audio is left unchanged."
    )
}

/// Parse argv, including its program-name element, without reading process state.
pub fn parse_args<I, S>(args: I) -> Result<ParseOutcome, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut windows = 1_usize;
    let mut count = 20_u32;
    let mut interval_secs = 3.0_f64;
    let mut open_browser = true;
    let mut stagger = 5_u32;
    let mut unmute = false;
    let mut args = args.into_iter().map(Into::into);
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
            "-h" | "--help" => return Ok(ParseOutcome::Help(usage(&program))),
            _ => return Err(format!("unknown option: {arg}")),
        }
    }

    let interval = Duration::try_from_secs_f64(interval_secs)
        .map_err(|_| "--interval must be a finite representable duration")?;
    Ok(ParseOutcome::Run(Config {
        windows,
        count,
        interval,
        open_browser,
        stagger,
        unmute,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<ParseOutcome, String> {
        parse_args(arguments.iter().copied())
    }

    #[test]
    fn defaults_and_compatible_invocations_are_preserved() {
        assert_eq!(
            parse(&["ig-reels-scroll"]).unwrap(),
            ParseOutcome::Run(Config {
                windows: 1,
                count: 20,
                interval: Duration::from_secs(3),
                open_browser: true,
                stagger: 5,
                unmute: false,
            })
        );
        for args in [
            &["ig-reels-scroll", "--windows", "2"][..],
            &["ig-reels-scroll", "--windows", "4", "--count", "20"][..],
            &["ig-reels-scroll", "--interval", "3", "--stagger", "5"][..],
            &["ig-reels-scroll", "--no-open"][..],
            &["ig-reels-scroll", "--stagger", "0"][..],
            &["ig-reels-scroll", "--unmute"][..],
            &["ig-reels-scroll", "--no-unmute"][..],
        ] {
            assert!(matches!(parse(args), Ok(ParseOutcome::Run(_))), "{args:?}");
        }
        assert!(matches!(
            parse(&["ig-reels-scroll", "--windows", "16"]),
            Ok(ParseOutcome::Run(Config { windows: 16, .. }))
        ));
    }

    #[test]
    fn options_validate_bounds_and_last_audio_flag_wins() {
        for (args, message) in [
            (
                &["app", "--windows", "0"][..],
                "--windows must be from 1 to 16",
            ),
            (
                &["app", "--windows", "17"][..],
                "--windows must be from 1 to 16",
            ),
            (
                &["app", "--count", "0"][..],
                "--count must be greater than zero",
            ),
            (
                &["app", "--interval", "0.1"][..],
                "--interval must be at least 0.2 seconds",
            ),
            (
                &["app", "--stagger", "101"][..],
                "--stagger must be from 0 to 100",
            ),
        ] {
            assert_eq!(parse(args).unwrap_err(), message);
        }
        let ParseOutcome::Run(config) = parse(&["app", "--unmute", "--no-unmute"]).unwrap() else {
            panic!("expected run configuration");
        };
        assert!(!config.unmute);
        assert!(matches!(
            parse(&["app", "--no-unmute", "--unmute"]),
            Ok(ParseOutcome::Run(Config { unmute: true, .. }))
        ));
        assert!(matches!(
            parse(&["app", "--help"]),
            Ok(ParseOutcome::Help(_))
        ));
    }
}
