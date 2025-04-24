mod diffing;
mod indexing;
mod reporting;

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

#[tokio::main]
async fn main() -> Result<()> {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::from_level(
            tracing::Level::DEBUG,
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
            use crate::indexing;
            let sources = if sources.contains('*') {
                enumset::EnumSet::all()
            } else {
                sources
                    .split(',')
                    .map(indexing::SourceSet::from_str)
                    .collect::<std::result::Result<_, ()>>()
                    .map_err(move |()| eyre!("Invalid source set '{}'", sources))?
            };
            indexing::build_index(sources, out).await?;
        }
        Command::NixParse {
            folder,
            nix_a,
            nix_b,
        } => {
            diffing::diff_parsers(folder, nix_a, nix_b).await?;
        }
    }
    Ok(())
}
