//! Colors and common styles. Always take colors from here so light/dark both work.

use ratatui::style::{Color, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub added: Color,
    pub removed: Color,
    pub modified: Color,
    pub renamed: Color,
    pub untracked: Color,
    pub ignored: Color,
    pub conflict: Color,
    pub added_bg: Color,
    pub removed_bg: Color,
    pub added_word_bg: Color,
    pub removed_word_bg: Color,
    pub hunk: Color,
    pub hash: Color,
    pub author: Color,
    pub date: Color,
    pub local_branch: Color,
    pub remote_branch: Color,
    pub tag: Color,
    pub head: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
    /// Lane colors for the commit graph
    pub graph: [Color; 8],
    /// syntect theme name used for syntax highlighting
    pub syntax_theme: &'static str,
}

impl Theme {
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            _ => Self::dark(),
        }
    }

    pub fn dark() -> Self {
        Self {
            fg: Color::Rgb(0xd4, 0xd7, 0xdd),
            dim: Color::Rgb(0x7f, 0x84, 0x8e),
            accent: Color::Rgb(0x6c, 0xa9, 0xf2),
            border: Color::Rgb(0x3a, 0x3e, 0x46),
            border_focus: Color::Rgb(0x6c, 0xa9, 0xf2),
            selection_bg: Color::Rgb(0x26, 0x36, 0x55),
            added: Color::Rgb(0x73, 0xc9, 0x91),
            removed: Color::Rgb(0xf0, 0x71, 0x78),
            modified: Color::Rgb(0x6c, 0xa9, 0xf2),
            renamed: Color::Rgb(0xc6, 0x95, 0xe8),
            untracked: Color::Rgb(0xd0, 0x8a, 0x5c),
            ignored: Color::Rgb(0x6b, 0x70, 0x78),
            conflict: Color::Rgb(0xff, 0x6b, 0x6b),
            added_bg: Color::Rgb(0x1f, 0x35, 0x28),
            removed_bg: Color::Rgb(0x3d, 0x22, 0x25),
            added_word_bg: Color::Rgb(0x2b, 0x5c, 0x3c),
            removed_word_bg: Color::Rgb(0x75, 0x2f, 0x35),
            hunk: Color::Rgb(0x56, 0xb6, 0xc2),
            hash: Color::Rgb(0xe5, 0xc0, 0x7b),
            author: Color::Rgb(0x98, 0xc3, 0x79),
            date: Color::Rgb(0x7f, 0x84, 0x8e),
            local_branch: Color::Rgb(0x73, 0xc9, 0x91),
            remote_branch: Color::Rgb(0xc6, 0x95, 0xe8),
            tag: Color::Rgb(0xe5, 0xc0, 0x7b),
            head: Color::Rgb(0x6c, 0xa9, 0xf2),
            warning: Color::Rgb(0xe5, 0xc0, 0x7b),
            error: Color::Rgb(0xff, 0x6b, 0x6b),
            success: Color::Rgb(0x73, 0xc9, 0x91),
            graph: [
                Color::Rgb(0x6c, 0xa9, 0xf2),
                Color::Rgb(0x73, 0xc9, 0x91),
                Color::Rgb(0xe5, 0xc0, 0x7b),
                Color::Rgb(0xc6, 0x95, 0xe8),
                Color::Rgb(0x56, 0xb6, 0xc2),
                Color::Rgb(0xf0, 0x71, 0x78),
                Color::Rgb(0xd0, 0x8a, 0x5c),
                Color::Rgb(0xa0, 0xa8, 0xb7),
            ],
            syntax_theme: "base16-ocean.dark",
        }
    }

    pub fn light() -> Self {
        Self {
            fg: Color::Rgb(0x24, 0x29, 0x2f),
            dim: Color::Rgb(0x6e, 0x77, 0x81),
            accent: Color::Rgb(0x09, 0x69, 0xda),
            border: Color::Rgb(0xd8, 0xdc, 0xe0),
            border_focus: Color::Rgb(0x09, 0x69, 0xda),
            selection_bg: Color::Rgb(0xcc, 0xe0, 0xff),
            added: Color::Rgb(0x1a, 0x7f, 0x37),
            removed: Color::Rgb(0xcf, 0x22, 0x2e),
            modified: Color::Rgb(0x09, 0x69, 0xda),
            renamed: Color::Rgb(0x82, 0x50, 0xdf),
            untracked: Color::Rgb(0xbc, 0x4c, 0x00),
            ignored: Color::Rgb(0x8c, 0x95, 0x9f),
            conflict: Color::Rgb(0xcf, 0x22, 0x2e),
            added_bg: Color::Rgb(0xe6, 0xff, 0xec),
            removed_bg: Color::Rgb(0xff, 0xeb, 0xe9),
            added_word_bg: Color::Rgb(0xab, 0xf2, 0xbc),
            removed_word_bg: Color::Rgb(0xff, 0xc1, 0xc0),
            hunk: Color::Rgb(0x05, 0x50, 0xae),
            hash: Color::Rgb(0x95, 0x38, 0x00),
            author: Color::Rgb(0x1a, 0x7f, 0x37),
            date: Color::Rgb(0x6e, 0x77, 0x81),
            local_branch: Color::Rgb(0x1a, 0x7f, 0x37),
            remote_branch: Color::Rgb(0x82, 0x50, 0xdf),
            tag: Color::Rgb(0x95, 0x38, 0x00),
            head: Color::Rgb(0x09, 0x69, 0xda),
            warning: Color::Rgb(0x9a, 0x67, 0x00),
            error: Color::Rgb(0xcf, 0x22, 0x2e),
            success: Color::Rgb(0x1a, 0x7f, 0x37),
            graph: [
                Color::Rgb(0x09, 0x69, 0xda),
                Color::Rgb(0x1a, 0x7f, 0x37),
                Color::Rgb(0x9a, 0x67, 0x00),
                Color::Rgb(0x82, 0x50, 0xdf),
                Color::Rgb(0x1b, 0x7c, 0x83),
                Color::Rgb(0xcf, 0x22, 0x2e),
                Color::Rgb(0xbc, 0x4c, 0x00),
                Color::Rgb(0x57, 0x60, 0x6a),
            ],
            syntax_theme: "InspiredGitHub",
        }
    }

    pub fn text(&self) -> Style {
        Style::default().fg(self.fg)
    }

    pub fn dimmed(&self) -> Style {
        Style::default().fg(self.dim)
    }

    /// Selection is shown by its background alone; text keeps its regular weight.
    pub fn selected(&self) -> Style {
        Style::default().bg(self.selection_bg)
    }

    pub fn title(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn border(&self, focused: bool) -> Style {
        Style::default().fg(if focused {
            self.border_focus
        } else {
            self.border
        })
    }

    pub fn key(&self) -> Style {
        Style::default().fg(self.accent)
    }
}
