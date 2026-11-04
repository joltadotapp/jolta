//! File-type icons for attachments: a page with the format's short name on
//! a muted color band per format family. One SVG serves the reader's HTML
//! and the app's own views.

/// A format family, from the file name or else the MIME type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Pdf,
    Word,
    Sheet,
    Slides,
    Calendar,
    Web,
    Text,
    Other,
}

/// (extension, family, label). The label is what the page says.
const KNOWN: &[(&str, Family, &str)] = &[
    ("pdf", Family::Pdf, "PDF"),
    ("doc", Family::Word, "DOC"),
    ("docx", Family::Word, "DOC"),
    ("odt", Family::Word, "ODT"),
    ("rtf", Family::Word, "RTF"),
    ("pages", Family::Word, "PAGES"),
    ("xls", Family::Sheet, "XLS"),
    ("xlsx", Family::Sheet, "XLS"),
    ("csv", Family::Sheet, "CSV"),
    ("ods", Family::Sheet, "ODS"),
    ("numbers", Family::Sheet, "NUM"),
    ("ppt", Family::Slides, "PPT"),
    ("pptx", Family::Slides, "PPT"),
    ("odp", Family::Slides, "ODP"),
    ("key", Family::Slides, "KEY"),
    ("ics", Family::Calendar, "ICS"),
    ("ical", Family::Calendar, "ICS"),
    ("vcs", Family::Calendar, "VCS"),
    ("html", Family::Web, "HTML"),
    ("htm", Family::Web, "HTML"),
    ("txt", Family::Text, "TXT"),
    ("log", Family::Text, "LOG"),
    ("md", Family::Text, "MD"),
    ("markdown", Family::Text, "MD"),
];

const BY_MIME: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    ("text/calendar", "ics"),
    ("text/html", "html"),
    ("text/markdown", "md"),
    ("text/plain", "txt"),
    ("text/csv", "csv"),
    ("application/msword", "doc"),
    ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "docx"),
    ("application/vnd.ms-excel", "xls"),
    ("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "xlsx"),
    ("application/vnd.ms-powerpoint", "ppt"),
    ("application/vnd.openxmlformats-officedocument.presentationml.presentation", "pptx"),
];

impl Family {
    /// The band's color, muted so it sits beside the calendar colors.
    fn color(self) -> Option<&'static str> {
        match self {
            Family::Pdf => Some("#d0675e"),
            Family::Word => Some("#6f8fd6"),
            Family::Sheet => Some("#5fae7e"),
            Family::Slides => Some("#d68a5a"),
            Family::Calendar => Some("#b07fd0"),
            Family::Web => Some("#5aa8c2"),
            Family::Text => Some("#9a9aa3"),
            Family::Other => None,
        }
    }
}

/// The family and the short name a file's page shows. Unknown formats show
/// their extension when it is short, on a page without a band.
pub fn kind(name: &str, mime: &str) -> (Family, String) {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let from_mime = BY_MIME.iter().find(|(m, _)| *m == mime).map(|(_, e)| *e);
    for candidate in [Some(ext.as_str()), from_mime].into_iter().flatten() {
        if let Some((_, family, label)) = KNOWN.iter().find(|(e, _, _)| *e == candidate) {
            return (*family, label.to_string());
        }
    }
    let label = if (1..=4).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) { ext.to_ascii_uppercase() } else { String::new() };
    (Family::Other, label)
}

/// A 20 by 24 icon for the file. `outline` is any CSS color for the page's
/// edge, such as `currentColor` in the reader or a theme color in the app.
pub fn svg(name: &str, mime: &str, outline: &str) -> String {
    let (family, label) = kind(name, mime);
    let outline = escape(outline);
    let page = format!(
        "<path d=\"M3 1.5h9.5L17.5 6.5V21a1.5 1.5 0 0 1-1.5 1.5H4A1.5 1.5 0 0 1 2.5 21V3A1.5 1.5 0 0 1 4 1.5Z\" fill=\"none\" stroke=\"{outline}\" stroke-width=\"1.2\"/>\
         <path d=\"M12.5 1.5V6.5h5\" fill=\"none\" stroke=\"{outline}\" stroke-width=\"1.2\"/>"
    );
    let size = match label.len() {
        0..=3 => "5.6",
        4 => "4.6",
        _ => "3.8",
    };
    let (band, ink) = match family.color() {
        Some(c) => (format!("<rect x=\"1.5\" y=\"12\" width=\"17\" height=\"7.5\" rx=\"1.5\" fill=\"{c}\"/>"), "#121214".to_string()),
        None => (String::new(), outline.clone()),
    };
    let text = if label.is_empty() {
        String::new()
    } else {
        format!(
            "<text x=\"10\" y=\"17.9\" text-anchor=\"middle\" font-family=\"-apple-system,system-ui,sans-serif\" \
             font-weight=\"700\" font-size=\"{size}\" fill=\"{ink}\">{label}</text>"
        )
    };
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"24\" viewBox=\"0 0 20 24\" aria-hidden=\"true\">{page}{band}{text}</svg>")
}

fn escape(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || "#(),. %-".contains(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_types_pick_a_family() {
        let cases = [
            ("Offerte Q1.pdf", "application/octet-stream", Family::Pdf, "PDF"),
            ("Contract v3.DOCX", "", Family::Word, "DOC"),
            ("Budget.xlsx", "", Family::Sheet, "XLS"),
            ("export.csv", "", Family::Sheet, "CSV"),
            ("Kick-off.pptx", "", Family::Slides, "PPT"),
            ("invite.ics", "", Family::Calendar, "ICS"),
            ("attachment", "text/calendar; method=REQUEST", Family::Calendar, "ICS"),
            ("page.htm", "", Family::Web, "HTML"),
            ("notes.md", "", Family::Text, "MD"),
            ("readme.txt", "", Family::Text, "TXT"),
            ("scan", "application/pdf", Family::Pdf, "PDF"),
            ("photo.heic", "image/heic", Family::Other, "HEIC"),
            ("archive.tar.gz", "", Family::Other, "GZ"),
            ("no-extension", "application/octet-stream", Family::Other, ""),
        ];
        for (name, mime, family, label) in cases {
            assert_eq!(kind(name, mime), (family, label.to_string()), "{name}");
        }
    }

    #[test]
    fn the_svg_is_one_element_with_the_label_and_a_safe_outline() {
        let s = svg("a.pdf", "", "currentColor");
        assert!(s.starts_with("<svg") && s.ends_with("</svg>"));
        assert!(s.contains(">PDF</text>") && s.contains("#d0675e") && s.contains("stroke=\"currentColor\""));
        assert!(!svg("a.pdf", "", "red\"/><script>").contains("<script>"));
        assert!(!svg("x", "", "#aaa").contains("<text"), "no label, no text");
    }
}
