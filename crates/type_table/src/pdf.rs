//! PDF pages as images (spec 004, a Blob's own tab), drawn by the system's PDF renderer: on
//! macOS CoreGraphics opens the document from memory and draws each page into a bitmap, whose
//! pixels become an image element's. Elsewhere there is no renderer and the tab offers the
//! system's viewer instead.

/// One page, drawn: its pixels, RGBA row by row from the top, and its pixel size.
#[derive(Debug, Clone)]
pub struct Page {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The most pixels one page is drawn with, a tall letter page's at 1,600 wide: a page far
/// taller than it is wide (a web page saved as one) is drawn narrower, not tens of thousands of
/// pixels tall.
const PAGE_PIXELS: f64 = 1600. * 2400.;

/// A document's pages drawn `width` pixels wide (narrower for a page that would pass
/// [`PAGE_PIXELS`]), the first `max_pages` of them, and how many pages it has.
#[cfg(target_os = "macos")]
pub fn render(bytes: &[u8], width: u32, max_pages: usize) -> Result<(Vec<Page>, usize), String> {
    use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGDataProvider, CGImageAlphaInfo, CGPDFBox,
        CGPDFDocument, CGPDFPage,
    };

    let data = CFData::from_bytes(bytes);
    let provider =
        CGDataProvider::with_cf_data(Some(&data)).ok_or("The value could not be read as a PDF.")?;
    let document = CGPDFDocument::with_provider(Some(&provider))
        .ok_or("The value is not a PDF the system can open.")?;
    let count = CGPDFDocument::number_of_pages(Some(&document));
    let colour = CGColorSpace::new_device_rgb().ok_or("No colour space to draw into.")?;
    let mut pages = Vec::new();
    for number in 1..=count.min(max_pages) {
        let Some(page) = CGPDFDocument::page(Some(&document), number) else {
            continue;
        };
        let media = CGPDFPage::box_rect(Some(&page), CGPDFBox::MediaBox);
        let rotation = CGPDFPage::rotation_angle(Some(&page));
        // A page turned a quarter is as wide as it is tall unturned.
        let (page_w, page_h) = if rotation % 180 == 0 {
            (media.size.width, media.size.height)
        } else {
            (media.size.height, media.size.width)
        };
        if page_w <= 0. || page_h <= 0. {
            continue;
        }
        let (mut draw_w, mut draw_h) = (width as f64, (page_h / page_w) * width as f64);
        if draw_w * draw_h > PAGE_PIXELS {
            let shrink = (PAGE_PIXELS / (draw_w * draw_h)).sqrt();
            (draw_w, draw_h) = (draw_w * shrink, draw_h * shrink);
        }
        let (w, h) = (
            draw_w.round().max(1.) as usize,
            draw_h.round().max(1.) as usize,
        );
        // RGBA, premultiplied: the page is drawn on white, so every pixel is opaque. Drawn
        // straight into the page's own pixels, which the context only borrows.
        let mut rgba = vec![0u8; w * h * 4];
        let context = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                Some(&colour),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .ok_or("No bitmap to draw the page into.")?;
        let full = CGRect::new(CGPoint::new(0., 0.), CGSize::new(w as f64, h as f64));
        CGContext::set_rgb_fill_color(Some(&context), 1., 1., 1., 1.);
        CGContext::fill_rect(Some(&context), full);
        // The page's own transform places and turns it but never enlarges it, so the bitmap's
        // scale is applied first and the page is fitted to its own size in points.
        let scale = w as f64 / page_w;
        CGContext::scale_ctm(Some(&context), scale, scale);
        let points = CGRect::new(CGPoint::new(0., 0.), CGSize::new(page_w, page_h));
        let transform =
            CGPDFPage::drawing_transform(Some(&page), CGPDFBox::MediaBox, points, 0, true);
        CGContext::concat_ctm(Some(&context), transform);
        CGContext::draw_pdf_page(Some(&context), Some(&page));
        // The bitmap's first row is the page's top; the context lets go of the pixels first.
        drop(context);
        pages.push(Page {
            rgba,
            width: w as u32,
            height: h as u32,
        });
    }
    Ok((pages, count))
}

#[cfg(not(target_os = "macos"))]
pub fn render(_bytes: &[u8], _width: u32, _max_pages: usize) -> Result<(Vec<Page>, usize), String> {
    Err("Studio draws PDFs on macOS only; open it in the system's viewer.".into())
}

/// Whether bytes are a PDF: its header within the first kilobyte, as readers accept.
pub fn is_pdf(prefix: &[u8]) -> bool {
    prefix
        .windows(5)
        .take(1024)
        .any(|window| window == b"%PDF-")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page PDF, 200 × 100 points, with a black square in its lower left, written with
    /// the byte offsets its cross-reference table needs.
    pub(crate) fn one_page_pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_owned(),
            {
                let stream = "0 0 0 rg 10 10 30 30 re f";
                format!(
                    "<< /Length {} >>\nstream\n{stream}\nendstream",
                    stream.len()
                )
            },
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            pdf.extend(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn pdfs_are_told_by_their_header() {
        assert!(is_pdf(&one_page_pdf()));
        assert!(is_pdf(b"\n\n%PDF-1.7"));
        assert!(!is_pdf(b"hello"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_page_is_drawn_to_scale_on_white_with_its_marks() {
        let (pages, count) = render(&one_page_pdf(), 400, 10).unwrap();
        assert_eq!(count, 1);
        let page = &pages[0];
        // 200 × 100 points drawn 400 wide is 200 tall.
        assert_eq!((page.width, page.height), (400, 200));
        let image = image::RgbaImage::from_raw(page.width, page.height, page.rgba.clone()).unwrap();
        // The top left is paper; the square sits near the bottom left (PDF's origin).
        assert_eq!(image.get_pixel(5, 5).0, [255, 255, 255, 255]);
        let mark = image.get_pixel(40, 200 - 40).0;
        assert!(mark[0] < 40 && mark[1] < 40 && mark[2] < 40, "{mark:?}");
    }

    /// A page far taller than wide is drawn narrower, to the page pixel budget, keeping its
    /// shape.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_very_tall_page_is_drawn_within_the_pixel_budget() {
        let tall = String::from_utf8(one_page_pdf())
            .unwrap()
            .replace("/MediaBox [0 0 200 100]", "/MediaBox [0 0 200 4000]");
        // The longer box moves the objects after it; the system's reader finds them anyway.
        let (pages, _) = render(tall.as_bytes(), 1600, 1).unwrap();
        let page = &pages[0];
        assert!(
            (page.width as f64) * (page.height as f64) <= PAGE_PIXELS * 1.01,
            "{} × {}",
            page.width,
            page.height
        );
        assert_eq!((page.height as f64 / page.width as f64).round(), 20.);
        assert_eq!(
            page.rgba.len(),
            page.width as usize * page.height as usize * 4
        );
    }

    #[test]
    fn something_else_is_refused_not_drawn() {
        assert!(render(b"not a pdf", 400, 10).is_err() || cfg!(not(target_os = "macos")));
    }
}
