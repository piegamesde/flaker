use crate::GithubOptions;
use clap::ValueEnum;
use enumset::EnumSetType;
use futures::{StreamExt, TryStreamExt};
use indicatif::ProgressStyle;
use npins::{NixPins, Pin};
use octorust::auth::Credentials;
use octorust::types::{Order, SearchCodeSort};
use octorust::{Client, ClientError};
use reqwest::IntoUrl;
use rootcause::prelude::ResultExt;
use rootcause::report_collection::ReportCollection;
use rootcause::{bail, report, Report};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, info_span, Instrument, Span};
use tracing_indicatif::span_ext::IndicatifSpanExt;
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
async fn get_and_deserialize<T, U>(url: U) -> Result<T, Report>
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
) -> Result<npins::Pin, Report> {
    // Always fetch default branch as a small first sanity check for the repo
    let default_branch = npins::git::fetch_default_branch(url)
        .await
        .map_err(|e| report!(e))?;
    let mut pin: npins::Pin = npins::git::GitPin::new(
        npins::git::Repository::git(url.clone()),
        branch.clone().unwrap_or(default_branch),
        submodules,
    )
    .into();
    pin.update()
        .await
        .map_err(|e| report!(e).attach(url.clone()))?;
    pin.fetch()
        .await
        .map_err(|e| report!(e).attach(url.clone()))?;
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
) -> Result<(), Report> {
    let mut pins = NixPins::default();
    let mut global_errors = ReportCollection::new();

    let _ = futures::stream::iter(sources)
        .map(|source| {
            let opt = options.clone();
            async move {
                let source_str = source.as_str();
                let (p, err) = (match source {
                    SourceSet::Nixpkgs => index_nixpkgs().await,
                    SourceSet::Nur => index_nur().await,
                    SourceSet::Github => index_github(opt).await,
                })
                .instrument(tracing::info_span!("Indexing", source = source_str))
                .await;
                let err = err
                    .context("Error while indexing SourceSet")
                    .attach(source_str);
                (p, err)
            }
        })
        .buffer_unordered(3)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .fold(
            vec![],
            |mut ps: Vec<(String, npins::Pin)>, (mut new_pins, err)| {
                global_errors.push(err.into_cloneable());
                ps.append(&mut new_pins);
                ps
            },
        )
        .into_iter()
        .for_each(|(name, pin)| {
            pins.pins.insert(name, pin);
        });

    let _ = write_file(&out, &mut pins)
        .instrument(tracing::info_span!("Writing pins", out_path = ?out.display()))
        .await
        .context("failed to write pins")
        .attach(format!("Outfile: {out:?}"))
        .map_err(|e| global_errors.push(e.into_cloneable()));

    if !global_errors.is_empty() {
        Err(global_errors
            .context("Building Index failed with errors: ")
            .into_dyn_any())
    } else {
        Ok(())
    }
}

async fn write_file(out: &PathBuf, pins: &mut NixPins) -> Result<(), Report> {
    let out = out;
    let parent = out.parent().ok_or(report!("cant go higher than root"))?;
    std::fs::create_dir_all(parent)?;
    let mut fh = std::fs::File::create(out)
        .context_with(|| format!("Failed to open {} for writing.", out.display()))
        .or_else(|_| std::fs::File::create("./index.json"))?;
    serde_json::to_writer_pretty(&mut fh, &pins.to_value_versioned())?;
    use std::io::Write;
    fh.write_all(b"\n")?;
    Ok(())
}

async fn index_github(options: GithubOptions) -> (Vec<(String, Pin)>, ReportCollection) {
    let (sender, receiver) = unbounded_channel();
    let fetcher = spawn(search_github(options, sender));
    let (ps, mut err) = UnboundedReceiverStream::new(receiver)
        .map(|res| async move {
            match res {
                Ok(url_string) => {
                    let url = Url::parse(url_string.as_str())?;
                    let pin = fetch_pin(&url, None, false).await?;
                    Ok((url.to_string(), pin))
                }
                Err(e) => Err(e),
            }
        })
        .buffer_unordered(10)
        .fold(
            (vec![], ReportCollection::new()),
            |(mut ok, mut err), res| async {
                match res {
                    Ok(v) => ok.push(v),
                    Err(e) => err.push(e.into_cloneable()),
                }
                (ok, err)
            },
        )
        .await;
    let _ = fetcher
        .await
        .map_err(|e| err.push(report!(e).into_cloneable().into()))
        .map(|o| o.map_err(|e| err.push(e.into_cloneable())));
    (ps, err)
}

async fn index_nixpkgs() -> (Vec<(String, Pin)>, ReportCollection) {
    let mut errs = ReportCollection::new();
    let nixpkgs_url = Url::parse("https://github.com/NixOS/nixpkgs").unwrap();
    let res = fetch_pin(&nixpkgs_url, Some("master".into()), false).await;
    let v = match res {
        Ok(pin) => vec![(nixpkgs_url.to_string(), pin)],
        Err(err) => {
            errs.push(err.into_cloneable());
            vec![]
        }
    };
    (v, errs)
}

async fn index_nur() -> (Vec<(String, npins::Pin)>, ReportCollection) {
    let mut err = ReportCollection::new();

    // <https://github.com/nix-community/NUR/blob/main/repos.json>
    let Ok(NurRepos { repos }) = get_and_deserialize(
        "https://raw.githubusercontent.com/nix-community/NUR/refs/heads/main/repos.json",
    )
    .await
    .map_err(|e| err.push(e.into_cloneable())) else {
        return (vec![], err);
    };

    let fetch_bar = Span::current();
    fetch_bar.pb_set_style(
        &ProgressStyle::with_template("{prefix:.bold.dim} {msg}: {wide_bar} [{pos:>7}/{len:7}]")
            .unwrap(),
    );
    fetch_bar.pb_set_length(repos.len() as u64);
    fetch_bar.pb_set_message("Indexing NUR pins");
    fetch_bar.pb_set_finish_message("Finished indexing NUR");
    fetch_bar.pb_start();

    let stream = futures::stream::iter(repos)
        .map(
            |(
                _,
                NurRepo {
                    url,
                    branch,
                    submodules,
                },
            )| {
                (url.as_str().to_string(), async move {
                    fetch_pin(&url, branch, submodules)
                        .await
                        .map(|pin| (url.to_string(), pin))
                        .context("fetch_pin failed")
                        .attach_with(|| url)
                })
            },
        )
        .map(|(url, f)| async {
            fetch_bar.pb_inc(1);
            tokio::time::timeout(Duration::from_secs(30), f)
                .await
                .context("Fetch timed out")
                .attach_with(|| url)
                .unwrap_or_else(|e| Err(e))
        })
        .buffer_unordered(20)
        .map_err(|e| e.context("Failed to fetch pin, ignoring"))
        .map_err(|e| err.push(e.into_cloneable().into()))
        .filter_map(|x| async { x.ok() });
    (stream.collect::<Vec<(String, npins::Pin)>>().await, err)
}

async fn search_github(
    options: GithubOptions,
    sender: UnboundedSender<Result<String, Report>>,
) -> Result<(), Report> {
    let token = match options.auth_token {
        Some(t) => Ok(t),
        None => Err(report!("Authentification token required to search Github")),
    }?;
    let gh_client = Client::new(String::from("flaker-indexer"), Credentials::Token(token))?;
    let s = octorust::search::Search { client: gh_client };
    let mut expected_total_pages = "?".to_string();
    let start_page = options.start_page;
    let mut page = start_page;
    let mut collected_what_github_calls_all = false;
    info_span!("Fetching repositories");
    while !collected_what_github_calls_all && options.end_page.map(|mp| page < mp).unwrap_or(true) {
        info!("Fetching page {page} of {expected_total_pages}...");
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
                        bail!("Possibly invalid Token!");
                    }
                    info_span!("Got rate limited, waiting...", seconds=%duration);
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
                    sender.send(
                        Err(e)
                            .context("Unexpected HTTP Error")
                            .map_err(|e| e.into_dyn_any()),
                    )?;
                }
                _ => {
                    sender.send(
                        Err(e)
                            .context("unknown error type")
                            .map_err(|e| e.into_dyn_any()),
                    )?;
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
