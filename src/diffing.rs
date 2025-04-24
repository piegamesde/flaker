use crate::reporting;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use tracing::instrument::Instrumented;
use tracing::Instrument;

mod parsing {
    use crate::diffing::{CompLog, ErrLog, Finds, Message, TraceLog, WarnLog};
    use regex::Regex;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::LazyLock;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
    struct LogEntry {
        action: String,
        file: Option<String>,
        level: i16,
        msg: Message,
        raw_msg: Option<Message>,
    }

    static DEP_FINDER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"--extra-deprecated-features (?<feature_name>[\w-]+)\b").unwrap()
    });

    fn simplify_msg(msg: Message) -> Message {
        let m = DEP_FINDER_RE.captures(msg.as_str());
        match m {
            Some(name) => "Deprecated Feature: ".to_string() + name["feature_name"].as_ref(),
            None => msg,
        }
    }

    fn dedup_log(entries: Vec<LogEntry>, file: &Path) -> CompLog {
        // entries.into_iter().map(|le| {(le.raw_msg, le.file)}).into_group_map();
        let mut hm: HashMap<Message, Finds> = HashMap::new();
        let fp: String = file.to_str().map(|s| s.to_string()).unwrap();
        for entr in entries {
            let key = entr.raw_msg.unwrap_or(entr.msg);
            let key = simplify_msg(key);
            hm.entry(key)
                .or_insert(Default::default())
                .positions
                .insert(entr.file.unwrap_or(fp.clone()));
        }
        hm
    }

    pub fn split_stderr(stderr: String, file: &Path) -> (ErrLog, WarnLog, TraceLog) {
        let mut errmsgs: Vec<LogEntry> = vec![];
        let mut warnmsgs: Vec<LogEntry> = vec![];
        let mut tracemsgs: Vec<LogEntry> = vec![];
        let mut logs: Vec<LogEntry> = vec![];
        let re = Regex::new(r"\n").unwrap();
        re.split(stderr.as_str()).for_each(|line| {
            match line.get(0..4) {
                Some("@nix") => {
                    //throw away the @nix part, otherwise its invalid json
                    let j = line.get(5..).unwrap();
                    match serde_json::from_str::<LogEntry>(j) {
                        Ok(v) => {
                            if v.action != "msg" {
                                todo!("new action type: {}", v.action);
                            }
                            logs.push(v)
                        }
                        Err(e) => tracing::error!("error parsing json: {}; {}", e, j),
                    }
                }
                Some(t) => {
                    todo!("new type: {}", t)
                }
                None => {}
            }
        });
        for log in logs {
            if log.level == 0 {
                errmsgs.push(log);
            } else if log.level == 1 {
                warnmsgs.push(log);
            } else {
                tracemsgs.push(log);
            }
        }
        (
            dedup_log(errmsgs, file),
            dedup_log(warnmsgs, file),
            dedup_log(tracemsgs, file),
        )
    }
}

type Message = String;
pub type Position = String;

#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
struct Finds {
    positions: HashSet<Position>,
}

type CompLog = HashMap<Message, Finds>;

type ErrLog = CompLog;
type WarnLog = CompLog;
type TraceLog = CompLog;

#[derive(Debug, Serialize, Deserialize, Default)]
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
    err_eq: Option<Diff<ErrLog>>,
    warn_eq: Option<Diff<WarnLog>>,
    trace_eq: Option<Diff<TraceLog>>,
}

impl Diff<CompLog> {
    fn from(result_a: CompLog, result_b: CompLog) -> Diff<CompLog> {
        fn extract(a: &CompLog, b: &CompLog) -> CompLog {
            let mut res_a: CompLog = Default::default();
            for key in a.keys() {
                if b.contains_key(key) {
                    let not_in_b = a[key].positions.difference(&b[key].positions);
                    for pos in not_in_b {
                        res_a
                            .entry(key.clone())
                            .or_insert(Default::default())
                            .positions
                            .insert(pos.clone());
                    }
                } else {
                    res_a.insert(
                        key.into(),
                        Finds {
                            positions: a[key].positions.clone(),
                        },
                    );
                }
            }
            res_a
        }

        let in_a_but_not_in_b = extract(&result_a, &result_b);
        let in_b_but_not_in_a = extract(&result_b, &result_a);

        Diff {
            result_a: in_a_but_not_in_b,
            result_b: in_b_but_not_in_a,
        }
    }

    pub fn merge(&mut self, b: Diff<CompLog>) {
        self.result_a.extend(b.result_a);
        self.result_b.extend(b.result_b);
    }
}
macro_rules! merge_complog {
    ($a: expr, $b: expr) => {
        match ($a.as_mut(), $b) {
            (Some(a), Some(b)) => a.merge(b),
            (None, b) => $a = b,
            _ => (),
        }
    };
}

impl ParserDiff {
    fn merge(&mut self, other: ParserDiff) {
        match (self.pass_eq.is_none(), other.pass_eq) {
            (true, Some(s)) => {
                self.pass_eq.replace(s);
            }
            _ => (),
        };
        match (self.exit_eq.is_none(), other.exit_eq) {
            (true, Some(s)) => {
                self.exit_eq.replace(s);
            }
            _ => (),
        }

        self.stdout_eq = match (self.stdout_eq.take(), other.stdout_eq) {
            (Some(_), Some(_)) => Some(Diff {
                result_a: "Multiple given".to_string(),
                result_b: "Multiple given".to_string(),
            }),
            (None, b) => b,
            (a, None) => a,
        };

        merge_complog!(self.err_eq, other.err_eq);
        merge_complog!(self.warn_eq, other.warn_eq);
        merge_complog!(self.trace_eq, other.trace_eq);
    }
}

fn diff_stderr(
    err_a: String,
    err_b: String,
    file: &Path,
) -> (
    Option<Diff<ErrLog>>,
    Option<Diff<WarnLog>>,
    Option<Diff<TraceLog>>,
) {
    if err_a != err_b {
        let (err_a, wrn_a, trc_a) = parsing::split_stderr(err_a, file);
        let (err_b, wrn_b, trc_b) = parsing::split_stderr(err_b, file);
        //TODO: Compare message sets (and count?) and only pass diffs into result
        // potentially split at first \n of err, and map line to list of at symbols (rest of line)
        // that would keep track of count, positions and types
        (
            (err_a != err_b).then_some(Diff::from(err_a, err_b)),
            (wrn_a != wrn_b).then_some(Diff::from(wrn_a, wrn_b)),
            (trc_a != trc_b).then_some(Diff::from(trc_a, trc_b)),
        )
    } else {
        (None, None, None)
    }
}

#[tracing::instrument(skip(nix_a, nix_b))]
async fn diff_file(
    file: &Path,
    nix_a: &Path,
    nix_b: &Path,
) -> color_eyre::Result<Option<ParserDiff>> {
    /* Execute the parsers */
    let run = |nix: &Path, runner: &str| {
        tokio::process::Command::new(nix)
            .arg0("nix-instantiate")
            .arg("--parse")
            .arg("--log-format")
            .arg("internal-json")
            .arg(file)
            .stdin(Stdio::null())
            // Cancellation safety
            .kill_on_drop(true)
            .output()
            .instrument(tracing::info_span!("Executing `nix-instantiate --parse`", runner, file = %file.display()))
    };
    let result_a = run(nix_a, "nix_a");
    let result_b = run(nix_b, "nix_b");
    let (result_a, result_b) = futures::join!(result_a, result_b);
    let (result_a, result_b) = (result_a?, result_b?);

    /* compare Results */
    //dbg!(&result_a, &result_b);
    let res = if result_a != result_b {
        let pass = result_a.status.success() && result_b.status.success();
        let exit = result_a.status == result_b.status;
        let stdout = result_a.stdout == result_b.stdout;
        let (err, warn, trace) = diff_stderr(
            String::from_utf8(result_a.stderr)?,
            String::from_utf8(result_b.stderr)?,
            file,
        );

        Some(ParserDiff {
            pass_eq: (!pass).then_some(Diff {
                result_a: result_a.status.success(),
                result_b: result_b.status.success(),
            }),
            exit_eq: (!exit).then_some(Diff {
                result_a: result_a.status.code(),
                result_b: result_b.status.code(),
            }),
            stdout_eq: (!stdout).then_some(Diff {
                result_a: String::from_utf8(result_a.stdout)?,
                result_b: String::from_utf8(result_b.stdout)?,
            }),
            err_eq: err,
            warn_eq: warn,
            trace_eq: trace,
        })
    } else {
        None
    };
    Ok(res)
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct DiffResult {
    err_diff: HashMap<Message, Diff<HashSet<Position>>>,
    wrn_diff: HashMap<Message, Diff<HashSet<Position>>>,
    trc_diff: HashMap<Message, Diff<HashSet<Position>>>,
}

impl DiffResult {
    fn from(diffs: Vec<ParserDiff>) -> DiffResult {
        if diffs.len() == 0 {
            return Default::default();
        }
        let rep = diffs
            .into_iter()
            .reduce(|mut acc, diff| {
                acc.merge(diff);
                acc
            })
            .unwrap();

        fn propagate_msg(log: Option<Diff<CompLog>>) -> HashMap<Message, Diff<HashSet<Position>>> {
            let mut hm: HashMap<Message, Diff<HashSet<Position>>> = HashMap::default();
            if log.is_none() {
                return hm;
            }
            let log = log.unwrap();
            for (msg, poss) in log.result_a {
                hm.entry(msg).or_insert(Default::default()).result_a = poss.positions;
            }
            for (msg, poss) in log.result_b {
                hm.entry(msg).or_insert(Default::default()).result_b = poss.positions;
            }

            hm
        }

        DiffResult {
            err_diff: propagate_msg(rep.err_eq),
            wrn_diff: propagate_msg(rep.warn_eq),
            trc_diff: propagate_msg(rep.trace_eq),
        }
    }
}

pub async fn diff_parsers(
    folder: PathBuf,
    nix_a: PathBuf,
    nix_b: PathBuf,
) -> color_eyre::Result<DiffResult> {
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
        .map(|file| {
            let nix_a = &nix_a;
            let nix_b = &nix_b;
            async move { diff_file(file.path(), nix_a, nix_b).await }
        })
        .buffer_unordered(10)
        .filter_map(|res| async move { res.unwrap_or_else(|_| None) })
        .collect::<Vec<ParserDiff>>()
        .await;
    let result = DiffResult::from(diffs);
    tracing::info!(?result);
    Ok(result)
}
