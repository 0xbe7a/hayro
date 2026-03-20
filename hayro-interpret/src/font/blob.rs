use crate::font::UNITS_PER_EM;
use crate::font::outline::OutlinePath;
use kurbo::{Affine, BezPath};
use read_fonts::tables::postscript::font::{CffFontRef, CffSubfont, Type1Font};
use read_fonts::types::Fixed;
use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::GlyphMetrics;
use skrifa::outline::{DrawSettings, Engine, HintingInstance, HintingOptions, Target};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider, OutlineGlyphCollection};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use yoke::{Yoke, Yokeable};

type FontData = Arc<dyn AsRef<[u8]> + Send + Sync>;
type OpenTypeFontYoke = Yoke<OTFYoke<'static>, FontData>;
type CffFontYoke = Yoke<CFFYoke<'static>, FontData>;

/// A font blob for type 1 fonts.
///
/// Type1Font from read-fonts owns its data (uses Vec<u8> internally),
/// so no Yoke is needed.
#[derive(Clone)]
pub(crate) struct Type1FontBlob(Arc<Type1Font>);

impl Debug for Type1FontBlob {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Type1 Font {{ .. }}")
    }
}

impl Type1FontBlob {
    pub(crate) fn new(data: FontData) -> Option<Self> {
        let table = Type1Font::new(data.as_ref().as_ref()).ok()?;
        Some(Self(Arc::new(table)))
    }

    pub(crate) fn table(&self) -> &Type1Font {
        self.0.as_ref()
    }

    pub(crate) fn outline_glyph(&self, gid: GlyphId) -> BezPath {
        let mut path = OutlinePath::new();

        let _ = self.table().evaluate_charstring(gid, &mut path);

        let matrix = self.table().matrix();
        let upem = self.table().upem();

        Affine::scale(UNITS_PER_EM as f64)
            * convert_font_matrix(&matrix.0, upem)
            * path.take()
    }
}

/// A font blob for CFF-based fonts.
#[derive(Clone)]
pub(crate) struct CffFontBlob(Arc<CffFontYoke>);

impl Debug for CffFontBlob {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "CFF Font {{ .. }}")
    }
}

impl CffFontBlob {
    pub(crate) fn new(data: FontData) -> Option<Self> {
        // Validate first
        let _ = CffFontRef::new_cff(data.as_ref().as_ref(), 0).ok()?;

        let yoke = Yoke::<CFFYoke<'static>, FontData>::attach_to_cart(data.clone(), |data| {
            let font = CffFontRef::new_cff(data.as_ref(), 0).unwrap();
            CFFYoke { font }
        });

        Some(Self(Arc::new(yoke)))
    }

    pub(crate) fn font_data(&self) -> FontData {
        self.0.backing_cart().clone()
    }

    pub(crate) fn table(&self) -> &CffFontRef<'_> {
        &self.0.as_ref().get().font
    }

    pub(crate) fn outline_glyph(&self, glyph: GlyphId) -> BezPath {
        let mut path = OutlinePath::new();
        let table = self.table();

        let subfont_index = table.subfont_index(glyph).unwrap_or(0);
        let Ok(subfont) = table.subfont(subfont_index, &[]) else {
            return BezPath::new();
        };

        let _ = table.evaluate_charstring(&subfont, &[], glyph, &mut path);

        let matrix = self.compute_glyph_matrix(glyph, &subfont);

        Affine::scale(UNITS_PER_EM as f64) * matrix * path.take()
    }

    /// Computes the effective glyph matrix, composing top-level and per-subfont
    /// matrices. This replicates the logic from hayro-font's glyph_matrix().
    ///
    /// The key subtlety: read-fonts normalizes the FontMatrix by dividing out
    /// the y-scale component and storing it as `scale` (upem). To get the
    /// equivalent raw matrix that hayro-font used, we must divide each matrix
    /// component by the scale.
    fn compute_glyph_matrix(&self, _glyph: GlyphId, subfont: &CffSubfont) -> Affine {
        let table = self.table();
        let top = table.matrix();
        let fd = subfont.matrix();

        match (top, fd) {
            (Some(top_matrix), Some(fd_matrix)) => {
                // Both top and FD have matrices.
                // Convert both to raw (de-normalized) form and compose.
                let top_raw = convert_font_matrix(&top_matrix.matrix.0, top_matrix.scale);
                let fd_raw = convert_font_matrix(&fd_matrix.matrix.0, fd_matrix.scale);
                top_raw * fd_raw
            }
            (None, Some(fd_matrix)) => {
                // No explicit top matrix. Default top is [0.001, 0, 0, 0.001, 0, 0].
                // FD matrix scaled by 1000 (as per hayro-font's behavior for
                // non-explicit top matrix).
                let default_top = default_cff_matrix();
                let fd_raw = convert_font_matrix(&fd_matrix.matrix.0, fd_matrix.scale);
                let scaled_fd = Affine::new([
                    fd_raw.as_coeffs()[0] * 1000.0,
                    fd_raw.as_coeffs()[1] * 1000.0,
                    fd_raw.as_coeffs()[2] * 1000.0,
                    fd_raw.as_coeffs()[3] * 1000.0,
                    fd_raw.as_coeffs()[4] * 1000.0,
                    fd_raw.as_coeffs()[5] * 1000.0,
                ]);
                default_top * scaled_fd
            }
            (Some(top_matrix), None) => {
                // Top matrix set, no FD matrix → de-normalize and use directly.
                convert_font_matrix(&top_matrix.matrix.0, top_matrix.scale)
            }
            (None, None) => {
                // No explicit matrices → use the default CFF matrix [0.001, ...].
                default_cff_matrix()
            }
        }
    }
}

/// A font blob for OpenType fonts.
#[derive(Clone)]
pub(crate) struct OpenTypeFontBlob(Arc<OpenTypeFontYoke>);

impl Debug for OpenTypeFontBlob {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "OpenType Font {{ .. }}")
    }
}

impl OpenTypeFontBlob {
    pub(crate) fn new(data: FontData, index: u32) -> Option<Self> {
        // Check first whether the font is valid so we can unwrap in the closure.
        let f = FontRef::from_index(data.as_ref().as_ref(), index).ok()?;
        // Reject fonts with invalid post table version, fixes pdf.js issue 9462. Not sure if there
        // is a better fix, for some reason skrifa accepts the font which is completely invalid.
        let invalid = f.post().is_ok_and(|p| {
            !matches!(
                p.version().to_major_minor(),
                (1, 0) | (2, 0) | (2, 5) | (3, 0)
            )
        });

        if invalid {
            return None;
        }

        let font_ref_yoke =
            Yoke::<OTFYoke<'static>, FontData>::attach_to_cart(data.clone(), |data| {
                let font_ref = FontRef::from_index(data.as_ref(), index).unwrap();

                let hinting_instance = if font_ref.outline_glyphs().require_interpreter() {
                    HintingInstance::new(
                        &font_ref.outline_glyphs(),
                        Size::new(UNITS_PER_EM),
                        LocationRef::default(),
                        HintingOptions {
                            engine: Engine::Interpreter,
                            target: Target::Mono,
                        },
                    )
                    .ok()
                } else {
                    None
                };

                OTFYoke {
                    font_ref: font_ref.clone(),
                    outline_glyphs: font_ref.outline_glyphs(),
                    hinting_instance,
                    glyph_metrics: font_ref
                        .glyph_metrics(Size::new(UNITS_PER_EM), LocationRef::default()),
                }
            });

        Some(Self(Arc::new(font_ref_yoke)))
    }

    pub(crate) fn font_data(&self) -> FontData {
        self.0.backing_cart().clone()
    }

    pub(crate) fn font_ref(&self) -> &FontRef<'_> {
        &self.0.as_ref().get().font_ref
    }

    pub(crate) fn glyph_metrics(&self) -> &GlyphMetrics<'_> {
        &self.0.as_ref().get().glyph_metrics
    }

    fn outline_glyphs(&self) -> &OutlineGlyphCollection<'_> {
        &self.0.as_ref().get().outline_glyphs
    }

    pub(crate) fn outline_glyph(&self, glyph: GlyphId) -> BezPath {
        let mut path = OutlinePath::new();

        let draw_settings = if let Some(instance) = self.0.get().hinting_instance.as_ref() {
            // Note: We always hint at the font size `UNITS_PER_EM`, which obviously isn't very useful. We don't do this
            // for better text quality (right now), but instead because there are some PDFs with obscure fonts that
            // actually render wrongly if hinting is disabled!
            DrawSettings::hinted(instance, false)
        } else {
            DrawSettings::unhinted(Size::new(UNITS_PER_EM), LocationRef::default())
        };

        let Some(outline) = self.outline_glyphs().get(glyph) else {
            return BezPath::new();
        };

        let _ = outline.draw(draw_settings, &mut path);
        path.take()
    }

    pub(crate) fn num_glyphs(&self) -> u16 {
        self.font_ref().maxp().map(|m| m.num_glyphs()).unwrap_or(0)
    }
}

/// Convert a Type1 font matrix (Fixed[6]) to an Affine, accounting for upem.
///
/// The read-fonts Type1 font matrix is normalized such that the y-scale
/// component is 1.0 (or -1.0), and the upem is extracted separately.
/// The original hayro-font matrix stored the raw 0.001 scale factor in the
/// matrix itself. To get equivalent behavior, we divide by upem.
fn convert_font_matrix(matrix: &[Fixed; 6], upem: i32) -> Affine {
    let scale = 1.0 / upem.max(1) as f64;
    Affine::new([
        matrix[0].to_f64() * scale,
        matrix[1].to_f64() * scale,
        matrix[2].to_f64() * scale,
        matrix[3].to_f64() * scale,
        matrix[4].to_f64() * scale,
        matrix[5].to_f64() * scale,
    ])
}

/// Returns the default CFF font matrix: [0.001, 0, 0, 0.001, 0, 0].
fn default_cff_matrix() -> Affine {
    Affine::new([0.001, 0.0, 0.0, 0.001, 0.0, 0.0])
}

#[derive(Yokeable, Clone)]
struct OTFYoke<'a> {
    font_ref: FontRef<'a>,
    glyph_metrics: GlyphMetrics<'a>,
    hinting_instance: Option<HintingInstance>,
    outline_glyphs: OutlineGlyphCollection<'a>,
}

#[derive(Yokeable, Clone)]
struct CFFYoke<'a> {
    font: CffFontRef<'a>,
}
