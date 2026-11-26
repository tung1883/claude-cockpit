//! A request to open Claude. Menus return one instead of launching it
//! themselves, so a single place decides between a pane and plain Claude.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Set for the cpit running as the main screen pane: the file it writes its
/// requests to, for the cockpit around it to open.
pub const HOME_PANE_ENV: &str = "CPIT_HOME_PANE";

/// The request file, when this cpit is a main screen pane.
pub fn home_pane() -> Option<PathBuf> {
    std::env::var_os(HOME_PANE_ENV).map(PathBuf::from)
}

/// Leaves `request` in `file`, whole or not at all.
pub fn send(file: &Path, request: &OpenRequest) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = file.with_extension("partial");
    std::fs::write(&partial, serde_json::to_string(request)?)?;
    std::fs::rename(&partial, file)
}

/// Takes the request waiting in `file`, if there is one.
pub fn take(file: &Path) -> Option<OpenRequest> {
    let text = std::fs::read_to_string(file).ok()?;
    let _ = std::fs::remove_file(file);
    serde_json::from_str(&text).ok()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct OpenRequest {
    pub profile: String,
    /// The folder to start in; the cockpit's own folder when None.
    pub dir: Option<PathBuf>,
    /// The session to continue; a new one when None.
    pub resume: Option<String>,
}

impl OpenRequest {
    pub fn fresh(profile: &str) -> OpenRequest {
        OpenRequest { profile: profile.to_string(), dir: None, resume: None }
    }

    pub fn resume(profile: &str, session_id: &str) -> OpenRequest {
        OpenRequest { resume: Some(session_id.to_string()), ..OpenRequest::fresh(profile) }
    }

    /// The arguments plain Claude is started with.
    pub fn claude_args(&self) -> Vec<String> {
        self.resume.iter().flat_map(|id| ["--resume".to_string(), id.clone()]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_survives_being_written_down() {
        let r = OpenRequest { dir: Some("C:/proj".into()), ..OpenRequest::resume("work", "abc") };
        let back: OpenRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn a_sent_request_is_taken_once() {
        let file = std::env::temp_dir().join(format!("cockpit-open-{}", std::process::id())).join("r.json");
        assert_eq!(take(&file), None);
        send(&file, &OpenRequest::fresh("a")).unwrap();
        send(&file, &OpenRequest::fresh("b")).unwrap();
        assert_eq!(take(&file), Some(OpenRequest::fresh("b")), "the newest wins");
        assert_eq!(take(&file), None);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn only_a_resume_passes_arguments_to_claude() {
        assert!(OpenRequest::fresh("a").claude_args().is_empty());
        assert_eq!(OpenRequest::resume("a", "id").claude_args(), ["--resume", "id"]);
    }
}
