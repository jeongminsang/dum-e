use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub border: Color,
    pub border_focused: Color,
    pub user_msg: Color,
    pub user_msg_bg: Color,
    pub assistant_msg: Color,
    pub system_msg: Color,
    pub status_bar_bg: Color,
    pub status_bar_fg: Color,
    pub accent: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Color::Reset,
            foreground: Color::Rgb(220, 220, 220),
            border: Color::Rgb(70, 70, 80),
            border_focused: Color::Rgb(130, 160, 240),
            user_msg: Color::Rgb(255, 255, 255),
            user_msg_bg: Color::Rgb(38, 48, 68), // clearly distinct, elegant deep indigo row highlight
            assistant_msg: Color::Rgb(180, 200, 255),
            system_msg: Color::Rgb(240, 180, 100),
            status_bar_bg: Color::Rgb(30, 30, 40),
            status_bar_fg: Color::Rgb(200, 200, 220),
            accent: Color::Rgb(140, 120, 240),
        }
    }
}

/// Generates smoothly cycling vibrant rainbow colors based on an animation tick.
pub fn rainbow_color(tick: usize) -> Color {
    const PALETTE: &[(u8, u8, u8)] = &[
        (255, 95, 135),  // Coral Pink
        (255, 135, 95),  // Orange
        (255, 215, 95),  // Warm Yellow
        (95, 255, 175),  // Mint Green
        (95, 215, 255),  // Cyan Blue
        (135, 135, 255), // Lavender Blue
        (215, 95, 255),  // Purple Magenta
        (255, 95, 215),  // Hot Pink
    ];
    let (r, g, b) = PALETTE[(tick / 2) % PALETTE.len()];
    Color::Rgb(r, g, b)
}
