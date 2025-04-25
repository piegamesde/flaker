use crate::diffing::{Diff, DiffResult, Message, MessageOccurrences, Position};
use crate::indexing::SourceSet;
use crate::reporting::ReportVerbosity::{Auto, Detailed, Summary};
use color_eyre::eyre::{eyre, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    fn from_path(path: &PathBuf) -> Result<DiffResult> {
        let mut report_file = File::open(path)?;
        let mut content = String::new();
        report_file.read_to_string(&mut content)?;
        let res: DiffResult = serde_json::from_str(content.as_str())?;
        Ok(res)
    }
}

/// repo -> stdout_diffs
type OutAnalysis = HashMap<String, HashSet<Diff<Message>>>;
/// Message -> (repo -> positions)
type MessageAnalysis = HashMap<Message, HashMap<String, Diff<HashSet<Position>>>>;

#[derive(Default, Debug, Serialize, Deserialize)]
struct Report {
    stdout: OutAnalysis,
    err_log: MessageAnalysis,
    wrn_log: MessageAnalysis,
    trc_log: MessageAnalysis,
}

impl Report {
    fn add(&mut self, diff_result: DiffResult, name: String) {
        let propagate_msg = |log: &mut MessageAnalysis, occ: MessageOccurrences| {
            for (msg, d) in occ {
                let mut di = log
                    .entry(msg.clone())
                    .or_insert(Default::default())
                    .entry(name.clone())
                    .or_insert(Default::default());
                di.result_a.extend(d.result_a);
                di.result_b.extend(d.result_b);
            }
        };
        propagate_msg(&mut self.err_log, diff_result.err_diff);
        propagate_msg(&mut self.wrn_log, diff_result.wrn_diff);
        propagate_msg(&mut self.trc_log, diff_result.trc_diff);
        self.stdout.insert(name.clone(), diff_result.stdout_diff);
    }
}

fn print_report(report: Report, verbosity: ReportVerbosity) {
    if report.stdout.iter().any(|(_, d)| !d.is_empty()) {
        tracing::warn!("Actual passing output differed between parsers!");
        tracing::info!("Stdout diffs:");
    }
    for (repo, out_diffs) in report.stdout {
        if out_diffs.is_empty() {
            continue;
        }
        let content = match verbosity {
            Summary => format!("{}", out_diffs.len()),
            Detailed => format!("{:#?}", out_diffs),
            _ => unreachable!(),
        };
        tracing::info!("\t|- \"{}\": {}", repo, content);
    }

    let print_log_report = |description: &str, log: MessageAnalysis| {
        if log.iter().any(|(_, d)| !d.is_empty()) {
            tracing::info!("{}", description);
        }
        for (msg, repo_info) in log {
            tracing::info!("\t|- `{}`:", msg);
            for (repo, diffs) in repo_info {
                let content = match verbosity {
                    Summary => format!("a: {} b: {}", diffs.result_a.len(), diffs.result_b.len()),
                    Detailed => format!("{:#?}", diffs),
                    _ => unreachable!(),
                };
                tracing::info!("\t|\t|- {}: {}", repo, content);
            }
        }
    };

    print_log_report("Error Messages:", report.err_log);
    print_log_report("Warn Messages:", report.wrn_log);
    print_log_report("Trace Messages", report.trc_log);
}

pub fn report(reports: Vec<PathBuf>, verbosity: ReportVerbosity) -> Result<()> {
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

    let diffs: HashMap<String, Result<DiffResult>> = reports
        .iter()
        .map(|path| {
            (
                path.file_stem()
                    .unwrap()
                    .to_os_string()
                    .into_string()
                    .unwrap(),
                DiffResult::from_path(path),
            )
        })
        .collect();

    let mut report = Report::default();

    for (repo_name, diff_result) in diffs {
        report.add(diff_result?, repo_name);
    }

    print_report(report, verbosity);

    Ok(())
}
