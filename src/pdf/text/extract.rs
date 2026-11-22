use std::{
    collections::HashMap,
    fs::{File, Metadata},
    io::{self, Read as _},
    os::windows::fs::MetadataExt as _,
    path::PathBuf,
};

use pdf_extract::{
    ColorSpace, Dictionary, Document, MediaBox, Object, ObjectId, OutputDev, OutputError, Path,
    Stream, Transform,
    content::{Content, Operation},
};

use super::{Bounds, Failure, Hit, ResultHits};

const MAX_INPUT: u64 = 64 * 1024 * 1024;
const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_PAGES: u32 = 10_000;
const MAX_GLYPHS: usize = 1_000_000;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_HITS: usize = 100_000;
pub(super) const FAILED: Failure =
    Failure::new("pdf_search", "extract", "Could not search this PDF");
pub(super) const LIMIT: Failure =
    Failure::new("pdf_search", "size_limit", "PDF is too large to search");
pub(super) const CHANGED: Failure = Failure::new(
    "pdf_search",
    "source_changed",
    "PDF changed on disk; reopen it to search",
);

pub(in crate::pdf) struct Input {
    path: PathBuf,
    stamp: Stamp,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    len: u64,
    created: u64,
    written: u64,
}

impl From<&Metadata> for Stamp {
    fn from(metadata: &Metadata) -> Self {
        Self {
            len: metadata.file_size(),
            created: metadata.creation_time(),
            written: metadata.last_write_time(),
        }
    }
}

impl Input {
    pub(in crate::pdf) fn new(path: PathBuf, metadata: &Metadata) -> Self {
        Self {
            path,
            stamp: metadata.into(),
        }
    }

    pub(super) fn packet(&self) -> Result<(&str, u64, u64, u64), Failure> {
        let path = self.path.to_str().ok_or(FAILED)?;
        Ok((path, self.stamp.len, self.stamp.created, self.stamp.written))
    }

    pub(super) fn from_packet(
        (path, len, created, written): (String, u64, u64, u64),
    ) -> Option<Self> {
        let path = PathBuf::from(path);
        path.is_absolute().then_some(Self {
            path,
            stamp: Stamp {
                len,
                created,
                written,
            },
        })
    }

    pub(super) fn read(&self) -> Result<Vec<u8>, Failure> {
        if self.stamp.len > MAX_INPUT {
            return Err(LIMIT);
        }
        let mut file = File::open(&self.path).map_err(|_| FAILED)?;
        let verify = |metadata: &Metadata| {
            if metadata.is_file() && Stamp::from(metadata) == self.stamp {
                Ok(())
            } else {
                Err(CHANGED)
            }
        };
        verify(&file.metadata().map_err(|_| FAILED)?)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_INPUT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| FAILED)?;
        verify(&file.metadata().map_err(|_| FAILED)?)?;
        if bytes.len() as u64 != self.stamp.len {
            return Err(CHANGED);
        }
        Ok(bytes)
    }
}

pub(super) fn find(bytes: &[u8], count: u32, query: &str) -> ResultHits {
    find_with_budget(bytes, count, query, MAX_CONTENT_BYTES)
}

fn find_with_budget(bytes: &[u8], count: u32, query: &str, mut budget: usize) -> ResultHits {
    let mut doc = Document::load_mem(bytes).map_err(|_| FAILED)?;
    let pages = doc.get_pages();
    if doc.is_encrypted() || pages.len() != count as usize {
        return Err(FAILED);
    }
    let mut fonts = HashMap::new();
    for id in pages.into_values() {
        prepare_page(&mut doc, id, &mut budget, &mut fonts)?;
    }
    let mut output = Positions::new(query);
    pdf_extract::output_doc(&doc, &mut output).map_err(|_| output.failure.unwrap_or(FAILED))?;
    Ok(output.hits.into())
}

fn inherited<'a>(
    doc: &'a Document,
    mut id: ObjectId,
    key: &[u8],
) -> Result<Option<&'a Object>, Failure> {
    // Resolve inheritance iteratively. Do not hand a cyclic/deep parent chain to the extractor.
    for _ in 0..64 {
        let node = doc.get_dictionary(id).map_err(|_| FAILED)?;
        if let Ok(value) = node.get_deref(key, doc) {
            return Ok(Some(value));
        }
        if node.has(key) {
            return Err(FAILED);
        }
        if let Ok(parent) = node.get(b"Parent") {
            id = parent.as_reference().map_err(|_| FAILED)?;
        } else {
            return Ok(None);
        }
    }
    Err(FAILED)
}

fn media_box(doc: &Document, id: ObjectId) -> Result<Option<[f32; 4]>, Failure> {
    fn rect(value: &Object) -> Option<[f32; 4]> {
        let [a, b, c, d] = value.as_array().ok()?.as_slice() else {
            return None;
        };
        Some([
            a.as_float().ok()?,
            b.as_float().ok()?,
            c.as_float().ok()?,
            d.as_float().ok()?,
        ])
    }
    let Some(media @ [x0, y0, x1, y1]) = inherited(doc, id, b"MediaBox")?.and_then(rect) else {
        return Ok(None);
    };
    let crop = inherited(doc, id, b"CropBox")?.map_or(Some(media), rect);
    let rotated =
        inherited(doc, id, b"Rotate")?.is_some_and(|r| r.as_i64().map_or(true, |r| r % 360 != 0));
    Ok((x0 == 0.0
        && y0 == 0.0
        && x1.is_finite()
        && y1.is_finite()
        && x1 > 0.0
        && y1 > 0.0
        && !rotated
        && crop == Some(media))
    .then_some(media))
}

fn prepare_page(
    doc: &mut Document,
    id: ObjectId,
    budget: &mut usize,
    font_cache: &mut FontCache,
) -> Result<(), Failure> {
    let media = media_box(doc, id)?;
    let mut content = Vec::new();
    let mut resources = Dictionary::new();
    if media.is_some() {
        let original = doc.get_page_content(id).map_err(|_| FAILED)?;
        *budget = budget.checked_sub(original.len()).ok_or(LIMIT)?;
        let page_resources = inherited(doc, id, b"Resources")?.and_then(|o| o.as_dict().ok());
        content = text_content(&original, doc, page_resources)?;
        if !content.is_empty() {
            resources = page_resources.cloned().unwrap_or_default();
            normalize_cid_fonts(doc, &mut resources, font_cache)?;
        }
    }
    let stream = doc.add_object(Stream::new(Dictionary::new(), content));
    let node = doc.get_dictionary_mut(id).map_err(|_| FAILED)?;
    node.set("Contents", Object::Reference(stream));
    node.set(
        "MediaBox",
        media
            .unwrap_or([0., 0., 1., 1.])
            .into_iter()
            .map(Object::Real)
            .collect::<Vec<_>>(),
    );
    node.set("Resources", resources);
    let _art = node.remove(b"ArtBox");
    Ok(())
}

fn vertical_font(doc: &Document, resources: Option<&Dictionary>, name: &Object) -> Option<bool> {
    let fonts = resources?.get_deref(b"Font", doc).ok()?.as_dict().ok()?;
    let font = fonts
        .get_deref(name.as_name().ok()?, doc)
        .ok()?
        .as_dict()
        .ok()?;
    Some(match font.get_deref(b"Encoding", doc).ok()? {
        Object::Name(name) => name.ends_with(b"-V"),
        // Embedded Encoding CMaps with no clear horizontal mode can use
        // vertical advances that pdf-extract does not implement.
        Object::Stream(cmap) => !cmap
            .dict
            .get_deref(b"WMode", doc)
            .and_then(Object::as_i64)
            .is_ok_and(|mode| mode == 0),
        _ => false,
    })
}

type FontCache = HashMap<ObjectId, Option<ObjectId>>;

// pdf-extract 0.12.1 reads the wrong operands in the CID width range form.
// Replace affected fonts only in the private extraction document, sharing the
// replacement by reference when multiple pages use the same font.
fn normalize_cid_fonts(
    doc: &mut Document,
    resources: &mut Dictionary,
    cache: &mut FontCache,
) -> Result<(), Failure> {
    let Ok(mut fonts) = resources
        .get_deref(b"Font", doc)
        .and_then(Object::as_dict)
        .cloned()
    else {
        return Ok(());
    };
    let mut changed = false;
    for (_, entry) in &mut fonts {
        let Ok((id, Object::Dictionary(font))) = doc.dereference(entry) else {
            continue;
        };
        if let Some(replacement) = id.and_then(|id| cache.get(&id)) {
            if let Some(replacement) = replacement {
                *entry = Object::Reference(*replacement);
                changed = true;
            }
            continue;
        }
        let replacement = normalized_cid_font(doc, font)?;
        if let Some(id) = id {
            let replacement = replacement.map(|font| doc.add_object(font));
            cache.insert(id, replacement);
            if let Some(replacement) = replacement {
                *entry = Object::Reference(replacement);
                changed = true;
            }
        } else if let Some(font) = replacement {
            *entry = Object::Dictionary(font);
            changed = true;
        }
    }
    if changed {
        resources.set("Font", fonts);
    }
    Ok(())
}

fn normalized_cid_font(doc: &Document, font: &Dictionary) -> Result<Option<Dictionary>, Failure> {
    if font.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Type0") {
        return Ok(None);
    }
    let Ok(descendants) = font
        .get_deref(b"DescendantFonts", doc)
        .and_then(Object::as_array)
    else {
        return Ok(None); // An unused malformed font need not affect the search.
    };
    let Some(descendant) = descendants.first() else {
        return Ok(None);
    };
    let Ok(cid) = doc.dereference(descendant).and_then(|(_, o)| o.as_dict()) else {
        return Ok(None);
    };
    let Ok(widths) = cid.get_deref(b"W", doc).and_then(Object::as_array) else {
        return Ok(None);
    };
    let Some(widths) = normalize_widths(widths)? else {
        return Ok(None);
    };
    let mut cid = cid.clone();
    cid.set("W", widths);
    let mut font = font.clone();
    font.set("DescendantFonts", vec![Object::Dictionary(cid)]);
    Ok(Some(font))
}

fn normalize_widths(widths: &[Object]) -> Result<Option<Vec<Object>>, Failure> {
    let pairs = widths.chunks_exact(2);
    if pairs.remainder().is_empty()
        && pairs
            .clone()
            .all(|pair| matches!(pair[1], Object::Array(_)))
    {
        return Ok(None);
    }
    let mut output = Vec::new();
    let mut remaining = widths;
    let mut expanded = 0;
    while let [start, next, rest @ ..] = remaining {
        let first = start
            .as_i64()
            .ok()
            .and_then(|value| u16::try_from(value).ok())
            .ok_or(FAILED)?;
        output.push(start.clone());
        if let Object::Array(values) = next {
            output.push(Object::Array(values.clone()));
            remaining = rest;
        } else {
            let last = next
                .as_i64()
                .ok()
                .and_then(|value| u16::try_from(value).ok())
                .ok_or(FAILED)?;
            let Some((width, rest)) = rest.split_first() else {
                return Err(FAILED);
            };
            if last < first || !width.as_float().is_ok_and(f32::is_finite) {
                return Err(FAILED);
            }
            let count = usize::from(last - first) + 1;
            expanded += count;
            if expanded > 65_536 {
                return Err(LIMIT);
            }
            output.push(Object::Array(vec![width.clone(); count]));
            remaining = rest;
        }
    }
    if !remaining.is_empty() {
        return Err(FAILED);
    }
    Ok(Some(output))
}

#[derive(Clone, Copy)]
struct TextStyle {
    render_mode: u8,
    vertical: bool,
    clipped: bool,
}

impl TextStyle {
    fn searchable(self) -> bool {
        !self.clipped && !matches!(self.render_mode, 3 | 7)
    }
}

fn shows_glyphs(op: &Operation) -> bool {
    let nonempty = |value: &Object| value.as_str().is_ok_and(|bytes| !bytes.is_empty());
    match op.operands.last() {
        Some(Object::Array(items)) => items.iter().any(nonempty),
        Some(value) => nonempty(value),
        None => false,
    }
}

fn text_content(
    original: &[u8],
    doc: &Document,
    resources: Option<&Dictionary>,
) -> Result<Vec<u8>, Failure> {
    let content = Content::decode(original).map_err(|_| FAILED)?;
    let mut style = TextStyle {
        render_mode: 0,
        vertical: false,
        clipped: false,
    };
    let mut stack = Vec::new();
    let mut operations = Vec::new();
    let mut positioned = true;
    let mut path_clip = false;
    let mut text_clip = false;
    for op in content.operations {
        // Only retain positioning and text operations. The extractor doesn't apply
        // Form XObject placement transforms and doesn't need to open image streams.
        match (op.operator.as_str(), op.operands.as_slice()) {
            ("q", _) => {
                stack.push(style);
                operations.push(op);
            }
            ("Q", _) => {
                if let Some(saved) = stack.pop() {
                    style = saved;
                }
                operations.push(op);
            }
            // Paths clip when the current path ends. We do not reconstruct
            // their geometry: omit text under that clip until Q restores it.
            ("W" | "W*", _) => path_clip = true,
            ("n" | "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*", _) => {
                style.clipped |= std::mem::take(&mut path_clip);
            }
            ("Tr", [Object::Integer(mode @ 0..=7)]) => {
                style.render_mode = u8::try_from(*mode).map_err(|_| FAILED)?;
            }
            ("Tr", _) => return Err(FAILED),
            ("Tf", [name, _]) => {
                style.vertical = vertical_font(doc, resources, name) == Some(true);
                if !style.vertical {
                    operations.push(op);
                }
            }
            ("ET", _) => {
                // Text clip paths are accumulated by the shows, then applied
                // at ET. Modes 4–6 still paint using the preceding clip.
                style.clipped |= std::mem::take(&mut text_clip);
                positioned = true;
                operations.push(op);
            }
            ("BT" | "Tm" | "Td" | "TD" | "T*", _) => {
                positioned = true;
                operations.push(op);
            }
            ("'", [_]) => {
                positioned = true;
                operations.push(Operation::new("T*", vec![]));
                show_text(
                    &mut operations,
                    &mut positioned,
                    style,
                    &mut text_clip,
                    Operation::new("Tj", op.operands),
                )?;
            }
            ("\"", [word, character, text]) => {
                positioned = true;
                operations.push(Operation::new("Tw", vec![word.clone()]));
                operations.push(Operation::new("Tc", vec![character.clone()]));
                operations.push(Operation::new("T*", vec![]));
                show_text(
                    &mut operations,
                    &mut positioned,
                    style,
                    &mut text_clip,
                    Operation::new("Tj", vec![text.clone()]),
                )?;
            }
            ("Tj" | "TJ", _) => {
                show_text(&mut operations, &mut positioned, style, &mut text_clip, op)?;
            }
            ("cm" | "Tc" | "Tw" | "Tz" | "TL" | "Ts", _) => operations.push(op),
            _ => {}
        }
    }
    Content { operations }.encode().map_err(|_| FAILED)
}

fn show_text(
    operations: &mut Vec<Operation>,
    positioned: &mut bool,
    style: TextStyle,
    text_clip: &mut bool,
    op: Operation,
) -> Result<(), Failure> {
    *text_clip |= style.render_mode >= 4 && shows_glyphs(&op);
    if style.vertical {
        // We can't compute the vertical advance. A later horizontal show needs
        // independent positioning, otherwise reporting a location would be wrong.
        *positioned = false;
        return Ok(());
    }
    let searchable = style.searchable();
    if !*positioned && searchable {
        return Err(FAILED);
    }
    if !searchable {
        // No source path/stroke operators survive. These private markers toggle
        // collection while retaining the extractor's real horizontal advance.
        operations.push(Operation::new("S", vec![]));
    }
    operations.push(op);
    if !searchable {
        operations.push(Operation::new("S", vec![]));
    }
    Ok(())
}

struct Glyph {
    text: String,
    x: f64,
    y: f64,
    right: f64,
    ink_end: f64,
    em: f64,
}

struct Positions<'a> {
    query: &'a str,
    page: u32,
    width: f64,
    height: f64,
    glyphs: Vec<Glyph>,
    hits: Vec<Hit>,
    remaining_bytes: usize,
    remaining_glyphs: usize,
    visible: bool,
    failure: Option<Failure>,
}

impl<'a> Positions<'a> {
    fn new(query: &'a str) -> Self {
        Self {
            query,
            page: 0,
            width: 0.,
            height: 0.,
            glyphs: Vec::new(),
            hits: Vec::new(),
            remaining_bytes: MAX_TEXT_BYTES,
            remaining_glyphs: MAX_GLYPHS,
            visible: true,
            failure: None,
        }
    }

    fn check(&mut self, result: Result<(), Failure>) -> Result<(), OutputError> {
        result.map_err(|failure| {
            self.failure = Some(failure);
            io::Error::other(failure.message).into()
        })
    }

    fn collect(&mut self, glyphs: &mut [Glyph]) -> Result<(), Failure> {
        glyphs.sort_unstable_by(|a, b| b.y.total_cmp(&a.y));
        let mut tail = glyphs;
        while let Some(first) = tail.first() {
            // Use the band's first baseline rather than chaining small Y differences.
            let baseline = first.y;
            let count = tail.partition_point(|g| baseline - g.y <= 0.25);
            let (band, rest) = tail.split_at_mut(count);
            tail = rest;
            band.sort_unstable_by(|a, b| a.x.total_cmp(&b.x));
            for line in band.chunk_by(|a, b| {
                let tolerance = (a.em.min(b.em) * 0.12).max(0.01);
                b.x > a.x && (a.right - b.x).abs() <= tolerance
            }) {
                self.match_line(line, baseline)?;
            }
        }
        // Bands are read top-to-bottom and matched left-to-right. Anchor all hits
        // on the band's baseline so viewport-based partitioning remains ordered.
        Ok(())
    }

    fn match_line(&mut self, line: &[Glyph], baseline: f64) -> Result<(), Failure> {
        let mut text = String::new();
        let mut offsets = Vec::with_capacity(line.len() + 1);
        for glyph in line {
            offsets.push(text.len());
            text.push_str(&glyph.text);
        }
        offsets.push(text.len());
        for (start, matched) in text.match_indices(self.query) {
            if self.hits.len() == MAX_HITS {
                return Err(LIMIT);
            }
            self.hits.push(Hit {
                page: self.page,
                from_top: (self.height - baseline) / self.height,
                aspect_height: self.height / self.width,
                bounds: offsets
                    .binary_search(&start)
                    .ok()
                    .zip(offsets.binary_search(&(start + matched.len())).ok())
                    .and_then(|(first, end)| self.bounds(&line[first..end])),
            });
        }
        Ok(())
    }

    fn bounds(&self, matched: &[Glyph]) -> Option<Bounds> {
        let first = matched.first()?;
        let last = matched.last()?;
        let top = matched
            .iter()
            .map(|g| self.height - g.y - g.em)
            .fold(f64::INFINITY, f64::min);
        let bottom = matched
            .iter()
            .map(|g| self.height - g.y + g.em * 0.22)
            .fold(0., f64::max);
        let right = last.ink_end.min(self.width);
        // Font advance is a cell estimate, not an outline. Do not include
        // trailing character spacing or approximate a substring of a ligature.
        (right > first.x).then(|| {
            Bounds([
                first.x / self.width,
                (top / self.height).clamp(0., 1.),
                right / self.width,
                (bottom / self.height).clamp(0., 1.),
            ])
        })
    }
}

impl OutputDev for Positions<'_> {
    fn begin_page(
        &mut self,
        number: u32,
        media: &MediaBox,
        _: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        self.visible = true;
        self.page = number - 1;
        self.width = media.urx;
        self.height = media.ury;
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &Transform,
        width: f64,
        spacing: f64,
        font_size: f64,
        text: &str,
    ) -> Result<(), OutputError> {
        if self.remaining_glyphs == 0 || self.remaining_bytes < text.len() {
            return self.check(Err(LIMIT));
        }
        self.remaining_glyphs -= 1;
        self.remaining_bytes -= text.len();

        let (x, y) = (trm.m31, trm.m32);
        let em = trm.m22 * font_size;
        let right = x + trm.m11 * (width * font_size + spacing);
        let ink_end = x + trm.m11 * width * font_size;
        if !self.visible
            || text.is_empty()
            || text.chars().any(char::is_control)
            || !(trm.m11 > 0.
                && em.is_finite()
                && em > 0.
                && trm.m12.abs() < 1e-6
                && trm.m21.abs() < 1e-6
                && x >= 0.
                && right > x
                && right <= self.width
                && y > 0.
                && y <= self.height)
        {
            return Ok(());
        }
        self.glyphs.push(Glyph {
            text: text.into(),
            x,
            y,
            right,
            ink_end,
            em,
        });
        Ok(())
    }

    fn stroke(
        &mut self,
        _: &Transform,
        _: &ColorSpace,
        _: &[f64],
        _: &Path,
    ) -> Result<(), OutputError> {
        self.visible = !self.visible;
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        let mut glyphs = std::mem::take(&mut self.glyphs);
        let result = self.collect(&mut glyphs);
        glyphs.clear();
        self.glyphs = glyphs;
        self.check(result)
    }

    // Text-show boundaries are not shown as line breaks or spaces in a PDF.
    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_literal_search_skips_unsupported_geometry() -> io::Result<()> {
        let fixture = crate::test_pdf::search_geometry_pages();
        let check = |query, expected: &[f64]| -> io::Result<()> {
            let hits = find(&fixture, 3, query)?;
            assert_eq!(hits.len(), expected.len(), "{query}: {hits:?}");
            for (hit, fraction) in hits.iter().zip(expected) {
                assert_eq!(hit.page, 0);
                assert!((hit.from_top - fraction).abs() < 1e-6, "{hit:?}");
            }
            Ok(())
        };

        check("Token", &[0.1125, 0.1125])?;
        check("token", &[0.2875])?;
        check("TOKEN", &[])?;
        check("alpha beta", &[0.6])?;
        check("alphabeta", &[])
    }

    #[test]
    fn equivalent_cid_widths_match_near_right_margin_and_on_another_line() -> io::Result<()> {
        let array = find(
            &crate::test_pdf::cid_text_page("65 [500 500]", "Identity-H", false),
            1,
            "ハイ",
        )?;
        let range = find(
            &crate::test_pdf::cid_text_page("65 66 500", "Identity-H", false),
            1,
            "ハイ",
        )?;
        assert_eq!(range, array);
        assert_eq!(
            range
                .iter()
                .map(|hit| (hit.page, hit.from_top))
                .collect::<Vec<_>>(),
            [(0, 0.2), (0, 0.65)]
        );
        Ok(())
    }

    #[test]
    fn preserves_visible_text_before_and_after_hidden_and_vertical_text() -> io::Result<()> {
        let fixture = crate::test_pdf::cid_text_page("65 66 500", "Identity-H", true);
        let hits = find(&fixture, 1, "ハイ")?;
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.page, hit.from_top))
                .collect::<Vec<_>>(),
            [(0, 0.2), (0, 0.65)]
        );
        // Last line's horizontal text is now off-page, because the hidden
        // glyph advanced x by 10pt. The other visible first glyphs still count.
        assert_eq!(find(&fixture, 1, "ハ")?.len(), 3);
        Ok(())
    }

    #[test]
    fn page_content_budget_rejects_expanded_content() {
        let fixture = crate::test_pdf::search_geometry_pages();
        assert!(
            matches!(find_with_budget(&fixture, 3, "Token", 32), Err(failure) if failure.reason == LIMIT.reason)
        );
    }

    #[test]
    fn image_only_page_has_no_hits() -> io::Result<()> {
        let hits = find(&crate::test_pdf::image_only_page(), 1, "text")?;
        assert!(hits.is_empty());
        Ok(())
    }
}
