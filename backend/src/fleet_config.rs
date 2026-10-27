//! This app's switches in Desktop's config.yaml ($DIMOS_HOME, else ~/.dimos), which the deck fleet sets per machine:
//!
//! ```yaml
//! apps:
//!   go2-ctrl:
//!     auto-record: true   # record from each connect (the app's default: only when Record is pressed)
//!     auto-upload: true   # overrides the page's Auto-upload at each start
//! ```
//!
//! Desktop keeps keys it doesn't know when it saves the file.

/// `apps.go2-ctrl.<key>` as a bool; None when the file, the key or a true/false value is missing.
pub fn go2_ctrl_flag(key: &str) -> Option<bool> {
    let home = std::env::var_os("DIMOS_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".dimos")))?;
    flag_in(&std::fs::read_to_string(home.join("config.yaml")).ok()?, key)
}

/// `apps.go2-ctrl.<key>` in a config.yaml's text (block style, the way Desktop writes it).
fn flag_in(text: &str, key: &str) -> Option<bool> {
    // the mappings the current line sits inside: (indent, key)
    let mut parents: Vec<(usize, &str)> = Vec::new();
    for line in text.lines() {
        let content = line.split(" #").next().unwrap_or("").trim_end();
        let trimmed = content.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = content.len() - trimmed.len();
        while parents.last().is_some_and(|&(i, _)| i >= indent) {
            parents.pop();
        }
        let Some((name, value)) = trimmed.split_once(':') else { continue };
        let (name, value) = (name.trim().trim_matches(|c| c == '"' || c == '\''), value.trim());
        if value.is_empty() {
            parents.push((indent, name));
        } else if name == key && parents.iter().map(|&(_, k)| k).eq(["apps", "go2-ctrl"]) {
            return match value.trim_matches(|c| c == '"' || c == '\'') {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::flag_in;

    #[test]
    fn reads_nested_flags() {
        let text = "machine_id: deck3-abc\napps:\n  other:\n    auto-record: false\n  go2-ctrl:\n    auto-record: true # fleet\n    auto-upload: \"false\"\nzenoh:\n  auto-record: false\n";
        assert_eq!(flag_in(text, "auto-record"), Some(true));
        assert_eq!(flag_in(text, "auto-upload"), Some(false));
        assert_eq!(flag_in(text, "missing"), None);
        assert_eq!(flag_in("auto-record: true\n", "auto-record"), None);
    }
}
