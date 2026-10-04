//! Readable PDF layout with standard fonts, embedded screenshots and bookmarks.
use std::{fs, io, path::Path};
const WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];
fn ascii(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' => '-',
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            c if c.is_ascii() && !c.is_control() => c,
            _ => ' ',
        })
        .collect()
}
fn literal(s: &str) -> String {
    ascii(s)
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}
fn measure(s: &str, size: f32, bold: bool) -> f32 {
    ascii(s)
        .bytes()
        .map(|b| WIDTHS[(b - 32) as usize] as f32 * size / 1000.0)
        .sum::<f32>()
        * if bold { 1.12 } else { 1.0 }
}
fn wrap(text: &str, size: f32, bold: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for p in text.split('\n') {
        let mut line = String::new();
        for word in ascii(p).split_whitespace() {
            if !line.is_empty() && measure(&format!("{line} {word}"), size, bold) > 516.0 {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            for c in word.chars() {
                if measure(&format!("{line}{c}"), size, bold) > 516.0 {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
        }
        lines.push(line);
    }
    lines
}
#[derive(Default)]
struct Page {
    content: String,
    images: Vec<usize>,
}
struct Picture {
    data: Vec<u8>,
    width: u32,
    height: u32,
}
pub struct Document {
    pages: Vec<Page>,
    pictures: Vec<Picture>,
    bookmarks: Vec<(String, usize)>,
    y: f32,
}
impl Document {
    pub fn new() -> Self {
        let mut d = Self {
            pages: Vec::new(),
            pictures: Vec::new(),
            bookmarks: Vec::new(),
            y: 0.0,
        };
        d.page();
        d
    }
    pub fn page(&mut self) {
        self.pages.push(Page{content:"0.12 0.27 0.35 rg\nBT /F2 9 Tf 48 757 Td (ERRORPULLER / EVIDENCE REVIEW) Tj ET\n0.80 0.85 0.88 RG 0.5 w 48 742 m 564 742 l S\n".into(),images:Vec::new()});
        self.y = 715.0;
    }
    pub fn bookmark(&mut self, title: &str) {
        self.bookmarks.push((title.into(), self.pages.len() - 1));
    }
    fn ensure(&mut self, height: f32) {
        if self.y - height < 54.0 {
            self.page();
        }
    }
    pub fn text(&mut self, text: &str, size: f32, bold: bool) {
        let leading = size * 1.43;
        for line in wrap(text, size, bold) {
            self.ensure(leading);
            let y = self.y;
            self.pages.last_mut().unwrap().content.push_str(&format!(
                "0.12 0.18 0.23 rg BT /F{} {size} Tf 48 {y:.2} Td ({}) Tj ET\n",
                if bold { 2 } else { 1 },
                literal(&line)
            ));
            self.y -= leading;
        }
        self.y -= 8.0;
    }
    pub fn title(&mut self, text: &str) {
        self.text(text, 23.0, true);
        self.y -= 6.0;
    }
    pub fn section(&mut self, label: &str, text: &str) {
        self.ensure(66.0);
        self.text(label, 12.0, true);
        self.text(text, 11.5, false);
    }
    pub fn image(&mut self, path: &Path, caption: &str) -> io::Result<()> {
        let (width, height) = image::image_dimensions(path).map_err(io::Error::other)?;
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 20_000_000 {
            return Err(io::Error::other("screenshot exceeds 20 million pixels"));
        }
        let rgb = image::open(path).map_err(io::Error::other)?.to_rgb8();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 94)
            .encode(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
            .map_err(io::Error::other)?;
        let scale = (516.0 / width as f32).min(548.0 / height as f32);
        let w = width as f32 * scale;
        let h = height as f32 * scale;
        self.ensure(h + 72.0);
        let bottom = self.y - h;
        let id = self.pictures.len();
        self.pictures.push(Picture {
            data: jpeg,
            width,
            height,
        });
        let page = self.pages.last_mut().unwrap();
        page.images.push(id);
        page.content.push_str(&format!(
            "q {w:.2} 0 0 {h:.2} 48 {bottom:.2} cm /Im{id} Do Q\n"
        ));
        self.y = bottom - 17.0;
        self.text(caption, 9.5, false);
        Ok(())
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut objects: Vec<Vec<u8>> = vec![
            Vec::new(),
            Vec::new(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_vec(),
        ];
        let mut picture_ids = Vec::new();
        for p in &self.pictures {
            picture_ids.push(objects.len() + 1);
            let mut o=format!("<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",p.width,p.height,p.data.len()).into_bytes();
            o.extend_from_slice(&p.data);
            o.extend_from_slice(b"\nendstream");
            objects.push(o);
        }
        let mut page_ids = Vec::new();
        for (i, p) in self.pages.iter().enumerate() {
            let id = objects.len() + 1;
            page_ids.push(id);
            let resources = p
                .images
                .iter()
                .map(|j| format!("/Im{j} {} 0 R", picture_ids[*j]))
                .collect::<Vec<_>>()
                .join(" ");
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> /XObject << {resources} >> >> /Contents {} 0 R >>",id+1).into_bytes());
            let content=format!("{}0.35 0.43 0.48 rg BT /F1 9 Tf 48 28 Td (Evidence-backed review) Tj ET BT /F1 9 Tf 487 28 Td ({} / {}) Tj ET\n",p.content,i+1,self.pages.len());
            objects.push(
                format!(
                    "<< /Length {} >>\nstream\n{}endstream",
                    content.len(),
                    content
                )
                .into_bytes(),
            );
        }
        objects[1] = format!(
            "<< /Type /Pages /Count {} /Kids [{}] >>",
            page_ids.len(),
            page_ids
                .iter()
                .map(|id| format!("{id} 0 R"))
                .collect::<Vec<_>>()
                .join(" ")
        )
        .into_bytes();
        let outline = objects.len() + 1;
        objects.push(Vec::new());
        for (i, (title, page)) in self.bookmarks.iter().enumerate() {
            let prev = if i > 0 {
                format!("/Prev {} 0 R", outline + i)
            } else {
                String::new()
            };
            let next = if i + 1 < self.bookmarks.len() {
                format!("/Next {} 0 R", outline + i + 2)
            } else {
                String::new()
            };
            objects.push(format!("<< /Title ({}) /Parent {outline} 0 R {prev} {next} /Dest [{} 0 R /XYZ null 730 null] >>",literal(title),page_ids[*page]).into_bytes());
        }
        if self.bookmarks.is_empty() {
            objects[outline - 1] = b"<< /Type /Outlines /Count 0 >>".to_vec();
        } else {
            objects[outline - 1] = format!(
                "<< /Type /Outlines /First {} 0 R /Last {} 0 R /Count {} >>",
                outline + 1,
                outline + self.bookmarks.len(),
                self.bookmarks.len()
            )
            .into_bytes();
        }
        objects[0] = format!(
            "<< /Type /Catalog /Pages 2 0 R /Outlines {outline} 0 R /PageMode /UseOutlines >>"
        )
        .into_bytes();
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            bytes.extend_from_slice(o);
            bytes.extend_from_slice(b"\nendobj\n");
        }
        let start = bytes.len();
        bytes.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        fs::write(path, bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_tokens_fit() {
        assert!(wrap(&"W".repeat(300), 11.5, false)
            .iter()
            .all(|l| measure(l, 11.5, false) <= 516.1));
    }
    #[test]
    fn escapes_pdf_syntax() {
        assert_eq!(literal("(a)\\b"), "\\(a\\)\\\\b");
    }
}
