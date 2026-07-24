//! Quadratic Bézier curve representation and glyph outline collection.

#[derive(Clone, Debug)]
pub struct QuadraticCurve {
    pub p1: (f32, f32),
    pub p2: (f32, f32),
    pub p3: (f32, f32),
}

impl QuadraticCurve {
    pub fn min_x(&self) -> f32 {
        self.p1.0.min(self.p2.0).min(self.p3.0)
    }
    pub fn max_x(&self) -> f32 {
        self.p1.0.max(self.p2.0).max(self.p3.0)
    }
    pub fn min_y(&self) -> f32 {
        self.p1.1.min(self.p2.1).min(self.p3.1)
    }
    pub fn max_y(&self) -> f32 {
        self.p1.1.max(self.p2.1).max(self.p3.1)
    }

    /// True when the curve degenerates to a horizontal segment (constant y).
    pub fn is_horizontal(&self) -> bool {
        let eps = 1.0 / 65536.0;
        (self.p1.1 - self.p3.1).abs() < eps && (self.p2.1 - self.p3.1).abs() < eps
    }

    /// True when the curve degenerates to a vertical segment (constant x).
    pub fn is_vertical(&self) -> bool {
        let eps = 1.0 / 65536.0;
        (self.p1.0 - self.p3.0).abs() < eps && (self.p2.0 - self.p3.0).abs() < eps
    }
}

/// Glyph outlines as a list of contours; each contour is a chain of quadratic
/// Béziers where consecutive curves share an endpoint
/// (`contour[i].p3 == contour[i+1].p1`). Coordinates are em-space (~ 0..1).
#[derive(Clone, Debug)]
pub struct GlyphOutlines {
    pub contours: Vec<Vec<QuadraticCurve>>,
}

impl GlyphOutlines {
    /// Em-space bounding box (min_x, min_y, max_x, max_y). Returns zeros for empty glyphs.
    pub fn bounding_box(&self) -> (f32, f32, f32, f32) {
        let mut found = false;
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for contour in &self.contours {
            for c in contour {
                min_x = min_x.min(c.min_x());
                min_y = min_y.min(c.min_y());
                max_x = max_x.max(c.max_x());
                max_y = max_y.max(c.max_y());
                found = true;
            }
        }
        if !found {
            return (0.0, 0.0, 0.0, 0.0);
        }
        (min_x, min_y, max_x, max_y)
    }

}

pub(crate) struct OutlineCollector {
    units_per_em: u16,
    contours: Vec<Vec<QuadraticCurve>>,
    current: Vec<QuadraticCurve>,
    last: (f32, f32),
    start: (f32, f32),
}

impl OutlineCollector {
    pub fn new(units_per_em: u16) -> Self {
        Self {
            units_per_em,
            contours: Vec::new(),
            current: Vec::new(),
            last: (0.0, 0.0),
            start: (0.0, 0.0),
        }
    }

    pub fn finish(mut self) -> GlyphOutlines {
        if !self.current.is_empty() {
            self.contours.push(std::mem::take(&mut self.current));
        }
        let upem = self.units_per_em as f32;
        let contours: Vec<Vec<QuadraticCurve>> = self
            .contours
            .into_iter()
            .map(|contour| {
                contour
                    .into_iter()
                    .map(|c| QuadraticCurve {
                        p1: (c.p1.0 / upem, c.p1.1 / upem),
                        p2: (c.p2.0 / upem, c.p2.1 / upem),
                        p3: (c.p3.0 / upem, c.p3.1 / upem),
                    })
                    .collect()
            })
            .collect();
        GlyphOutlines { contours }
    }
}

impl ttf_parser::OutlineBuilder for OutlineCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        if !self.current.is_empty() {
            self.contours.push(std::mem::take(&mut self.current));
        }
        self.last = (x, y);
        self.start = (x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        // Encode a line as a quadratic with duplicated endpoint, per Lengyel's
        // recommendation: avoids the degenerate a == 0 path in the solver.
        let (lx, ly) = self.last;
        self.current.push(QuadraticCurve {
            p1: (lx, ly),
            p2: (x, y),
            p3: (x, y),
        });
        self.last = (x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (lx, ly) = self.last;
        self.current.push(QuadraticCurve {
            p1: (lx, ly),
            p2: (x1, y1),
            p3: (x, y),
        });
        self.last = (x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (lx, ly) = self.last;
        let cubic = [(lx, ly), (x1, y1), (x2, y2), (x, y)];
        for q in cubic_to_quadratics(&cubic) {
            self.current.push(q);
        }
        self.last = (x, y);
    }

    fn close(&mut self) {
        if (self.last.0 - self.start.0).abs() > 1e-3 || (self.last.1 - self.start.1).abs() > 1e-3 {
            self.line_to(self.start.0, self.start.1);
        }
    }
}

fn cubic_to_quadratics(cubic: &[(f32, f32); 4]) -> [QuadraticCurve; 2] {
    let (p0, p1, p2, p3) = (cubic[0], cubic[1], cubic[2], cubic[3]);
    let t = 0.5;
    let mt = 1.0 - t;
    let mid_x =
        mt * mt * mt * p0.0 + 3.0 * mt * mt * t * p1.0 + 3.0 * mt * t * t * p2.0 + t * t * t * p3.0;
    let mid_y =
        mt * mt * mt * p0.1 + 3.0 * mt * mt * t * p1.1 + 3.0 * mt * t * t * p2.1 + t * t * t * p3.1;
    let q1cp = (
        (p0.0 + 2.0 * p1.0) / 3.0 * 0.5 + (2.0 * p0.0 + p1.0) / 3.0 * 0.5,
        (p0.1 + 2.0 * p1.1) / 3.0 * 0.5 + (2.0 * p0.1 + p1.1) / 3.0 * 0.5,
    );
    let q2cp = (
        (p2.0 + 2.0 * p3.0) / 3.0 * 0.5 + (2.0 * p2.0 + p3.0) / 3.0 * 0.5,
        (p2.1 + 2.0 * p3.1) / 3.0 * 0.5 + (2.0 * p2.1 + p3.1) / 3.0 * 0.5,
    );
    [
        QuadraticCurve {
            p1: p0,
            p2: q1cp,
            p3: (mid_x, mid_y),
        },
        QuadraticCurve {
            p1: (mid_x, mid_y),
            p2: q2cp,
            p3,
        },
    ]
}
