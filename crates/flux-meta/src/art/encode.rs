//! JPEG encoding with embedded provenance (XMP) and a blurhash placeholder.
//!
//! Every image flux makes says what it is, machine-readably, in an XMP APP1
//! segment: IPTC `Iptc4xmpExt:DigitalSourceType` (the vocabulary the EU AI Act
//! Art. 50 marking guidance and C2PA both build on) plus flux's own
//! `flux:artSource` (`frame` / `generated`), model id and prompt hash. A
//! signed C2PA manifest is a later step (research/05 §5.7).

use flux_core::api::{ArtRef, ArtSource};
use image::RgbImage;
use jpeg_encoder::{ColorType, Encoder};
use serde::{Deserialize, Serialize};

use super::ArtError;

/// IPTC Digital Source Type (<http://cv.iptc.org/newscodes/digitalsourcetype/>).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DigitalSourceType {
    /// Made by a generative model (genAI posters).
    TrainedAlgorithmicMedia,
    /// A capture combined with generative output (img2img from a frame).
    CompositeWithTrainedAlgorithmicMedia,
    /// A composite with synthetic (non-AI) elements: a video frame plus typeset text.
    CompositeSynthetic,
    /// An unmodified capture (episode thumbnails).
    DigitalCapture,
}

impl DigitalSourceType {
    /// The IPTC NewsCodes URI.
    pub fn uri(self) -> &'static str {
        match self {
            DigitalSourceType::TrainedAlgorithmicMedia => {
                "http://cv.iptc.org/newscodes/digitalsourcetype/trainedAlgorithmicMedia"
            }
            DigitalSourceType::CompositeWithTrainedAlgorithmicMedia => {
                "http://cv.iptc.org/newscodes/digitalsourcetype/compositeWithTrainedAlgorithmicMedia"
            }
            DigitalSourceType::CompositeSynthetic => {
                "http://cv.iptc.org/newscodes/digitalsourcetype/compositeSynthetic"
            }
            DigitalSourceType::DigitalCapture => {
                "http://cv.iptc.org/newscodes/digitalsourcetype/digitalCapture"
            }
        }
    }
}

/// What gets written into the image's XMP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: ArtSource,
    pub digital_source_type: DigitalSourceType,
    /// Generator model id, for generated art.
    pub model: Option<String>,
    /// xxh3-128 of the prompt (the prompt itself is not embedded).
    pub prompt_hash: Option<String>,
    /// Human-readable description (`dc:description`).
    pub description: String,
}

impl Provenance {
    /// A frame poster: video frame + flux typography, no AI.
    pub fn frame(description: impl Into<String>) -> Self {
        Provenance {
            source: ArtSource::Frame,
            digital_source_type: DigitalSourceType::CompositeSynthetic,
            model: None,
            prompt_hash: None,
            description: description.into(),
        }
    }
}

/// An encoded image ready to serve.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub blurhash: String,
    pub source: ArtSource,
    /// The generator model (`provider/model`) for [`ArtSource::Generated`]
    /// images, from [`Provenance::model`]; `None` otherwise.
    pub generator: Option<String>,
}

impl ArtImage {
    /// The API reference for this image once it is stored at `url`. flux's
    /// own images credit no provider (`attribution: None`); generated ones
    /// name their model in `generator`. Store both in the catalog
    /// (`NewArt::attribution` / `generator`) so lists return them too.
    pub fn to_art_ref(&self, url: impl Into<String>) -> ArtRef {
        ArtRef {
            url: url.into(),
            width: Some(self.width),
            height: Some(self.height),
            blurhash: Some(self.blurhash.clone()),
            source: self.source,
            attribution: None,
            generator: self.generator.clone(),
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn art_source_str(s: ArtSource) -> &'static str {
    match s {
        ArtSource::Provider => "provider",
        ArtSource::Local => "local",
        ArtSource::Frame => "frame",
        ArtSource::Generated => "generated",
    }
}

/// The XMP packet for `p`.
pub fn xmp_packet(p: &Provenance) -> String {
    let mut attrs = format!(
        "Iptc4xmpExt:DigitalSourceType=\"{}\" xmp:CreatorTool=\"flux-meta {}\" flux:artSource=\"{}\"",
        p.digital_source_type.uri(),
        env!("CARGO_PKG_VERSION"),
        art_source_str(p.source)
    );
    if let Some(m) = &p.model {
        attrs.push_str(&format!(" flux:model=\"{}\"", xml_escape(m)));
    }
    if let Some(h) = &p.prompt_hash {
        attrs.push_str(&format!(" flux:promptHash=\"{}\"", xml_escape(h)));
    }
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
<rdf:Description rdf:about=\"\" xmlns:Iptc4xmpExt=\"http://iptc.org/std/Iptc4xmpExt/2008-02-29/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" \
xmlns:flux=\"https://github.com/vibecook-dev/flux/ns/art/1.0/\" {attrs}>\
<dc:description><rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt></dc:description>\
</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>",
        xml_escape(&p.description)
    )
}

/// Blurhash of an image (computed on a ≤32 px wide thumbnail).
pub fn blurhash_of(img: &RgbImage) -> Result<String, ArtError> {
    let (w, h) = img.dimensions();
    let tw = w.clamp(1, 32);
    let th = ((u64::from(h) * u64::from(tw) / u64::from(w.max(1))) as u32).max(1);
    let small = image::imageops::resize(img, tw, th, image::imageops::FilterType::Triangle);
    let rgba: Vec<u8> = small
        .pixels()
        .flat_map(|p| [p.0[0], p.0[1], p.0[2], 255])
        .collect();
    let (cx, cy) = if w >= h { (4, 3) } else { (3, 4) };
    blurhash::encode(cx, cy, tw, th, &rgba).map_err(|e| ArtError::Encode(format!("blurhash: {e:?}")))
}

/// Encode `img` as a baseline JPEG with the provenance XMP and a blurhash.
pub fn encode_jpeg(img: &RgbImage, quality: u8, p: &Provenance) -> Result<ArtImage, ArtError> {
    let (w, h) = img.dimensions();
    let (w16, h16) = (
        u16::try_from(w).map_err(|_| ArtError::Encode("image too wide for JPEG".into()))?,
        u16::try_from(h).map_err(|_| ArtError::Encode("image too tall for JPEG".into()))?,
    );
    let mut bytes = Vec::with_capacity((w * h / 4) as usize);
    let mut enc = Encoder::new(&mut bytes, quality.clamp(1, 100));
    let mut app1 = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    app1.extend_from_slice(xmp_packet(p).as_bytes());
    enc.add_app_segment(1, app1)
        .map_err(|e| ArtError::Encode(e.to_string()))?;
    enc.encode(img.as_raw(), w16, h16, ColorType::Rgb)
        .map_err(|e| ArtError::Encode(e.to_string()))?;
    Ok(ArtImage {
        bytes,
        mime: "image/jpeg",
        width: w,
        height: h,
        blurhash: blurhash_of(img)?,
        source: p.source,
        generator: if p.source == ArtSource::Generated {
            p.model.clone()
        } else {
            None
        },
    })
}

/// Read the XMP packet back out of a JPEG (for tests and provenance checks).
pub fn read_xmp(jpeg: &[u8]) -> Option<String> {
    const SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    let mut i = 2; // skip SOI
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        let marker = jpeg[i + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        if marker == 0xE1 && jpeg.get(i + 4..i + 4 + SIG.len()) == Some(SIG) {
            let body = jpeg.get(i + 4 + SIG.len()..i + 2 + len)?;
            return String::from_utf8(body.to_vec()).ok();
        }
        if marker == 0xDA {
            break; // start of scan: no more headers
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_carries_xmp_and_decodes() {
        let img = RgbImage::from_fn(120, 180, |x, y| image::Rgb([(x * 2) as u8, (y) as u8, 90]));
        let p = Provenance {
            source: ArtSource::Generated,
            digital_source_type: DigitalSourceType::TrainedAlgorithmicMedia,
            model: Some("gpt-image-1-mini".into()),
            prompt_hash: Some("abc".into()),
            description: "AI-generated key art <test> & title typeset by flux".into(),
        };
        let art = encode_jpeg(&img, 85, &p).unwrap();
        assert_eq!((art.width, art.height, art.mime), (120, 180, "image/jpeg"));
        let xmp = read_xmp(&art.bytes).expect("xmp");
        assert!(xmp.contains("digitalsourcetype/trainedAlgorithmicMedia"));
        assert!(xmp.contains("flux:artSource=\"generated\""));
        assert!(xmp.contains("&lt;test&gt; &amp;"));
        let back = image::load_from_memory(&art.bytes).unwrap();
        assert_eq!((back.width(), back.height()), (120, 180));
        assert!(art.blurhash.len() >= 6);
        let r = art.to_art_ref("/api/v1/art/1");
        assert_eq!(r.source, ArtSource::Generated);
        assert_eq!(r.generator.as_deref(), Some("gpt-image-1-mini"));
        assert_eq!(r.attribution, None);
        let frame = encode_jpeg(&img, 85, &Provenance::frame("a frame")).unwrap();
        assert_eq!(frame.to_art_ref("/x").generator, None, "frames name no model");
    }
}
