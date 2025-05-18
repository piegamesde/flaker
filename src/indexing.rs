use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, eyre, Context};
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
    let mut pin: npins::Pin = npins::git::GitPin::new(
        npins::git::Repository::git(url.clone()),
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
    out: PathBuf,
) -> color_eyre::Result<()> {
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
                    color_eyre::Result::<(), eyre::Report>::Ok(())
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
        color_eyre::Result::<(), eyre::Report>::Ok(())
    }
    .instrument(tracing::info_span!("Writing pins", out_path = ?out.display()))
    .await?;

    Ok(())
}
