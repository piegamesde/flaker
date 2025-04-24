use crate::diffing::{Diff, ParserDiff};

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

pub fn report(mut diffs: Vec<ParserDiff>) {
    if diffs.is_empty() {
        tracing::info!("All Clear!");
        return;
    }

    tracing::error!("Parsers differ!");
    let total_cnt = diffs.len();
    let err_cnt = diffs.iter().filter(|d| d.err_eq.is_some()).count();
    let wrn_cnt = diffs.iter().filter(|d| d.warn_eq.is_some()).count();
    let trc_cnt = diffs.iter().filter(|d| d.trace_eq.is_some()).count();

    tracing::info!(
        "{} outputs differ; {} error, {} warn and {} trace diffs",
        total_cnt,
        err_cnt,
        wrn_cnt,
        trc_cnt
    );

    diffs.iter().for_each(|diff| {
        tracing::debug!(?diff);
    });

    let rep = diffs.into_iter().reduce(|mut acc, diff| {
        acc.merge(diff);
        acc
    });

    tracing::info!(?rep);
}
