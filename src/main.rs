use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, eyre, Context, Result};
use enumset::EnumSetType;
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
                    fetch_pin(&NIXPKGS_URL, Some("master".into()), false)
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
    err_eq: Option<Diff<Vec<Message>>>,
    warn_eq: Option<Diff<Vec<Message>>>,
    trace_eq: Option<Diff<Vec<Message>>>,
}

#[tracing::instrument(skip(nix_a, nix_b))]
async fn diff_file(file: &Path, nix_a: &Path, nix_b: &Path) -> Result<Option<ParserDiff>> {
    let result_a = tokio::process::Command::new(nix_a)
        .arg0("nix-instantiate")
        .arg("--parse")
        .arg(file)
        .stdin(Stdio::null())
        // Cancellation safety
        .kill_on_drop(true)
        .output()
        .instrument(tracing::info_span!("[nix_a] Executing `nix-instantiate --parse`", file = %file.display()))
        .await?;
    let result_b = tokio::process::Command::new(nix_b)
        .arg0("nix-instantiate")
        .arg("--parse")
        .arg(file)
        .stdin(Stdio::null())
        // Cancellation safety
        .kill_on_drop(true)
        .output()
        .instrument(tracing::info_span!("[nix_b] Executing `nix-instantiate --parse`", file = %file.display()))
        .await?;
    let res = if result_a != result_b {
        //dbg!(&result_a, &result_b);
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
                if err_a == err_b {
                    None
                } else {
                    Some(Diff {
                        result_a: err_a,
                        result_b: err_b,
                    })
                },
                if wrn_a == wrn_b {
                    None
                } else {
                    Some(Diff {
                        result_a: wrn_a,
                        result_b: wrn_b,
                    })
                },
                if trc_a == trc_b {
                    None
                } else {
                    Some(Diff {
                        result_a: trc_a,
                        result_b: trc_b,
                    })
                },
            )
        } else {
            (None, None, None)
        };

        Some(ParserDiff {
            pass_eq: if pass {
                None
            } else {
                Some(Diff {
                    result_a: result_a.status.success(),
                    result_b: result_b.status.success(),
                })
            },
            exit_eq: if exit {
                None
            } else {
                Some(Diff {
                    result_a: result_a.status.code(),
                    result_b: result_b.status.code(),
                })
            },
            stdout_eq: if stdout {
                None
            } else {
                Some(Diff {
                    result_a: String::from_utf8(result_a.stdout)?,
                    result_b: String::from_utf8(result_b.stdout)?,
                })
            },
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
type ErrLog = Vec<Message>;
type WarnLog = Vec<Message>;
type TraceLog = Vec<Message>;

fn split_stderr(stderr: String) -> (ErrLog, WarnLog, TraceLog) {
    let mut errmsgs: ErrLog = vec![];
    let mut warnmsgs: WarnLog = vec![];
    let mut tracemsgs: TraceLog = vec![];
    let re = Regex::new(r"\n\w").unwrap();
    re.split(stderr.as_str()).for_each(|line| {
        match line.get(1..3) {
            //First letter consumed by split, sadly lookahead is not supported.
            //Check for letter required, to not have to deal with indention lines
            //
            //e rr or
            Some("rr") => errmsgs.push(String::from(line)),
            //w ar ning
            Some("ar") => warnmsgs.push(String::from(line)),
            //t ra ce
            Some("ra") => tracemsgs.push(String::from(line)),
            Some(_) => unreachable!("unknown message type"),
            None => {}
        }
    });

    (errmsgs, warnmsgs, tracemsgs)
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
