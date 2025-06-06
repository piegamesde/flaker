use anyhow::{Error, Result};
use std::fmt;
use std::fmt::Formatter;
use thiserror::Error;

#[derive(Debug, Error)]
pub struct ErrorGroup {
    msg: String,
    errors: Vec<Error>,
}

impl fmt::Display for ErrorGroup {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("ErrorGroup({}):\n", self.msg))?;
        f.write_fmt(format_args!("+-+{:->40}\n", "-"))?;
        for (i, err) in self.errors.iter().enumerate() {
            f.write_fmt(format_args!("  +{:-^40}\n", i + 1))?;
            f.write_str("  | ")?;
            err.fmt(f)?;
            f.write_str("\n")?;
        }
        f.write_fmt(format_args!("  +{:->40}", "-"))?;
        Ok(())
    }
}

impl ErrorGroup {
    pub fn add(&mut self, error: Error) {
        self.errors.push(error)
    }

    pub fn has_content(&self) -> bool {
        self.errors.len() > 0
    }

    pub fn add_error_to(self, group: &mut ErrorGroup) -> &mut ErrorGroup {
        if self.has_content() {
            group.add(self.into());
        }
        group
    }

    pub fn to_result(self) -> Result<(), ErrorGroup> {
        self.into()
    }
}

impl Into<Result<(), ErrorGroup>> for ErrorGroup {
    fn into(self) -> Result<(), Self> {
        if self.has_content() {
            Err(self)
        } else {
            Ok(())
        }
    }
}

impl From<String> for ErrorGroup {
    fn from(value: String) -> Self {
        ErrorGroup {
            msg: value,
            errors: vec![],
        }
    }
}

impl From<&str> for ErrorGroup {
    fn from(value: &str) -> Self {
        value.to_string().into()
    }
}

pub trait AddErrorResult<T> {
    fn add_error_to(self, group: &mut ErrorGroup) -> Result<T, &mut ErrorGroup>;
}

impl<T> AddErrorResult<T> for Result<T, Error> {
    fn add_error_to(self, group: &mut ErrorGroup) -> Result<T, &mut ErrorGroup> {
        match self {
            Ok(t) => Ok(t),
            Err(e) => {
                group.add(e.into());
                Err(group)
            }
        }
    }
}

pub trait AddError {
    #[allow(dead_code)]
    fn add_error_to(self, group: &mut ErrorGroup) -> &mut ErrorGroup;
}

impl<T> AddError for T
where
    T: std::error::Error + Sync + Send + 'static,
{
    fn add_error_to(self, group: &mut ErrorGroup) -> &mut ErrorGroup {
        group.add(self.into());
        group
    }
}
