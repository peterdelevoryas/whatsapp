//! `GET /health`: an unauthenticated summary for an external monitor. It
//! reports whether the message log answers and how full its disk is: 200 when
//! all is well, 503 with the problems otherwise. It reveals no messages.

use std::{path::PathBuf, sync::Arc};

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;

use crate::store::Store;

#[derive(Clone)]
pub struct Health(Arc<Config>);

pub struct Config {
    pub store: Store,
    /// Directory whose filesystem is checked for free space (the database's).
    pub data_dir: PathBuf,
    pub max_disk_percent: f64,
}

impl Health {
    pub fn new(config: Config) -> Self {
        Self(Arc::new(config))
    }
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Report {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_used_percent: Option<f64>,
    pub problems: Vec<String>,
}

pub async fn handler(State(Health(c)): State<Health>) -> (StatusCode, Json<Report>) {
    let mut problems = Vec::new();
    let messages = match c.store.count().await {
        Ok(n) => Some(n),
        Err(e) => {
            problems.push(format!("database: {e:#}"));
            None
        }
    };
    let disk = match disk_used_percent(&c.data_dir) {
        Ok(used) => Some(used),
        Err(e) => {
            problems.push(format!("disk: {e}"));
            None
        }
    };
    if let Some(used) = disk
        && used > c.max_disk_percent
    {
        problems.push(format!(
            "disk is {used:.0}% full (limit {:.0}%)",
            c.max_disk_percent
        ));
    }
    let report = Report {
        ok: problems.is_empty(),
        messages,
        disk_used_percent: disk.map(|d| d.round()),
        problems,
    };
    let status = if report.ok {
        StatusCode::OK
    } else {
        tracing::warn!(problems = ?report.problems, "health check failing");
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(report))
}

fn disk_used_percent(dir: &std::path::Path) -> std::io::Result<f64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a valid out-pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let total = stat.f_blocks as f64;
    let available = stat.f_bavail as f64;
    if total == 0.0 {
        return Ok(0.0);
    }
    Ok((total - available) / total * 100.0)
}
