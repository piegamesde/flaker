use crate::bistate_result::BistateResult;
use crate::GithubOptions;
use clap::ValueEnum;
use enumset::EnumSetType;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{future, FutureExt, StreamExt, TryFutureExt};
use indicatif::ProgressStyle;
use npins::{NixPins, Pin};
use octorust::auth::Credentials;
use octorust::types::{Order, SearchCodeSort};
use octorust::{Client, ClientError};
use reqwest::IntoUrl;
use rootcause::prelude::ResultExt;
use rootcause::report_collection::ReportCollection;
use rootcause::{report, Report};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::{sleep, timeout};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, info_span, Instrument};
use tracing_indicatif::span_ext::IndicatifSpanExt;
use url::Url;

type FetcherStream = BoxStream<'static, BoxFuture<'static, Result<(String, Pin), Report>>>;

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
async fn fetch_pin(url: Url, branch: Option<String>, submodules: bool) -> Result<Pin, Report> {
    tracing::Span::current().pb_set_message(format!("Fetching {url}").as_str());
    // Always fetch default branch as a small first sanity check for the repo
    let default_branch = npins::git::fetch_default_branch(&url)
        .await
        .map_err(|e| report!(e))?;
    let mut pin: Pin = npins::git::GitPin::new(
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
    let progress = Arc::new(tracing::info_span!("Building index"));
    progress.pb_set_style(&ProgressStyle::with_template(
        "{prefix:.bold.dim} {msg}: {wide_bar} [{pos:>7}/{len:7}]",
    )?);
    progress.pb_set_message("Building index");
    progress.pb_set_length(0);
    progress.pb_start();

    let BistateResult(pins, mut err): BistateResult<_, ReportCollection> =
        futures::stream::iter(sources)
            .then(|source| {
                let opt = options.clone();
                let progress = progress.clone();
                async move {
                    let source_str = source.as_str();
                    (match source {
                        SourceSet::Nixpkgs => index_nixpkgs(progress).await,
                        SourceSet::Nur => index_nur(progress).await,
                        SourceSet::Github => index_github(opt, progress).await,
                    })
                    .context("Error while indexing SourceSet")
                    .attach(source_str)
                }
            })
            .flat_map_unordered(None, |res| match res {
                Ok(stream) => stream.boxed(),
                Err(e) => {
                    futures::stream::once(async { async { Err(e.into_dyn_any()) }.boxed() }).boxed()
                }
            })
            .map(|f| {
                timeout(Duration::from_secs(90), f).map(|r| {
                    r.map_err(|e| report!(e).context("Fetch timed out").into_dyn_any())
                        .flatten()
                })
            })
            .buffer_unordered(10)
            .collect()
            .await;

    let _ = write_file(&out, &NixPins { pins })
        .instrument(tracing::info_span!("Writing pins", out_path = ?out.display()))
        .await
        .context("failed to write pins")
        .attach(format!("Outfile: {out:?}"))
        .map_err(|e| err.push(e.into_dyn_any().into_cloneable()));

    if !err.is_empty() {
        Err(err
            .context("Building Index failed with errors: ")
            .into_dyn_any())
    } else {
        Ok(())
    }
}

async fn write_file(out: &PathBuf, pins: &NixPins) -> Result<(), Report> {
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

async fn index_nixpkgs(span: Arc<tracing::Span>) -> Result<FetcherStream, Report> {
    const NIXPKGS_STRING: &'static str = "https://github.com/NixOS/nixpkgs";
    let nixpkgs_url = Url::parse(NIXPKGS_STRING).unwrap();

    /* The current master branch and all releases since 2021 (arbitrarily picked) */
    let branches = std::iter::once("master".to_string())
        .chain(
            (21..=25)
                .into_iter()
                .flat_map(|year| [format!("nixos-{year}.05"), format!("nixos-{year}.11")]),
        )
        .collect::<Vec<_>>();
    span.pb_inc_length(branches.len() as u64);
    Ok(futures::stream::iter(branches)
        .map(move |branch| {
            span.pb_inc(1);
            fetch_pin(nixpkgs_url.clone(), Some(branch.clone()), false)
                .map_ok(|pin| (NIXPKGS_STRING.to_string(), pin))
                .map_err(|err| {
                    err.context("While indexing Nixpkgs")
                        .attach(NIXPKGS_STRING.to_string())
                        .attach(branch)
                        .into_dyn_any()
                })
                .boxed()
        })
        .boxed())
}

async fn index_nur(span: Arc<tracing::Span>) -> Result<FetcherStream, Report> {
    // <https://github.com/nix-community/NUR/blob/main/repos.json>
    let NurRepos { repos } = get_and_deserialize(
        "https://raw.githubusercontent.com/nix-community/NUR/refs/heads/main/repos.json",
    )
    .await?;
    span.pb_inc_length(repos.len() as u64);

    Ok(futures::stream::iter(repos)
        .map(
            move |(
                _,
                NurRepo {
                    url,
                    branch,
                    submodules,
                },
            )| {
                span.pb_inc(1);
                fetch_pin(url.clone(), branch, submodules)
                    .map(move |r| match r {
                        Ok(pin) => Ok((url.to_string(), pin)),
                        Err(e) => Err(e
                            .attach(url.to_string())
                            .context("While indexing NUR")
                            .into_dyn_any()),
                    })
                    .boxed()
            },
        )
        .boxed())
}

async fn index_github(
    options: GithubOptions,
    span: Arc<tracing::Span>,
) -> Result<FetcherStream, Report> {
    let (sender, receiver) = unbounded_channel();
    search_github(options, sender, span.clone())?;
    Ok(UnboundedReceiverStream::new(receiver)
        .map(move |res| match res {
            Ok(url) => {
                span.pb_inc(1);
                fetch_pin(url.clone(), None, false)
                    .map(move |r| match r {
                        Ok(pin) => Ok((url.to_string(), pin)),
                        Err(e) => Err(e
                            .attach(url.to_string())
                            .context("While indexing Github")
                            .into_dyn_any()),
                    })
                    .boxed()
            }
            Err(e) => future::ready(Err(e
                .context("While running the Github scraper".to_string())
                .into_dyn_any()))
            .boxed(),
        })
        .boxed())
}

fn search_github(
    options: GithubOptions,
    sender: UnboundedSender<Result<Url, Report>>,
    span: Arc<tracing::Span>,
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

    spawn(async move {
        while options.end_page.map(|mp| page < mp).unwrap_or(true) {
            info_span!(
                "Fetching Github page",
                "{}",
                format!("{page} of {expected_total_pages}...")
            );
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
                            if let Err(_) = sender.send(Err(report!("Possibly invalid token!"))) {
                                break;
                            }
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
                            break;
                        }
                        if let Err(_) = sender.send(
                            Err(e)
                                .context("Unexpected HTTP Error")
                                .map_err(|e| e.into_dyn_any()),
                        ) {
                            break;
                        }
                    }
                    _ => {
                        let _ = sender.send(
                            Err(e)
                                .context("unknown error type")
                                .map_err(|e| e.into_dyn_any()),
                        );
                        // Kill because we don't know if it is sensible to continue...
                        break;
                    }
                },
                Ok(response) => {
                    if expected_total_pages == "?" {
                        expected_total_pages = format!("{}", response.body.total_count / 100);
                    }

                    let items = response.body.items;

                    if items.len() == 0 {
                        break;
                    }
                    span.pb_inc_length(items.len() as u64);
                    for code_result in items {
                        let repo_url_string = code_result
                            .repository
                            .url
                            .replace("https://api.github.com/repos/", "https://github.com/");
                        debug!("new repo: {}", repo_url_string);
                        if let Err(_) = sender.send(
                            Url::parse(repo_url_string.as_str())
                                .map_err(|e| report!(e).into_dyn_any()),
                        ) {
                            break;
                        }
                    }
                    page += 1;
                }
            }
        }
        info!("Finished gathering Repos");
    });
    Ok(())
}
