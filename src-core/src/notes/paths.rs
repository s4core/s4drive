use crate::error::{CoreError, CoreResult};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

pub const NOTES_ROOT_NAME: &str = "Notes";
pub const MAX_COMPONENT_SCALARS: usize = 120;
pub const MAX_COMPONENT_BYTES: usize = 240;
pub const MAX_FOLDER_DEPTH: usize = 32;
pub const MAX_RELATIVE_PATH_BYTES: usize = 1024;

pub fn notes_root(sync_root: &Path) -> PathBuf {
    sync_root.join(NOTES_ROOT_NAME)
}

pub fn validate_component(name: &str) -> CoreResult<()> {
    validate_portable_component(name)?;
    if name.chars().count() > MAX_COMPONENT_SCALARS || name.len() > MAX_COMPONENT_BYTES {
        return Err(CoreError::InvalidName("This name is too long.".to_owned()));
    }
    let portable_name = name.to_ascii_lowercase();
    if portable_name.starts_with(".s4drive") || portable_name == ".md" {
        return Err(CoreError::InvalidName("This name is reserved.".to_owned()));
    }
    Ok(())
}

/// Checks a name needs to mean exactly one entry on every supported OS. Unlike
/// `validate_component` it has no Notes UI limits, so any name a user may keep
/// next to notes (hidden, long, any extension) passes.
pub fn validate_portable_component(name: &str) -> CoreResult<()> {
    let invalid = |reason: &str| CoreError::InvalidName(reason.to_owned());
    if name.trim().is_empty() || matches!(name, "." | "..") {
        return Err(invalid("Enter a name."));
    }
    if name.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
            )
    }) {
        return Err(invalid("This name contains unsupported characters."));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(invalid("Names cannot end with a dot or space."));
    }
    let portable_name = name.to_ascii_lowercase();
    let stem = portable_name.split('.').next().unwrap_or("");
    let reserved_device = matches!(stem, "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if reserved_device {
        return Err(invalid("This name is reserved by your operating system."));
    }
    Ok(())
}

pub fn note_filename(title: &str) -> CoreResult<String> {
    let mut filename: String = title.nfc().collect();
    validate_component(&filename)?;
    if filename.to_ascii_lowercase().ends_with(".md") {
        filename.truncate(filename.len() - 3);
        if filename.to_ascii_lowercase().ends_with(".md") {
            return Err(CoreError::InvalidName(
                "Use only one .md extension.".to_owned(),
            ));
        }
        validate_component(&filename)?;
    }
    filename.push_str(".md");
    validate_component(&filename)?;
    Ok(filename)
}

pub fn portable_name_key(name: &str) -> String {
    name.nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .nfc()
        .collect()
}

/// Validate a locator relative to the sync root; the repository checks ancestry by id.
/// The final component is a leaf, so folder callers must also count that component.
pub fn validate_relative_path(relative_path: &str) -> CoreResult<()> {
    if relative_path.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(CoreError::InvalidName("This path is too long.".to_owned()));
    }
    let mut components = relative_path.split('/');
    if components.next() != Some(NOTES_ROOT_NAME) {
        return Err(CoreError::OutsideNotes);
    }
    let mut child_count: usize = 0;
    for component in components {
        validate_component(component)?;
        child_count += 1;
    }
    if child_count.saturating_sub(1) > MAX_FOLDER_DEPTH {
        return Err(CoreError::InvalidName(
            "This folder is nested too deeply.".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_always_the_fixed_notes_child() {
        assert_eq!(notes_root(Path::new("/sync")), Path::new("/sync/Notes"));
        assert_eq!(notes_root(Path::new("sync")), Path::new("sync/Notes"));
    }

    #[test]
    fn components_reject_unsafe_portable_names() {
        for name in [
            "",
            " ",
            "\t",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a\0b",
            "a\nb",
            "a\u{0085}b",
            "a<b",
            "a>b",
            "a:b",
            "a\"b",
            "a|b",
            "a?b",
            "a*b",
            "tail.",
            "tail ",
            "CON",
            "con.txt",
            "PRN",
            "AUX.txt",
            "NUL",
            "COM1",
            "com9.txt",
            "LPT1",
            "lpt9.md",
            ".s4drive",
            ".S4Drive-cache",
            ".md",
            ".MD",
        ] {
            assert!(
                matches!(validate_component(name), Err(CoreError::InvalidName(_))),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn components_preserve_unicode_and_allowed_names() {
        for name in [
            "Планы 🚀",
            "cafe\u{0301}",
            "CONSOLE",
            "COM10",
            "LPT0",
            ".hidden",
            " leading",
            "a.b",
        ] {
            assert!(validate_component(name).is_ok(), "rejected {name:?}");
        }
    }

    #[test]
    fn components_enforce_scalar_and_utf8_byte_limits() {
        assert!(validate_component(&"a".repeat(120)).is_ok());
        assert!(validate_component(&"a".repeat(121)).is_err());
        assert!(validate_component(&"é".repeat(120)).is_ok());
        assert!(validate_component(&"🚀".repeat(60)).is_ok());
        assert!(validate_component(&"🚀".repeat(61)).is_err());
    }

    #[test]
    fn filenames_normalize_nfc_and_add_one_markdown_extension() {
        assert_eq!(note_filename("Планы 🚀").unwrap(), "Планы 🚀.md");
        assert_eq!(note_filename("cafe\u{0301}").unwrap(), "café.md");
        assert_eq!(note_filename("Draft.md").unwrap(), "Draft.md");
        assert_eq!(note_filename("Draft.MD").unwrap(), "Draft.md");
        assert_eq!(note_filename("Draft.txt").unwrap(), "Draft.txt.md");
    }

    #[test]
    fn filenames_reject_repeated_extensions_and_unsafe_titles() {
        for title in [
            "Draft.md.md",
            "Draft.MD.md",
            ".md",
            "",
            "a/b",
            "CON",
            "tail.",
            "tail ",
        ] {
            assert!(
                matches!(note_filename(title), Err(CoreError::InvalidName(_))),
                "accepted {title:?}"
            );
        }
    }

    #[test]
    fn filenames_apply_limits_after_adding_extension() {
        assert!(note_filename(&"a".repeat(117)).is_ok());
        assert!(note_filename(&"a".repeat(118)).is_err());
        assert!(note_filename(&"🚀".repeat(59)).is_ok());
        assert!(note_filename(&"🚀".repeat(60)).is_err());
        assert!(note_filename(&format!("{}.md", "a".repeat(117))).is_ok());
        assert_eq!(
            note_filename(&"e\u{0301}".repeat(117)).unwrap(),
            format!("{}.md", "é".repeat(117))
        );
    }

    #[test]
    fn filenames_reject_whitespace_stems_with_markdown_extension() {
        assert!(matches!(
            note_filename(" .md"),
            Err(CoreError::InvalidName(_))
        ));
    }

    #[test]
    fn filenames_reject_trailing_dot_stems_with_markdown_extension() {
        assert!(matches!(
            note_filename("tail..md"),
            Err(CoreError::InvalidName(_))
        ));
    }

    #[test]
    fn portable_collision_keys_normalize_unicode_and_ignore_case() {
        assert_eq!(
            portable_name_key("CAFÉ.md"),
            portable_name_key("cafe\u{0301}.MD")
        );
        assert_eq!(portable_name_key("ПЛАН.md"), portable_name_key("план.md"));
        assert_ne!(
            portable_name_key("first.md"),
            portable_name_key("second.md")
        );
    }

    #[test]
    fn relative_paths_reject_absolute_traversal_and_separators() {
        assert!(validate_relative_path("Notes").is_ok());
        assert!(validate_relative_path("Notes/Work/Meeting.md").is_ok());
        for path in [
            "",
            "Notes2/a.md",
            "notes/a.md",
            "/Notes/a.md",
            "Notes//a.md",
            "Notes/../a.md",
            "Notes/./a.md",
            "Notes\\a.md",
            "C:/Notes/a.md",
            "Notes/a.md/",
        ] {
            assert!(validate_relative_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn relative_paths_enforce_depth_and_total_bytes() {
        let depth32 = format!("Notes/{}a.md", "folder/".repeat(32));
        let depth33 = format!("Notes/{}a.md", "folder/".repeat(33));
        assert!(validate_relative_path(&depth32).is_ok());
        assert!(validate_relative_path(&depth33).is_err());
        let component = "a".repeat(120);
        let length1024 = format!(
            "Notes/{}{}/a.md",
            format!("{component}/").repeat(8),
            "a".repeat(45)
        );
        assert_eq!(length1024.len(), 1024);
        assert!(validate_relative_path(&length1024).is_ok());
        assert!(validate_relative_path(&length1024.replace("/a.md", "/aa.md")).is_err());
    }
}
