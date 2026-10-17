//! Images for the model: loading and sniffing files, keeping them under the
//! providers' size limit, and finding image attachments in a prompt.
//!
//! A prompt attaches an image by naming it: `@shot.png` (workspace-relative),
//! or an absolute / `~` path — which is what dragging a file into a terminal
//! pastes, spaces backslash-escaped or the whole path quoted. Attachments are
//! the user's own act, so unlike tool paths they may point outside the
//! workspace.

use std::path::{Path, PathBuf};

use base64::Engine as _;

use crate::Image;

/// Largest image sent as-is. Providers cap an image at 5 MB; base64 inflates
/// by 4/3, so staying under 3.75 MB of file keeps every provider happy.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;
/// Long edge a too-large image is scaled to (what providers downscale to
/// anyway, so no detail the model would have seen is lost).
const DOWNSCALE_EDGE: u32 = 1568;

const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Media type from magic bytes — the extension can lie, the header cannot.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

pub fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Load an image file for the model, downscaling it first when it is over
/// the size limit.
pub fn load(path: &Path) -> Result<Image, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    from_bytes(bytes, path)
}

pub fn from_bytes(bytes: Vec<u8>, path: &Path) -> Result<Image, String> {
    let Some(media_type) = sniff(&bytes) else {
        return Err(format!(
            "{} is not a PNG, JPEG, GIF or WebP image",
            path.display()
        ));
    };
    let (bytes, media_type) = if bytes.len() > MAX_IMAGE_BYTES {
        downscale(path).ok_or_else(|| {
            format!(
                "{} is {:.1} MB; images over {:.2} MB need `sips` (macOS) or ImageMagick to \
                 downscale, and neither worked",
                path.display(),
                bytes.len() as f64 / 1e6,
                MAX_IMAGE_BYTES as f64 / 1e6
            )
        })?
    } else {
        (bytes, media_type)
    };
    Ok(Image {
        media_type: media_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// Re-encode as a JPEG no larger than [`DOWNSCALE_EDGE`] on its long edge,
/// using whatever the platform has. `None` when nothing worked.
fn downscale(path: &Path) -> Option<(Vec<u8>, &'static str)> {
    let out = std::env::temp_dir().join(format!(
        "hasp-img-{}-{}.jpg",
        std::process::id(),
        crate::session::now_ms()
    ));
    let edge = DOWNSCALE_EDGE.to_string();
    let attempts: [(&str, Vec<std::ffi::OsString>); 2] = [
        (
            "sips",
            vec![
                "-s".into(),
                "format".into(),
                "jpeg".into(),
                "-Z".into(),
                edge.clone().into(),
                path.into(),
                "--out".into(),
                out.clone().into(),
            ],
        ),
        (
            "magick",
            vec![
                path.into(),
                "-resize".into(),
                format!("{edge}x{edge}>").into(),
                "-quality".into(),
                "85".into(),
                out.clone().into(),
            ],
        ),
    ];
    for (bin, args) in attempts {
        let ok = std::process::Command::new(bin)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok && let Ok(bytes) = std::fs::read(&out) {
            let _ = std::fs::remove_file(&out);
            if bytes.len() <= MAX_IMAGE_BYTES && sniff(&bytes) == Some("image/jpeg") {
                return Some((bytes, "image/jpeg"));
            }
        }
    }
    let _ = std::fs::remove_file(&out);
    None
}

/// Whitespace-separated words with their byte spans, honoring backslash
/// escapes and single/double quotes the way a shell (and a terminal's
/// drag-and-drop) writes paths.
fn words(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut word = String::new();
        let mut end = start;
        let mut quote: Option<char> = None;
        while let Some(&(i, c)) = chars.peek() {
            if quote.is_none() && c.is_whitespace() {
                break;
            }
            chars.next();
            end = i + c.len_utf8();
            match (quote, c) {
                (None, '\\') => {
                    if let Some((j, n)) = chars.next() {
                        word.push(n);
                        end = j + n.len_utf8();
                    }
                }
                (None, '\'' | '"') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                _ => word.push(c),
            }
        }
        out.push((start, end, word));
    }
    out
}

fn resolve(word: &str, root: &Path) -> Option<PathBuf> {
    let path = if let Some(rel) = word.strip_prefix('@') {
        root.join(rel)
    } else if let Some(rest) = word.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else if word.starts_with('/') {
        PathBuf::from(word)
    } else {
        return None;
    };
    (has_image_extension(&path) && path.is_file()).then_some(path)
}

/// Pull image attachments out of a prompt. Each attached path is replaced by
/// `[image N: name]` so the text can still refer to it. Files that look like
/// images but fail to load stay in the text and produce a note.
pub fn attachments(prompt: &str, root: &Path) -> (String, Vec<Image>, Vec<String>) {
    let mut text = String::with_capacity(prompt.len());
    let mut images = Vec::new();
    let mut notes = Vec::new();
    let mut last = 0;
    for (start, mut end, mut word) in words(prompt) {
        // Prose puts punctuation after a path: "see @ui.png?" names ui.png.
        let path = loop {
            if let Some(p) = resolve(&word, root) {
                break Some(p);
            }
            match word.pop() {
                Some(c) if ",.;:!?)]}".contains(c) && prompt[..end].ends_with(c) => {
                    end -= c.len_utf8();
                }
                _ => break None,
            }
        };
        let Some(path) = path else {
            continue;
        };
        match load(&path) {
            Ok(img) => {
                images.push(img);
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                text.push_str(&prompt[last..start]);
                text.push_str(&format!("[image {}: {name}]", images.len()));
                last = end;
            }
            Err(e) => notes.push(format!("image not attached: {e}")),
        }
    }
    text.push_str(&prompt[last..]);
    (text, images, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest valid PNG header prefix is enough for sniffing.
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];

    #[test]
    fn sniffs_by_magic_not_extension() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"hello"), None);
    }

    #[test]
    fn words_follow_shell_quoting() {
        let w: Vec<String> = words(r#"look at /tmp/My\ Shot.png and '/a b/c.png' "x y""#)
            .into_iter()
            .map(|(_, _, w)| w)
            .collect();
        assert_eq!(
            w,
            vec!["look", "at", "/tmp/My Shot.png", "and", "/a b/c.png", "x y"]
        );
    }

    #[test]
    fn attaches_dragged_and_mentioned_images() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("dir with space")).unwrap();
        let dragged = root.join("dir with space/Screen Shot.png");
        std::fs::write(&dragged, PNG).unwrap();
        std::fs::write(root.join("ui.png"), PNG).unwrap();
        std::fs::write(root.join("fake.png"), b"not an image").unwrap();

        let escaped = dragged.display().to_string().replace(' ', "\\ ");
        let prompt =
            format!("why is {escaped} different from @ui.png? also @fake.png @missing.png");
        let (text, images, notes) = attachments(&prompt, root);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(
            text,
            "why is [image 1: Screen Shot.png] different from [image 2: ui.png]? also @fake.png @missing.png"
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("not a PNG"));
    }

    #[test]
    fn prompts_without_images_pass_through_untouched() {
        let (text, images, notes) = attachments("fix src/main.rs and /usr/bin/env", Path::new("/"));
        assert_eq!(text, "fix src/main.rs and /usr/bin/env");
        assert!(images.is_empty() && notes.is_empty());
    }
}
