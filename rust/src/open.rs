//! A request to open Claude. Menus return one instead of launching it
//! themselves, so a single place decides between a pane and plain Claude.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    fn only_a_resume_passes_arguments_to_claude() {
        assert!(OpenRequest::fresh("a").claude_args().is_empty());
        assert_eq!(OpenRequest::resume("a", "id").claude_args(), ["--resume", "id"]);
    }
}
