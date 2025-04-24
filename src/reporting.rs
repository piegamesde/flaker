use crate::diffing::DiffResult;
use crate::indexing::SourceSet;
use crate::reporting::ReportVerbosity::{Auto, Detailed, Summary};
use color_eyre::eyre::{eyre, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy)]
pub enum ReportVerbosity {
    Summary,
    Detailed,
    Auto,
}

impl FromStr for ReportVerbosity {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        match s {
            "summary" => Ok(ReportVerbosity::Summary),
            "detailed" => Ok(ReportVerbosity::Detailed),
            "auto" => Ok(ReportVerbosity::Auto),
            "0" => Ok(ReportVerbosity::Summary),
            "1" => Ok(ReportVerbosity::Detailed),
            "" => Ok(ReportVerbosity::Auto),
            _ => Err(()),
        }
    }
}

impl DiffResult {
    fn from(path: PathBuf) -> Result<Self> {
        let mut report_file = File::open(path)?;
        let mut content = String::new();
        report_file.read_to_string(&mut content)?;
        let res = serde_json::from_str(content.as_str())?;
        res
    }
}

pub fn report(reports: Vec<PathBuf>, verbosity: ReportVerbosity) {
    // for pb in reports {
    //     tracing::info!("{}", pb.to_str().unwrap());
    // }

    let verbosity = match verbosity {
        Auto => {
            if reports.len() == 1 {
                Detailed
            } else {
                Summary
            }
        }
        v => v,
    };

    // let mut report_file = File::open(report_path)?;
    // let mut content = String::new();
    // report_file.read_to_string(&mut content)?;
    // let diff_result: DiffResult = serde_json::from_str(content.as_str())?;
    //tracing::info!(?result);
}
