use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixGeometry {
    pub model: MatrixModel,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentArea {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixModel {
    Ga401,
    Ga402,
    Gu604,
    Strix,
    Unknown,
}

impl MatrixGeometry {
    pub fn detect() -> Self {
        let board_name = fs::read_to_string("/sys/class/dmi/id/board_name").unwrap_or_default();
        Self::for_board_name(board_name.trim())
    }

    pub fn for_board_name(board_name: &str) -> Self {
        let board_name = board_name.to_ascii_uppercase();
        let model = if board_name.contains("GA401") {
            MatrixModel::Ga401
        } else if board_name.contains("GA402") {
            MatrixModel::Ga402
        } else if board_name.contains("GU604") {
            MatrixModel::Gu604
        } else if board_name.contains("G635L") || board_name.contains("G835L") {
            MatrixModel::Strix
        } else {
            MatrixModel::Unknown
        };
        match model {
            MatrixModel::Ga401 => Self { model, width: 74, height: 36 },
            MatrixModel::Ga402 => Self { model, width: 74, height: 39 },
            MatrixModel::Gu604 => Self { model, width: 70, height: 43 },
            MatrixModel::Strix => Self { model, width: 68, height: 34 },
            MatrixModel::Unknown => Self { model, width: 74, height: 39 },
        }
    }

    /// The whole pixel-image canvas, including parts the panel may clip.
    pub fn full_area(&self) -> ContentArea {
        ContentArea { x: 0, y: 0, width: self.width, height: self.height }
    }

    pub fn safe_content_area(&self) -> ContentArea {
        match self.model {
            // The GA402 diagonal transport canvas is triangular. Rows 19..=38
            // share x=14..=53 in the asusctl pixel-image mapping.
            MatrixModel::Ga402 | MatrixModel::Unknown => ContentArea {
                x: 14,
                y: 19,
                width: 40,
                height: 20,
            },
            _ => ContentArea {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: Vec<DisplayProfile>,
    #[serde(default)]
    pub policy: DevicePolicy,
    /// Draw the black tray icons in white, for dark panels.
    #[serde(default)]
    pub invert_tray_icon: bool,
    /// Left click opens the tray menu and middle click toggles the light
    /// show, instead of the other way round.
    #[serde(default)]
    pub tray_menu_on_left_click: bool,
    /// Main window size when it was last closed; `None` until then.
    #[serde(default)]
    pub window: Option<WindowState>,
    #[serde(default)]
    pub triggers: ProfileTriggers,
}

/// Main window geometry, restored on the next open. `width` and `height`
/// are the unmaximized size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: i32,
    pub height: i32,
    #[serde(default)]
    pub maximized: bool,
}

/// Profiles to switch to when the power or lid state changes; `None` does
/// nothing. Entries pointing at deleted profiles are ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileTriggers {
    #[serde(default)]
    pub plugged_in: Option<String>,
    #[serde(default)]
    pub unplugged: Option<String>,
    #[serde(default)]
    pub lid_closed: Option<String>,
    #[serde(default)]
    pub lid_opened: Option<String>,
}

impl ProfileTriggers {
    /// Forgets a deleted profile.
    pub fn forget(&mut self, id: &str) {
        for trigger in [&mut self.plugged_in, &mut self.unplugged, &mut self.lid_closed, &mut self.lid_opened] {
            if trigger.as_deref() == Some(id) {
                *trigger = None;
            }
        }
    }
}

/// Rotation through a profile's elements, one at a time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CycleSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cycle_seconds")]
    pub seconds: u32,
    /// Elements with a play limit stay until they have played that many
    /// times instead of for `seconds`.
    #[serde(default)]
    pub after_animations: bool,
}

impl Default for CycleSettings {
    fn default() -> Self {
        Self { enabled: false, seconds: default_cycle_seconds(), after_animations: false }
    }
}

const fn default_plays() -> u32 {
    1
}

impl Default for AppConfig {
    fn default() -> Self {
        let profile = DisplayProfile::new("Clock", vec![Element::clock()]);
        Self {
            enabled: true,
            active_profile: Some(profile.id.clone()),
            profiles: vec![profile],
            policy: DevicePolicy::default(),
            invert_tray_icon: false,
            tray_menu_on_left_click: false,
            window: None,
            triggers: ProfileTriggers::default(),
        }
    }
}

impl AppConfig {
    pub fn active_profile(&self) -> Option<&DisplayProfile> {
        let id = self.active_profile.as_deref()?;
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub fn validate(&self) -> Result<()> {
        if self.profiles.iter().any(|profile| profile.name.trim().is_empty()) {
            bail!("profile names cannot be empty");
        }
        for profile in &self.profiles {
            profile.validate()?;
        }
        if let Some(id) = &self.active_profile {
            if !self.profiles.iter().any(|profile| &profile.id == id) {
                bail!("active profile does not exist: {id}");
            }
        }
        Ok(())
    }
}

/// A named group of elements drawn together, layered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredProfile")]
pub struct DisplayProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub elements: Vec<Element>,
    /// When enabled, show one element at a time instead of layering them.
    #[serde(default)]
    pub cycle: CycleSettings,
}

impl DisplayProfile {
    pub fn new(name: &str, elements: Vec<Element>) -> Self {
        Self { id: new_id(), name: name.into(), elements, cycle: CycleSettings::default() }
    }

    pub fn validate(&self) -> Result<()> {
        if self.cycle.seconds == 0 {
            bail!("profile '{}' element cycle interval must be at least one second", self.name);
        }
        for element in &self.elements {
            element.validate().with_context(|| format!("profile '{}'", self.name))?;
        }
        Ok(())
    }

    pub fn element_mut(&mut self, id: &str) -> Option<&mut Element> {
        self.elements.iter_mut().find(|element| element.id == id)
    }
}

/// Accepts the current layout and the pre-0.4 one, where each profile was a
/// single element with its fields inline.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredProfile {
    Group {
        id: String,
        name: String,
        elements: Vec<Element>,
        #[serde(default)]
        cycle: CycleSettings,
    },
    Single { id: String, name: String, #[serde(flatten)] kind: ElementKind },
}

impl From<StoredProfile> for DisplayProfile {
    fn from(stored: StoredProfile) -> Self {
        match stored {
            StoredProfile::Group { id, name, elements, cycle } => Self { id, name, elements, cycle },
            StoredProfile::Single { id, name, kind } => {
                let element = Element { id: new_id(), kind };
                Self { id, name, elements: vec![element], cycle: CycleSettings::default() }
            }
        }
    }
}

/// One thing drawn on the matrix: a clock, text, GIF, …
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub id: String,
    #[serde(flatten)]
    pub kind: ElementKind,
}

impl Element {
    pub fn clock() -> Self {
        Self {
            id: new_id(),
            kind: ElementKind::Clock {
                font: default_font(),
                font_size: 15.0,
                use_24_hour: true,
                show_seconds: false,
                show_date: false,
                date_format: "%Y-%m-%d".into(),
                show_millis: false,
                fps: default_clock_fps(),
                y_offset: 0,
                ignore_safe_area: false,
            },
        }
    }

    pub fn text() -> Self {
        Self {
            id: new_id(),
            kind: ElementKind::Text {
                text: "Hello, AniMe Matrix!".into(),
                font: default_font(),
                font_size: 14.0,
                mode: TextMode::Scroll,
                speed: 18.0,
                fps: default_text_fps(),
                ignore_safe_area: false,
                scroll_pause: 0.0,
                direction: ScrollDirection::Left,
                period: default_text_period(),
                y_offset: 0,
                limit_plays: false,
                plays: default_plays(),
            },
        }
    }

    pub fn gif() -> Self {
        Self {
            id: new_id(),
            kind: ElementKind::Gif {
                path: PathBuf::from(crate::assets::DEFAULT_GIF),
                brightness: 1.0,
                contrast: default_contrast(),
                black_level: 0.0,
                fps: 0.0,
                limit_plays: false,
                plays: default_plays(),
                looping: GifLoop::Restart,
                layout: GifLayout::AsIs,
                size: default_sprite_size(),
                mode: TextMode::Scroll,
                direction: ScrollDirection::Left,
                speed: default_sprite_speed(),
                motion_fps: default_motion_fps(),
                period: default_text_period(),
                scroll_pause: 0.0,
                ignore_safe_area: false,
                y_offset: 0,
                overlay_text: String::new(),
                overlay_color: OverlayColor::White,
                overlay_font: default_font(),
                overlay_size: default_overlay_size(),
                overlay_bold: false,
                overlay_italic: false,
                overlay_outline: 0,
            },
        }
    }

    pub fn flashlight() -> Self {
        Self {
            id: new_id(),
            kind: ElementKind::Flashlight { brightness: 1.0 },
        }
    }

    pub fn battery() -> Self {
        Self {
            id: new_id(),
            kind: ElementKind::Battery {
                font: default_font(),
                font_size: 11.0,
                style: BatteryStyle::Classic,
                label: String::new(),
                y_offset: 0,
                ignore_safe_area: false,
            },
        }
    }

    pub fn validate(&self) -> Result<()> {
        match &self.kind {
            ElementKind::Clock { font, font_size, fps, .. } => {
                validate_font(font, *font_size)?;
                if !(1.0..=MAX_FPS).contains(fps) {
                    bail!("clock element FPS must be between 1 and {MAX_FPS}");
                }
            }
            ElementKind::Battery { font, font_size, .. } => {
                validate_font(font, *font_size)?;
            }
            ElementKind::Text {
                text,
                font,
                font_size,
                speed,
                fps,
                scroll_pause,
                period,
                ..
            } => {
                if text.is_empty() {
                    bail!("text element has no text");
                }
                validate_font(font, *font_size)?;
                if *speed <= 0.0 {
                    bail!("text element must have a positive speed");
                }
                if !(1.0..=MAX_FPS).contains(fps) {
                    bail!("text element FPS must be between 1 and {MAX_FPS}");
                }
                if !(0.05..=60.0).contains(period) {
                    bail!("text element animation period must be between 0.05 and 60 seconds");
                }
                if !(0.0..=MAX_SCROLL_PAUSE).contains(scroll_pause) {
                    bail!("text element scroll pause must be between 0 and {MAX_SCROLL_PAUSE} seconds");
                }
            }
            ElementKind::Gif {
                path, brightness, contrast, black_level, fps, size, speed, motion_fps, period, scroll_pause, overlay_text,
                overlay_font, overlay_size, overlay_outline, ..
            } => {
                if *overlay_outline > MAX_OUTLINE {
                    bail!("GIF element outline must be at most {MAX_OUTLINE} pixels");
                }
                if !overlay_text.is_empty() {
                    validate_font(overlay_font, *overlay_size)?;
                }
                if !(1..=MAX_SPRITE_SIZE).contains(size) {
                    bail!("GIF element size must be between 1 and {MAX_SPRITE_SIZE} pixels");
                }
                if *speed <= 0.0 {
                    bail!("GIF element must have a positive speed");
                }
                if !(1.0..=MAX_FPS).contains(motion_fps) {
                    bail!("GIF element movement FPS must be between 1 and {MAX_FPS}");
                }
                if !(0.05..=60.0).contains(period) {
                    bail!("GIF element animation period must be between 0.05 and 60 seconds");
                }
                if !(0.0..=MAX_SCROLL_PAUSE).contains(scroll_pause) {
                    bail!("GIF element pause must be between 0 and {MAX_SCROLL_PAUSE} seconds");
                }
                // An empty or missing file falls back to a bundled GIF.
                if !path.as_os_str().is_empty()
                    && path.extension().and_then(|value| value.to_str()).map(str::to_lowercase)
                        != Some("gif".into())
                {
                    bail!("GIF element must reference a .gif file");
                }
                if !(0.0..=MAX_GIF_BRIGHTNESS).contains(brightness) {
                    bail!("GIF element brightness must be between 0 and {MAX_GIF_BRIGHTNESS}");
                }
                if !(MIN_CONTRAST..=MAX_CONTRAST).contains(contrast) {
                    bail!("GIF element contrast must be between {MIN_CONTRAST} and {MAX_CONTRAST}");
                }
                if !(0.0..=MAX_BLACK_LEVEL).contains(black_level) {
                    bail!("GIF element black level must be between 0 and {MAX_BLACK_LEVEL} percent");
                }
                if !(0.0..=MAX_FPS).contains(fps) {
                    bail!("GIF element FPS must be between 0 and {MAX_FPS}");
                }
            }
            ElementKind::Flashlight { brightness } => {
                if !(0.0..=1.0).contains(brightness) {
                    bail!("flashlight element brightness must be between 0 and 1");
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElementKind {
    Clock {
        font: PathBuf,
        font_size: f32,
        use_24_hour: bool,
        show_seconds: bool,
        show_date: bool,
        date_format: String,
        /// Append milliseconds (implies seconds), redrawn at `fps`.
        #[serde(default)]
        show_millis: bool,
        #[serde(default = "default_clock_fps")]
        fps: f32,
        /// Pixels to move the element down (negative: up).
        #[serde(default)]
        y_offset: i32,
        /// Lay out on the whole canvas instead of the guaranteed-visible area.
        #[serde(default)]
        ignore_safe_area: bool,
    },
    Text {
        text: String,
        font: PathBuf,
        font_size: f32,
        mode: TextMode,
        speed: f32,
        /// Redraw rate while scrolling.
        #[serde(default = "default_text_fps")]
        fps: f32,
        /// Lay out on the whole canvas instead of the guaranteed-visible area.
        #[serde(default)]
        ignore_safe_area: bool,
        /// Seconds between passes: blank after a scroll, holding the full
        /// text after typewriter.
        #[serde(default)]
        scroll_pause: f32,
        /// Scroll/bounce direction.
        #[serde(default)]
        direction: ScrollDirection,
        /// Seconds per blink/pulse/wave cycle, or per typed character.
        #[serde(default = "default_text_period")]
        period: f32,
        /// Pixels to move the element down (negative: up).
        #[serde(default)]
        y_offset: i32,
        /// Whether element cycling waits for `plays` complete animations.
        #[serde(default)]
        limit_plays: bool,
        #[serde(default = "default_plays")]
        plays: u32,
    },
    Gif {
        path: PathBuf,
        /// Gain on every LED: below 1 dims, above 1 boosts (clipping at full).
        brightness: f32,
        /// 1 leaves the GIF as is; above 1 darkens dim pixels and brightens
        /// bright ones, below 1 flattens them. Black and white stay put.
        #[serde(default = "default_contrast")]
        contrast: f32,
        /// Percent of brightness below which pixels turn off; the rest is
        /// stretched back to the full range. Removes haze around shapes.
        #[serde(default)]
        black_level: f32,
        /// Fixed playback rate; 0 keeps the GIF's own frame timing.
        #[serde(default)]
        fps: f32,
        /// Whether element cycling waits for `plays` complete animations.
        #[serde(default)]
        limit_plays: bool,
        #[serde(default = "default_plays")]
        plays: u32,
        /// What happens at the last frame.
        #[serde(default, rename = "loop")]
        looping: GifLoop,
        /// How the GIF is placed on the panel.
        #[serde(default)]
        layout: GifLayout,
        // The rest only applies to `GifLayout::Animate`, where the GIF moves
        // like a single text character; names match the text fields.
        /// Sprite height in pixels.
        #[serde(default = "default_sprite_size")]
        size: u32,
        #[serde(default)]
        mode: TextMode,
        #[serde(default)]
        direction: ScrollDirection,
        #[serde(default = "default_sprite_speed")]
        speed: f32,
        /// Redraw rate for the movement (the GIF's own frames use `fps`).
        #[serde(default = "default_motion_fps")]
        motion_fps: f32,
        #[serde(default = "default_text_period")]
        period: f32,
        #[serde(default)]
        scroll_pause: f32,
        #[serde(default)]
        ignore_safe_area: bool,
        #[serde(default)]
        y_offset: i32,
        /// Optional text drawn over the GIF, centred.
        #[serde(default)]
        overlay_text: String,
        #[serde(default)]
        overlay_color: OverlayColor,
        #[serde(default = "default_font")]
        overlay_font: PathBuf,
        #[serde(default = "default_overlay_size")]
        overlay_size: f32,
        #[serde(default)]
        overlay_bold: bool,
        #[serde(default)]
        overlay_italic: bool,
        /// Border in the opposite color, in pixels (0 = none).
        #[serde(default)]
        overlay_outline: u32,
    },
    /// Every LED on at `brightness`.
    Flashlight {
        brightness: f32,
    },
    /// Battery icon with charge level and percentage.
    Battery {
        font: PathBuf,
        font_size: f32,
        #[serde(default)]
        style: BatteryStyle,
        /// Optional text shown under the gauge.
        #[serde(default)]
        label: String,
        /// Pixels to move the element down (negative: up).
        #[serde(default)]
        y_offset: i32,
        /// Lay out on the whole canvas instead of the guaranteed-visible area.
        #[serde(default)]
        ignore_safe_area: bool,
    },
}

impl ElementKind {
    /// The play count, for animated elements that have one enabled.
    pub fn play_limit(&self) -> Option<u32> {
        match self {
            Self::Text { limit_plays: true, plays, .. } | Self::Gif { limit_plays: true, plays, .. } => {
                Some((*plays).max(1))
            }
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Clock { .. } => "Clock",
            Self::Text { .. } => "Text",
            Self::Gif { .. } => "GIF",
            Self::Flashlight { .. } => "Flashlight",
            Self::Battery { .. } => "Battery",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMode {
    Static,
    #[default]
    Scroll,
    /// Back and forth along the scroll direction's axis.
    Bounce,
    Blink,
    /// Fades in and out.
    Pulse,
    /// Types one character per period, holds, then restarts.
    Typewriter,
    /// Characters ride a sine wave.
    Wave,
}

impl TextMode {
    pub const ALL: [Self; 7] = [
        Self::Static, Self::Scroll, Self::Bounce, Self::Blink, Self::Pulse, Self::Typewriter, Self::Wave,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Static => "Static",
            Self::Scroll => "Scroll",
            Self::Bounce => "Bounce",
            Self::Blink => "Blink",
            Self::Pulse => "Pulse",
            Self::Typewriter => "Typewriter",
            Self::Wave => "Wave",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

impl ScrollDirection {
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Up => "Up",
            Self::Down => "Down",
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Up | Self::Down)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DevicePolicy {
    #[serde(default)]
    pub off_when_unplugged: bool,
    #[serde(default = "default_true")]
    pub off_when_suspended: bool,
    #[serde(default = "default_true")]
    pub off_when_lid_closed: bool,
    /// With `off_when_lid_closed`: keep running while on mains power.
    #[serde(default)]
    pub lid_stay_on_when_plugged: bool,
    /// Seconds to keep animating after the lid closes before blanking, when
    /// `off_when_lid_closed` is set (on battery only, with
    /// `lid_stay_on_when_plugged`).
    #[serde(default)]
    pub lid_close_delay_secs: u32,
    #[serde(default)]
    pub powersave_animation: bool,
    #[serde(default = "default_brightness")]
    pub brightness: String,
    #[serde(default = "default_boot")]
    pub boot_animation: String,
    #[serde(default = "default_awake")]
    pub awake_animation: String,
    #[serde(default = "default_sleep")]
    pub sleep_animation: String,
    #[serde(default = "default_shutdown")]
    pub shutdown_animation: String,
}

impl DevicePolicy {
    /// Whether Animatrix watches the lid itself instead of leaving it to
    /// asusd, which can only turn the panel off immediately.
    pub fn engine_handles_lid(&self) -> bool {
        self.off_when_lid_closed && (self.lid_close_delay_secs > 0 || self.lid_stay_on_when_plugged)
    }
}

impl Default for DevicePolicy {
    fn default() -> Self {
        Self {
            off_when_unplugged: false,
            off_when_suspended: true,
            off_when_lid_closed: true,
            lid_stay_on_when_plugged: false,
            lid_close_delay_secs: 0,
            powersave_animation: false,
            brightness: default_brightness(),
            boot_animation: default_boot(),
            awake_animation: default_awake(),
            sleep_animation: default_sleep(),
            shutdown_animation: default_shutdown(),
        }
    }
}

fn validate_font(path: &Path, size: f32) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("font path cannot be empty");
    }
    if size <= 0.0 {
        bail!("font size must be positive");
    }
    Ok(())
}

fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn default_font() -> PathBuf {
    PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")
}

pub const MAX_SCROLL_PAUSE: f32 = 3600.0;
pub const MAX_SPRITE_SIZE: u32 = 64;
pub const MAX_GIF_BRIGHTNESS: f32 = 2.0;
pub const MIN_CONTRAST: f32 = 0.2;
pub const MAX_CONTRAST: f32 = 4.0;
pub const MAX_BLACK_LEVEL: f32 = 90.0;
pub const MAX_OUTLINE: u32 = 4;

const fn default_contrast() -> f32 {
    1.0
}

const fn default_overlay_size() -> f32 {
    12.0
}

/// Color of a GIF's text overlay: lit LEDs, or LEDs cut out of the GIF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayColor {
    #[default]
    White,
    Black,
}

impl OverlayColor {
    pub const ALL: [Self; 2] = [Self::White, Self::Black];

    pub fn label(self) -> &'static str {
        match self {
            Self::White => "White",
            Self::Black => "Black (cut out)",
        }
    }
}

/// How a battery element draws the charge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatteryStyle {
    /// Battery icon with fill and percentage.
    #[default]
    Classic,
    /// Upright battery with a sloshing liquid surface; bubbles while charging.
    Liquid,
    /// Five cells; a chaser runs while charging, the last cell blinks when low.
    Segments,
    /// Circular gauge; a comet orbits while charging.
    Ring,
    /// Large percentage digits filled up to the charge level.
    Big,
}

impl BatteryStyle {
    pub const ALL: [Self; 5] = [Self::Classic, Self::Liquid, Self::Segments, Self::Ring, Self::Big];

    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Liquid => "Liquid",
            Self::Segments => "Segments",
            Self::Ring => "Ring",
            Self::Big => "Big digits",
        }
    }

    /// Whether the style moves over time and needs regular redraws.
    pub fn is_animated(self) -> bool {
        self != Self::Classic
    }
}

const fn default_sprite_size() -> u32 {
    16
}

const fn default_sprite_speed() -> f32 {
    18.0
}

const fn default_motion_fps() -> f32 {
    10.0
}

/// What a GIF does after its last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GifLoop {
    /// Jump back to the first frame.
    #[default]
    Restart,
    /// Play backwards to the first frame, then forwards again.
    Bounce,
}

impl GifLoop {
    pub const ALL: [Self; 2] = [Self::Restart, Self::Bounce];

    pub fn label(self) -> &'static str {
        match self {
            Self::Restart => "Restart from the beginning",
            Self::Bounce => "Bounce back and forth",
        }
    }
}

/// How a GIF element is placed on the panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GifLayout {
    /// Pixels 1:1, centred; larger GIFs are cropped.
    #[default]
    AsIs,
    /// Scaled up or down to fit the panel, keeping the aspect ratio.
    Fit,
    /// Scaled to fill the panel exactly.
    Stretch,
    /// Moves like a single text character; see the Animate GIF fields.
    Animate,
}

impl GifLayout {
    pub const ALL: [Self; 4] = [Self::AsIs, Self::Fit, Self::Stretch, Self::Animate];

    pub fn label(self) -> &'static str {
        match self {
            Self::AsIs => "Leave as is",
            Self::Fit => "Fit",
            Self::Stretch => "Stretch",
            Self::Animate => "Animate",
        }
    }
}

const fn default_text_period() -> f32 {
    1.0
}

/// Upper bound for every FPS setting; one D-Bus frame takes ~30 ms.
pub const MAX_FPS: f32 = 30.0;

const fn default_clock_fps() -> f32 {
    10.0
}

const fn default_text_fps() -> f32 {
    5.0
}

const fn default_cycle_seconds() -> u32 {
    30
}

const fn default_true() -> bool {
    true
}

fn default_brightness() -> String {
    "med".into()
}

fn default_boot() -> String {
    "GlitchConstruction".into()
}

fn default_awake() -> String {
    "BinaryBannerScroll".into()
}

fn default_sleep() -> String {
    "BannerSwipe".into()
}

fn default_shutdown() -> String {
    "GlitchOut".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_valid_active_clock() {
        let config = AppConfig::default();
        assert!(config.validate().is_ok());
        let elements = &config.active_profile().unwrap().elements;
        assert!(matches!(elements.as_slice(), [Element { kind: ElementKind::Clock { .. }, .. }]));
    }

    #[test]
    fn missing_active_profile_is_rejected() {
        let mut config = AppConfig::default();
        config.active_profile = Some("missing".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn old_global_cycle_setting_is_ignored() {
        let json = r#"{"enabled":true,"active_profile":null,"profiles":[],"cycle":{"enabled":true,"seconds":5}}"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn legacy_single_element_profile_migrates_with_defaults() {
        let json = r#"{"id":"a","name":"T","type":"text","text":"hi","font":"f.ttf",
            "font_size":14.0,"mode":"scroll","speed":18.0}"#;
        let profile: DisplayProfile = serde_json::from_str(json).unwrap();
        assert_eq!((profile.id.as_str(), profile.name.as_str()), ("a", "T"));
        assert!(matches!(
            profile.elements.as_slice(),
            [Element { kind: ElementKind::Text { fps: 5.0, y_offset: 0, .. }, .. }]
        ));
    }

    #[test]
    fn grouped_profile_round_trips() {
        let profile = DisplayProfile::new("Both", vec![Element::clock(), Element::battery()]);
        let json = serde_json::to_string(&profile).unwrap();
        assert_eq!(serde_json::from_str::<DisplayProfile>(&json).unwrap(), profile);
    }

    #[test]
    fn ga402_uses_asusctl_diagonal_canvas() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        assert_eq!(geometry.model, MatrixModel::Ga402);
        assert_eq!((geometry.width, geometry.height), (74, 39));
        assert_eq!(
            geometry.safe_content_area(),
            ContentArea { x: 14, y: 19, width: 40, height: 20 }
        );
    }
}
