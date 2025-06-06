use crate::errors::{AddErrorResult, ErrorGroup, StrError};
use crate::GithubOptions;
use anyhow::{anyhow, format_err};
use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, eyre, Context, OptionExt};
use color_eyre::Report;
use color_eyre::Section;
use enumset::EnumSetType;
use futures::future::err;
use futures::{Stream, StreamExt, TryStreamExt};
use npins::NixPins;
use octorust::auth::Credentials;
use octorust::git::Git;
use octorust::types::{GitHubApp, Order, SearchCodeSort};
use octorust::{Client, ClientError};
use regex::Regex;
use reqwest::IntoUrl;
use serde::{Deserialize, Serialize};
use std::any::{type_name_of_val, Any};
use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::Formatter;
use std::io::BufRead;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};
use std::{fmt, future};
use thiserror::Error;
use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedSender};
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info, warn, Instrument};
use url::Url;

/// Helper method to build you a client.
// TODO make injectable via a configuration mechanism
pub fn build_client() -> color_eyre::Result<reqwest::Client, reqwest::Error> {
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
async fn get_and_deserialize<T, U>(url: U) -> color_eyre::Result<T>
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

#[tracing::instrument(fields(url = %url), skip_all)]
async fn fetch_pin(
    url: &Url,
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

#[derive(EnumSetType, Debug)]
pub enum SourceSet {
    /// The Nixpkgs repo
    Nixpkgs,
    /// All NUR repositories
    Nur,
    /// All GitHub repositories with a flake.lock
    /// <https://github.com/search?q=path%3A**%2F**%2Fflake.lock&type=code&ref=advsearch&p=3>
    Github,
}

impl SourceSet {
    fn as_str(&self) -> &'static str {
        match self {
            SourceSet::Nixpkgs => "Nixpkgs",
            SourceSet::Nur => "NUR",
            SourceSet::Github => "Github",
        }
    }
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

pub async fn build_index(
    sources: enumset::EnumSet<SourceSet>,
    options: GithubOptions,
    out: PathBuf,
) -> color_eyre::Result<()> {
    let mut pins = npins::NixPins::default();
    let mut global_errors: ErrorGroup = "Building Index failed with errors: ".into();

    tracing::info!(sources = ?sources, "Scraping sources");
    for source in sources {
        let mut sourceset_errors: ErrorGroup = format!(
            "Indexing SourceSet {} failed with errors: ",
            source.as_str()
        )
        .into();
        let _ = index_source_set(options.clone(), &mut pins, source)
            .await
            .add_error_to(sourceset_errors.borrow_mut());
        sourceset_errors.add_error_to(global_errors.borrow_mut());
    }

    async {
        let out = &out;
        let parent = out.parent().ok_or_eyre("cant go higher than root")?;
        std::fs::create_dir_all(parent)?;
        let mut fh = std::fs::File::create(out)
            .with_context(|| format!("Failed to open {} for writing.", out.display()))
            .or(std::fs::File::create("./index.json"))?;
        serde_json::to_writer_pretty(&mut fh, &pins.to_value_versioned())?;
        use std::io::Write;
        fh.write_all(b"\n")?;
        color_eyre::Result::<(), eyre::Report>::Ok(())
    }
    .instrument(tracing::info_span!("Writing pins", out_path = ?out.display()))
    .await?;
    if global_errors.has_content() {
        Err(eyre!(global_errors))
    } else {
        Ok(())
    }
}

async fn index_source_set(
    options: GithubOptions,
    pins: &mut NixPins,
    source: SourceSet,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    match source {
        SourceSet::Nixpkgs => {
            let NIXPKGS_URL = Url::parse("https://github.com/NixOS/Nixpkgs").unwrap();
            pins.pins.insert(
                NIXPKGS_URL.to_string(),
                fetch_pin(&NIXPKGS_URL, Some("master".into()), false)
                    .await
                    .map_err(|err| err)?,
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
                    .filter_map(|val| async { val });
                futures::pin_mut!(stream);
                while let Some((k, v)) = stream.next().await {
                    pins.pins.insert(k, v);
                }
                color_eyre::Result::<(), eyre::Report>::Ok(())
            }.instrument(tracing::info_span!("Scraping NUR")).await?;
        }
        SourceSet::Github => {
            info!("Fetching Github repos...");
            let errors: ErrorGroup = "Scraping Github failed with Errors: ".into();
            let (sender, mut receiver) = unbounded_channel();
            let fetcher = spawn(search_github(options, sender));
            let (ps, error_group) = UnboundedReceiverStream::new(receiver)
                .map_err(|err| {
                    Into::<Box<dyn std::error::Error + Send + Sync + 'static>>::into(StrError(err))
                })
                .and_then(|url_string| async move {
                    let url = Url::parse(url_string.as_str())?;
                    let pin = fetch_pin(&url, None, false).await?;
                    Ok((url, pin))
                })
                .fold(
                    (Vec::new(), errors),
                    |(mut ps, mut eg),
                     itm: Result<
                        (Url, npins::Pin),
                        Box<dyn std::error::Error + Send + Sync + 'static>,
                    >| async move {
                        match itm {
                            Ok((url, pin)) => {
                                ps.push((format!("gh-{}", url), pin));
                            }
                            Err(e) => {
                                eg.add(e);
                            }
                        };
                        (ps, eg)
                    },
                )
                .await;
            fetcher.await??;
            for (name, pin) in ps {
                pins.pins.insert(name, pin);
            }
            error_group.to_result()?;
        }
    };
    Ok(())
}

async fn search_github(
    options: GithubOptions,
    sender: UnboundedSender<Result<String, String>>,
) -> color_eyre::Result<()> {
    let gh_client = Client::new(
        String::from("flaker-indexer"),
        Credentials::Token(options.auth_token.clone()),
    )?;
    let s = octorust::search::Search { client: gh_client };
    let mut expected_total_pages = "?".to_string();
    let start_page = options.start_page;
    let mut page = start_page;
    let mut collected_what_github_calls_all = false;

    while !collected_what_github_calls_all && options.end_page.map(|mp| page < mp).unwrap_or(true) {
        info!("Fetching page {} of {}...", page, expected_total_pages);
        let search_result = s
            .code(
                "filename:flake.nix path:/",
                SearchCodeSort::Noop,
                Order::Noop,
                100,
                page as i64,
            )
            .await;
        match search_result {
            Err(e) => match &e {
                ClientError::RateLimited { ref duration } => {
                    if page == start_page && *duration == 60 {
                        error!("Possibly invalid token provided!");
                        return Err(eyre!(
                            Box::<dyn std::error::Error + Send + Sync + 'static>::from(
                                "Possibly invalid token!"
                            )
                        ));
                    }
                    info!("Got rate limited, waiting for {} seconds...", duration);
                    sleep(Duration::from_secs(*duration + 2)).await;
                }
                ClientError::HttpError {
                    status,
                    headers: _,
                    error,
                } => {
                    if *status == 422
                        && error.contains("Cannot access beyond the first 1000 results")
                    {
                        collected_what_github_calls_all = true;
                        continue;
                    }
                    let err_msg = format!("HTTP Error: {} {}", status, error);
                    warn!(err_msg);
                    sender.send(Err(err_msg))?;
                }
                _ => {
                    let err_msg = "unknown error while fetching";
                    warn!(err_msg);
                    sender.send(Err(err_msg.to_string()))?;
                    // Kill because we don't know if it is sensible to continue...
                    collected_what_github_calls_all = true;
                }
            },
            Ok(response) => {
                if expected_total_pages == "?" {
                    expected_total_pages = format!("{}", response.body.total_count / 100);
                }

                if response.body.items.len() == 0 {
                    collected_what_github_calls_all = true;
                    continue;
                }

                for code_result in response.body.items {
                    let repo_url_string = code_result
                        .repository
                        .url
                        .replace("https://api.github.com/repos/", "https://github.com/");
                    debug!("new repo: {}", repo_url_string);
                    sender.send(Ok(repo_url_string))?;
                }
                page += 1;
            }
        }
    }
    info!("Finished gathering Repos");
    Ok(())
}

async fn fetch_github_pins(
    repos: &mut HashSet<String>,
    pins: &mut NixPins,
) -> color_eyre::Result<()> {
    for repo in repos.drain() {
        pins.pins.insert(
            String::from("gh-") + repo.as_str(),
            fetch_pin(
                &Url::parse(repo.as_str())?,
                None, //Some("master".to_string()),
                false,
            )
            .await
            .map_err(|err| {
                eyre!(Box::<dyn std::error::Error + Send + Sync + 'static>::from(
                    err
                ))
            })?,
        );
    }
    Ok(())
}
