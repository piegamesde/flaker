use crate::diffing::{Diff, DiffResult, Message, MessageOccurrences, Position};
use clap::ValueEnum;
use rootcause::{bail, Report};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum ReportVerbosity {
    Summary,
    Detailed,
    #[default]
    Auto,
}

impl FromStr for ReportVerbosity {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
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
    fn from_path(path: &PathBuf) -> Result<DiffResult, Report> {
        let mut report_file = File::open(path)?;
        let mut content = String::new();
        report_file.read_to_string(&mut content)?;
        let res: DiffResult = serde_json::from_str(content.as_str())?;
        Ok(res)
    }
}

/// repo -> stdout_diffs
type OutAnalysis = HashMap<String, BTreeSet<Diff<Message>>>;
/// Message -> (repo -> positions)
type MessageAnalysis = HashMap<Message, HashMap<String, Diff<BTreeSet<Position>>>>;
/// repo -> (file -> crash dump)
type CrashAnalysis = HashMap<String, Diff<BTreeMap<Position, String>>>;

#[derive(Default, Debug, Serialize, Deserialize, Clone)]
struct DiffReport {
    stdout: OutAnalysis,
    crash_log: CrashAnalysis,
    err_log: MessageAnalysis,
    wrn_log: MessageAnalysis,
    trc_log: MessageAnalysis,
    fail_cnt: HashMap<String, u64>,
    total_failures: u64,
}

impl DiffReport {
    fn add(&mut self, diff_result: DiffResult, name: String) {
        let propagate_msg = |log: &mut MessageAnalysis, occ: MessageOccurrences| {
            for (msg, d) in occ {
                let di = log
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
        if !diff_result.stdout_diff.is_empty() {
            self.stdout
                .insert(name.clone(), diff_result.stdout_diff.into_iter().collect());
        }
        if !diff_result.crash_diff.result_a.is_empty()
            || !diff_result.crash_diff.result_b.is_empty()
        {
            self.crash_log.insert(name.clone(), diff_result.crash_diff);
        }
        let fails = diff_result.fail_cnt;
        if fails > 0 {
            self.fail_cnt.insert(name.clone(), fails);
        }
        self.total_failures += fails;
    }

    fn is_empty(&self) -> bool {
        self.stdout.iter().all(|(_name, diff)| diff.is_empty())
            && self.err_log.is_empty()
            && self.wrn_log.is_empty()
            && self.trc_log.is_empty()
            && self.total_failures == 0
    }
}

fn print_report(report: DiffReport, verbosity: ReportVerbosity) {
    if report.is_empty() {
        tracing::info!("No diff found!");
        return;
    } else {
        tracing::warn!("Output differs!");
    }
    if report.stdout.iter().any(|(_, d)| !d.is_empty()) {
        tracing::warn!("Actual passing output differed between parsers!");
        tracing::info!("Stdout diffs:");
    }
    for (repo, out_diffs) in report.stdout {
        if out_diffs.is_empty() {
            continue;
        }
        let content = match verbosity {
            ReportVerbosity::Summary => format!("{}", out_diffs.len()),
            ReportVerbosity::Detailed => format!("{:#?}", out_diffs),
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
                    ReportVerbosity::Summary => {
                        format!("a: {} b: {}", diffs.result_a.len(), diffs.result_b.len())
                    }
                    ReportVerbosity::Detailed => format!("{:#?}", diffs),
                    _ => unreachable!(),
                };
                tracing::info!("\t|\t|- {}: {}", repo, content);
            }
        }
    };

    print_log_report("Error Messages:", report.err_log);
    print_log_report("Warn Messages:", report.wrn_log);
    print_log_report("Trace Messages:", report.trc_log);

    match verbosity {
        ReportVerbosity::Summary => {
            tracing::info!("\t|- Total both failures: {}", report.total_failures);
        }
        ReportVerbosity::Detailed => {
            tracing::info!("\t|- Both failure counts:");
            for (repo, cnt) in report.fail_cnt {
                tracing::info!("\t|\t|- {repo}: {cnt}");
            }
        }
        _ => unreachable!(),
    }
}

pub fn report(
    reports: Vec<PathBuf>,
    verbosity: ReportVerbosity,
    output_file: Option<PathBuf>,
) -> Result<(), Report> {
    let verbosity = match verbosity {
        ReportVerbosity::Auto => {
            if reports.len() == 1 {
                ReportVerbosity::Detailed
            } else {
                ReportVerbosity::Summary
            }
        }
        v => v,
    };

    fn recurse(path: PathBuf) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(path.clone()) else {
            return vec![path];
        };
        entries
            .flatten()
            .flat_map(|entry| {
                let p = entry.path();
                if p.is_dir() {
                    return recurse(p);
                }
                if p.is_file() {
                    return vec![p];
                }
                tracing::error!("neither file nor dir");
                vec![]
            })
            .collect()
    }

    let diffs: HashMap<String, Result<DiffResult, Report>> = reports
        .into_iter()
        .flat_map(|path| recurse(path))
        .map(|path| {
            (
                path.file_stem()
                    .unwrap()
                    .to_os_string()
                    .into_string()
                    .unwrap(),
                DiffResult::from_path(&path),
            )
        })
        .collect();

    if diffs.is_empty() {
        bail!("No report files found");
    }

    let mut report = DiffReport::default();

    for (repo_name, diff_result) in diffs {
        report.add(diff_result?, repo_name);
    }

    print_report(report.clone(), verbosity);

    if output_file.is_some() {
        let mut file = File::create(output_file.unwrap())?;
        file.write_all(
            serde_json::to_string_pretty(&report)?
                .into_bytes()
                .as_slice(),
        )?;
    }

    Ok(())
}
