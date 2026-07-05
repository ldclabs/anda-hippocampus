//! Markdown normalization, slugs, checksums and the fence-aware chunker.
//!
//! Chunks tile the normalized content exactly: contiguous byte ranges with no
//! gaps and no overlap, so `content[start..end]` is always the authoritative
//! chunk text and citations can be re-verified from the immutable version
//! content alone.

use sha3::{Digest, Sha3_256};
use unicode_normalization::UnicodeNormalization;

/// Bump when the chunking algorithm changes so maintenance can find and
/// rebuild chunks produced by older algorithms.
pub const CHUNKER_VERSION: u32 = 1;

/// Sections smaller than this merge forward with siblings under the same
/// parent heading.
pub const CHUNK_TARGET_MIN: usize = 800;
/// Soft packing bound: units stop accumulating once a chunk would pass this.
pub const CHUNK_TARGET_MAX: usize = 2000;
/// Non-atomic runs without blank lines are force-split at this size.
pub const CHUNK_HARD_MAX: usize = 4096;
/// Upper bound on chunks per version so any per-document chunk query fits in
/// a single AndaDB search (`MAX_SEARCH_LIMIT` is 1000).
pub const MAX_CHUNKS_PER_VERSION: usize = 1000;

const SLUG_MAX_CHARS: usize = 120;

/// A chunk boundary plan over normalized content. Text is not stored here:
/// it is always the exact `content[byte_start..byte_end]` slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDraft {
    pub heading_path: Vec<String>,
    pub anchor: String,
    pub byte_start: usize,
    pub byte_end: usize,
    /// True when this chunk came from a pathological force-split (a run
    /// without blank lines exceeding [`CHUNK_HARD_MAX`]).
    pub forced: bool,
}

/// Chunking result with quality signals for the commit event.
#[derive(Debug, Clone, Default)]
pub struct ChunkPlan {
    pub drafts: Vec<ChunkDraft>,
    pub forced_splits: usize,
    pub capped: bool,
}

/// Normalizes Markdown for storage: CRLF/CR → LF, Unicode NFC, trailing
/// whitespace stripped per line, exactly one trailing newline.
pub fn normalize_content(content: &str) -> String {
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    let content: String = content.nfc().collect();
    let mut out = String::with_capacity(content.len() + 1);
    for line in content.split('\n') {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if out.trim().is_empty() {
        return String::new();
    }
    out
}

/// Unicode-preserving slug: keeps any alphanumeric character (so Chinese
/// titles produce Chinese slugs instead of collapsing to a shared
/// placeholder), collapses everything else into single dashes.
pub fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut chars = 0usize;
    let mut last_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if chars >= SLUG_MAX_CHARS {
            break;
        }
        if ch.is_alphanumeric() {
            slug.push(ch);
            chars += 1;
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            chars += 1;
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
}

/// `"sha3-256:<hex>"` over the given parts.
pub fn checksum_for<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut hasher = Sha3_256::new();
    for part in parts {
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2 + 9);
    hex.push_str("sha3-256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Chunk checksum binding the version, the byte range and the exact text, so
/// a citation can be re-verified from the immutable version content alone.
pub fn chunk_checksum(version_checksum: &str, start: usize, end: usize, text: &str) -> String {
    let range = format!("{start}:{end}");
    checksum_for([
        version_checksum.as_bytes(),
        range.as_bytes(),
        text.as_bytes(),
    ])
}

/// Splits normalized content into citation-ready chunks. See module docs for
/// the tiling invariant.
pub fn chunk_markdown(content: &str) -> ChunkPlan {
    if content.is_empty() {
        return ChunkPlan::default();
    }

    let lines = scan_lines(content);
    let sections = split_sections(content, &lines);

    let mut drafts = Vec::new();
    let mut forced_splits = 0usize;
    for section in &sections {
        pack_section(content, section, &mut drafts, &mut forced_splits);
    }
    merge_small_siblings(&mut drafts);

    let mut capped = false;
    while drafts.len() > MAX_CHUNKS_PER_VERSION {
        capped = true;
        halve_adjacent(&mut drafts);
    }

    for (idx, draft) in drafts.iter_mut().enumerate() {
        let base = draft
            .heading_path
            .last()
            .map(|h| slugify(h))
            .unwrap_or_else(|| "section".to_string());
        draft.anchor = format!("{base}-{idx}");
    }

    ChunkPlan {
        drafts,
        forced_splits,
        capped,
    }
}

struct LineInfo {
    start: usize,
    end: usize,
    blank: bool,
    /// Inside a code fence, including both delimiter lines.
    in_fence: bool,
    table_row: bool,
    /// An h1–h3 heading outside any fence: a section boundary.
    boundary: Option<(usize, String)>,
}

fn scan_lines(content: &str) -> Vec<LineInfo> {
    let mut lines = Vec::new();
    let mut offset = 0usize;
    let mut fence: Option<(char, usize)> = None;

    for raw in content.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let trimmed = line.trim_start();

        let mut in_fence = fence.is_some();
        match fence {
            Some((ch, len)) => {
                let run = trimmed.chars().take_while(|c| *c == ch).count();
                if run >= len && trimmed.chars().all(|c| c == ch || c.is_whitespace()) {
                    fence = None; // closing delimiter; this line stays in_fence
                }
            }
            None => {
                for ch in ['`', '~'] {
                    let run = trimmed.chars().take_while(|c| *c == ch).count();
                    if run >= 3 {
                        fence = Some((ch, run));
                        in_fence = true;
                        break;
                    }
                }
            }
        }

        let blank = trimmed.is_empty();
        let boundary = if in_fence {
            None
        } else {
            parse_heading(trimmed).filter(|(level, _)| *level <= 3)
        };

        lines.push(LineInfo {
            start,
            end: offset,
            blank,
            in_fence,
            table_row: !in_fence && trimmed.starts_with('|'),
            boundary,
        });
    }
    lines
}

fn parse_heading(trimmed: &str) -> Option<(usize, String)> {
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = trimmed.get(level..)?;
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        None
    } else {
        Some((level, title.to_string()))
    }
}

/// A maximal run of lines with no blank-line break (blank lines attach to
/// the preceding unit so units tile their section).
struct Unit {
    start: usize,
    end: usize,
    /// Contains fence lines or is mostly a table: never split internally.
    atomic: bool,
}

struct Section {
    heading_path: Vec<String>,
    units: Vec<Unit>,
}

fn split_sections(content: &str, lines: &[LineInfo]) -> Vec<Section> {
    let _ = content;
    let mut sections: Vec<Section> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut units: Vec<Unit> = Vec::new();

    let mut unit: Option<(usize, usize, bool, usize, usize)> = None; // start, end, fence, table_rows, total_rows
    let mut prev_blank_outside = false;

    let flush_unit = |unit: &mut Option<(usize, usize, bool, usize, usize)>,
                      units: &mut Vec<Unit>| {
        if let Some((start, end, fence, table_rows, total_rows)) = unit.take() {
            let atomic = fence || (total_rows > 0 && table_rows * 2 > total_rows);
            units.push(Unit { start, end, atomic });
        }
    };

    for line in lines {
        if let Some((level, title)) = &line.boundary {
            flush_unit(&mut unit, &mut units);
            if !units.is_empty() {
                sections.push(Section {
                    heading_path: stack.clone(),
                    units: std::mem::take(&mut units),
                });
            }
            stack.truncate(level.saturating_sub(1));
            stack.push(title.clone());
            prev_blank_outside = false;
        }

        if unit.is_some() && prev_blank_outside && !line.blank {
            flush_unit(&mut unit, &mut units);
        }

        match &mut unit {
            Some((_, end, fence, table_rows, total_rows)) => {
                *end = line.end;
                *fence |= line.in_fence;
                if !line.blank {
                    *total_rows += 1;
                    if line.table_row {
                        *table_rows += 1;
                    }
                }
            }
            None => {
                unit = Some((
                    line.start,
                    line.end,
                    line.in_fence,
                    line.table_row as usize,
                    (!line.blank) as usize,
                ));
            }
        }

        prev_blank_outside = line.blank && !line.in_fence;
    }

    flush_unit(&mut unit, &mut units);
    if !units.is_empty() {
        sections.push(Section {
            heading_path: stack,
            units,
        });
    }
    sections
}

fn pack_section(
    content: &str,
    section: &Section,
    drafts: &mut Vec<ChunkDraft>,
    forced_splits: &mut usize,
) {
    let mut cur: Option<(usize, usize, bool)> = None; // start, end, single_atomic_unit

    let close =
        |range: (usize, usize, bool), drafts: &mut Vec<ChunkDraft>, forced_splits: &mut usize| {
            let (start, end, atomic) = range;
            let len = end - start;
            if len == 0 {
                return;
            }
            if len > CHUNK_HARD_MAX && !atomic {
                force_split(content, start, end, &section.heading_path, drafts);
                *forced_splits += 1;
            } else {
                drafts.push(ChunkDraft {
                    heading_path: section.heading_path.clone(),
                    anchor: String::new(),
                    byte_start: start,
                    byte_end: end,
                    forced: false,
                });
            }
        };

    for unit in &section.units {
        let ulen = unit.end - unit.start;
        match cur {
            None => cur = Some((unit.start, unit.end, unit.atomic)),
            Some((start, end, atomic)) => {
                if (end - start) + ulen <= CHUNK_TARGET_MAX {
                    cur = Some((start, unit.end, false));
                } else {
                    close((start, end, atomic), drafts, forced_splits);
                    cur = Some((unit.start, unit.end, unit.atomic));
                }
            }
        }
    }
    if let Some(range) = cur {
        close(range, drafts, forced_splits);
    }
}

fn force_split(
    content: &str,
    start: usize,
    end: usize,
    heading_path: &[String],
    drafts: &mut Vec<ChunkDraft>,
) {
    let mut piece_start = start;
    let mut cursor = start;
    for raw in content[start..end].split_inclusive('\n') {
        let line_end = cursor + raw.len();
        if line_end - piece_start > CHUNK_HARD_MAX && cursor > piece_start {
            drafts.push(ChunkDraft {
                heading_path: heading_path.to_vec(),
                anchor: String::new(),
                byte_start: piece_start,
                byte_end: cursor,
                forced: true,
            });
            piece_start = cursor;
        }
        cursor = line_end;
    }
    if piece_start < end {
        drafts.push(ChunkDraft {
            heading_path: heading_path.to_vec(),
            anchor: String::new(),
            byte_start: piece_start,
            byte_end: end,
            forced: true,
        });
    }
}

/// Merges undersized chunks forward when both sides sit under the same
/// parent heading, so FAQ-style documents with many tiny sections do not
/// explode into per-question chunks. The merged heading path is the shared
/// prefix, keeping citations honest about what the chunk covers.
fn merge_small_siblings(drafts: &mut Vec<ChunkDraft>) {
    let mut i = 0usize;
    while i < drafts.len() {
        let len = drafts[i].byte_end - drafts[i].byte_start;
        if len >= CHUNK_TARGET_MIN || i + 1 >= drafts.len() {
            i += 1;
            continue;
        }
        let (a, b) = (&drafts[i], &drafts[i + 1]);
        if a.forced || b.forced {
            i += 1;
            continue;
        }
        let combined = b.byte_end - a.byte_start;
        if combined > CHUNK_TARGET_MAX {
            i += 1;
            continue;
        }
        let common = common_prefix_len(&a.heading_path, &b.heading_path);
        let siblings = a.heading_path == b.heading_path
            || (common >= 1
                && a.heading_path.len() <= common + 1
                && b.heading_path.len() <= common + 1);
        if !siblings {
            i += 1;
            continue;
        }
        let merged_path = if a.heading_path == b.heading_path {
            a.heading_path.clone()
        } else {
            a.heading_path[..common].to_vec()
        };
        drafts[i].heading_path = merged_path;
        drafts[i].byte_end = drafts[i + 1].byte_end;
        drafts.remove(i + 1);
        // stay on i: it may still be under CHUNK_TARGET_MIN
    }
}

/// Pathology guard: merges adjacent pairs unconditionally, halving the chunk
/// count per sweep, until the per-version cap holds.
fn halve_adjacent(drafts: &mut Vec<ChunkDraft>) {
    let mut merged = Vec::with_capacity(drafts.len() / 2 + 1);
    let mut iter = drafts.drain(..);
    while let Some(a) = iter.next() {
        match iter.next() {
            Some(b) => {
                let common = common_prefix_len(&a.heading_path, &b.heading_path);
                merged.push(ChunkDraft {
                    heading_path: a.heading_path[..common].to_vec(),
                    anchor: String::new(),
                    byte_start: a.byte_start,
                    byte_end: b.byte_end,
                    forced: a.forced || b.forced,
                });
            }
            None => merged.push(a),
        }
    }
    drop(iter);
    *drafts = merged;
}

fn common_prefix_len(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Floors `pos` to a UTF-8 char boundary within `text`.
pub fn floor_char_boundary(text: &str, pos: usize) -> usize {
    let mut pos = pos.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Collapses whitespace and truncates to a short excerpt for citations.
pub fn quote_excerpt(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 320 {
        collapsed
    } else {
        let mut excerpt: String = collapsed.chars().take(320).collect();
        excerpt.push_str("...");
        excerpt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_tiling(content: &str, plan: &ChunkPlan) {
        assert!(!plan.drafts.is_empty());
        assert_eq!(plan.drafts.first().unwrap().byte_start, 0);
        assert_eq!(plan.drafts.last().unwrap().byte_end, content.len());
        for pair in plan.drafts.windows(2) {
            assert_eq!(pair[0].byte_end, pair[1].byte_start, "chunks must tile");
        }
        for draft in &plan.drafts {
            assert!(content.get(draft.byte_start..draft.byte_end).is_some());
        }
    }

    #[test]
    fn normalize_unifies_line_endings_and_trailing_whitespace() {
        let normalized = normalize_content("a  \r\nb\t\r\n\r\nc");
        assert_eq!(normalized, "a\nb\n\nc\n");
        assert_eq!(normalize_content("   \n\t\n"), "");
        // NFC: decomposed é (e + combining acute) becomes composed é
        assert_eq!(normalize_content("Cafe\u{0301}"), "Café\n");
    }

    #[test]
    fn slugify_preserves_unicode_titles() {
        assert_eq!(slugify("Recall API v1"), "recall-api-v1");
        assert_eq!(slugify("产品手册"), "产品手册");
        assert_eq!(slugify("安全政策 2026"), "安全政策-2026");
        assert_eq!(slugify("  !!!  "), "untitled");
        assert_ne!(slugify("产品手册"), slugify("安全政策"));
    }

    #[test]
    fn headings_inside_code_fences_do_not_split() {
        let content = normalize_content(
            "# Deploy\n\nrun this:\n\n```bash\n# not a heading\necho hi\n```\n\ntail text\n",
        );
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        assert_eq!(plan.drafts.len(), 1);
        assert_eq!(plan.drafts[0].heading_path, vec!["Deploy"]);
    }

    #[test]
    fn tilde_fence_and_unclosed_fence_are_respected() {
        let content = normalize_content("# A\n~~~\n# inside\n~~~\n\n# B\nreal section\n");
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        let paths: Vec<_> = plan.drafts.iter().map(|d| d.heading_path.clone()).collect();
        assert!(paths.contains(&vec!["B".to_string()]));
        assert!(!paths.iter().any(|p| p.contains(&"inside".to_string())));

        let unclosed = normalize_content("# A\n```\n# swallowed\n\n# also swallowed\n");
        let plan = chunk_markdown(&unclosed);
        assert_tiling(&unclosed, &plan);
        assert_eq!(plan.drafts.len(), 1);
    }

    #[test]
    fn heading_paths_nest_h1_to_h3() {
        let big = "x".repeat(900);
        let content = normalize_content(&format!(
            "# Root\n{big}\n\n## API\n{big}\n\n### Auth\n{big}\n\n## Policy\n{big}\n\n#### h4-stays\ninside policy\n"
        ));
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        let paths: Vec<_> = plan.drafts.iter().map(|d| d.heading_path.clone()).collect();
        assert!(paths.contains(&vec!["Root".to_string()]));
        assert!(paths.contains(&vec!["Root".to_string(), "API".to_string()]));
        assert!(paths.contains(&vec![
            "Root".to_string(),
            "API".to_string(),
            "Auth".to_string()
        ]));
        assert!(paths.contains(&vec!["Root".to_string(), "Policy".to_string()]));
        assert!(!paths.iter().any(|p| p.contains(&"h4-stays".to_string())));
    }

    #[test]
    fn tiny_sibling_sections_merge_under_parent() {
        let content = normalize_content(
            "# FAQ\n\n## Q1\nshort answer one\n\n## Q2\nshort answer two\n\n## Q3\nshort answer three\n",
        );
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        assert_eq!(plan.drafts.len(), 1);
        assert_eq!(plan.drafts[0].heading_path, vec!["FAQ"]);
    }

    #[test]
    fn distinct_h1_topics_do_not_merge() {
        let content = normalize_content("# Alpha\nshort a\n\n# Beta\nshort b\n");
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        assert_eq!(plan.drafts.len(), 2);
        assert_eq!(plan.drafts[0].heading_path, vec!["Alpha"]);
        assert_eq!(plan.drafts[1].heading_path, vec!["Beta"]);
    }

    #[test]
    fn oversized_code_fence_stays_atomic_but_prose_force_splits() {
        let code_body = "0123456789abcdef\n".repeat(400); // ~6.8 KiB fenced block
        let content = normalize_content(&format!("# Code\n```\n{code_body}```\n"));
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        assert_eq!(plan.drafts.len(), 1, "fenced block must not be split");
        assert_eq!(plan.forced_splits, 0);

        let prose = "word ".repeat(2000); // ~10 KiB single paragraph, no blank lines
        let content = normalize_content(&format!("# Prose\n{prose}\n"));
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        assert!(plan.drafts.len() > 1);
        assert!(plan.forced_splits > 0);
        assert!(plan.drafts.iter().any(|d| d.forced));
    }

    #[test]
    fn tables_stay_whole() {
        let rows: String = (0..40)
            .map(|i| format!("| cell {i} | value {i} |\n"))
            .collect();
        let content =
            normalize_content(&format!("# Data\n\nintro\n\n| a | b |\n|---|---|\n{rows}"));
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        let table_start = content.find("| a | b |").unwrap();
        let covering: Vec<_> = plan
            .drafts
            .iter()
            .filter(|d| d.byte_start <= table_start && table_start < d.byte_end)
            .collect();
        assert_eq!(covering.len(), 1);
        assert!(covering[0].byte_end >= content.rfind("| cell 39").unwrap());
    }

    #[test]
    fn pathological_many_headings_hit_chunk_cap() {
        let content =
            normalize_content(&(0..4000).map(|i| format!("# T{i}\n")).collect::<String>());
        let plan = chunk_markdown(&content);
        assert_tiling(&content, &plan);
        assert!(plan.capped);
        assert!(plan.drafts.len() <= MAX_CHUNKS_PER_VERSION);
    }

    #[test]
    fn anchors_are_unique_and_stable() {
        let content = normalize_content("# 部署指南\n\ncontent one\n\n# 部署指南\n\ncontent two\n");
        let plan = chunk_markdown(&content);
        let anchors: Vec<_> = plan.drafts.iter().map(|d| d.anchor.clone()).collect();
        let unique: std::collections::BTreeSet<_> = anchors.iter().collect();
        assert_eq!(anchors.len(), unique.len());
        assert!(anchors[0].starts_with("部署指南-"));

        let again = chunk_markdown(&content);
        let anchors_again: Vec<_> = again.drafts.iter().map(|d| d.anchor.clone()).collect();
        assert_eq!(anchors, anchors_again);
    }

    #[test]
    fn chunk_checksum_is_recomputable_from_slice() {
        let content = normalize_content("# A\n\nhello world\n");
        let plan = chunk_markdown(&content);
        let d = &plan.drafts[0];
        let version_checksum = checksum_for([content.as_bytes()]);
        let text = &content[d.byte_start..d.byte_end];
        let c1 = chunk_checksum(&version_checksum, d.byte_start, d.byte_end, text);
        let c2 = chunk_checksum(&version_checksum, d.byte_start, d.byte_end, text);
        assert_eq!(c1, c2);
        assert!(c1.starts_with("sha3-256:"));
        let c3 = chunk_checksum(&version_checksum, d.byte_start, d.byte_end + 1, text);
        assert_ne!(c1, c3);
    }

    #[test]
    fn floor_char_boundary_respects_utf8() {
        let text = "中文测试";
        assert_eq!(floor_char_boundary(text, 0), 0);
        assert_eq!(floor_char_boundary(text, 1), 0);
        assert_eq!(floor_char_boundary(text, 3), 3);
        assert_eq!(floor_char_boundary(text, 4), 3);
        assert_eq!(floor_char_boundary(text, 100), text.len());
    }
}
