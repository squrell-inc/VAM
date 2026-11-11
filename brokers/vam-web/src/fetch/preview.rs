//! The preview the model reads: the sections of the page that best match
//! `focus`, or the top of the page.

/// Largest preview, bytes.
pub const MAX_PREVIEW_BYTES: usize = 4 * 1024;
/// Between two chosen sections that are not neighbours.
const GAP: &str = "\n\n[…]\n\n";
/// Below this many spare bytes another section is not worth adding.
const MIN_SECTION_ROOM: usize = 120;

/// A chosen preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub text: String,
    /// True when `focus` matched at least one section.
    pub focus_matched: bool,
}

/// Cut `text` to at most `max` bytes at a paragraph or line boundary.
pub fn cut_at(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let window = &text[..end];
    let floor = end - end / 4;
    if let Some(i) = window.rfind("\n\n").filter(|i| *i >= floor) {
        return &window[..i];
    }
    if let Some(i) = window.rfind('\n').filter(|i| *i >= floor) {
        return &window[..i];
    }
    window
}

/// Split markdown into sections that start at a heading outside code fences.
fn sections(md: &str) -> Vec<(usize, usize)> {
    let mut starts = vec![0usize];
    let mut in_fence = false;
    let mut offset = 0;
    for line in md.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && offset > 0 {
            let hashes = t.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
                starts.push(offset);
            }
        }
        offset += line.len();
    }
    starts.dedup();
    let mut out = Vec::with_capacity(starts.len());
    for (i, s) in starts.iter().enumerate() {
        let e = starts.get(i + 1).copied().unwrap_or(md.len());
        if e > *s {
            out.push((*s, e));
        }
    }
    out
}

fn focus_words(focus: &str) -> Vec<String> {
    let lower = focus.to_lowercase();
    let split = |min: usize| -> Vec<String> {
        let mut words: Vec<String> = Vec::new();
        for w in lower.split(|c: char| !c.is_alphanumeric()) {
            if w.chars().count() >= min && !words.iter().any(|x| x == w) {
                words.push(w.to_string());
            }
        }
        words
    };
    let words = split(3);
    if words.is_empty() {
        split(2)
    } else {
        words
    }
}

fn score(section: &str, words: &[String], phrase: Option<&str>) -> usize {
    let lower = section.to_lowercase();
    let heading = lower.lines().next().unwrap_or("");
    let mut total = 0;
    for w in words {
        total += lower.matches(w.as_str()).count() + 3 * heading.matches(w.as_str()).count();
    }
    if let Some(p) = phrase {
        total += 5 * lower.matches(p).count();
    }
    total
}

/// Choose the preview of `markdown` (at most `max` bytes).
pub fn select(markdown: &str, focus: Option<&str>, max: usize) -> Preview {
    let top = |matched: bool| {
        let cut = cut_at(markdown, max.saturating_sub(6));
        let mut text = cut.trim_end().to_string();
        if cut.len() < markdown.len() {
            text.push_str("\n[…]");
        }
        Preview {
            text,
            focus_matched: matched,
        }
    };
    let Some(focus) = focus.map(str::trim).filter(|f| !f.is_empty()) else {
        return top(false);
    };
    let words = focus_words(focus);
    let phrase = focus
        .contains(char::is_whitespace)
        .then(|| focus.to_lowercase());
    if words.is_empty() {
        return top(false);
    }
    let secs = sections(markdown);
    let mut ranked: Vec<(usize, usize)> = secs
        .iter()
        .enumerate()
        .map(|(i, (s, e))| (score(&markdown[*s..*e], &words, phrase.as_deref()), i))
        .filter(|(sc, _)| *sc > 0)
        .collect();
    if ranked.is_empty() {
        return top(false);
    }
    // Best score first, earlier section on a tie.
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    let mut chosen: Vec<(usize, String)> = Vec::new();
    let mut used = 0usize;
    for (_, idx) in ranked {
        let (s, e) = secs[idx];
        let text = markdown[s..e].trim();
        let room = max.saturating_sub(used + GAP.len());
        if room < MIN_SECTION_ROOM {
            break;
        }
        if text.len() <= room {
            used += text.len() + GAP.len();
            chosen.push((idx, text.to_string()));
        } else if chosen.is_empty() {
            let cut = cut_at(text, room.saturating_sub(6));
            used += cut.len() + 6 + GAP.len();
            chosen.push((idx, format!("{}\n[…]", cut.trim_end())));
        }
    }
    chosen.sort_by_key(|(idx, _)| *idx);
    let mut text = String::new();
    let mut prev: Option<usize> = None;
    for (idx, part) in chosen {
        match prev {
            None => {}
            Some(p) if p + 1 == idx => text.push_str("\n\n"),
            Some(_) => text.push_str(GAP),
        }
        text.push_str(&part);
        prev = Some(idx);
    }
    // Guard the bound whatever the arithmetic above did.
    let text = cut_at(&text, max).to_string();
    Preview {
        text,
        focus_matched: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        let mut md = String::from("# Guide\n\nIntro about many things.\n\n");
        md.push_str("## Installation\n\nRun the installer and reboot.\n\n");
        md.push_str(
            "## Pricing\n\nThe pricing plans are Free, Pro and Team. Pricing is monthly.\n\n",
        );
        md.push_str("## History\n\n");
        md.push_str(&"Long unrelated history text. ".repeat(300));
        md.push_str("\n\n## FAQ\n\nQuestion about pricing discounts? Ask sales.\n");
        md
    }

    #[test]
    fn no_focus_returns_top_of_page_within_bound() {
        let md = page();
        let p = select(&md, None, MAX_PREVIEW_BYTES);
        assert!(!p.focus_matched);
        assert!(p.text.starts_with("# Guide"));
        assert!(p.text.len() <= MAX_PREVIEW_BYTES);
        assert!(p.text.ends_with("[…]"));
    }

    #[test]
    fn focus_picks_matching_sections_in_document_order() {
        let md = page();
        let p = select(&md, Some("pricing plans"), MAX_PREVIEW_BYTES);
        assert!(p.focus_matched);
        assert!(p.text.contains("## Pricing"));
        assert!(p.text.contains("Free, Pro and Team"));
        assert!(p.text.contains("## FAQ"));
        assert!(!p.text.contains("Run the installer"));
        assert!(!p.text.contains("Long unrelated"));
        assert!(p.text.find("## Pricing") < p.text.find("## FAQ"));
        assert!(p.text.len() <= MAX_PREVIEW_BYTES);
    }

    #[test]
    fn heading_match_outranks_body_mentions() {
        let md = "# T\n\nbody mentions install install install\n\n## Install\n\nsteps\n";
        let p = select(md, Some("install"), 400);
        assert!(p.focus_matched);
        assert!(p.text.contains("## Install"));
    }

    #[test]
    fn no_match_falls_back_to_the_top() {
        let md = page();
        let p = select(&md, Some("zebra giraffe"), MAX_PREVIEW_BYTES);
        assert!(!p.focus_matched);
        assert!(p.text.starts_with("# Guide"));
    }

    #[test]
    fn a_huge_matching_section_is_cut_to_the_bound() {
        let md = format!("# A\n\n## Big\n\n{}\n", "needle hay ".repeat(2000));
        let p = select(&md, Some("needle"), MAX_PREVIEW_BYTES);
        assert!(p.focus_matched);
        assert!(p.text.len() <= MAX_PREVIEW_BYTES);
        assert!(p.text.contains("needle"));
    }

    #[test]
    fn code_fences_do_not_start_sections_and_multibyte_text_is_safe() {
        let md = "# A\n\n```\n# not a heading\n```\n\n## B\n\nünïcödé ✓ text\n";
        assert_eq!(sections(md).len(), 2);
        let p = select(&md.repeat(400), Some("ünïcödé"), 300);
        assert!(p.text.len() <= 300);
        let _ = select(&"é".repeat(5000), None, 301);
    }

    #[test]
    fn short_focus_words_still_work() {
        let md = "# T\n\n## AI\n\nAI stuff here\n\n## Other\n\nnothing\n";
        assert!(select(md, Some("AI"), 400).focus_matched);
    }
}
