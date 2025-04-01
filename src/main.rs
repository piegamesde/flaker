use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, eyre, Context, Result};
use enumset::EnumSetType;
use futures::future::err;
use futures::{Stream, StreamExt, TryStreamExt};
use regex::Regex;
use reqwest::IntoUrl;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::str::FromStr;
use tracing::{warn, Instrument};
use url::Url;

/// Helper method to build you a client.
// TODO make injectable via a configuration mechanism
pub fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            " v",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
}

/// Helper method for doing various API calls
#[tracing::instrument]
async fn get_and_deserialize<T, U>(url: U) -> Result<T>
where
    T: for<'a> Deserialize<'a> + 'static,
    U: IntoUrl + std::fmt::Debug,
{
    let response = build_client()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(serde_json::from_str(&response)?)
}
#[derive(EnumSetType, Debug)]
enum SourceSet {
    /// The Nixpkgs repo
    Nixpkgs,
    /// All NUR repositories
    Nur,
    /// All GitHub repositories with a flake.lock
    /// <https://github.com/search?q=path%3A**%2F**%2Fflake.lock&type=code&ref=advsearch&p=3>
    Github,
}

impl FromStr for SourceSet {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        match s {
            "nixpkgs" => Ok(SourceSet::Nixpkgs),
            "nur" => Ok(SourceSet::Nur),
            "github" => Ok(SourceSet::Github),
            _ => Err(()),
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    version,
    about,
    arg_required_else_help = true,
    // Confirm clap defaults
    propagate_version = false,
    disable_colored_help = false,
    color = clap::ColorChoice::Auto
)]
enum Command {
    /// Build an index of repositories based on source sets
    BuildIndex {
        /// Which source sets to include.
        /// Comma separated list. Available source sets: `nixpkgs`, `nur`, `github`
        #[arg(long, default_value = "*")]
        sources: String,
        #[arg()]
        out: PathBuf,
    },
    /// Run two Nix versions on all sources and diff the results
    NixParse {
        /// Path to the folder to diff
        #[arg()]
        folder: PathBuf,
        /// Path to a Nix binary
        #[arg()]
        nix_a: PathBuf,
        /// Path to a Nix binary
        #[arg()]
        nix_b: PathBuf,
    },
}

#[tracing::instrument(fields(url = %url), skip_all)]
async fn fetch_pin(
    url: &url::Url,
    branch: Option<String>,
    submodules: bool,
) -> anyhow::Result<npins::Pin> {
    // Always fetch default branch as a small first sanity check for the repo
    let default_branch = npins::git::fetch_default_branch(url).await?;
    let mut pin: npins::Pin = npins::git::GitPin::git(
        url.clone(),
        branch.clone().unwrap_or(default_branch),
        submodules,
    )
    .into();
    pin.update().await?;
    pin.fetch().await?;
    Ok(pin)
}

async fn build_index(sources: enumset::EnumSet<SourceSet>, out: PathBuf) -> Result<()> {
    let mut pins = npins::NixPins::default();

    tracing::info!(sources = ?sources, "Scraping sources");
    for source in sources {
        match source {
            SourceSet::Nixpkgs => {
                let NIXPKGS_URL = Url::parse("https://github.com/NixOS/Nixpkgs").unwrap();
                pins.pins.insert(
                    NIXPKGS_URL.to_string(),
                    fetch_pin(&NIXPKGS_URL, Some("release-24.05".into()), false)
                        .await
                        .map_err(|err| {
                            eyre!(Box::<dyn std::error::Error + Send + Sync + 'static>::from(
                                err
                            ))
                        })?,
                );
            }
            SourceSet::Nur => {
                #[derive(Debug, Deserialize)]
                struct Repo {
                    url: url::Url,
                    branch: Option<String>,
                    #[serde(default)]
                    submodules: bool,
                }
                #[derive(Debug, Deserialize)]
                struct Repos {
                    repos: HashMap<String, Repo>,
                }
                async {
                    // <https://github.com/nix-community/NUR/blob/main/repos.json>
                    let Repos { repos } = get_and_deserialize("https://raw.githubusercontent.com/nix-community/NUR/refs/heads/main/repos.json").await?;
                    let stream = futures::stream::iter(repos)
                        .map(|(_, Repo { url, branch, submodules })| async move {
                            match fetch_pin(&url, branch, submodules).await {
                                Ok(pin) => Some((url.to_string(), pin)),
                                Err(err) => {
                                    tracing::warn!(err = ?err, %url, "Failed to fetch pin, ignoring");
                                    None
                                }
                            }
                        })
                        .buffer_unordered(20)
                        .filter_map(|val| async {val});
                    futures::pin_mut!(stream);
                    while let Some((k, v)) = stream.next().await {
                        pins.pins.insert(k, v);
                    }
                    Result::<(), eyre::Report>::Ok(())
                }.instrument(tracing::info_span!("Scraping NUR")).await?;
            }
            SourceSet::Github => {}
        }
    }

    async {
        let out = &out;
        let mut fh = std::fs::File::create(out)
            .with_context(|| format!("Failed to open {} for writing.", out.display()))?;
        serde_json::to_writer_pretty(&mut fh, &pins.to_value_versioned())?;
        use std::io::Write;
        fh.write_all(b"\n")?;
        Result::<(), eyre::Report>::Ok(())
    }
    .instrument(tracing::info_span!("Writing pins", out_path = ?out.display()))
    .await?;

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct Diff<T> {
    result_a: T,
    result_b: T,
}
#[derive(Debug, Serialize, Deserialize, Default)]
struct ParserDiff {
    // if both sides passed, otherwise info which didn't pass
    pass_eq: Option<Diff<bool>>,
    // exit code difference
    exit_eq: Option<Diff<Option<i32>>>,
    stdout_eq: Option<Diff<Message>>,
    err_eq: Option<Diff<Vec<LogEntry>>>,
    warn_eq: Option<Diff<Vec<LogEntry>>>,
    trace_eq: Option<Diff<Vec<LogEntry>>>,
}

#[tracing::instrument(skip(nix_a, nix_b))]
async fn diff_file(file: &Path, nix_a: &Path, nix_b: &Path) -> Result<Option<ParserDiff>> {
    let result_a = tokio::process::Command::new(nix_a)
        .arg0("nix-instantiate")
        .arg("--parse")
        .arg("--log-format")
        .arg("internal-json")
        .arg(file)
        .stdin(Stdio::null())
        // Cancellation safety
        .kill_on_drop(true)
        .output()
        .instrument(tracing::info_span!("[nix_a] Executing `nix-instantiate --parse`", file = %file.display()));
    let result_b = tokio::process::Command::new(nix_b)
        .arg0("nix-instantiate")
        .arg("--parse")
        .arg("--log-format")
        .arg("internal-json")
        .arg(file)
        .stdin(Stdio::null())
        // Cancellation safety
        .kill_on_drop(true)
        .output()
        .instrument(tracing::info_span!("[nix_b] Executing `nix-instantiate --parse`", file = %file.display()));
    let (result_a, result_b) = futures::join!(result_a, result_b);
    let (result_a, result_b) = (result_a?, result_b?);

    dbg!(&result_a, &result_b);
    let res = if result_a != result_b {
        let pass = result_a.status.success() && result_b.status.success();
        let exit = result_a.status == result_b.status;
        let stdout = result_a.stdout == result_b.stdout;
        let (err, warn, trace) = if result_a.stderr != result_b.stderr {
            let (err_a, wrn_a, trc_a) = split_stderr(String::from_utf8(result_a.stderr)?);
            let (err_b, wrn_b, trc_b) = split_stderr(String::from_utf8(result_b.stderr)?);
            //TODO: Compare message sets (and count?) and only pass diffs into result
            // potentially split at first \n of err, and map line to list of at symbols (rest of line)
            // that would keep track of count, positions and types
            (
                (err_a != err_b).then_some(Diff {
                    result_a: err_a,
                    result_b: err_b,
                }),
                (wrn_a != wrn_b).then_some(Diff {
                    result_a: wrn_a,
                    result_b: wrn_b,
                }),
                (trc_a != trc_b).then_some(Diff {
                    result_a: trc_a,
                    result_b: trc_b,
                }),
            )
        } else {
            (None, None, None)
        };

        Some(ParserDiff {
            pass_eq: (!pass).then_some(Diff {
                result_a: result_a.status.success(),
                result_b: result_b.status.success(),
            }),
            exit_eq: (!exit).then_some(Diff {
                result_a: result_a.status.code(),
                result_b: result_b.status.code(),
            }),
            stdout_eq: (!stdout).then_some(Diff {
                result_a: String::from_utf8(result_a.stdout)?,
                result_b: String::from_utf8(result_b.stdout)?,
            }),
            err_eq: err,
            warn_eq: warn,
            trace_eq: trace,
        })
    } else {
        None
    };
    Ok(res)
}

async fn diff_parsers(folder: PathBuf, nix_a: PathBuf, nix_b: PathBuf) -> Result<()> {
    let files = walkdir::WalkDir::new(folder)
        .follow_links(false)
        .follow_root_links(true)
        .into_iter()
        .filter_map(|res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::warn!(err = ?err, "Failed to walk some file");
                None
            }
        })
        .filter(|e| {
            e.file_type().is_file()
                && e.file_name()
                    .to_str()
                    .expect("UTF-8 file paths only please")
                    .ends_with(".nix")
        });

    let diffs = futures::stream::iter(files)
        .then(|file| {
            let nix_a = &nix_a;
            let nix_b = &nix_b;
            async move { diff_file(file.path(), nix_a, nix_b).await }
        })
        .filter_map(|res| async move { res.unwrap_or_else(|_| None) })
        .for_each(|diff| {
            //TODO: Print with origin file
            tracing::warn!(?diff);
            futures::future::ready(())
        })
        .await;
    Ok(())
}

type Message = String;
type ErrLog = Vec<LogEntry>;
type WarnLog = Vec<LogEntry>;
type TraceLog = Vec<LogEntry>;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
struct LogEntry {
    action: String,
    line: i16,
    column: i16,
    file: String,
    level: i16,
    msg: Message,
    raw_msg: Message,
}

fn split_stderr(stderr: String) -> (ErrLog, WarnLog, TraceLog) {
    let mut errmsgs: ErrLog = vec![];
    let mut warnmsgs: WarnLog = vec![];
    let mut tracemsgs: TraceLog = vec![];
    let mut logs: Vec<LogEntry> = vec![];
    let re = Regex::new(r"\n").unwrap();
    re.split(stderr.as_str()).for_each(|line| {
        match line.get(0..4) {
            Some("@nix") => {
                //throw away the @nix part, otherwise its invalid json
                let j = line.get(5..).unwrap();
                match serde_json::from_str::<LogEntry>(j) {
                    Ok(v) => {
                        if v.action != "msg" {
                            todo!("new action type: {}", v.action);
                        }
                        logs.push(v)
                    }
                    Err(e) => tracing::error!("error parsing json: {}", e),
                }
            }
            Some(t) => {
                todo!("new type: {}", t)
            }
            None => {}
        }
    });
    for log in logs {
        dbg!(&log);
        if log.level == 0 {
            errmsgs.push(log);
        } else if log.level == 1 {
            warnmsgs.push(log);
        } else {
            tracemsgs.push(log);
        }
    }
    dbg!(dedup_log(errmsgs.clone()));
    (errmsgs, warnmsgs, tracemsgs)
}

#[derive(Default, Debug)]
struct Finds {
    positions: HashSet<String>,
}

fn dedup_log(entries: Vec<LogEntry>) -> HashMap<Message, Finds> {
    // entries.into_iter().map(|le| {(le.raw_msg, le.file)}).into_group_map();
    let mut hm: HashMap<Message, Finds> = HashMap::new();
    for entr in entries {
        hm.entry(entr.raw_msg)
            .or_insert(Default::default())
            .positions
            .insert(entr.file);
    }
    hm
}

#[tokio::main]
async fn main() -> Result<()> {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::from_level(
            tracing::Level::INFO,
        ))
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW),
        )
        .with(tracing_error::ErrorLayer::default())
        .init();

    color_eyre::install()?;

    match Command::parse() {
        Command::BuildIndex { sources, out } => {
            let sources = if sources.contains('*') {
                enumset::EnumSet::all()
            } else {
                sources
                    .split(',')
                    .map(SourceSet::from_str)
                    .collect::<std::result::Result<_, ()>>()
                    .map_err(move |()| eyre!("Invalid source set '{}'", sources))?
            };
            build_index(sources, out).await?;
        }
        Command::NixParse {
            folder,
            nix_a,
            nix_b,
        } => {
            diff_parsers(folder, nix_a, nix_b).await?;
        }
    }
    Ok(())
}
