//! File type identification for the file manager. **No egui dependency.**
//!
//! Wraps [`mime_guess`] and adds a coarse [`FileCategory`] plus a Windows-style
//! "type name" ("PNG image", "Text document"). The UI maps `FileCategory` to an
//! icon itself, so UI changes never touch this crate.

use std::path::Path;

/// Coarse classification used for icons, grouping and contextual ribbon tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileCategory {
    Image,
    Audio,
    Video,
    Pdf,
    /// Source code and config/markup formats.
    Code,
    /// Plain text (txt, md, log, csv, ...).
    Text,
    /// Office-style documents (docx, odt, xlsx, pptx, epub, ...).
    Document,
    Archive,
    Executable,
    Other,
}

impl FileCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Pdf => "PDF document",
            Self::Code => "Source code",
            Self::Text => "Text document",
            Self::Document => "Document",
            Self::Archive => "Compressed archive",
            Self::Executable => "Application",
            Self::Other => "File",
        }
    }
}

/// Result of [`guess`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKind {
    /// Best-guess MIME type, `application/octet-stream` when unknown.
    pub mime: String,
    pub category: FileCategory,
    /// Lowercased extension without the dot, if any.
    pub extension: Option<String>,
    /// Human readable, e.g. "PNG image", "Rust source file", "File".
    pub type_name: String,
}

/// Extension groups are matched directly rather than scanning several slices for
/// every file. `guess` is called for directory listings, so this keeps the hot
/// path predictable without introducing a runtime lookup table or extra state.
fn categorize(ext: Option<&str>, mime: &str) -> FileCategory {
    // Extension lists win: they are more specific than the MIME top-level type
    // (e.g. `.ts` is "video/mp2t" in some tables but is TypeScript to users).
    if let Some(e) = ext {
        if matches!(
            e,
            "rs" | "c"
                | "h"
                | "cc"
                | "cpp"
                | "hpp"
                | "cs"
                | "py"
                | "js"
                | "mjs"
                | "ts"
                | "tsx"
                | "jsx"
                | "json"
                | "toml"
                | "yaml"
                | "yml"
                | "html"
                | "htm"
                | "css"
                | "scss"
                | "sh"
                | "bash"
                | "zsh"
                | "fish"
                | "go"
                | "java"
                | "kt"
                | "swift"
                | "rb"
                | "php"
                | "lua"
                | "sql"
                | "xml"
                | "gradle"
                | "cmake"
                | "mk"
                | "nix"
        ) {
            return FileCategory::Code;
        }
        if matches!(
            e,
            "zip"
                | "tar"
                | "gz"
                | "tgz"
                | "xz"
                | "txz"
                | "bz2"
                | "tbz2"
                | "7z"
                | "rar"
                | "zst"
                | "lz4"
                | "deb"
                | "rpm"
                | "iso"
                | "jar"
        ) {
            return FileCategory::Archive;
        }
        if matches!(
            e,
            "exe" | "bin" | "appimage" | "run" | "msi" | "apk" | "flatpakref"
        ) {
            return FileCategory::Executable;
        }
        if matches!(
            e,
            "doc"
                | "docx"
                | "odt"
                | "rtf"
                | "xls"
                | "xlsx"
                | "ods"
                | "ppt"
                | "pptx"
                | "odp"
                | "epub"
                | "mobi"
        ) {
            return FileCategory::Document;
        }
        if matches!(
            e,
            "txt"
                | "md"
                | "markdown"
                | "log"
                | "csv"
                | "tsv"
                | "ini"
                | "conf"
                | "cfg"
                | "rst"
                | "tex"
        ) {
            return FileCategory::Text;
        }
        if e == "pdf" {
            return FileCategory::Pdf;
        }
    }

    match mime.split('/').next().unwrap_or("") {
        "image" => FileCategory::Image,
        "audio" => FileCategory::Audio,
        "video" => FileCategory::Video,
        "text" => FileCategory::Text,
        _ if mime == "application/pdf" => FileCategory::Pdf,
        _ => FileCategory::Other,
    }
}

/// Identify a file by its name only (never touches the disk).
pub fn guess(path: &Path) -> FileKind {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .map(|e| e.to_ascii_lowercase());
    let mime = mime_guess::from_path(path)
        .first()
        .map(|m| m.essence_str().to_owned())
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let category = categorize(extension.as_deref(), &mime);
    let type_name = type_name(extension.as_deref(), category);
    FileKind {
        mime,
        category,
        extension,
        type_name,
    }
}

/// Convenience: just the category.
pub fn category_of(path: &Path) -> FileCategory {
    guess(path).category
}

fn type_name(ext: Option<&str>, cat: FileCategory) -> String {
    // Avoid allocating an uppercase extension for labels that do not use it.
    if cat == FileCategory::Pdf {
        return "PDF document".to_owned();
    }
    let Some(ext) = ext else {
        return match cat {
            FileCategory::Other => "File".to_owned(),
            other => other.label().to_owned(),
        };
    };
    let upper = ext.to_ascii_uppercase();
    match cat {
        FileCategory::Other => format!("{upper} file"),
        FileCategory::Code => format!("{upper} source file"),
        FileCategory::Image => format!("{upper} image"),
        FileCategory::Audio => format!("{upper} audio file"),
        FileCategory::Video => format!("{upper} video"),
        FileCategory::Archive => format!("{upper} archive"),
        FileCategory::Document => format!("{upper} document"),
        FileCategory::Executable => format!("{upper} application"),
        FileCategory::Text if upper == "TXT" => "Text document".to_owned(),
        FileCategory::Text => format!("{upper} text file"),
        FileCategory::Pdf => "PDF document".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str) -> FileKind {
        guess(Path::new(p))
    }

    #[test]
    fn images_audio_video() {
        assert_eq!(g("a/b.PNG").category, FileCategory::Image);
        assert_eq!(g("a/b.PNG").mime, "image/png");
        assert_eq!(g("a/b.PNG").type_name, "PNG image");
        assert_eq!(g("x.mp3").category, FileCategory::Audio);
        assert_eq!(g("x.mp4").category, FileCategory::Video);
    }

    #[test]
    fn extension_lists_beat_mime() {
        assert_eq!(g("main.ts").category, FileCategory::Code);
        assert_eq!(g("lib.rs").category, FileCategory::Code);
        assert_eq!(g("lib.rs").type_name, "RS source file");
        assert_eq!(g("a.tar").category, FileCategory::Archive);
        assert_eq!(g("setup.AppImage").category, FileCategory::Executable);
    }

    #[test]
    fn text_pdf_documents() {
        assert_eq!(g("notes.txt").type_name, "Text document");
        assert_eq!(g("r.pdf").category, FileCategory::Pdf);
        assert_eq!(g("r.docx").category, FileCategory::Document);
    }

    #[test]
    fn extension_categories_take_precedence_over_mime() {
        assert_eq!(g("main.ts").category, FileCategory::Code);
        assert_eq!(g("notes.csv").category, FileCategory::Text);
        assert_eq!(g("package.flatpakref").category, FileCategory::Executable);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_extension_is_handled_without_panicking() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"file.\xff"));
        let kind = guess(path);
        assert_eq!(kind.extension, None);
        assert_eq!(kind.category, FileCategory::Other);
        assert_eq!(kind.type_name, "File");
    }

    #[test]
    fn unknown_and_no_extension() {
        let k = g("Makefile");
        assert_eq!(k.category, FileCategory::Other);
        assert_eq!(k.mime, "application/octet-stream");
        assert_eq!(k.type_name, "File");
        assert_eq!(k.extension, None);
        assert_eq!(g("weird.zzzzq").type_name, "ZZZZQ file");
        assert_eq!(g(".bashrc").extension, None); // dotfile, not an extension
    }
}
