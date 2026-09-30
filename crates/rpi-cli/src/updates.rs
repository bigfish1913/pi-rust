//! rpi self-update and release notification support.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    pub name: String,
    pub current: String,
    pub latest: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateWarning {
    pub message: String,
    pub command: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub notices: Vec<UpdateNotice>,
    pub warnings: Vec<UpdateWarning>,
}

impl UpdateReport {
    pub fn is_empty(&self) -> bool {
        self.notices.is_empty() && self.warnings.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    crate_info: CrateInfo,
}
#[derive(Debug, Deserialize)]
struct CrateInfo {
    max_version: String,
}

fn updates_disabled() -> bool {
    crate::args::offline_env_enabled() || std::env::var_os("RPI_DISABLE_UPDATE_CHECK").is_some()
}

pub async fn check_rpi_startup() -> UpdateReport {
    if updates_disabled() {
        return UpdateReport::default();
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
    {
        Ok(c) => c,
        Err(_) => return UpdateReport::default(),
    };
    let url = "https://crates.io/api/v1/crates/rpi-cli";
    let latest = match client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(response) => match response.json::<CratesResponse>().await {
            Ok(v) => v.crate_info.max_version,
            Err(_) => return UpdateReport::default(),
        },
        Err(_) => return UpdateReport::default(),
    };
    if is_newer(crate::VERSION, &latest) {
        UpdateReport {
            notices: vec![UpdateNotice {
                name: "rpi".into(),
                current: crate::VERSION.into(),
                latest,
                command: "rpi update".into(),
            }],
            warnings: Vec::new(),
        }
    } else {
        UpdateReport::default()
    }
}

pub fn is_newer(current: &str, latest: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(latest),
    ) {
        (Ok(a), Ok(b)) => b > a,
        _ => current != latest,
    }
}

pub fn print_startup_notices(report: &UpdateReport) {
    for notice in &report.notices {
        eprintln!(
            "update available: {} {} -> {} ({})",
            notice.name, notice.current, notice.latest, notice.command
        );
    }
    for warning in &report.warnings {
        eprintln!("warning: {} ({})", warning.message, warning.command);
    }
}

pub fn run_self_update(_args: &[String]) -> i32 {
    let status = std::process::Command::new("cargo")
        .args(["install", "rpi-cli", "--locked"])
        .status();
    match status {
        Ok(s) if s.success() => 0,
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("rpi update failed: {e}");
            1
        }
    }
}
