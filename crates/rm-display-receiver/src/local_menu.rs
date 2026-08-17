use rm_display_core::{LocalOverlay, RefreshProfile};

use crate::evdev::{PhysicalPointerEvent, PointerPhase};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMenuAction {
    SetProfile(RefreshProfile),
    TogglePartialRefresh,
    FullRefresh,
    NewPair,
    CloseApp,
    Close,
}

#[derive(Debug, Default)]
pub struct LocalMenu {
    visible: bool,
    custom_available: bool,
}

impl LocalMenu {
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    pub fn damage_height(&self, panel_height: u32) -> u32 {
        menu_height(panel_height)
    }

    pub fn action_for_report(
        &self,
        report: &[PhysicalPointerEvent],
        width: u32,
        height: u32,
    ) -> Option<LocalMenuAction> {
        if !self.visible {
            return None;
        }
        report
            .iter()
            .rev()
            .find(|event| event.phase == PointerPhase::Up)
            .map(|event| hit_test(event.x, event.y, width, height, self.custom_available))
    }

    pub fn render(
        &mut self,
        overlay: &mut LocalOverlay,
        width: u32,
        height: u32,
        profile: RefreshProfile,
        partial_refresh: bool,
        custom_available: bool,
    ) {
        self.custom_available = custom_available;
        if !self.visible {
            overlay.clear();
            return;
        }
        let menu_height = menu_height(height);
        let len = width as usize * height as usize;
        let mut luma = vec![0_u8; len];
        let mut alpha = vec![0_u8; len];
        let mut canvas = Canvas {
            luma: &mut luma,
            alpha: &mut alpha,
            stride: width,
        };
        canvas.fill_rect(0, 0, width, menu_height, 255);
        canvas.fill_rect(0, 0, width, 5, 0);
        let profile_buttons = profile_button_bounds(width, menu_height, custom_available);
        let profile_scale = if profile_buttons[0].0 .2 >= 100 { 2 } else { 1 };
        for ((x, y, button_width, button_height), candidate) in profile_buttons {
            let active = profile == candidate;
            let background = if active { 0 } else { 235 };
            let foreground = if active { 255 } else { 0 };
            canvas.fill_rect(x, y, button_width, button_height, background);
            canvas.draw_text_centered(
                x,
                y,
                button_width,
                button_height,
                profile_name(candidate),
                profile_scale,
                foreground,
            );
        }
        let rows = control_row_bounds(menu_height);
        let labels = [
            if partial_refresh {
                "PARTIAL ON"
            } else {
                "PARTIAL OFF"
            },
            "FULL REFRESH",
            "NEW PAIR",
            "CLOSE APP",
            "CLOSE MENU",
        ];
        canvas.draw_text(24, 18, "RM DISPLAY", 3, 0);
        canvas.draw_text(
            width.saturating_sub(310),
            18,
            &format!("ACTIVE {}", profile_name(profile)),
            2,
            0,
        );
        for ((top, bottom), label) in rows.into_iter().zip(labels) {
            canvas.fill_rect(
                16,
                top + 5,
                width.saturating_sub(32),
                bottom.saturating_sub(top + 10),
                235,
            );
            canvas.draw_text(32, top + (bottom - top).saturating_sub(14) / 2, label, 2, 0);
        }
        overlay
            .replace_planes(&luma, &alpha)
            .expect("local menu planes match overlay geometry");
    }
}

fn hit_test(x: u32, y: u32, width: u32, height: u32, custom_available: bool) -> LocalMenuAction {
    if x >= width || y >= menu_height(height) {
        return LocalMenuAction::Close;
    }
    for ((left, top, button_width, button_height), profile) in
        profile_button_bounds(width, menu_height(height), custom_available)
    {
        if x >= left
            && x < left.saturating_add(button_width)
            && y >= top
            && y < top.saturating_add(button_height)
        {
            return LocalMenuAction::SetProfile(profile);
        }
    }
    let actions = [
        LocalMenuAction::TogglePartialRefresh,
        LocalMenuAction::FullRefresh,
        LocalMenuAction::NewPair,
        LocalMenuAction::CloseApp,
        LocalMenuAction::Close,
    ];
    control_row_bounds(menu_height(height))
        .into_iter()
        .position(|(top, bottom)| y >= top && y < bottom)
        .map_or(LocalMenuAction::Close, |index| actions[index])
}

fn menu_height(height: u32) -> u32 {
    height.clamp(240, 560)
}

const PROFILES: [RefreshProfile; 6] = [
    RefreshProfile::Realtime,
    RefreshProfile::Animate,
    RefreshProfile::Balanced,
    RefreshProfile::Reading,
    RefreshProfile::Quality,
    RefreshProfile::Custom,
];

fn profile_button_bounds(
    width: u32,
    height: u32,
    custom_available: bool,
) -> Vec<((u32, u32, u32, u32), RefreshProfile)> {
    let profiles = if custom_available {
        &PROFILES[..]
    } else {
        &PROFILES[..5]
    };
    let count = profiles.len() as u32;
    let left = 16.min(width / 8);
    let gap = 8.min(width / 20);
    let available = width
        .saturating_sub(left.saturating_mul(2))
        .saturating_sub(gap.saturating_mul(count.saturating_sub(1)));
    let top = 68.min(height / 4);
    let bottom = profile_buttons_bottom(height);
    profiles
        .iter()
        .copied()
        .enumerate()
        .map(|(index, profile)| {
            let index = index as u32;
            let x = left + available * index / count + gap * index;
            let next = left + available * (index + 1) / count + gap * index;
            (
                (x, top, next.saturating_sub(x), bottom.saturating_sub(top)),
                profile,
            )
        })
        .collect()
}

fn profile_buttons_bottom(height: u32) -> u32 {
    154.min(height / 2)
}

fn control_row_bounds(height: u32) -> [(u32, u32); 5] {
    let top = profile_buttons_bottom(height).saturating_add(8).min(height);
    let available = height.saturating_sub(top);
    std::array::from_fn(|index| {
        let row_top = top + available * index as u32 / 5;
        let row_bottom = top + available * (index as u32 + 1) / 5;
        (row_top, row_bottom)
    })
}

fn profile_name(profile: RefreshProfile) -> &'static str {
    match profile {
        RefreshProfile::Realtime => "REALTIME",
        RefreshProfile::Animate => "ANIMATE",
        RefreshProfile::Balanced => "BALANCED",
        RefreshProfile::Reading => "READING",
        RefreshProfile::Quality => "QUALITY",
        RefreshProfile::Custom => "CUSTOM",
    }
}

struct Canvas<'a> {
    luma: &'a mut [u8],
    alpha: &'a mut [u8],
    stride: u32,
}

impl Canvas<'_> {
    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, value: u8) {
        if x >= self.stride || self.stride == 0 {
            return;
        }
        let width = width.min(self.stride - x);
        let rows = self.luma.len() / self.stride as usize;
        for row in y..y.saturating_add(height) {
            if row as usize >= rows {
                break;
            }
            let start = row as usize * self.stride as usize + x as usize;
            let end = start + width as usize;
            self.luma[start..end].fill(value);
            self.alpha[start..end].fill(255);
        }
    }

    fn draw_text(&mut self, x: u32, y: u32, text: &str, scale: u32, value: u8) {
        let mut cursor = x;
        for byte in text.bytes() {
            let glyph = glyph(byte);
            for (row, bits) in glyph.into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) != 0 {
                        self.fill_rect(
                            cursor + column * scale,
                            y + row as u32 * scale,
                            scale,
                            scale,
                            value,
                        );
                    }
                }
            }
            cursor = cursor.saturating_add(6 * scale);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text_centered(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        text: &str,
        scale: u32,
        value: u8,
    ) {
        let text_width = text.len() as u32 * 6 * scale;
        let text_height = 7 * scale;
        self.draw_text(
            x.saturating_add(width.saturating_sub(text_width) / 2),
            y.saturating_add(height.saturating_sub(text_height) / 2),
            text,
            scale,
            value,
        );
    }
}

fn glyph(byte: u8) -> [u8; 7] {
    match byte {
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'B' => [30, 17, 17, 30, 17, 17, 30],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'F' => [31, 16, 16, 30, 16, 16, 16],
        b'G' => [14, 17, 16, 23, 17, 17, 14],
        b'H' => [17, 17, 17, 31, 17, 17, 17],
        b'I' => [31, 4, 4, 4, 4, 4, 31],
        b'L' => [16, 16, 16, 16, 16, 16, 31],
        b'M' => [17, 27, 21, 21, 17, 17, 17],
        b'N' => [17, 25, 21, 19, 17, 17, 17],
        b'O' => [14, 17, 17, 17, 17, 17, 14],
        b'P' => [30, 17, 17, 30, 16, 16, 16],
        b'Q' => [14, 17, 17, 17, 21, 18, 13],
        b'R' => [30, 17, 17, 30, 20, 18, 17],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'T' => [31, 4, 4, 4, 4, 4, 4],
        b'U' => [17, 17, 17, 17, 17, 17, 14],
        b'V' => [17, 17, 17, 17, 17, 10, 4],
        b'W' => [17, 17, 17, 21, 21, 21, 10],
        b'X' => [17, 17, 10, 4, 10, 17, 17],
        b'Y' => [17, 17, 10, 4, 4, 4, 4],
        b' ' => [0; 7],
        _ => [31, 1, 2, 4, 4, 0, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touches_map_to_actions_and_outside_closes() {
        let mut menu = LocalMenu::default();
        menu.toggle();
        let event = |x, y| PhysicalPointerEvent {
            phase: PointerPhase::Up,
            contact_id: 1,
            x,
            y,
        };
        menu.custom_available = true;
        for ((left, top, width, height), profile) in
            profile_button_bounds(960, menu_height(1696), true)
        {
            assert_eq!(
                menu.action_for_report(&[event(left + width / 2, top + height / 2)], 960, 1696,),
                Some(LocalMenuAction::SetProfile(profile))
            );
        }
        assert_eq!(
            menu.action_for_report(&[event(100, 300)], 960, 1696),
            Some(LocalMenuAction::FullRefresh)
        );
        assert_eq!(
            menu.action_for_report(&[event(100, 1650)], 960, 1696),
            Some(LocalMenuAction::Close)
        );
    }

    #[test]
    fn closing_clears_overlay_instead_of_mutating_base() {
        let mut menu = LocalMenu::default();
        let mut overlay = LocalOverlay::transparent(100, 300).unwrap();
        menu.toggle();
        menu.render(
            &mut overlay,
            100,
            300,
            RefreshProfile::Balanced,
            true,
            false,
        );
        assert!(!overlay.is_transparent());
        menu.close();
        menu.render(
            &mut overlay,
            100,
            300,
            RefreshProfile::Balanced,
            true,
            false,
        );
        assert!(overlay.is_transparent());
    }

    #[test]
    fn active_profile_button_is_visually_distinct() {
        let mut menu = LocalMenu::default();
        let mut overlay = LocalOverlay::transparent(960, 1696).unwrap();
        menu.toggle();
        menu.render(
            &mut overlay,
            960,
            1696,
            RefreshProfile::Reading,
            true,
            false,
        );

        let profiles = profile_button_bounds(960, menu_height(1696), false);
        let active = profiles[3].0;
        let inactive = profiles[2].0;
        let base = rm_display_core::PixelSurface::new(960, 1696, 255).unwrap();
        let composed = base.compose(&overlay).unwrap();
        let pixel = |x: u32, y: u32| composed.pixels()[y as usize * 960 + x as usize];
        assert_eq!(pixel(active.0 + 2, active.1 + 2), 0);
        assert_eq!(pixel(inactive.0 + 2, inactive.1 + 2), 235);
    }
}
