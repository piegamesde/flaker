use crate::errors::{AddErrorResult, ErrorGroup};
use crate::GithubOptions;
use anyhow::{anyhow, Context, Error, Result};
use clap::ValueEnum;
use enumset::EnumSetType;
use futures::{StreamExt, TryStreamExt};
use npins::NixPins;
use octorust::auth::Credentials;
use octorust::types::{Order, SearchCodeSort};
use octorust::{Client, ClientError};
use reqwest::IntoUrl;
use serde::Deserialize;
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info, warn, Instrument};
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

#[tracing::instrument(fields(url = %url), skip_all)]
async fn fetch_pin(url: &Url, branch: Option<String>, submodules: bool) -> Result<npins::Pin> {
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

#[derive(EnumSetType, Debug, ValueEnum)]
pub enum SourceSet {
    /// The Nixpkgs repo
    Nixpkgs,
    /// All NUR repositories
    Nur,
    /// All GitHub repositories with a flake.nix file
    /// <https://github.com/search?q=path%3A**%2F**%2Fflake.nix&type=code&ref=advsearch&p=3>
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

#[derive(Debug, Deserialize)]
struct NurRepo {
    url: Url,
    branch: Option<String>,
    #[serde(default)]
    submodules: bool,
}
#[derive(Debug, Deserialize)]
struct NurRepos {
    repos: HashMap<String, NurRepo>,
}

pub async fn build_index(
    sources: enumset::EnumSet<SourceSet>,
    options: GithubOptions,
    out: PathBuf,
) -> Result<()> {
    let mut pins = NixPins::default();
    let mut global_errors: ErrorGroup = "Building Index failed with errors: ".into();

    info!(sources = ?sources, "Scraping sources");
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

    let _ = write_file(&out, &mut pins)
        .instrument(tracing::info_span!("Writing pins", out_path = ?out.display()))
        .await
        .add_error_to(&mut global_errors);

    if global_errors.has_content() {
        Err(global_errors)?
    } else {
        Ok(())
    }
}

async fn write_file(out: &PathBuf, pins: &mut NixPins) -> Result<()> {
    let out = out;
    let parent = out.parent().ok_or(anyhow!("cant go higher than root"))?;
    std::fs::create_dir_all(parent)?;
    let mut fh = std::fs::File::create(out)
        .with_context(|| format!("Failed to open {} for writing.", out.display()))
        .or(std::fs::File::create("./index.json"))?;
    serde_json::to_writer_pretty(&mut fh, &pins.to_value_versioned())?;
    use std::io::Write;
    fh.write_all(b"\n")?;
    Ok(())
}

async fn index_source_set(
    options: GithubOptions,
    pins: &mut NixPins,
    source: SourceSet,
) -> Result<()> {
    match source {
        SourceSet::Nixpkgs => {
            let nixpkgs_url = Url::parse("https://github.com/NixOS/Nixpkgs").unwrap();
            pins.pins.insert(
                nixpkgs_url.to_string(),
                fetch_pin(&nixpkgs_url, Some("master".into()), false)
                    .await
                    .map_err(|err| err)?,
            );
        }
        SourceSet::Nur => {
            index_nur(pins)
                .instrument(tracing::info_span!("Scraping NUR"))
                .await?;
        }
        SourceSet::Github => {
            info!("Fetching Github repos...");
            let errors: ErrorGroup = "Scraping Github failed with Errors: ".into();
            let (sender, receiver) = unbounded_channel();
            let fetcher = spawn(search_github(options, sender));
            let (ps, error_group) = UnboundedReceiverStream::new(receiver)
                .and_then(|url_string| async move {
                    let url = Url::parse(url_string.as_str())?;
                    let pin = fetch_pin(&url, None, false).await?;
                    Ok((url, pin))
                })
                .fold(
                    (Vec::new(), errors),
                    |(mut ps, mut eg): (Vec<(String, npins::Pin)>, ErrorGroup),
                     itm: Result<(Url, npins::Pin), Error>| async move {
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

async fn index_nur(pins: &mut NixPins) -> Result<()> {
    // <https://github.com/nix-community/NUR/blob/main/repos.json>
    let NurRepos { repos } = get_and_deserialize(
        "https://raw.githubusercontent.com/nix-community/NUR/refs/heads/main/repos.json",
    )
    .await?;
    let stream = futures::stream::iter(repos)
        .map(
            |(
                _,
                NurRepo {
                    url,
                    branch,
                    submodules,
                },
            )| async move {
                match fetch_pin(&url, branch, submodules).await {
                    Ok(pin) => Some((url.to_string(), pin)),
                    Err(err) => {
                        warn!(err = ?err, %url, "Failed to fetch pin, ignoring");
                        None
                    }
                }
            },
        )
        .buffer_unordered(20)
        .filter_map(|val| async { val });
    futures::pin_mut!(stream);
    while let Some((k, v)) = stream.next().await {
        pins.pins.insert(k, v);
    }
    Ok(())
}

async fn search_github(
    options: GithubOptions,
    sender: UnboundedSender<Result<String>>,
) -> Result<()> {
    let token = match options.auth_token {
        Some(t) => Ok(t),
        None => Err(anyhow!("Authentification token required to search Github")),
    }?;
    let gh_client = Client::new(String::from("flaker-indexer"), Credentials::Token(token))?;
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
                        Err(anyhow!("Possibly invalid Token!"))?;
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
                    sender.send(Err(Error::new(e).context("Unexpected HTTP Error")))?;
                }
                _ => {
                    sender.send(Err(Error::new(e).context("unknown error type")))?;
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
