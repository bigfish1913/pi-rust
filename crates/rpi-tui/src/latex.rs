//! LaTeX math rendering for terminal output.
//!
//! A terminal cannot typeset TeX, and most terminals cannot display an image,
//! so this module lowers the common math constructs to Unicode text instead:
//! symbols, scripts (`x^2` → `x²`), fractions, roots, decorations, and
//! `\begin{matrix}`-style grids. Rendering is deliberately best-effort: unknown
//! commands degrade to their bare name instead of leaking backslashes into the
//! transcript, and unsupported scripts fall back to `^(...)` notation rather
//! than dropping information.
//!
//! Entry points:
//!
//! - [`render_latex`] renders a single math expression (inline math).
//! - [`render_latex_block`] renders display math, including multi-row
//!   environments such as `aligned`, `cases`, and the `matrix` family.

use crate::utils::visible_width;

/// Options for LaTeX rendering.
#[derive(Debug, Clone)]
pub struct RenderLatexOptions {
    /// Maximum width in columns; `0` disables truncation.
    pub max_width: usize,
    /// Whether to use Unicode symbols (`x²`) instead of ASCII (`x^2`).
    pub use_unicode: bool,
    /// Whether to keep an ASCII fallback for constructs Unicode cannot express.
    pub fallback_ascii: bool,
}

impl Default for RenderLatexOptions {
    fn default() -> Self {
        Self {
            max_width: 0,
            use_unicode: true,
            fallback_ascii: true,
        }
    }
}

/// LaTeX command names (without the leading backslash) and their Unicode form.
const SYMBOLS: &[(&str, &str)] = &[
    // -- Greek letters --
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ε"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("omicron", "ο"),
    ("pi", "π"),
    ("varpi", "ϖ"),
    ("rho", "ρ"),
    ("varrho", "ϱ"),
    ("sigma", "σ"),
    ("varsigma", "ς"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "φ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Upsilon", "Υ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
    // -- Binary operators --
    ("pm", "±"),
    ("mp", "∓"),
    ("times", "×"),
    ("div", "÷"),
    ("cdot", "·"),
    ("ast", "∗"),
    ("star", "⋆"),
    ("circ", "∘"),
    ("bullet", "•"),
    ("oplus", "⊕"),
    ("ominus", "⊖"),
    ("otimes", "⊗"),
    ("oslash", "⊘"),
    ("odot", "⊙"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigodot", "⨀"),
    // -- Relations --
    ("leq", "≤"),
    ("le", "≤"),
    ("geq", "≥"),
    ("ge", "≥"),
    ("neq", "≠"),
    ("ne", "≠"),
    ("approx", "≈"),
    ("cong", "≅"),
    ("equiv", "≡"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("propto", "∝"),
    ("ll", "≪"),
    ("gg", "≫"),
    ("prec", "≺"),
    ("succ", "≻"),
    ("subset", "⊂"),
    ("subseteq", "⊆"),
    ("supset", "⊃"),
    ("supseteq", "⊇"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("perp", "⊥"),
    ("top", "⊤"),
    ("bot", "⊥"),
    ("colon", ":"),
    ("parallel", "∥"),
    ("mid", "∣"),
    ("nmid", "∤"),
    // -- Logic --
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("neg", "¬"),
    ("lnot", "¬"),
    ("land", "∧"),
    ("wedge", "∧"),
    ("lor", "∨"),
    ("vee", "∨"),
    ("implies", "⟹"),
    ("impliedby", "⟸"),
    ("iff", "⟺"),
    ("models", "⊨"),
    ("vdash", "⊢"),
    ("therefore", "∴"),
    ("because", "∵"),
    // -- Sets and large operators --
    ("cup", "∪"),
    ("cap", "∩"),
    ("setminus", "∖"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("complement", "∁"),
    ("bigcup", "⋃"),
    ("bigcap", "⋂"),
    ("bigvee", "⋁"),
    ("bigwedge", "⋀"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("coprod", "∐"),
    ("int", "∫"),
    ("iint", "∬"),
    ("iiint", "∭"),
    ("oint", "∮"),
    // -- Arrows --
    ("rightarrow", "→"),
    ("to", "→"),
    ("leftarrow", "←"),
    ("gets", "←"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("Rightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("Uparrow", "⇑"),
    ("Downarrow", "⇓"),
    ("leftrightarrow", "↔"),
    ("Leftrightarrow", "⇔"),
    ("longrightarrow", "⟶"),
    ("longleftarrow", "⟵"),
    ("Longrightarrow", "⟹"),
    ("Longleftarrow", "⟸"),
    ("mapsto", "↦"),
    ("longmapsto", "⟼"),
    ("hookrightarrow", "↪"),
    ("nearrow", "↗"),
    ("searrow", "↘"),
    ("swarrow", "↙"),
    ("nwarrow", "↖"),
    // -- Delimiters --
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lbrace", "{"),
    ("rbrace", "}"),
    // -- Ellipses and symbols --
    ("cdots", "⋯"),
    ("ldots", "…"),
    ("dots", "…"),
    ("dotsc", "…"),
    ("dotsb", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("infty", "∞"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("angle", "∠"),
    ("triangle", "△"),
    ("square", "□"),
    ("blacksquare", "■"),
    ("lozenge", "◊"),
    ("clubsuit", "♣"),
    ("diamondsuit", "♢"),
    ("heartsuit", "♡"),
    ("spadesuit", "♠"),
    ("checkmark", "✓"),
    ("dagger", "†"),
    ("ddagger", "‡"),
    ("degree", "°"),
    ("prime", "′"),
    ("hbar", "ℏ"),
    ("ell", "ℓ"),
    ("wp", "℘"),
    ("aleph", "ℵ"),
    ("Re", "ℜ"),
    ("Im", "ℑ"),
    ("S", "§"),
    ("P", "¶"),
    ("copyright", "©"),
    ("registered", "®"),
    ("trademark", "™"),
    ("hline", "─"),
    ("lvert", "|"),
    ("rvert", "|"),
    ("vert", "|"),
    ("lVert", "‖"),
    ("rVert", "‖"),
    ("Vert", "‖"),
];

/// Function names that render as plain upright text.
const FUNCTIONS: &[&str] = &[
    "arcsin", "arccos", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det", "dim",
    "exp", "gcd", "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log", "max", "min",
    "mod", "bmod", "Pr", "sec", "sin", "sinh", "sup", "tan", "tanh",
];

/// Commands whose argument is typeset as upright text.
const TEXT_COMMANDS: &[&str] = &[
    "text",
    "textrm",
    "textnormal",
    "textsf",
    "texttt",
    "mbox",
    "mathrm",
    "operatorname",
];

/// Blackboard-bold letters (`\mathbb`).
const BLACKBOARD: &[(char, char)] = &[
    ('R', 'ℝ'),
    ('N', 'ℕ'),
    ('Z', 'ℤ'),
    ('Q', 'ℚ'),
    ('C', 'ℂ'),
    ('P', 'ℙ'),
    ('E', '𝔼'),
    ('H', 'ℍ'),
    ('1', '𝟙'),
];

/// Script letters (`\mathcal`).
const CALIGRAPHIC: &[(char, char)] = &[
    ('B', 'ℬ'),
    ('E', 'ℰ'),
    ('F', 'ℱ'),
    ('H', 'ℋ'),
    ('I', 'ℐ'),
    ('L', 'ℒ'),
    ('M', 'ℳ'),
    ('O', '𝒪'),
    ('P', '𝒫'),
    ('R', 'ℛ'),
    ('S', '𝒮'),
    ('T', '𝒯'),
    ('g', 'ℊ'),
];

/// Digits and symbols that have Unicode superscript forms.
const SUPERSCRIPTS: &[(char, char)] = &[
    ('0', '⁰'),
    ('1', '¹'),
    ('2', '²'),
    ('3', '³'),
    ('4', '⁴'),
    ('5', '⁵'),
    ('6', '⁶'),
    ('7', '⁷'),
    ('8', '⁸'),
    ('9', '⁹'),
    ('+', '⁺'),
    ('-', '⁻'),
    ('=', '⁼'),
    ('(', '⁽'),
    (')', '⁾'),
    ('a', 'ᵃ'),
    ('b', 'ᵇ'),
    ('c', 'ᶜ'),
    ('d', 'ᵈ'),
    ('e', 'ᵉ'),
    ('f', 'ᶠ'),
    ('g', 'ᵍ'),
    ('h', 'ʰ'),
    ('i', 'ⁱ'),
    ('j', 'ʲ'),
    ('k', 'ᵏ'),
    ('l', 'ˡ'),
    ('m', 'ᵐ'),
    ('n', 'ⁿ'),
    ('o', 'ᵒ'),
    ('p', 'ᵖ'),
    ('r', 'ʳ'),
    ('s', 'ˢ'),
    ('t', 'ᵗ'),
    ('u', 'ᵘ'),
    ('v', 'ᵛ'),
    ('w', 'ʷ'),
    ('x', 'ˣ'),
    ('y', 'ʸ'),
    ('z', 'ᶻ'),
];

/// Digits and symbols that have Unicode subscript forms.
const SUBSCRIPTS: &[(char, char)] = &[
    ('0', '₀'),
    ('1', '₁'),
    ('2', '₂'),
    ('3', '₃'),
    ('4', '₄'),
    ('5', '₅'),
    ('6', '₆'),
    ('7', '₇'),
    ('8', '₈'),
    ('9', '₉'),
    ('+', '₊'),
    ('-', '₋'),
    ('=', '₌'),
    ('(', '₍'),
    (')', '₎'),
    ('a', 'ₐ'),
    ('e', 'ₑ'),
    ('h', 'ₕ'),
    ('i', 'ᵢ'),
    ('j', 'ⱼ'),
    ('k', 'ₖ'),
    ('l', 'ₗ'),
    ('m', 'ₘ'),
    ('n', 'ₙ'),
    ('o', 'ₒ'),
    ('p', 'ₚ'),
    ('r', 'ᵣ'),
    ('s', 'ₛ'),
    ('t', 'ₜ'),
    ('u', 'ᵤ'),
    ('v', 'ᵥ'),
    ('x', 'ₓ'),
];

/// Math environments that [`render_latex_block`] lays out as a grid.
const MATH_ENVIRONMENTS: &[&str] = &[
    "aligned",
    "align",
    "align*",
    "array",
    "Bmatrix",
    "bmatrix",
    "cases",
    "dcases",
    "equation",
    "equation*",
    "gather",
    "gather*",
    "gathered",
    "matrix",
    "multline",
    "multline*",
    "pmatrix",
    "rcases",
    "smallmatrix",
    "split",
    "substack",
    "Vmatrix",
    "vmatrix",
];

/// Check whether a `\begin{...}` environment is one this renderer lays out.
pub fn is_math_environment(env: &str) -> bool {
    MATH_ENVIRONMENTS.contains(&env)
}

/// Render one math expression to a single line of text.
pub fn render_latex(latex: &str, options: &RenderLatexOptions) -> String {
    let body = strip_delimiters(latex);
    let rendered = render_text(&body, options.use_unicode, true);
    let mut out = rendered.trim().to_string();
    if options.max_width > 0 && out.chars().count() > options.max_width {
        let keep = options.max_width.saturating_sub(1);
        let mut truncated: String = out.chars().take(keep).collect();
        truncated.push('…');
        out = truncated;
    }
    out
}

/// Render display math, returning one terminal line per rendered row.
///
/// Multi-row environments (`\\`) become multiple lines and `&` alignment
/// points become padded columns, so matrices and `aligned` blocks keep their
/// shape in a monospace transcript. Nested environments are laid out
/// recursively, so a row like `A x = b` keeps each matrix as a block instead of
/// collapsing it into one line.
pub fn render_latex_block(latex: &str, options: &RenderLatexOptions) -> Vec<String> {
    let body = strip_delimiters(latex);
    render_block_body(&body, options)
}

/// A rendered grid cell: one or more physical lines (a nested environment).
type CellLines = Vec<String>;

fn render_block_body(body: &str, options: &RenderLatexOptions) -> Vec<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return vec![String::new()];
    }

    let (env, inner) = match whole_environment(trimmed) {
        Some((env, inner)) => {
            let inner = if env == "array" {
                strip_array_spec(&inner)
            } else {
                inner
            };
            (Some(env), inner)
        }
        None => (None, trimmed.to_string()),
    };

    let rows: Vec<Vec<CellLines>> = split_top_rows(&inner)
        .iter()
        .map(|row| {
            split_top_cells(row)
                .iter()
                .map(|cell| render_block_cell(cell, options))
                .collect()
        })
        .collect();

    layout_grid(env.as_deref(), &rows)
}

fn render_block_cell(cell: &str, options: &RenderLatexOptions) -> CellLines {
    let trimmed = cell.trim();
    if trimmed.is_empty() {
        return vec![String::new()];
    }

    // A cell can mix plain math with one or more matrices, for example
    // `A x = b`. Each nested environment is laid out as its own block and the
    // pieces are composed side by side, keeping the matrices readable.
    let segments = split_cell_segments(trimmed);
    if segments.len() <= 1
        && !segments
            .iter()
            .any(|segment| matches!(segment, Segment::Env(_)))
    {
        return vec![render_text(trimmed, options.use_unicode, true)
            .trim()
            .to_string()];
    }

    let blocks: Vec<(bool, CellLines)> = segments
        .iter()
        .map(|segment| match segment {
            Segment::Text(text) => (
                true,
                vec![render_text(text, options.use_unicode, true)
                    .trim()
                    .to_string()],
            ),
            Segment::Env(source) => (false, render_block_body(source, options)),
        })
        .collect();

    let height = blocks
        .iter()
        .map(|(_, lines)| lines.len())
        .max()
        .unwrap_or(1);
    let mut out: Vec<String> = Vec::with_capacity(height);
    for line_index in 0..height {
        let mut line = String::new();
        for (position, (is_text, lines)) in blocks.iter().enumerate() {
            if position > 0 {
                line.push(' ');
            }
            if *is_text {
                // Inline text (`=`, `+`) is centered against a tall matrix.
                let text = lines.first().map(String::as_str).unwrap_or("");
                let width = visible_width(text);
                if line_index == (height - 1) / 2 {
                    line.push_str(text);
                } else {
                    line.push_str(&" ".repeat(width));
                }
            } else {
                let text = lines.get(line_index).map(String::as_str).unwrap_or("");
                line.push_str(text);
                let block_width = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
                let pad = block_width.saturating_sub(visible_width(text));
                line.push_str(&" ".repeat(pad));
            }
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// One piece of a cell: plain math text or a nested environment.
enum Segment {
    Text(String),
    Env(String),
}

/// Split a cell into plain text and nested `\begin{…}…\end{…}` blocks.
fn split_cell_segments(cell: &str) -> Vec<Segment> {
    let chars: Vec<char> = cell.chars().collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut text = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '\\' {
            let command: String = chars[index + 1..(index + 8).min(chars.len())]
                .iter()
                .collect();
            if command.starts_with("begin{") {
                if let Some(end) = environment_end(&chars, index) {
                    if !text.trim().is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    text.clear();
                    let source: String = chars[index..end].iter().collect();
                    segments.push(Segment::Env(source));
                    index = end;
                    continue;
                }
            }
        }
        text.push(chars[index]);
        index += 1;
    }
    if !text.trim().is_empty() {
        segments.push(Segment::Text(text));
    }
    segments
}

/// Index just past the `}` of the `\end{…}` matching a `\begin{…}` at `start`.
fn environment_end(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = start;
    while index < chars.len() {
        if chars[index] == '\\' {
            let command: String = chars[index + 1..(index + 8).min(chars.len())]
                .iter()
                .collect();
            if command.starts_with("begin{") {
                depth += 1;
            } else if command.starts_with("end{") {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let mut cursor = index;
                    while cursor < chars.len() && chars[cursor] != '}' {
                        cursor += 1;
                    }
                    return Some((cursor + 1).min(chars.len()));
                }
            }
            index += 1;
            continue;
        }
        index += 1;
    }
    None
}

/// Lay out a grid of multi-line cells, adding matrix brackets for the
/// environment (if any) and padding every column to a common width.
fn layout_grid(env: Option<&str>, rows: &[Vec<CellLines>]) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .flat_map(|cell| cell.iter())
                .map(|line| visible_width(line))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let (left, right) = bracket_glyphs(env);
    let total_rows = rows.len();
    let mut out: Vec<String> = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let height = row.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for line_index in 0..height {
            let mut line = String::new();
            for (column, cell) in row.iter().enumerate() {
                if column > 0 {
                    line.push_str("  ");
                }
                let text = cell.get(line_index).map(String::as_str).unwrap_or("");
                line.push_str(text);
                let pad = widths[column].saturating_sub(visible_width(text));
                line.push_str(&" ".repeat(pad));
            }
            let body = if left.is_none() && right.is_none() {
                line.trim_end().to_string()
            } else {
                line
            };
            // A bracket glyph follows the physical line it sits on: the first
            // and last line of the whole matrix get the corner glyphs, every
            // line in between gets the straight one. A one-row, one-line
            // matrix uses the plain `(`/`[`/`{` pair.
            let pick = |set: Option<GlyphSet>| -> String {
                match set {
                    Some((single, top, middle, bottom)) => {
                        let is_first = row_index == 0 && line_index == 0;
                        let is_last = row_index + 1 == total_rows && line_index + 1 == height;
                        let glyph = if is_first && is_last {
                            single
                        } else if is_first {
                            top
                        } else if is_last {
                            bottom
                        } else {
                            middle
                        };
                        glyph.to_string()
                    }
                    None => String::new(),
                }
            };
            let open = pick(left);
            let close = pick(right);
            let left_text = if open.is_empty() {
                String::new()
            } else {
                format!("{open} ")
            };
            let right_text = if close.is_empty() {
                String::new()
            } else {
                format!(" {close}")
            };
            out.push(format!("{left_text}{body}{right_text}"));
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// `(single, top, middle, bottom)` glyphs for one side of an environment.
type GlyphSet = (&'static str, &'static str, &'static str, &'static str);

/// Bracket glyphs for the matrix family (`cases` uses a left brace only).
fn bracket_glyphs(env: Option<&str>) -> (Option<GlyphSet>, Option<GlyphSet>) {
    match env {
        Some("pmatrix") => (Some(("(", "⎛", "⎜", "⎝")), Some((")", "⎞", "⎟", "⎠"))),
        Some("bmatrix") => (Some(("[", "⎡", "⎢", "⎣")), Some(("]", "⎤", "⎥", "⎦"))),
        Some("Bmatrix") => (Some(("{", "⎧", "⎨", "⎩")), Some(("}", "⎫", "⎬", "⎭"))),
        Some("vmatrix") => (Some(("|", "│", "│", "│")), Some(("|", "│", "│", "│"))),
        Some("Vmatrix") => (Some(("‖", "┃", "┃", "┃")), Some(("‖", "┃", "┃", "┃"))),
        Some("cases") | Some("dcases") => (Some(("{", "⎧", "⎨", "⎩")), None),
        Some("rcases") => (None, Some(("}", "⎫", "⎬", "⎭"))),
        _ => (None, None),
    }
}

/// Check if a string looks like it contains LaTeX math.
pub fn is_latex(s: &str) -> bool {
    s.contains('$')
        || s.contains("\\[")
        || s.contains("\\(")
        || s.contains("\\frac")
        || s.contains("\\sqrt")
        || s.contains("\\begin")
        || SYMBOLS.iter().any(|(name, _)| {
            s.match_indices(&format!("\\{name}")).any(|(index, _)| {
                !s[index + 1 + name.len()..].starts_with(|c: char| c.is_alphabetic())
            })
        })
}

/// Strip LaTeX markup, keeping readable content only.
pub fn strip_latex(latex: &str) -> String {
    let body = strip_delimiters(latex);
    render_text(&body, true, false).trim().to_string()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render `src` to text. `markup` keeps emphasis markers from `\textbf` /
/// `\textit`; `strip_latex` renders the same content without them.
fn render_text(src: &str, unicode: bool, markup: bool) -> String {
    let mut renderer = Renderer {
        src: src.chars().collect(),
        pos: 0,
        unicode,
        markup,
    };
    renderer.sequence(false)
}

struct Renderer {
    src: Vec<char>,
    pos: usize,
    unicode: bool,
    markup: bool,
}

impl Renderer {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// Render until end of input, or until the matching `}` when `in_group`.
    fn sequence(&mut self, in_group: bool) -> String {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            match ch {
                '}' => {
                    self.pos += 1;
                    if in_group {
                        return out;
                    }
                }
                '{' => {
                    self.pos += 1;
                    out.push_str(&self.sequence(true));
                }
                '\\' => {
                    self.pos += 1;
                    self.command(&mut out);
                }
                '^' | '_' => {
                    self.pos += 1;
                    self.script(ch == '^', &mut out);
                }
                '&' => {
                    self.pos += 1;
                    out.push(' ');
                }
                '~' => {
                    self.pos += 1;
                    out.push(' ');
                }
                _ => {
                    self.pos += 1;
                    out.push(ch);
                }
            }
        }
        out
    }

    /// Read a command argument: a balanced `{...}` group or a single token.
    fn raw_argument(&mut self) -> String {
        self.skip_whitespace();
        match self.peek() {
            Some('{') => {
                self.pos += 1;
                let mut depth = 1usize;
                let mut raw = String::new();
                while let Some(ch) = self.bump() {
                    match ch {
                        '{' => {
                            depth += 1;
                            raw.push(ch);
                        }
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                return raw;
                            }
                            raw.push(ch);
                        }
                        '\\' => {
                            raw.push(ch);
                            if let Some(next) = self.bump() {
                                raw.push(next);
                            }
                        }
                        _ => raw.push(ch),
                    }
                }
                raw
            }
            Some('\\') => {
                self.pos += 1;
                let mut raw = String::from("\\");
                while matches!(self.peek(), Some(c) if c.is_alphabetic()) {
                    raw.push(self.bump().unwrap());
                }
                if raw.len() == 1 {
                    if let Some(next) = self.bump() {
                        raw.push(next);
                    }
                }
                raw
            }
            Some(ch) => {
                self.pos += 1;
                ch.to_string()
            }
            None => String::new(),
        }
    }

    fn argument(&mut self) -> String {
        let raw = self.raw_argument();
        render_text(&raw, self.unicode, self.markup)
    }

    /// Render a raw (already-read) argument as upright text.
    fn text_argument(&mut self) -> String {
        let raw = self.raw_argument();
        render_text(&raw, self.unicode, self.markup)
            .trim()
            .to_string()
    }

    fn script(&mut self, superscript: bool, out: &mut String) {
        let raw = self.raw_argument();
        let rendered = render_text(&raw, self.unicode, self.markup)
            .trim()
            .to_string();
        if self.unicode {
            let table = if superscript {
                SUPERSCRIPTS
            } else {
                SUBSCRIPTS
            };
            if let Some(mapped) = map_chars(&rendered, table) {
                out.push_str(&mapped);
                return;
            }
        }
        if rendered.is_empty() {
            return;
        }
        // `QK^⊤` reads better than `QK^(⊤)`; multi-character scripts keep the
        // bracketed form so the extent stays unambiguous.
        let marker = if superscript { '^' } else { '_' };
        if rendered.chars().count() == 1 {
            out.push(marker);
            out.push_str(&rendered);
        } else {
            out.push(marker);
            out.push('(');
            out.push_str(&rendered);
            out.push(')');
        }
    }

    fn command(&mut self, out: &mut String) {
        let Some(first) = self.peek() else {
            return;
        };

        // Escaped literal characters and spacing commands.
        if !first.is_alphabetic() {
            self.pos += 1;
            match first {
                '\\' => out.push('\n'),
                ',' | ';' | ':' | '>' => out.push(' '),
                '!' => {}
                ' ' => out.push(' '),
                '{' | '}' | '$' | '%' | '&' | '#' | '_' | '|' => out.push(first),
                '[' | ']' | '(' | ')' => {}
                _ => out.push(first),
            }
            return;
        }

        let mut name = String::new();
        while matches!(self.peek(), Some(c) if c.is_alphabetic()) {
            name.push(self.bump().unwrap());
        }
        match name.as_str() {
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let numerator = self.argument();
                let denominator = self.argument();
                out.push_str(&fraction(&numerator, &denominator));
            }
            "binom" => {
                let n = self.argument();
                let k = self.argument();
                out.push_str(&format!("C({n}, {k})"));
            }
            "sqrt" => {
                let mut degree = String::new();
                if self.peek() == Some('[') {
                    self.pos += 1;
                    let mut raw = String::new();
                    while let Some(ch) = self.bump() {
                        if ch == ']' {
                            break;
                        }
                        raw.push(ch);
                    }
                    degree = render_text(&raw, self.unicode, self.markup)
                        .trim()
                        .to_string();
                }
                let radicand = self.argument();
                let root = if degree.is_empty() {
                    "√".to_string()
                } else if self.unicode {
                    format!("{}√", map_chars(&degree, SUPERSCRIPTS).unwrap_or(degree))
                } else {
                    format!("{degree}√")
                };
                out.push_str(&root);
                out.push_str(&group(&radicand));
            }
            "mathbb" => {
                let arg = self.text_argument();
                out.push_str(&map_chars(&arg, BLACKBOARD).unwrap_or(arg));
            }
            "mathcal" => {
                let arg = self.text_argument();
                out.push_str(&map_chars(&arg, CALIGRAPHIC).unwrap_or(arg));
            }
            "mathfrak" | "mathsf" | "mathbf" | "mathit" | "boldsymbol" | "bm" => {
                let arg = self.argument();
                out.push_str(&arg);
            }
            "textbf" => {
                let arg = self.text_argument();
                if self.markup {
                    out.push_str(&format!("**{arg}**"));
                } else {
                    out.push_str(&arg);
                }
            }
            "textit" | "emph" | "textsl" => {
                let arg = self.text_argument();
                if self.markup {
                    out.push_str(&format!("*{arg}*"));
                } else {
                    out.push_str(&arg);
                }
            }
            "underline" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{0332}'));
            }
            "overline" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{0305}'));
            }
            "hat" | "widehat" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{0302}'));
            }
            "bar" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{0304}'));
            }
            "vec" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{20D7}'));
            }
            "tilde" | "widetilde" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{0303}'));
            }
            "dot" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{0307}'));
            }
            "ddot" => {
                let arg = self.argument();
                out.push_str(&combine_over(&arg, '\u{0308}'));
            }
            // Size modifiers only affect glyph size in TeX; the delimiter that
            // follows renders on its own. `\right.` is an empty delimiter.
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "Bigl"
            | "Bigr" | "middle" => {
                if self.peek() == Some('.') {
                    self.pos += 1;
                }
            }
            "quad" => out.push_str("  "),
            "qquad" => out.push_str("    "),
            "begin" | "end" => {
                // Environments are laid out by `render_latex_block`; inline they
                // contribute nothing but their contents are rendered normally.
                let _ = self.raw_argument();
            }
            _ if TEXT_COMMANDS.contains(&name.as_str()) => {
                out.push_str(&self.text_argument());
            }
            _ if FUNCTIONS.contains(&name.as_str()) => out.push_str(&name),
            _ => {
                if let Some((_, symbol)) = SYMBOLS.iter().find(|(cmd, _)| *cmd == name) {
                    out.push_str(symbol);
                } else {
                    // Unknown command: keep the readable name, drop the mark.
                    out.push_str(&name);
                    if self.peek() == Some('{') {
                        let arg = self.argument();
                        out.push_str(&format!("({arg})"));
                    }
                }
            }
        }
    }
}

/// Map every character through `table`, returning `None` if any is missing.
fn map_chars(text: &str, table: &[(char, char)]) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == ' ' {
            out.push(' ');
            continue;
        }
        match table.iter().find(|(from, _)| *from == ch) {
            Some((_, to)) => out.push(*to),
            None => return None,
        }
    }
    Some(out)
}

/// Append a combining mark to every character of `text`.
fn combine_over(text: &str, mark: char) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        out.push(ch);
        if !ch.is_whitespace() {
            out.push(mark);
        }
    }
    out
}

/// Wrap a multi-character or operator-containing operand in parentheses.
fn group(text: &str) -> String {
    let trimmed = text.trim();
    let complex = trimmed.chars().count() > 1
        && trimmed
            .chars()
            .any(|c| c.is_whitespace() || "+-−×·/=<>≤≥≠∑∏∫".contains(c));
    if complex {
        format!("({trimmed})")
    } else {
        trimmed.to_string()
    }
}

fn fraction(numerator: &str, denominator: &str) -> String {
    format!("{}/{}", group(numerator), group(denominator))
}

/// If `body` is entirely wrapped in exactly one environment, return its name
/// and body.
///
/// The check is balance-aware: `\begin{pmatrix}…\end{pmatrix}
/// \begin{pmatrix}…\end{pmatrix} = …` starts and ends with environment markers
/// but is *not* one environment, and unwrapping it would silently drop every
/// term after the first matrix.
fn whole_environment(body: &str) -> Option<(String, String)> {
    let trimmed = body.trim();
    let rest = trimmed.strip_prefix("\\begin{")?;
    let name_end = rest.find('}')?;
    let env = rest[..name_end].trim().to_string();
    let open_len = "\\begin{".len() + name_end + 1;
    let content: Vec<char> = trimmed.chars().skip(open_len).collect();

    let mut depth = 1usize;
    let mut index = 0usize;
    while index < content.len() {
        if content[index] != '\\' {
            index += 1;
            continue;
        }
        let command: String = content[index + 1..(index + 8).min(content.len())]
            .iter()
            .collect();
        if command.starts_with("begin{") {
            depth += 1;
        } else if command.starts_with("end{") {
            depth -= 1;
            if depth == 0 {
                let mut cursor = index;
                while cursor < content.len() && content[cursor] != '}' {
                    cursor += 1;
                }
                if cursor == content.len() {
                    return None;
                }
                let tail: String = content[cursor + 1..].iter().collect();
                if !tail.trim().is_empty() {
                    return None;
                }
                let inner: String = content[..index].iter().collect();
                return Some((env, inner.trim().to_string()));
            }
        }
        index += 1;
    }
    None
}

/// Drop the column specification of `\begin{array}{cc}`.
fn strip_array_spec(body: &str) -> String {
    let trimmed = body.trim_start();
    let Some(rest) = trimmed.strip_prefix('{') else {
        return body.to_string();
    };
    match rest.find('}') {
        Some(index) => rest[index + 1..].trim().to_string(),
        None => body.to_string(),
    }
}

/// Split block bodies on `\\` row separators, ignoring separators that sit
/// inside a nested environment.
fn split_top_rows(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' {
            let command: String = chars[index + 1..(index + 8).min(chars.len())]
                .iter()
                .collect();
            if command.starts_with("begin{") {
                depth += 1;
            } else if command.starts_with("end{") {
                depth = depth.saturating_sub(1);
            } else if depth == 0 && command.starts_with('\\') {
                index += 2;
                // Optional row spacing: `\\[2pt]`.
                if chars.get(index) == Some(&'[') {
                    index += 1;
                    while index < chars.len() && chars[index] != ']' {
                        index += 1;
                    }
                    index += 1;
                }
                rows.push(current.trim().to_string());
                current.clear();
                continue;
            } else if depth == 0 && command.starts_with("cr") {
                index += 3;
                rows.push(current.trim().to_string());
                current.clear();
                continue;
            }
            current.push(chars[index]);
            index += 1;
            continue;
        }
        current.push(chars[index]);
        index += 1;
    }
    if !current.trim().is_empty() || rows.is_empty() {
        rows.push(current.trim().to_string());
    }
    rows
}

/// Split one row on top-level `&` alignment tabs (inside environments `&` is a
/// cell separator for that inner grid, not for this row).
fn split_top_cells(row: &str) -> Vec<String> {
    let chars: Vec<char> = row.chars().collect();
    let mut cells: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\\' {
            let command: String = chars[index + 1..(index + 8).min(chars.len())]
                .iter()
                .collect();
            if command.starts_with("begin{") {
                depth += 1;
            } else if command.starts_with("end{") {
                depth = depth.saturating_sub(1);
            }
            current.push(ch);
            index += 1;
            if let Some(next) = chars.get(index) {
                current.push(*next);
                index += 1;
            }
            continue;
        }
        if ch == '&' && depth == 0 {
            cells.push(current.clone());
            current.clear();
            index += 1;
            continue;
        }
        current.push(ch);
        index += 1;
    }
    cells.push(current);
    cells
}

/// Remove `$...$`, `$$...$$`, `\(...\)` and `\[...\]` delimiters.
fn strip_delimiters(latex: &str) -> String {
    let mut text = latex.trim();
    for (open, close) in [("$$", "$$"), ("\\[", "\\]"), ("\\(", "\\)"), ("$", "$")] {
        if let Some(rest) = text.strip_prefix(open) {
            if let Some(inner) = rest.strip_suffix(close) {
                text = inner.trim();
                break;
            }
            // An unterminated opener (streaming) still renders its body.
            text = rest.trim();
            break;
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(latex: &str) -> String {
        render_latex(latex, &RenderLatexOptions::default())
    }

    #[test]
    fn renders_greek_and_operators() {
        assert_eq!(render("$\\alpha + \\beta$"), "α + β");
        assert_eq!(render("\\lambda \\cdot \\pi"), "λ · π");
        assert_eq!(render("$a \\times b \\leq c$"), "a × b ≤ c");
    }

    #[test]
    fn renders_scripts_as_unicode() {
        assert_eq!(render("$x^2$"), "x²");
        assert_eq!(render("$x_i$"), "xᵢ");
        assert_eq!(render("$x^2 + y^{10}$"), "x² + y¹⁰");
        assert_eq!(render("$\\sum_{i=1}^{n} i$"), "∑ᵢ₌₁ⁿ i");
        // Characters without a Unicode form degrade to `^(...)`.
        assert_eq!(render("$x^q$"), "x^q");
        assert_eq!(render("$x^{qr}$"), "x^(qr)");
    }

    #[test]
    fn ascii_mode_keeps_caret_notation() {
        let options = RenderLatexOptions {
            use_unicode: false,
            ..RenderLatexOptions::default()
        };
        assert_eq!(render_latex("$x^2$", &options), "x^2");
    }

    #[test]
    fn renders_fractions_and_roots() {
        assert_eq!(render("$\\frac{1}{2}$"), "1/2");
        assert_eq!(render("$\\frac{a+b}{c}$"), "(a+b)/c");
        assert_eq!(render("$\\sqrt{x}$"), "√x");
        assert_eq!(render("$\\sqrt{x+1}$"), "√(x+1)");
        assert_eq!(render("$\\sqrt[3]{x}$"), "³√x");
    }

    #[test]
    fn renders_text_commands_without_markup() {
        assert_eq!(render("$\\text{if } x > 0$"), "if x > 0");
        assert_eq!(render("$\\mathbb{R}^n$"), "ℝⁿ");
        assert_eq!(render("$\\operatorname{softmax}(x)$"), "softmax(x)");
        assert_eq!(render("$\\textbf{bold}$"), "**bold**");
    }

    #[test]
    fn renders_decorations() {
        assert_eq!(render("$\\hat{x}$"), "x\u{0302}");
        assert_eq!(render("$\\vec{v}$"), "v\u{20D7}");
        assert_eq!(render("$\\overline{AB}$"), "A\u{0305}B\u{0305}");
    }

    #[test]
    fn unknown_commands_keep_their_name() {
        assert_eq!(render("$\\foobar$"), "foobar");
        assert_eq!(render("$\\unknown{x}$"), "unknown(x)");
    }

    #[test]
    fn escaped_literals_are_preserved() {
        assert_eq!(render("$a\\,b$"), "a b");
        assert_eq!(render("$100\\%\\ \\text{of}\\ x$"), "100% of x");
        assert_eq!(render("$\\{1, 2\\}$"), "{1, 2}");
    }

    #[test]
    fn display_block_renders_rows() {
        let options = RenderLatexOptions::default();
        let lines = render_latex_block(
            "$$\\begin{aligned} a &= b \\\\ c &= d \\end{aligned}$$",
            &options,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "a  = b");
        assert_eq!(lines[1], "c  = d");
    }

    #[test]
    fn display_block_pads_matrix_columns() {
        let options = RenderLatexOptions::default();
        let lines = render_latex_block(
            "\\begin{pmatrix} 1 & 22 \\\\ 333 & 4 \\end{pmatrix}",
            &options,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("⎛ "), "{:?}", lines[0]);
        assert!(lines[1].starts_with("⎝ "), "{:?}", lines[1]);
        // Column two starts at the same display column in every row.
        let col = |line: &str| line.find('2').or_else(|| line.find('4'));
        assert_eq!(col(&lines[0]).map(|i| i > 0), Some(true));
        assert_eq!(visible_width(&lines[0]), visible_width(&lines[1]));
    }

    #[test]
    fn display_block_composes_matrix_equation() {
        let options = RenderLatexOptions::default();
        let lines = render_latex_block(
            "\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix} \
             \\begin{pmatrix} x \\\\ y \\end{pmatrix} = \
             \\begin{pmatrix} ax + by \\\\ cx + dy \\end{pmatrix}",
            &options,
        );
        assert_eq!(lines.len(), 2, "{lines:?}");
        // Three matrices per row, all top-aligned on row 0 and bottom on row 1.
        assert_eq!(lines[0].matches('⎛').count(), 3, "{lines:?}");
        assert_eq!(lines[1].matches('⎝').count(), 3, "{lines:?}");
        assert!(lines[0].contains('='), "operator lost: {lines:?}");
    }

    #[test]
    fn display_block_supports_cases_and_array() {
        let options = RenderLatexOptions::default();
        let lines = render_latex_block(
            "\\begin{cases} x & x > 0 \\\\ -x & x \\leq 0 \\end{cases}",
            &options,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("⎧ "), "{lines:?}");
        assert!(lines[1].starts_with("⎩ "), "{lines:?}");
        assert!(lines[1].contains('≤'), "{lines:?}");

        let lines = render_latex_block(
            "\\begin{array}{cc} 1 & 22 \\\\ 333 & 4 \\end{array}",
            &options,
        );
        assert_eq!(lines, vec!["1    22", "333  4"]);
    }

    #[test]
    fn aligned_rows_align_on_the_equals_sign() {
        let options = RenderLatexOptions::default();
        let lines = render_latex_block(
            "\\begin{aligned} y &= f(x) \\\\ zz &= g(x) \\end{aligned}",
            &options,
        );
        assert_eq!(lines.len(), 2);
        let first = lines[0].find('=').expect("first row has =");
        let second = lines[1].find('=').expect("second row has =");
        assert_eq!(first, second, "{lines:?}");
    }

    #[test]
    fn display_block_without_environment_keeps_rows() {
        let options = RenderLatexOptions::default();
        let lines = render_latex_block("\\[ a = 1 \\\\ b = 2 \\]", &options);
        assert_eq!(lines, vec!["a = 1", "b = 2"]);
    }

    #[test]
    fn unterminated_delimiter_still_renders() {
        assert_eq!(render("$$x^2"), "x²");
        assert_eq!(render("$x^2"), "x²");
    }

    #[test]
    fn is_latex_detects_math_markers() {
        assert!(is_latex("$x^2$"));
        assert!(is_latex("\\alpha"));
        assert!(is_latex("\\frac{1}{2}"));
        assert!(!is_latex("plain text"));
        // A backslash command prefix must not match a longer command name.
        assert!(!is_latex("\\pixel"));
    }

    #[test]
    fn strip_latex_removes_markup() {
        assert_eq!(strip_latex("\\textbf{bold}"), "bold");
        assert_eq!(strip_latex("$\\frac{1}{2}$"), "1/2");
    }

    #[test]
    fn max_width_truncates_on_char_boundary() {
        let options = RenderLatexOptions {
            max_width: 2,
            ..RenderLatexOptions::default()
        };
        let rendered = render_latex("$\\alpha\\beta\\gamma$", &options);
        assert_eq!(rendered.chars().count(), 2, "{rendered}");
        assert!(rendered.ends_with('…'));
    }
}
