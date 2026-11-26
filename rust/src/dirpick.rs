//! Folder completion for the directory prompts: the folders under what has been
//! typed, filtered by the part after the last separator. Pure apart from
//! reading a directory.

use std::path::Path;

/// `input` as the folder part (up to and with the last separator) and the name
/// being typed after it.
pub fn split(input: &str) -> (&str, &str) {
    match input.rfind(['/', '\\']) {
        Some(i) => input.split_at(i + 1),
        None => ("", input),
    }
}

/// The separator to continue `input` with: the one it already uses.
fn separator(input: &str) -> char {
    input.chars().rev().find(|c| matches!(c, '/' | '\\')).unwrap_or(std::path::MAIN_SEPARATOR)
}

/// `input` ending in a separator, so that its folders are listed.
pub fn open(input: &str) -> String {
    if input.is_empty() || input.ends_with(['/', '\\']) {
        input.to_string()
    } else {
        format!("{input}{}", separator(input))
    }
}

/// `input` with the name being typed replaced by `name`, ready to go further.
pub fn accept(input: &str, name: &str) -> String {
    let (parent, _) = split(input);
    format!("{parent}{name}{}", separator(input))
}

/// One folder up: drops the name being typed, or the last folder.
pub fn up(input: &str) -> String {
    let (parent, name) = split(input);
    if !name.is_empty() {
        return parent.to_string();
    }
    let (above, _) = split(parent.trim_end_matches(['/', '\\']));
    if above.is_empty() {
        input.to_string()
    } else {
        above.to_string()
    }
}

/// The names of the folders in `dir` that start with `prefix`, ignoring case.
/// Dot folders show only once a dot has been typed.
pub fn matches(dir: &Path, prefix: &str) -> Vec<String> {
    let wanted = prefix.to_lowercase();
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        // A link to a folder counts; `file_type` alone would call it a link.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir())))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.to_lowercase().starts_with(&wanted) && (wanted.starts_with('.') || !n.starts_with('.')))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// The slice of `count` entries to draw in `room` rows with `selected` in it.
pub fn window(count: usize, selected: usize, room: usize) -> std::ops::Range<usize> {
    let room = room.max(1);
    let start = selected.saturating_sub(room / 2).min(count.saturating_sub(room));
    start..(start + room).min(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_the_last_separator() {
        assert_eq!(split("C:/Users/Tung/Docu"), ("C:/Users/Tung/", "Docu"));
        assert_eq!(split(r"C:\Users\"), (r"C:\Users\", ""));
        assert_eq!(split("Docu"), ("", "Docu"));
    }

    #[test]
    fn accepting_and_going_up_keep_the_separator_in_use() {
        assert_eq!(accept("C:/Users/Tung/Docu", "Documents"), "C:/Users/Tung/Documents/");
        assert_eq!(accept(r"C:\Users\", "Tung"), r"C:\Users\Tung\");
        assert_eq!(open("C:/Users"), "C:/Users/");
        assert_eq!(open(r"C:\Users\"), r"C:\Users\");
        assert_eq!(up("C:/Users/Tung/Docu"), "C:/Users/Tung/");
        assert_eq!(up("C:/Users/Tung/"), "C:/Users/");
        assert_eq!(up("C:/"), "C:/");
    }

    #[test]
    fn lists_folders_by_prefix_ignoring_case() {
        let root = std::env::temp_dir().join(format!("cockpit-dirpick-{}", std::process::id()));
        for d in ["Documents", "docs", "Downloads", ".git"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("Document.txt"), "").unwrap();
        assert_eq!(matches(&root, "doc"), vec!["docs", "Documents"]);
        assert_eq!(matches(&root, ""), vec!["docs", "Documents", "Downloads"]);
        assert_eq!(matches(&root, ".g"), vec![".git"]);
        assert!(matches(&root.join("nowhere"), "").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_window_follows_the_selection() {
        assert_eq!(window(3, 0, 5), 0..3);
        assert_eq!(window(20, 0, 5), 0..5);
        assert_eq!(window(20, 10, 5), 8..13);
        assert_eq!(window(20, 19, 5), 15..20);
        assert_eq!(window(0, 0, 5), 0..0);
    }
}
