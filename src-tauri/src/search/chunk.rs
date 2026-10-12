//! Pure chunkers: turn a meeting's title, transcript, summary, notes and facets
//! into the units that are indexed for keyword and semantic search. No I/O.

use std::collections::HashMap;

/// Target size of a transcript window, in characters (about 250 tokens).
pub const WINDOW_TARGET: usize = 1_000;
/// Hard cap of a transcript window; a single turn beyond this is split at sentence ends.
pub const WINDOW_MAX: usize = 1_600;
/// The previous window's last turn is repeated at the start of the next one when shorter than this.
pub const OVERLAP_MAX: usize = 300;
/// Summary sections and notes longer than this are split at blank lines.
pub const SECTION_MAX: usize = 2_000;
/// Consecutive segments of one speaker closer than this are one turn (as in the summariser).
const TURN_GAP_MS: i64 = 60_000;

#[derive(Debug, Clone)]
pub struct SegmentIn {
    pub idx: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub kind: &'static str,
    pub source_id: Option<String>,
    pub ord: i64,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub seg_first_idx: Option<i64>,
    pub seg_last_idx: Option<i64>,
    pub section: Option<String>,
    /// Diarisation labels that speak in the chunk, in order of appearance.
    pub speakers: Vec<String>,
    /// Display and keyword text, without any prompt prefix.
    pub text: String,
    /// Title handed to the embedding prompt; None embeds `title: none`.
    pub embed_title: Option<String>,
    /// Facet chunks are keyword-only.
    pub embed: bool,
}

pub struct ChunkInput<'a> {
    pub title: &'a str,
    pub transcript_id: Option<&'a str>,
    pub segments: &'a [SegmentIn],
    /// diar label -> display name
    pub names: &'a HashMap<String, String>,
    /// (summary id, body markdown) of the latest finished summary
    pub summary: Option<(&'a str, &'a str)>,
    pub notes: &'a str,
    pub facets_text: Option<&'a str>,
}

/// The exact text that is embedded for a chunk (title + text), used for change detection.
pub fn embed_hash(chunk: &Chunk) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(chunk.embed_title.as_deref().unwrap_or("none").as_bytes());
    h.update([0u8]);
    h.update(chunk.text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn build_chunks(input: &ChunkInput) -> Vec<Chunk> {
    let mut out = Vec::new();
    let title = input.title.trim();
    if !title.is_empty() {
        out.push(Chunk {
            kind: "title",
            source_id: None,
            ord: 0,
            start_ms: None,
            end_ms: None,
            seg_first_idx: None,
            seg_last_idx: None,
            section: None,
            speakers: Vec::new(),
            text: title.to_string(),
            embed_title: None,
            embed: true,
        });
    }
    out.extend(transcript_chunks(title, input.transcript_id, input.segments, input.names));
    if let Some((id, body)) = input.summary {
        out.extend(summary_chunks(title, id, body));
    }
    out.extend(notes_chunks(title, input.notes));
    if let Some(f) = input.facets_text {
        let f = f.trim();
        if !f.is_empty() {
            out.push(Chunk {
                kind: "facets",
                source_id: None,
                ord: 0,
                start_ms: None,
                end_ms: None,
                seg_first_idx: None,
                seg_last_idx: None,
                section: None,
                speakers: Vec::new(),
                text: f.to_string(),
                embed_title: None,
                embed: false,
            });
        }
    }
    out
}

// ---- transcript -------------------------------------------------------------

#[derive(Debug, Clone)]
struct Piece {
    start_ms: i64,
    end_ms: i64,
    first_idx: i64,
    last_idx: i64,
    label: String,
    line: String,
}

/// Merge consecutive same-speaker segments into turns, then render `Name: text`
/// pieces no longer than `WINDOW_MAX`.
fn pieces(segments: &[SegmentIn], names: &HashMap<String, String>) -> Vec<Piece> {
    let name_of = |label: &str| names.get(label).cloned().unwrap_or_else(|| label.to_string());
    let mut turns: Vec<Piece> = Vec::new();
    for s in segments {
        let text = s.text.trim();
        if text.is_empty() {
            continue;
        }
        if let Some(last) = turns.last_mut()
            && last.label == s.label
            && s.start_ms - last.end_ms < TURN_GAP_MS
        {
            last.line.push(' ');
            last.line.push_str(text);
            last.end_ms = last.end_ms.max(s.end_ms);
            last.last_idx = s.idx;
            continue;
        }
        turns.push(Piece {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            first_idx: s.idx,
            last_idx: s.idx,
            label: s.label.clone(),
            line: text.to_string(),
        });
    }
    let mut out = Vec::new();
    for t in turns {
        let name = name_of(&t.label);
        let prefix = format!("{name}: ");
        let limit = WINDOW_MAX.saturating_sub(prefix.len()).max(200);
        for part in split_sentences(&t.line, limit) {
            out.push(Piece { line: format!("{prefix}{part}"), ..t.clone() });
        }
    }
    out
}

/// Split `text` into parts of at most `limit` chars, preferring sentence ends,
/// then whitespace. Never splits inside a character.
pub fn split_sentences(text: &str, limit: usize) -> Vec<String> {
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut sentences: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        cur.push(chars[i]);
        let boundary =
            matches!(chars[i], '.' | '?' | '!') && chars.get(i + 1).map(|c| c.is_whitespace()).unwrap_or(true);
        if boundary {
            sentences.push(std::mem::take(&mut cur).trim().to_string());
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        sentences.push(cur.trim().to_string());
    }
    // Hard-split any single sentence that is still too long, at whitespace.
    let mut units: Vec<String> = Vec::new();
    for s in sentences {
        if s.chars().count() <= limit {
            units.push(s);
            continue;
        }
        let mut piece = String::new();
        for word in s.split_whitespace() {
            if !piece.is_empty() && piece.chars().count() + 1 + word.chars().count() > limit {
                units.push(std::mem::take(&mut piece));
            }
            if !piece.is_empty() {
                piece.push(' ');
            }
            piece.push_str(word);
        }
        if !piece.is_empty() {
            units.push(piece);
        }
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for u in units {
        if !cur.is_empty() && cur.chars().count() + 1 + u.chars().count() > limit {
            parts.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&u);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

fn transcript_chunks(
    title: &str,
    transcript_id: Option<&str>,
    segments: &[SegmentIn],
    names: &HashMap<String, String>,
) -> Vec<Chunk> {
    let pieces = pieces(segments, names);
    let mut windows: Vec<Vec<Piece>> = Vec::new();
    let mut cur: Vec<Piece> = Vec::new();
    let mut cur_len = 0usize;
    let mut cur_is_overlap_only = false;
    for p in pieces {
        let plen = p.line.chars().count();
        if !cur.is_empty() && cur_len + 1 + plen > WINDOW_TARGET {
            // Would the overlap plus this piece break the hard cap? Then drop the overlap.
            if cur_is_overlap_only && cur_len + 1 + plen > WINDOW_MAX {
                cur.clear();
                cur_len = 0;
            } else {
                let overlap = cur.last().filter(|l| l.line.chars().count() < OVERLAP_MAX).cloned();
                windows.push(std::mem::take(&mut cur));
                cur_len = 0;
                if let Some(o) = overlap {
                    cur_len = o.line.chars().count();
                    cur.push(o);
                    cur_is_overlap_only = true;
                }
                if cur_is_overlap_only && cur_len + 1 + plen > WINDOW_MAX {
                    cur.clear();
                    cur_len = 0;
                }
            }
        }
        cur_len += if cur.is_empty() { plen } else { 1 + plen };
        cur.push(p);
        cur_is_overlap_only = false;
    }
    if !cur.is_empty() && !cur_is_overlap_only {
        windows.push(cur);
    }
    windows
        .into_iter()
        .enumerate()
        .map(|(ord, w)| {
            let mut speakers: Vec<String> = Vec::new();
            for p in &w {
                if !speakers.contains(&p.label) {
                    speakers.push(p.label.clone());
                }
            }
            Chunk {
                kind: "transcript",
                source_id: transcript_id.map(str::to_string),
                ord: ord as i64,
                start_ms: w.iter().map(|p| p.start_ms).min(),
                end_ms: w.iter().map(|p| p.end_ms).max(),
                seg_first_idx: w.iter().map(|p| p.first_idx).min(),
                seg_last_idx: w.iter().map(|p| p.last_idx).max(),
                section: None,
                speakers,
                text: w.iter().map(|p| p.line.as_str()).collect::<Vec<_>>().join("\n"),
                embed_title: Some(title.to_string()),
                embed: true,
            }
        })
        .collect()
}

// ---- summary / notes --------------------------------------------------------

/// Split text at blank lines into parts of at most `limit` chars (a single
/// oversized paragraph is split at sentence ends).
pub fn split_paragraphs(text: &str, limit: usize) -> Vec<String> {
    let text = text.trim();
    if text.chars().count() <= limit {
        return if text.is_empty() { Vec::new() } else { vec![text.to_string()] };
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        let units = if para.chars().count() > limit { split_sentences(para, limit) } else { vec![para.to_string()] };
        for u in units {
            if !cur.is_empty() && cur.chars().count() + 2 + u.chars().count() > limit {
                parts.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(&u);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

fn is_empty_section(body: &str) -> bool {
    let b = body.trim().trim_end_matches(['.', '!']).trim().to_ascii_lowercase();
    b.is_empty() || b == "none noted" || b == "none" || b == "n/a"
}

fn summary_chunks(title: &str, summary_id: &str, body: &str) -> Vec<Chunk> {
    // (heading, body) sections; text before the first heading is a preamble.
    let mut sections: Vec<(Option<String>, String)> = vec![(None, String::new())];
    for line in body.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            sections.push((Some(h.trim().to_string()), String::new()));
        } else {
            let cur = &mut sections.last_mut().expect("at least one section").1;
            cur.push_str(line);
            cur.push('\n');
        }
    }
    let mut out = Vec::new();
    let mut ord = 0i64;
    for (heading, text) in sections {
        if is_empty_section(&text) {
            continue;
        }
        for part in split_paragraphs(&text, SECTION_MAX) {
            out.push(Chunk {
                kind: "summary",
                source_id: Some(summary_id.to_string()),
                ord,
                start_ms: None,
                end_ms: None,
                seg_first_idx: None,
                seg_last_idx: None,
                section: heading.clone(),
                speakers: Vec::new(),
                text: part,
                embed_title: Some(match &heading {
                    Some(h) => format!("{title} — {h}"),
                    None => title.to_string(),
                }),
                embed: true,
            });
            ord += 1;
        }
    }
    out
}

fn notes_chunks(title: &str, notes: &str) -> Vec<Chunk> {
    split_paragraphs(notes, SECTION_MAX)
        .into_iter()
        .enumerate()
        .map(|(ord, part)| Chunk {
            kind: "notes",
            source_id: None,
            ord: ord as i64,
            start_ms: None,
            end_ms: None,
            seg_first_idx: None,
            seg_last_idx: None,
            section: None,
            speakers: Vec::new(),
            text: part,
            embed_title: Some(title.to_string()),
            embed: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(idx: i64, start_s: i64, label: &str, text: &str) -> SegmentIn {
        SegmentIn {
            idx,
            start_ms: start_s * 1000,
            end_ms: start_s * 1000 + 900,
            text: text.into(),
            label: label.into(),
        }
    }

    fn names() -> HashMap<String, String> {
        HashMap::from([("S1".to_string(), "Alice".to_string()), ("S2".to_string(), "Bob".to_string())])
    }

    #[test]
    fn turns_merge_and_render_names() {
        let segs =
            vec![seg(0, 0, "S1", "one"), seg(1, 5, "S1", "two"), seg(2, 10, "S2", "three"), seg(3, 100, "S2", "four")];
        let p = pieces(&segs, &names());
        assert_eq!(
            p.iter().map(|p| p.line.as_str()).collect::<Vec<_>>(),
            vec!["Alice: one two", "Bob: three", "Bob: four"]
        );
        assert_eq!((p[0].first_idx, p[0].last_idx, p[0].end_ms), (0, 1, 5900));
    }

    #[test]
    fn windows_fill_cap_and_overlap() {
        // 30 turns of ~100 chars alternating speakers -> windows of ~1000 chars.
        let segs: Vec<SegmentIn> = (0..30)
            .map(|i| seg(i, i * 10, if i % 2 == 0 { "S1" } else { "S2" }, &format!("turn {i} {}", "word ".repeat(18))))
            .collect();
        let chunks = transcript_chunks("Weekly", Some("t1"), &segs, &names());
        assert!(chunks.len() >= 3, "got {}", chunks.len());
        for c in &chunks {
            assert!(c.text.chars().count() <= WINDOW_MAX, "window too long: {}", c.text.len());
            assert_eq!(c.kind, "transcript");
            assert_eq!(c.source_id.as_deref(), Some("t1"));
            assert_eq!(c.embed_title.as_deref(), Some("Weekly"));
        }
        // Overlap: the first line of window 2 is the last line of window 1.
        let w1_last = chunks[0].text.lines().last().unwrap();
        let w2_first = chunks[1].text.lines().next().unwrap();
        assert_eq!(w1_last, w2_first);
        // Segment ranges and timestamps follow the content.
        assert_eq!(chunks[0].seg_first_idx, Some(0));
        assert_eq!(chunks[0].start_ms, Some(0));
        assert!(chunks[1].seg_first_idx.unwrap() <= chunks[0].seg_last_idx.unwrap());
        let last = chunks.last().unwrap();
        assert_eq!(last.seg_last_idx, Some(29));
        assert_eq!(chunks[0].speakers, vec!["S1".to_string(), "S2".to_string()]);
    }

    #[test]
    fn long_turn_is_split_at_sentence_ends() {
        let long = (0..80).map(|i| format!("Sentence number {i} is here.")).collect::<Vec<_>>().join(" ");
        assert!(long.len() > WINDOW_MAX);
        let segs = vec![seg(0, 0, "S1", &long)];
        let chunks = transcript_chunks("T", None, &segs, &names());
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.text.chars().count() <= WINDOW_MAX);
            assert!(c.text.starts_with("Alice: "));
            assert!(c.text.ends_with('.'), "split should end at a sentence: {:?}", c.text);
        }
        let joined: String =
            chunks.iter().map(|c| c.text.trim_start_matches("Alice: ").to_string()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, long);
    }

    #[test]
    fn hard_split_without_sentence_ends() {
        let text = "word ".repeat(500).trim().to_string();
        let parts = split_sentences(&text, 100);
        assert!(parts.len() >= 20);
        assert!(parts.iter().all(|p| p.chars().count() <= 100));
        assert_eq!(parts.join(" "), text);
    }

    #[test]
    fn summary_sections_skip_none_noted_and_keep_headings() {
        let body = "Intro line\n\n## TL;DR\nWe agreed on pricing.\n\n## Main points by speaker\n- Alice: tiers\n\n## Actions and takeaways\nNone noted.\n";
        let chunks = summary_chunks("Pricing call", "sum1", body);
        let sections: Vec<Option<&str>> = chunks.iter().map(|c| c.section.as_deref()).collect();
        assert_eq!(sections, vec![None, Some("TL;DR"), Some("Main points by speaker")]);
        assert_eq!(chunks[1].text, "We agreed on pricing.");
        assert_eq!(chunks[1].embed_title.as_deref(), Some("Pricing call — TL;DR"));
        assert_eq!(chunks[0].embed_title.as_deref(), Some("Pricing call"));
        assert!(chunks.iter().all(|c| c.source_id.as_deref() == Some("sum1")));
        assert_eq!(chunks.iter().map(|c| c.ord).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn long_section_and_notes_split_at_blank_lines() {
        let para = "p ".repeat(400).trim().to_string(); // 799 chars
        let body = format!("## Notes\n{para}\n\n{para}\n\n{para}\n\n{para}\n");
        let chunks = summary_chunks("T", "s", &body);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= SECTION_MAX));
        assert!(chunks.iter().all(|c| c.section.as_deref() == Some("Notes")));
        let notes = notes_chunks("T", &format!("{para}\n\n{para}\n\n{para}"));
        assert_eq!(notes.len(), 2);
        assert!(notes_chunks("T", "   \n").is_empty());
    }

    #[test]
    fn full_build_is_deterministic_and_marks_facets_keyword_only() {
        let segs = vec![seg(0, 0, "S1", "Hello there."), seg(1, 3, "S2", "Hi.")];
        let input = ChunkInput {
            title: "Kickoff",
            transcript_id: Some("t"),
            segments: &segs,
            names: &names(),
            summary: Some(("s", "## TL;DR\nShort.\n")),
            notes: "my note",
            facets_text: Some("Type: planning\nTopics: launch"),
        };
        let a = build_chunks(&input);
        let b = build_chunks(&input);
        assert_eq!(a, b);
        let kinds: Vec<&str> = a.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, vec!["title", "transcript", "summary", "notes", "facets"]);
        assert!(!a[4].embed);
        assert_eq!(a[1].text, "Alice: Hello there.\nBob: Hi.");
        assert_eq!(embed_hash(&a[0]).len(), 64);
        assert_ne!(embed_hash(&a[0]), embed_hash(&a[3]));
    }
}
